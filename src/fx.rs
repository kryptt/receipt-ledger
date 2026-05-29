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

use anyhow::{Context, Result, anyhow, bail};
use chrono::NaiveDate;
use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer};

/// Default Frankfurter base URL. Key-free, ECB rates. The historical `.app`
/// host 301-redirects here; we pin the `.dev` base so no redirect-following is
/// required. Mirrors [`crate::config`]'s `DEFAULT_FX_URL`.
pub const DEFAULT_FX_URL: &str = "https://api.frankfurter.dev/v1";

/// Cache key: the (from, to, date) triple a rate is requested for. `from`/`to`
/// are stored upper-cased so lookups are case-insensitive.
type CacheKey = (String, String, NaiveDate);

/// Async FX-rate client over the shared reqwest client, with an in-process
/// per-`(from,to,date)` cache so a batch with repeated currency pairs hits the
/// network at most once per distinct triple.
pub struct FxClient<'a> {
    http: &'a Client,
    fx_url: String,
    cache: Mutex<HashMap<CacheKey, Decimal>>,
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
            dop: None,
        }
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

        // Cache hit — no network. Scoped lock: released before any await. A
        // poisoned lock is recovered (a panic in another batch item must not
        // sink the whole run); the cached data is plain values, never a
        // half-written invariant.
        if let Some(rate) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return Ok(*rate);
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
                .with_context(|| format!("resolving DOP rate for {from}->{to} on {date}"))?;
            let rate = if from == "DOP" {
                Decimal::ONE / venta
            } else {
                venta
            };
            self.cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, rate);
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
            .send()
            .await
            .with_context(|| format!("requesting FX rate {from}->{to} on {date}"))?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!(
                "FX provider returned {status} for {from}->{to} on {date}: {body}"
            ));
        }

        let rate = parse_rate(&body, &to)
            .with_context(|| format!("parsing FX rate {from}->{to} on {date}"))?;

        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, rate);
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
            rate,
        );
        Self {
            http,
            fx_url: DEFAULT_FX_URL.to_string(),
            cache: Mutex::new(cache),
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
    /// Process-lifetime cache: currency (upper-case) → `venta` (DOP per unit).
    /// `None` until the first successful fetch.
    table: Mutex<Option<HashMap<String, Decimal>>>,
}

impl<'a> DopRate<'a> {
    /// Build a provider. URLs default in [`crate::config`]; the credentials are a
    /// SealedSecret in deployment.
    pub fn new(
        http: &'a Client,
        rates_url: impl Into<String>,
        token_url: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        scope: impl Into<String>,
    ) -> Self {
        Self {
            http,
            rates_url: rates_url.into(),
            token_url: token_url.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            scope: scope.into(),
            table: Mutex::new(None),
        }
    }

    /// `venta` (DOP per 1 unit) for `currency`, fetching+caching the table on the
    /// first call. Errors for a currency the bank does not publish (only USD/EUR)
    /// so the caller routes to Review rather than booking at a missing rate.
    async fn dop_per_unit(&self, currency: &str) -> Result<Decimal> {
        let cur = currency.trim().to_ascii_uppercase();

        // Cache hit — no network. Scoped lock, released before any await.
        if let Some(table) = self
            .table
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return table
                .get(&cur)
                .copied()
                .ok_or_else(|| anyhow!("Banco Popular publishes no '{cur}' rate (only USD/EUR)"));
        }

        let token = self.fetch_token().await?;
        let body = self.fetch_rates(&token).await?;
        let table = parse_rate_table(&body).context("parsing consultaTasa rates")?;
        let resolved = table.get(&cur).copied();
        *self.table.lock().unwrap_or_else(|e| e.into_inner()) = Some(table);
        resolved.ok_or_else(|| anyhow!("Banco Popular publishes no '{cur}' rate (only USD/EUR)"))
    }

    /// Mint an OAuth2 access token via the client-credentials grant.
    async fn fetch_token(&self) -> Result<String> {
        let form = serde_urlencoded::to_string([
            ("grant_type", "client_credentials"),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("scope", self.scope.as_str()),
        ])
        .context("encoding DOP token request body")?;
        let resp = self
            .http
            .post(&self.token_url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .body(form)
            .send()
            .await
            .context("requesting DOP OAuth token")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("DOP token endpoint returned {status}: {body}"));
        }
        parse_token(&body).context("decoding DOP OAuth token response")
    }

    /// Fetch the raw `consultaTasa` body with the bearer token + client-id header
    /// (IBM API Connect requires both).
    async fn fetch_rates(&self, token: &str) -> Result<String> {
        let resp = self
            .http
            .get(&self.rates_url)
            .bearer_auth(token)
            .header("X-IBM-Client-Id", self.client_id.as_str())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .context("requesting DOP exchange rates")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("DOP rates endpoint returned {status}: {body}"));
        }
        Ok(body)
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
        other => return Err(D::Error::custom(format!("expected number or string, got {other}"))),
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
            table: Mutex::new(Some(table)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let r = rt.block_on(fx.rate("USD", "usd", date)).unwrap();
        assert_eq!(r, Decimal::ONE);
    }

    #[test]
    fn parses_rate_from_frankfurter_body() {
        let body = r#"{"amount":1.0,"base":"USD","date":"2026-05-27","rates":{"EUR":0.92}}"#;
        let rate = parse_rate(body, "EUR").unwrap();
        assert_eq!(rate, Decimal::from_str("0.92").unwrap());
    }

    #[test]
    fn parse_rate_is_case_insensitive_on_target() {
        let body = r#"{"amount":1.0,"base":"USD","date":"2026-05-27","rates":{"JPY":143.5}}"#;
        assert_eq!(
            parse_rate(body, "jpy").unwrap(),
            Decimal::from_str("143.5").unwrap()
        );
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
        assert_eq!(t.get("USD").unwrap(), &Decimal::from_str("56.95").unwrap());
        assert_eq!(t.get("EUR").unwrap(), &Decimal::from_str("62.5").unwrap());
    }

    #[test]
    fn rate_table_tolerates_string_numbers_and_skips_nonpositive() {
        let body = r#"{"monedas":{"moneda":[
            {"descripcion":"USD","compra":"55","venta":"56.95"},
            {"descripcion":"BAD","compra":0,"venta":0}
        ]}}"#;
        let t = parse_rate_table(body).unwrap();
        assert_eq!(t.get("USD").unwrap(), &Decimal::from_str("56.95").unwrap());
        assert!(!t.contains_key("BAD"), "non-positive rate row is skipped");
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
        let venta = Decimal::from_str("56.95").unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 5, 27).unwrap();
        let fx = FxClient::new(&http, "http://fx.invalid")
            .with_dop(DopRate::with_seeded_table(&http, "USD", venta));
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert_eq!(rt.block_on(fx.rate("USD", "DOP", date)).unwrap(), venta);
        assert_eq!(
            rt.block_on(fx.rate("DOP", "usd", date)).unwrap(),
            Decimal::ONE / venta
        );
        // A currency the bank doesn't publish errors → caller routes to Review.
        assert!(rt.block_on(fx.rate("DOP", "JPY", date)).is_err());
    }

    #[test]
    fn seeded_rate_returns_without_network() {
        let http = Client::new();
        let date = NaiveDate::from_ymd_opt(2026, 5, 27).unwrap();
        let fx = FxClient::with_seeded_rate(
            &http,
            "JPY",
            "USD",
            date,
            Decimal::from_str("0.0064").unwrap(),
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let r = rt.block_on(fx.rate("jpy", "USD", date)).unwrap();
        assert_eq!(r, Decimal::from_str("0.0064").unwrap());
    }
}
