//! Foreign-exchange rate lookup, used to convert a charge into its target
//! Firefly account's currency before booking.
//!
//! Firefly forces a withdrawal's currency to the *source account's* currency.
//! When a charge arrives in a different currency (a JPY/EUR/KRW Banco Popular
//! charge routed to the USD account, say), booking the foreign *number* as if
//! it were the account currency mis-states the ledger (¥5,130 → $5,130). We
//! instead convert to the account currency and attach the original as Firefly's
//! "foreign amount". This module resolves the conversion *rate*; the arithmetic
//! and payload shaping live in [`crate::firefly`].
//!
//! Rates come from Frankfurter (<https://api.frankfurter.app>), an ECB-backed,
//! key-free API. It auto-snaps weekends/holidays to the nearest prior business
//! day, so any reasonable transaction date resolves. The response shape is:
//!
//! ```json
//! {"amount":1.0,"base":"USD","date":"2026-05-27","rates":{"EUR":0.92}}
//! ```
//!
//! A missing/unsupported currency or a non-200 is an `Err`: the caller routes
//! the message to Review rather than booking at a wrong (or zero) amount.
//!
//! Design for testability: the network-free pieces — the `from == to`
//! short-circuit and the JSON→[`Decimal`] parse — are pure functions
//! ([`is_identity`], [`parse_rate`]) exercised directly in unit tests. The
//! per-`(from,to,date)` cache also doubles as a test seam:
//! [`FxClient::with_seeded_rate`] pre-populates it so downstream tests
//! (e.g. `firefly`) get a deterministic rate with no live call.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{NaiveDate, Utc};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize};

/// How long a cached rate for *today* (or a future date) stays usable before it
/// is re-fetched. Past-date rates are immutable and never expire (see
/// [`cache_entry_fresh`]); this TTL only bounds staleness of the still-moving
/// current-day rate. 15 minutes balances freshness against not hammering the
/// provider when the same date is touched repeatedly across runs.
const FX_CACHE_TTL_SECS: i64 = 15 * 60;

/// Default Frankfurter base URL. Key-free, ECB rates. The historical `.app`
/// host 301-redirects here; we pin the `.dev` base so no redirect-following is
/// required. Mirrors [`crate::config`]'s `DEFAULT_FX_URL`.
pub const DEFAULT_FX_URL: &str = "https://api.frankfurter.dev/v1";

// A rate-lookup failure, classified by whether retrying could help.
// Transient = 5xx/408/429/network/timeout (defer + retry next run).
// Permanent = 4xx/parse/unsupported-currency (route to Review).
crate::transient::define_provider_error!(RateError, "rate");

/// Cache key: the (from, to, date) triple a rate is requested for. `from`/`to`
/// are stored upper-cased so lookups are case-insensitive.
type CacheKey = (String, String, NaiveDate);

/// A cached rate plus when it was fetched (unix seconds), so the TTL on
/// current-day rates can be enforced. Past-date rates ignore `fetched_at`
/// (immutable). See [`cache_entry_fresh`].
#[derive(Clone, Copy)]
struct CacheEntry {
    rate: Decimal,
    fetched_at: i64,
}

/// On-disk form of one cache entry. The in-memory cache keys on a non-string
/// tuple (which JSON can't use as an object key), so the file is a flat list of
/// these records. `from`/`to`/`date` reconstruct the [`CacheKey`].
#[derive(Serialize, Deserialize)]
struct PersistedRate {
    from: String,
    to: String,
    date: NaiveDate,
    rate: Decimal,
    fetched_at: i64,
}

/// Whether a cached rate is still usable. A past-date rate is immutable (an ECB
/// daily close or a historical bank rate never changes) → cached indefinitely; a
/// rate for today or a future date can still move, so it expires after
/// [`FX_CACHE_TTL_SECS`]. Pure — `today`/`now` are injected so the TTL boundary
/// is unit-testable without a clock.
fn cache_entry_fresh(entry_date: NaiveDate, fetched_at: i64, today: NaiveDate, now: i64) -> bool {
    entry_date < today || now.saturating_sub(fetched_at) < FX_CACHE_TTL_SECS
}

/// Load a persisted FX cache from `path`. A missing/unreadable/corrupt file is
/// not an error — the cache is an optimization, so any failure yields an empty
/// map and the run proceeds (re-fetching as needed).
fn load_cache(path: &str) -> HashMap<CacheKey, CacheEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(entries) = serde_json::from_str::<Vec<PersistedRate>>(&text) else {
        return HashMap::new();
    };
    entries
        .into_iter()
        .map(|p| {
            (
                (
                    p.from.to_ascii_uppercase(),
                    p.to.to_ascii_uppercase(),
                    p.date,
                ),
                CacheEntry {
                    rate: p.rate,
                    fetched_at: p.fetched_at,
                },
            )
        })
        .collect()
}

/// Async FX-rate client over the shared reqwest client, with an in-process
/// per-`(from,to,date)` cache so a batch with repeated currency pairs hits the
/// network at most once per distinct triple.
pub struct FxClient<'a> {
    http: &'a Client,
    fx_url: String,
    cache: Mutex<HashMap<CacheKey, CacheEntry>>,
    /// Path of the persistent cache file, when one is attached via
    /// [`with_cache_file`](Self::with_cache_file). `None` → in-memory only (the
    /// default, used by tests and any deployment without a `/state` volume).
    cache_path: Option<String>,
    /// Optional DOP-rate override. Frankfurter has no Dominican Peso, so when
    /// either side of a conversion is `DOP` this provider (Banco Popular's
    /// `consultaTasa`) is consulted instead. `None` → DOP conversions fall
    /// through to Frankfurter and fail → the caller routes to Review.
    dop: Option<DopRate<'a>>,
}

impl<'a> FxClient<'a> {
    /// Build a client against `fx_url` (e.g. [`DEFAULT_FX_URL`]).
    pub fn new(http: &'a Client, fx_url: impl Into<String>) -> Self {
        Self {
            http,
            fx_url: fx_url.into(),
            cache: Mutex::new(HashMap::new()),
            cache_path: None,
            dop: None,
        }
    }

    /// Attach a persistent on-disk cache at `path`, pre-loading any rates it
    /// already holds. Builder so the common in-memory path stays a plain
    /// [`new`](Self::new). Survives across the hourly one-shot runs, so a date's
    /// rate is fetched once and then reused instead of re-hitting the provider
    /// every run a statement sits in the INBOX.
    #[must_use]
    pub fn with_cache_file(mut self, path: impl Into<String>) -> Self {
        let path = path.into();
        let loaded = load_cache(&path);
        if !loaded.is_empty() {
            *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = loaded;
        }
        self.cache_path = Some(path);
        self
    }

    /// Write the current cache to its file, if one is configured (else a no-op).
    /// Called once at end-of-run. The cache is a pure optimization — never a
    /// correctness dependency — so the caller logs a write failure rather than
    /// failing the run.
    pub fn persist(&self) -> Result<()> {
        let Some(path) = &self.cache_path else {
            return Ok(());
        };
        let entries: Vec<PersistedRate> = {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache
                .iter()
                .map(|((from, to, date), e)| PersistedRate {
                    from: from.clone(),
                    to: to.clone(),
                    date: *date,
                    rate: e.rate,
                    fetched_at: e.fetched_at,
                })
                .collect()
        };
        crate::config::ensure_parent_dir(path, "FX cache")?;
        let json = serde_json::to_string(&entries).context("serializing FX cache")?;
        std::fs::write(path, json).with_context(|| format!("writing FX cache {path}"))?;
        Ok(())
    }

    /// Insert a freshly-resolved rate into the cache, stamped with the current
    /// time so the per-day TTL can later expire a today/future rate.
    fn cache_put(&self, key: CacheKey, rate: Decimal) {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).insert(
            key,
            CacheEntry {
                rate,
                fetched_at: Utc::now().timestamp(),
            },
        );
    }

    /// Attach a Banco Popular DOP-rate provider, used for any conversion where
    /// one side is `DOP`. Builder so the common (DOP-less) path stays a plain
    /// [`new`](Self::new).
    pub fn with_dop(mut self, dop: DopRate<'a>) -> Self {
        self.dop = Some(dop);
        self
    }

    /// Resolve the conversion rate from `from` to `to` on `date`: multiply an
    /// amount in `from` by this to get the amount in `to`.
    ///
    /// - `from == to` (case-insensitive) short-circuits to [`Decimal::ONE`] with
    ///   no network call.
    /// - Otherwise the cache is consulted; a miss does one GET, caches, returns.
    /// - A missing/unsupported currency or non-200 is an `Err`.
    pub async fn rate(&self, from: &str, to: &str, date: NaiveDate) -> Result<Decimal> {
        if is_identity(from, to) {
            return Ok(Decimal::ONE);
        }

        let from = from.trim().to_ascii_uppercase();
        let to = to.trim().to_ascii_uppercase();
        let key: CacheKey = (from.clone(), to.clone(), date);

        // Cache hit — no network, when the entry is still fresh (past-date rates
        // never expire; a today/future rate expires after the TTL). Scoped lock:
        // released before any await. A poisoned lock is recovered (a panic in
        // another batch item must not sink the whole run); the cached data is
        // plain values, never a half-written invariant.
        {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = cache.get(&key)
                && cache_entry_fresh(
                    key.2,
                    entry.fetched_at,
                    Utc::now().date_naive(),
                    Utc::now().timestamp(),
                )
            {
                return Ok(entry.rate);
            }
        }

        // DOP override: Frankfurter has no Dominican Peso. When one side is DOP
        // and a provider is configured, resolve via Banco Popular's `venta`
        // (sell) rate — DOP per 1 unit of the foreign currency — and invert for
        // direction. No provider → fall through to Frankfurter (which lacks DOP,
        // so it errors and the caller routes to Review).
        if (from == "DOP" || to == "DOP")
            && let Some(dop) = &self.dop
        {
            let foreign = if from == "DOP" { &to } else { &from };
            let venta = dop
                .dop_per_unit(foreign)
                .await
                .map_err(anyhow::Error::new)
                .with_context(|| format!("resolving DOP rate for {from}->{to} on {date}"))?;
            let rate = if from == "DOP" {
                Decimal::ONE / venta
            } else {
                venta
            };
            self.cache_put(key, rate);
            return Ok(rate);
        }

        let url = format!(
            "{}/{}?from={}&to={}",
            self.fx_url.trim_end_matches('/'),
            date.format("%Y-%m-%d"),
            from,
            to
        );

        let resp = self
            .http
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send() // frankfurter rate fetch
            .await
            .map_err(|e| {
                anyhow::Error::new(RateError::Transient(format!(
                    "requesting FX rate {from}->{to} on {date}: {e}"
                )))
            })?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let msg = format!("FX provider returned {status} for {from}->{to} on {date}: {body}");
            return Err(anyhow::Error::new(classify_status(status, msg)));
        }

        let rate = parse_rate(&body, &to)
            .with_context(|| format!("parsing FX rate {from}->{to} on {date}"))?;

        self.cache_put(key, rate);
        Ok(rate)
    }

    /// Test seam: a client whose cache is pre-seeded with one
    /// `(from,to,date) → rate` entry, so [`rate`](Self::rate) returns it without
    /// any network call. Keeps `firefly`'s foreign-amount tests deterministic.
    #[cfg(test)]
    pub fn with_seeded_rate(
        http: &'a Client,
        from: &str,
        to: &str,
        date: NaiveDate,
        rate: Decimal,
    ) -> Self {
        let mut cache = HashMap::new();
        cache.insert(
            (
                from.trim().to_ascii_uppercase(),
                to.trim().to_ascii_uppercase(),
                date,
            ),
            // `fetched_at: i64::MAX` makes the seeded entry never expire under the
            // TTL, so a fixture seeded on today's date still returns without a
            // network call regardless of the wall clock.
            CacheEntry {
                rate,
                fetched_at: i64::MAX,
            },
        );
        Self {
            http,
            fx_url: DEFAULT_FX_URL.to_string(),
            cache: Mutex::new(cache),
            cache_path: None,
            dop: None,
        }
    }
}

/// Whether `from` and `to` are the same currency (case-insensitive, trimmed),
/// in which case the rate is exactly one and no lookup is needed. Pure.
fn is_identity(from: &str, to: &str) -> bool {
    from.trim().eq_ignore_ascii_case(to.trim())
}

/// The Frankfurter response envelope. Only `rates` matters for our purposes;
/// `amount`/`base`/`date` are deserialized loosely (ignored) by leaving them
/// off — serde ignores unknown fields by default.
#[derive(Deserialize)]
struct FrankfurterResponse {
    rates: HashMap<String, Decimal>,
}

/// Parse a Frankfurter JSON body into the rate for `to`. Pure — no I/O — so the
/// extraction and decimal conversion are unit-testable without a live API.
///
/// Errors when the body is not the expected shape or when `to` is absent from
/// `rates` (an unsupported/typo'd currency), so the caller can route to Review.
fn parse_rate(body: &str, to: &str) -> Result<Decimal> {
    let parsed: FrankfurterResponse =
        serde_json::from_str(body).context("decoding Frankfurter JSON")?;
    let to_upper = to.trim().to_ascii_uppercase();
    // Frankfurter keys `rates` by upper-case ISO code; match case-insensitively
    // to be tolerant of any provider quirk.
    parsed
        .rates
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(&to_upper))
        .map(|(_, v)| *v)
        .ok_or_else(|| anyhow!("rate for {to_upper:?} missing from response rates"))
}

/// Banco Popular DOP exchange-rate provider (`BPDConsultaTasa`). Frankfurter
/// has no Dominican Peso, so this fronts the bank's own `consultaTasa` endpoint
/// for any DOP↔{USD,EUR} conversion.
///
/// Auth is IBM API Connect: an OAuth2 *client-credentials* token (the
/// "application" flow) **plus** the `X-IBM-Client-Id` header — both are required
/// together. The rate table is "latest" (no historical date parameter), so it is
/// fetched once and cached for the process lifetime; the ledger's hourly cron
/// makes a single token + rates call per run regardless of how many DOP rows or
/// dates it touches (well under the 50/day sandbox quota).
///
/// `venta` (the bank's sell rate, DOP per 1 unit of the foreign currency) is
/// used: for the USD-equivalent ceiling it is the conservative choice (yields the
/// smaller USD value, so it won't false-trip the gate).
pub struct DopRate<'a> {
    http: &'a Client,
    /// Full `consultaTasa` URL (host + basePath + `/consultaTasa`).
    rates_url: String,
    /// OAuth2 token endpoint (client-credentials grant).
    token_url: String,
    client_id: String,
    client_secret: String,
    scope: String,
    /// In-run retry budget: on a transient failure the token+rates fetch is
    /// retried with exponential backoff until this elapses, then it gives up
    /// (the caller defers — the hourly cron is the outer retry loop).
    retry_budget: Duration,
    /// Process-lifetime memo of the load outcome (success or a typed failure), so
    /// the retry budget is spent at most once per run and every DOP item in the
    /// same run sees the same verdict.
    table: Mutex<Option<TableState>>,
}

/// Memoized outcome of loading the rate table once per run.
enum TableState {
    Loaded(HashMap<String, Decimal>),
    Failed(RateError),
}

impl<'a> DopRate<'a> {
    /// Build a provider. URLs default in [`crate::config`]; the credentials are a
    /// SealedSecret in deployment. `retry_budget` bounds the in-run backoff.
    pub fn new(
        http: &'a Client,
        rates_url: impl Into<String>,
        token_url: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        scope: impl Into<String>,
        retry_budget: Duration,
    ) -> Self {
        Self {
            http,
            rates_url: rates_url.into(),
            token_url: token_url.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            scope: scope.into(),
            retry_budget,
            table: Mutex::new(None),
        }
    }

    /// `venta` (DOP per 1 unit) for `currency`. Loads + memoizes the table on the
    /// first call (with bounded retry on transient outages). A
    /// [`RateError::Transient`] tells the caller to defer; [`RateError::Permanent`]
    /// (incl. an unsupported currency) routes to Review.
    async fn dop_per_unit(&self, currency: &str) -> std::result::Result<Decimal, RateError> {
        let cur = currency.trim().to_ascii_uppercase();

        // Memo hit — no network. Scoped lock, released before any await.
        if let Some(state) = self
            .table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return Self::resolve(state, &cur);
        }

        let state = self.load_table_retrying().await;
        let result = Self::resolve(&state, &cur);
        *self.table.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
        result
    }

    /// Look one currency up in a memoized [`TableState`].
    fn resolve(state: &TableState, cur: &str) -> std::result::Result<Decimal, RateError> {
        match state {
            TableState::Loaded(table) => table.get(cur).copied().ok_or_else(|| {
                RateError::Permanent(format!(
                    "Banco Popular publishes no '{cur}' rate (only USD/EUR)"
                ))
            }),
            TableState::Failed(e) => Err(e.clone()),
        }
    }

    /// Load the rate table, retrying transient failures with exponential backoff
    /// until [`retry_budget`](Self::retry_budget) elapses. A permanent failure
    /// returns immediately. Never panics — always resolves to a [`TableState`].
    async fn load_table_retrying(&self) -> TableState {
        let start = Instant::now();
        let mut backoff = Duration::from_secs(1);
        let cap = Duration::from_secs(30);
        loop {
            match self.try_load_table().await {
                Ok(table) => return TableState::Loaded(table),
                Err(e @ RateError::Permanent(_)) => return TableState::Failed(e),
                Err(e @ RateError::Transient(_)) => {
                    if start.elapsed() + backoff >= self.retry_budget {
                        return TableState::Failed(e);
                    }
                    tracing::warn!(error = %e, "DOP rate transient failure; retrying after backoff");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(cap);
                }
            }
        }
    }

    /// One token + rates fetch + parse. 5xx/408/429/network → transient;
    /// 4xx/parse → permanent.
    async fn try_load_table(&self) -> std::result::Result<HashMap<String, Decimal>, RateError> {
        let token = self.fetch_token().await?;
        let body = self.fetch_rates(&token).await?;
        parse_rate_table(&body)
            .map_err(|e| RateError::Permanent(format!("parsing consultaTasa: {e}")))
    }

    /// Mint an OAuth2 access token via the client-credentials grant.
    async fn fetch_token(&self) -> std::result::Result<String, RateError> {
        let form = serde_urlencoded::to_string([
            ("grant_type", "client_credentials"),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("scope", self.scope.as_str()),
        ])
        .map_err(|e| RateError::Permanent(format!("encoding DOP token request body: {e}")))?;
        let body = self
            .send_classified(
                self.http
                    .post(&self.token_url)
                    .header(
                        reqwest::header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    )
                    .header(reqwest::header::ACCEPT, "application/json")
                    .body(form),
                "DOP token endpoint",
                // The token endpoint's error body can echo client-credentials
                // detail — never embed it in a logged error (it surfaces via
                // `warn!(error = %e)`). Status only.
                false,
            )
            .await?;
        parse_token(&body)
            .map_err(|e| RateError::Permanent(format!("decoding DOP token response: {e}")))
    }

    /// Fetch the raw `consultaTasa` body with the bearer token + client-id header
    /// (IBM API Connect requires both).
    async fn fetch_rates(&self, token: &str) -> std::result::Result<String, RateError> {
        self.send_classified(
            self.http
                .get(&self.rates_url)
                .bearer_auth(token)
                .header("X-IBM-Client-Id", self.client_id.as_str())
                .header(reqwest::header::ACCEPT, "application/json"),
            "DOP rates endpoint",
            // Rates error body is just rate JSON (no secret) — safe to include.
            true,
        )
        .await
    }

    /// Send a request and return the body, mapping network/timeout errors and
    /// non-success statuses to a classified [`RateError`].
    async fn send_classified(
        &self,
        req: reqwest::RequestBuilder,
        what: &str,
        include_body_in_error: bool,
    ) -> std::result::Result<String, RateError> {
        let resp = req
            .send()
            .await
            .map_err(|e| RateError::Transient(format!("{what}: {e}")))?;
        let http_status = resp.status(); // classified-send status
        let body = resp.text().await.unwrap_or_default();
        if http_status.is_success() {
            return Ok(body);
        }
        // The body is omitted for endpoints whose error response could echo a
        // secret (the OAuth token endpoint) — these errors are logged verbatim
        // via `warn!(error = %e)`, so a leaked body would reach Loki.
        let msg = if include_body_in_error {
            format!("{what} returned {http_status}: {}", body.trim())
        } else {
            format!("{what} returned {http_status}")
        };
        Err(classify_status(http_status, msg))
    }
}

/// Extract `access_token` from an OAuth2 token response. Pure.
fn parse_token(body: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct Token {
        access_token: String,
    }
    let token: Token = serde_json::from_str(body).context("decoding OAuth token JSON")?;
    if token.access_token.trim().is_empty() {
        bail!("OAuth token response had an empty access_token");
    }
    Ok(token.access_token)
}

/// Parse a `consultaTasa` body into `currency → venta` (DOP per unit). Pure — no
/// I/O — so the extraction is unit-testable without a live API. Rows with a
/// non-positive `venta` are skipped (a zero rate would divide-by-zero); an empty
/// result is an error so the caller routes to Review.
fn parse_rate_table(body: &str) -> Result<HashMap<String, Decimal>> {
    #[derive(Deserialize)]
    struct Resp {
        monedas: Monedas,
    }
    #[derive(Deserialize)]
    struct Monedas {
        moneda: Vec<Moneda>,
    }
    #[derive(Deserialize)]
    struct Moneda {
        descripcion: String,
        // Schema types this `integer`, but real values are decimals (e.g. 56.95)
        // and may arrive as a JSON number or string — decode both losslessly.
        #[serde(deserialize_with = "de_decimal")]
        venta: Decimal,
    }
    let resp: Resp = serde_json::from_str(body).context("decoding consultaTasa JSON")?;
    let mut table = HashMap::new();
    for m in resp.monedas.moneda {
        if m.venta <= Decimal::ZERO {
            continue;
        }
        table.insert(m.descripcion.trim().to_ascii_uppercase(), m.venta);
    }
    if table.is_empty() {
        bail!("consultaTasa response carried no usable rates");
    }
    Ok(table)
}

/// Deserialize a [`Decimal`] from either a JSON number or a JSON string,
/// losslessly (via the textual form — never through `f64`).
fn de_decimal<'de, D>(de: D) -> Result<Decimal, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;
    let text = match serde_json::Value::deserialize(de)? {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s,
        other => {
            return Err(D::Error::custom(format!(
                "expected number or string, got {other}"
            )));
        }
    };
    Decimal::from_str(text.trim()).map_err(D::Error::custom)
}

#[cfg(test)]
impl<'a> DopRate<'a> {
    /// Test seam: a provider whose rate table is pre-seeded, so
    /// [`dop_per_unit`](Self::dop_per_unit) (and thus `FxClient::rate` for DOP)
    /// returns deterministically with no network call.
    fn with_seeded_table(http: &'a Client, currency: &str, venta: Decimal) -> Self {
        let mut table = HashMap::new();
        table.insert(currency.trim().to_ascii_uppercase(), venta);
        Self {
            http,
            rates_url: String::new(),
            token_url: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            scope: String::new(),
            retry_budget: Duration::ZERO,
            table: Mutex::new(Some(TableState::Loaded(table))),
        }
    }
}

// -- fx unit tests (rate lookup, cache, DOP provider) --
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{dec, test_runtime};

    #[test]
    fn identity_is_case_and_whitespace_insensitive() {
        assert!(is_identity("USD", "USD"));
        assert!(is_identity("usd", "USD"));
        assert!(is_identity(" usd ", "Usd"));
        assert!(!is_identity("USD", "EUR"));
    }

    #[test]
    fn rate_short_circuits_for_same_currency() {
        // No network: a same-currency lookup returns ONE regardless of the
        // (never-contacted) URL. Exercised via the public async method on a
        // throwaway client.
        let http = Client::new();
        let fx = FxClient::new(&http, "http://fx.invalid");
        let date = NaiveDate::from_ymd_opt(2026, 5, 27).unwrap();
        let r = test_runtime().block_on(fx.rate("USD", "usd", date)).unwrap();
        assert_eq!(r, Decimal::ONE);
    }

    #[test]
    fn parses_rate_from_frankfurter_body() {
        let body = r#"{"amount":1.0,"base":"USD","date":"2026-05-27","rates":{"EUR":0.92}}"#;
        assert_eq!(parse_rate(body, "EUR").unwrap(), dec("0.92"));
    }

    #[test]
    fn parse_rate_is_case_insensitive_on_target() {
        let body = r#"{"amount":1.0,"base":"USD","date":"2026-05-27","rates":{"JPY":143.5}}"#;
        assert_eq!(parse_rate(body, "jpy").unwrap(), dec("143.5"));
    }

    #[test]
    fn parse_rate_errors_when_target_currency_absent() {
        // An unsupported/typo'd currency must error so the caller routes to
        // Review rather than booking at a missing rate.
        let body = r#"{"amount":1.0,"base":"USD","date":"2026-05-27","rates":{"EUR":0.92}}"#;
        assert!(parse_rate(body, "XYZ").is_err());
    }

    #[test]
    fn parse_rate_errors_on_malformed_body() {
        assert!(parse_rate("not json", "EUR").is_err());
        assert!(parse_rate(r#"{"unexpected":true}"#, "EUR").is_err());
    }

    #[test]
    fn parses_consultatasa_rate_table() {
        // Real shape: integer `compra`, decimal `venta`, multiple currencies.
        let body = r#"{"monedas":{"moneda":[
            {"descripcion":"USD","compra":55,"venta":56.95},
            {"descripcion":"EUR","compra":60,"venta":62.5}
        ]}}"#;
        let t = parse_rate_table(body).unwrap();
        assert_eq!(t.get("USD"), Some(&dec("56.95")));
        assert_eq!(t.get("EUR"), Some(&dec("62.5")));
    }

    #[test]
    fn rate_table_tolerates_string_numbers_and_skips_nonpositive() {
        let body = r#"{"monedas":{"moneda":[
            {"descripcion":"USD","compra":"55","venta":"56.95"},
            {"descripcion":"BAD","compra":0,"venta":0}
        ]}}"#;
        let tbl = parse_rate_table(body).unwrap(); // string-number tolerance
        assert_eq!(tbl.get("USD"), Some(&dec("56.95")));
        assert!(!tbl.contains_key("BAD"), "non-positive rate row is skipped");
    }

    #[test]
    fn rate_table_errors_on_empty_or_garbage() {
        assert!(parse_rate_table("not json").is_err());
        assert!(parse_rate_table(r#"{"monedas":{"moneda":[]}}"#).is_err());
    }

    #[test]
    fn parses_oauth_token_and_rejects_empty() {
        let ok = r#"{"access_token":"abc.def","token_type":"Bearer","expires_in":3600}"#;
        assert_eq!(parse_token(ok).unwrap(), "abc.def");
        assert!(parse_token(r#"{"access_token":""}"#).is_err());
        assert!(parse_token("{}").is_err());
    }

    #[test]
    fn dop_rate_inverts_by_direction() {
        // `venta` is DOP per 1 USD. USD→DOP is venta; DOP→USD is its reciprocal.
        let http = Client::new();
        let venta = dec("56.95");
        let date = NaiveDate::from_ymd_opt(2026, 5, 27).unwrap();
        let fx = FxClient::new(&http, "http://fx.invalid")
            .with_dop(DopRate::with_seeded_table(&http, "USD", venta));
        let rt = test_runtime();
        assert_eq!(rt.block_on(fx.rate("USD", "DOP", date)).unwrap(), venta);
        assert_eq!(
            rt.block_on(fx.rate("DOP", "usd", date)).unwrap(),
            Decimal::ONE / venta
        );
        // A currency the bank doesn't publish errors → caller routes to Review.
        assert!(rt.block_on(fx.rate("DOP", "JPY", date)).is_err());
    }

    crate::transient::define_classify_assertions!(classify_status, RateError);

    #[test]
    fn classify_status_transient_vs_permanent() {
        use reqwest::StatusCode;
        // 521 (Cloudflare origin down), 500, 429, 408 → transient (retry).
        assert_transient(StatusCode::from_u16(521).unwrap());
        assert_transient(StatusCode::INTERNAL_SERVER_ERROR);
        assert_transient(StatusCode::TOO_MANY_REQUESTS);
        assert_transient(StatusCode::REQUEST_TIMEOUT);
        // Auth, client, and forbidden errors are permanent (route to Review).
        assert_permanent(StatusCode::BAD_REQUEST);
        assert_permanent(StatusCode::UNAUTHORIZED);
        assert_permanent(StatusCode::FORBIDDEN);
    }

    #[test]
    fn transient_chain_propagation() {
        crate::transient::assert_transient_chain!(is_transient, RateError);
    }

    #[test]
    fn seeded_rate_returns_without_network() {
        let http = Client::new();
        let may_27 = NaiveDate::from_ymd_opt(2026, 5, 27).unwrap();
        let fx = FxClient::with_seeded_rate(&http, "JPY", "USD", may_27, dec("0.0064"));
        // Case-insensitive lookup and seeded value round-trip.
        assert_eq!(
            test_runtime().block_on(fx.rate("jpy", "USD", may_27)).unwrap(),
            dec("0.0064"),
        );
    }

    // --- persistent cache + TTL ------------------------------------------

    #[test]
    fn cache_entry_fresh_past_date_never_expires() {
        let today = NaiveDate::from_ymd_opt(2026, 5, 31).unwrap();
        let past = NaiveDate::from_ymd_opt(2026, 5, 12).unwrap();
        // A past-date rate is immutable → fresh even with an ancient fetched_at
        // far beyond the TTL.
        assert!(cache_entry_fresh(past, 0, today, 10_000_000_000));
    }

    #[test]
    fn cache_entry_fresh_today_and_future_respect_ttl() {
        let today = NaiveDate::from_ymd_opt(2026, 5, 31).unwrap();
        let now = 1_000_000i64;
        // Today, fetched 5 min ago (< 15-min TTL) → fresh.
        assert!(cache_entry_fresh(today, now - 5 * 60, today, now));
        // Today, fetched 20 min ago (> TTL) → stale (re-fetch).
        assert!(!cache_entry_fresh(today, now - 20 * 60, today, now));
        // A future date is bound by the TTL just like today.
        let future = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        assert!(!cache_entry_fresh(future, now - 20 * 60, today, now));
    }

    #[test]
    fn fx_cache_persists_and_reloads_round_trip() {
        // persist() then load_cache() must round-trip an entry by (from,to,date).
        let path = std::env::temp_dir()
            .join("receipt-ledger-fxcache-roundtrip.json")
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_file(&path);

        let http = Client::new();
        let key = (
            "USD".to_string(),
            "EUR".to_string(),
            NaiveDate::from_ymd_opt(2026, 5, 12).unwrap(),
        );
        let fx = FxClient::new(&http, DEFAULT_FX_URL).with_cache_file(&path);
        fx.cache_put(key.clone(), dec("0.92"));
        fx.persist().expect("persist writes the cache file");

        let loaded = load_cache(&path);
        assert_eq!(
            loaded.get(&key).map(|e| e.rate),
            Some(dec("0.92")),
            "persisted rate reloads under the same key"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_cache_missing_file_is_empty_not_error() {
        // The cache is an optimization: a missing/unreadable file yields an empty
        // map and the run proceeds (no panic, no error).
        assert!(load_cache("/nonexistent/receipt-ledger/fx-cache.json").is_empty());
    }
}
