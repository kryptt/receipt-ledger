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
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use chrono::NaiveDate;
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;

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
}

impl<'a> FxClient<'a> {
    /// Build a client against `fx_url` (e.g. [`DEFAULT_FX_URL`]).
    pub fn new(http: &'a Client, fx_url: impl Into<String>) -> Self {
        Self {
            http,
            fx_url: fx_url.into(),
            cache: Mutex::new(HashMap::new()),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

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
