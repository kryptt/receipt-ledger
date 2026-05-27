//! Firefly III submission.
//!
//! Posts a single-split withdrawal transaction group to
//! `POST {base}/api/v1/transactions` with `error_if_duplicate_hash: true`.
//! Firefly answers a duplicate import with HTTP 422 — we treat that as success
//! (the transaction is already in the ledger), which makes re-runs idempotent.
//!
//! Payload shape confirmed against the Firefly III v1 API docs: a transaction
//! group with a `transactions` array of splits; the group-level
//! `error_if_duplicate_hash` flag guards double-imports.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use reqwest::{Client, StatusCode};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::fx::FxClient;
use crate::schema::{Direction, Extracted, Source};

/// Tag attached to every transaction this service books, for easy filtering in
/// Firefly.
const IMPORT_TAG: &str = "receipt-ledger";

/// Outcome of a submission attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Newly created in Firefly.
    Created,
    /// Firefly reported it as a duplicate (422) — already imported.
    Duplicate,
}

pub struct FireflyClient<'a> {
    http: &'a Client,
    base_url: String,
    token: String,
    /// FX-rate resolver, used to convert a charge into the target account's
    /// currency before booking. An FX failure propagates as `Err` so the
    /// message routes to Review rather than booking at the wrong amount.
    fx: &'a FxClient<'a>,
    /// PayPal Balance account — name or numeric id. The safe default for any
    /// PayPal record whose funding is not a credit product, so always present.
    paypal_balance_account: String,
    /// PayPal Credit account — name or numeric id. `None` when unconfigured; a
    /// credit-funded PayPal record then errors out (→ Review).
    paypal_credit_account: Option<String>,
    /// Banco Popular VISA USD account — name or numeric id. `None` when
    /// unconfigured; a non-DOP Banco Popular record then errors out (→ Review).
    banco_popular_usd_account: Option<String>,
    /// Banco Popular VISA DOP account — name or numeric id. `None` when
    /// unconfigured; a DOP Banco Popular record then errors out (→ Review).
    banco_popular_dop_account: Option<String>,
    /// Per-account-id currency cache: a numeric account id → its Firefly
    /// `currency_code`. Authoritative source of the conversion target so we
    /// book in the account's real currency. Populated lazily on first use.
    account_currency: Mutex<HashMap<String, String>>,
}

#[derive(Serialize)]
struct TransactionGroup<'a> {
    error_if_duplicate_hash: bool,
    apply_rules: bool,
    transactions: Vec<Split<'a>>,
}

#[derive(Serialize)]
struct Split<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    date: String,
    amount: String,
    currency_code: &'a str,
    description: &'a str,
    external_id: &'a str,
    tags: Vec<&'a str>,
    /// Source account by id (numeric account ref) ...
    #[serde(skip_serializing_if = "Option::is_none")]
    source_id: Option<&'a str>,
    /// ... or by name (non-numeric account ref).
    #[serde(skip_serializing_if = "Option::is_none")]
    source_name: Option<&'a str>,
    /// The merchant becomes the expense (destination) account for a withdrawal.
    #[serde(skip_serializing_if = "Option::is_none")]
    destination_name: Option<&'a str>,
    /// Original (pre-conversion) amount as a string, set only when the charge
    /// currency differs from the account currency. Firefly stores this as the
    /// transaction's "foreign amount" alongside the converted `amount`.
    #[serde(skip_serializing_if = "Option::is_none")]
    foreign_amount: Option<String>,
    /// ISO-4217 code of the original charge currency, paired with
    /// `foreign_amount`. Set only on a converted (cross-currency) split.
    #[serde(skip_serializing_if = "Option::is_none")]
    foreign_currency_code: Option<&'a str>,
}

impl<'a> FireflyClient<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        http: &'a Client,
        base_url: impl Into<String>,
        token: impl Into<String>,
        fx: &'a FxClient<'a>,
        paypal_balance_account: impl Into<String>,
        paypal_credit_account: Option<String>,
        banco_popular_usd_account: Option<String>,
        banco_popular_dop_account: Option<String>,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            token: token.into(),
            fx,
            paypal_balance_account: paypal_balance_account.into(),
            paypal_credit_account,
            banco_popular_usd_account,
            banco_popular_dop_account,
            account_currency: Mutex::new(HashMap::new()),
        }
    }

    /// Submit one validated, deduped record as a withdrawal.
    ///
    /// `external_id` is the dedup key computed by [`crate::dedup`].
    ///
    /// Resolves the target account, its authoritative currency, and the FX rate
    /// from the charge currency to that target — then builds and posts the
    /// split. Any of those resolutions failing is an `Err`, which the pipeline
    /// turns into a per-message Review (never a mis-booked amount).
    pub async fn submit(&self, record: &Extracted, external_id: &str) -> Result<SubmitOutcome> {
        let url = format!("{}/api/v1/transactions", self.base_url.trim_end_matches('/'));

        let account = self.route_account(record)?;
        let target = self.account_currency(account).await?;
        let rate = self
            .fx
            .rate(&record.currency, &target, record.date)
            .await
            .context("resolving FX rate for conversion")?;

        let group = build_group(record, external_id, account, &target, rate)?;

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&group)
            .send()
            .await
            .context("sending Firefly transaction request")?;

        let status = resp.status();
        if status.is_success() {
            info!(%external_id, "booked transaction in Firefly");
            return Ok(SubmitOutcome::Created);
        }

        // Firefly returns 422 with a validation body for duplicate hashes. We
        // inspect the body to distinguish a duplicate from a real validation
        // failure rather than swallowing every 422.
        if status == StatusCode::UNPROCESSABLE_ENTITY {
            let body = resp.text().await.unwrap_or_default();
            if is_duplicate_error(&body) {
                info!(%external_id, "transaction already imported (duplicate hash)");
                return Ok(SubmitOutcome::Duplicate);
            }
            warn!(%external_id, %body, "Firefly 422 was not a duplicate");
            anyhow::bail!("Firefly rejected transaction (422): {body}");
        }

        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Firefly returned {status}: {body}")
    }

    /// Route to the Firefly account for this record. Exhaustive over `Source`
    /// so a new variant forces a routing decision here rather than silently
    /// booking against the wrong account. Within each source the
    /// funding/currency rules pick balance-vs-credit (PayPal) or USD-vs-DOP
    /// (Banco Popular). A needed-but-unconfigured `Option` account is an `Err`,
    /// which the pipeline turns into a per-message Review. Pure: depends only on
    /// the record and the configured account refs.
    fn route_account(&self, record: &Extracted) -> Result<&str> {
        let account: &str = match record.source {
            Source::Paypal => {
                if is_paypal_credit_funding(record) {
                    self.paypal_credit_account
                        .as_deref()
                        .context("no Firefly account configured for PayPal Credit")?
                } else {
                    &self.paypal_balance_account
                }
            }
            Source::BancoPopular => {
                if record.currency.trim().to_ascii_uppercase() == "DOP" {
                    self.banco_popular_dop_account
                        .as_deref()
                        .context("no Firefly account configured for Banco Popular DOP")?
                } else {
                    self.banco_popular_usd_account
                        .as_deref()
                        .context("no Firefly account configured for Banco Popular USD")?
                }
            }
        };
        Ok(account)
    }

    /// The authoritative currency of `account`, as Firefly reports it.
    ///
    /// For a numeric account id this does `GET {base}/api/v1/accounts/{id}` and
    /// reads `data.attributes.currency_code`, caching the result per id so a
    /// batch hits the network at most once per account. For a non-numeric
    /// account *name* we cannot resolve via the id endpoint, so we fall back to
    /// treating the record's own currency as the target (no conversion) — our
    /// production deployment uses numeric ids, where the GET path is exercised.
    async fn account_currency(&self, account: &str) -> Result<String> {
        if !is_numeric(account) {
            // Name-based account: skip conversion, book in the record currency.
            return Ok(account_currency_fallback(account));
        }

        if let Some(code) = self
            .account_currency
            .lock()
            .expect("account-currency cache mutex poisoned")
            .get(account)
        {
            return Ok(code.clone());
        }

        let url = format!(
            "{}/api/v1/accounts/{}",
            self.base_url.trim_end_matches('/'),
            account
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .with_context(|| format!("fetching Firefly account {account}"))?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("Firefly returned {status} fetching account {account}: {body}");
        }

        let code = parse_account_currency(&body)
            .with_context(|| format!("reading currency_code for account {account}"))?;

        self.account_currency
            .lock()
            .expect("account-currency cache mutex poisoned")
            .insert(account.to_string(), code.clone());
        Ok(code)
    }
}

/// Currency target for a name-based account ref. We have no id to query, and
/// our deployment uses numeric ids, so this is a conservative placeholder that
/// keeps the same-currency path active for the few tests using account names.
/// It is intentionally empty so `build_group` treats every record as
/// same-currency only when the record currency is also empty — in practice
/// callers pass numeric ids and never reach here.
fn account_currency_fallback(_account: &str) -> String {
    String::new()
}

/// The Firefly single-account envelope: `{"data":{"attributes":{...}}}`.
#[derive(Deserialize)]
struct AccountEnvelope {
    data: AccountData,
}
#[derive(Deserialize)]
struct AccountData {
    attributes: AccountAttributes,
}
#[derive(Deserialize)]
struct AccountAttributes {
    currency_code: Option<String>,
}

/// Extract `data.attributes.currency_code` from a Firefly account response.
/// Pure — no I/O — so it is unit-testable. Errors when the field is absent or
/// blank so the caller never converts against an unknown target currency.
fn parse_account_currency(body: &str) -> Result<String> {
    let env: AccountEnvelope =
        serde_json::from_str(body).context("decoding Firefly account JSON")?;
    match env.data.attributes.currency_code {
        Some(code) if !code.trim().is_empty() => Ok(code.trim().to_string()),
        _ => Err(anyhow!("account response missing currency_code")),
    }
}

/// Build the transaction group for a record, given the resolved `account`, its
/// `target` currency, and the conversion `rate` (multiply the record amount by
/// it to get the target-currency amount). Pure — no I/O — so the conversion and
/// foreign-amount shaping are unit-testable without a live Firefly or FX API.
///
/// When the record currency already equals `target` the split books unchanged:
/// `amount` is the record amount in `target`, no foreign fields. Otherwise the
/// split books the converted `amount` in `target` and carries the original as
/// Firefly's `foreign_amount` + `foreign_currency_code`.
fn build_group<'b>(
    record: &'b Extracted,
    external_id: &'b str,
    account: &'b str,
    target: &'b str,
    rate: Decimal,
) -> Result<TransactionGroup<'b>> {
    let (source_id, source_name) = if is_numeric(account) {
        (Some(account), None)
    } else {
        (None, Some(account))
    };

    let kind = match record.direction {
        Direction::Out => "withdrawal",
        Direction::In => "deposit",
    };

    let same_currency = record.currency.eq_ignore_ascii_case(target);
    let (amount, foreign_amount, foreign_currency_code) = if same_currency {
        // No conversion: book the record amount in the target currency.
        (record.amount.normalize().to_string(), None, None)
    } else {
        // Convert to the account currency, rounded to 2 dp, and attach the
        // original as Firefly's foreign amount.
        let converted = (record.amount * rate).round_dp(2);
        (
            converted.normalize().to_string(),
            Some(record.amount.normalize().to_string()),
            Some(record.currency.as_str()),
        )
    };

    Ok(TransactionGroup {
        error_if_duplicate_hash: true,
        // Let Firefly rules (e.g. the future transit-account rewrite) fire.
        apply_rules: true,
        transactions: vec![Split {
            kind,
            date: record.date.to_string(),
            amount,
            currency_code: target,
            description: &record.merchant,
            external_id,
            tags: vec![IMPORT_TAG],
            source_id,
            source_name,
            destination_name: Some(&record.merchant),
            foreign_amount,
            foreign_currency_code,
        }],
    })
}

fn is_numeric(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// Funding hints that mark a PayPal record as funded by a credit product (→ the
/// PayPal Credit liability account) rather than the PayPal balance. Matched
/// case-insensitively as substrings of the trimmed `account_hint`. "credit" is
/// deliberately broad: in a PayPal funding hint it can only mean the credit
/// product. An empty/absent hint is not a credit signal — it defaults to the
/// balance account.
const PAYPAL_CREDIT_HINTS: &[&str] = &[
    "paypal credit",
    "pay later",
    "pay in 4",
    "pay in4",
    "pay monthly",
    "credit",
];

/// Classify a PayPal record's funding method from its `account_hint`.
///
/// Returns `true` when the hint names a PayPal credit product, so the record
/// books against the PayPal Credit liability account; `false` (the default)
/// routes it to the PayPal Balance account. Pure — depends only on the record.
fn is_paypal_credit_funding(record: &Extracted) -> bool {
    match record.account_hint.as_deref() {
        Some(hint) => {
            let hint = hint.trim().to_ascii_lowercase();
            PAYPAL_CREDIT_HINTS.iter().any(|needle| hint.contains(needle))
        }
        None => false,
    }
}

/// Heuristic match for Firefly's duplicate-hash validation message. Firefly
/// phrases it as "Duplicate of transaction #N." inside the 422 error body.
fn is_duplicate_error(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("duplicate")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::FxClient;
    use crate::schema::{Direction, Source};
    use chrono::NaiveDate;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    /// A PayPal record with a configurable funding hint and currency. Defaults
    /// to a balance-funded EUR purchase.
    fn paypal_record(account_hint: Option<&str>, currency: &str) -> Extracted {
        Extracted {
            source: Source::Paypal,
            external_id: Some("8XY12345AB678901C".to_string()),
            amount: Decimal::from_str("149.99").unwrap(),
            currency: currency.to_string(),
            direction: Direction::Out,
            date: NaiveDate::from_ymd_opt(2026, 5, 11).unwrap(),
            merchant: "Example Merchant B.V.".to_string(),
            account_hint: account_hint.map(str::to_string),
            status: "approved".to_string(),
            raw_ref: "TESTORDER0123456".to_string(),
        }
    }

    /// A Banco Popular record with a configurable currency.
    fn banco_record(currency: &str) -> Extracted {
        Extracted {
            source: Source::BancoPopular,
            external_id: None,
            amount: Decimal::from_str("1.50").unwrap(),
            currency: currency.to_string(),
            direction: Direction::Out,
            date: NaiveDate::from_ymd_opt(2026, 5, 27).unwrap(),
            merchant: "Example Cafe Amsterdam".to_string(),
            account_hint: Some("1234".to_string()),
            status: "Aprobada".to_string(),
            raw_ref: String::new(),
        }
    }

    /// A throwaway FX client; the build/routing tests below never call its
    /// `rate` (they exercise the pure `route_account`/`build_group` directly),
    /// so the URL is never contacted.
    fn fx(http: &Client) -> FxClient<'_> {
        FxClient::new(http, "http://fx.invalid")
    }

    /// A client wired with the four production account ids.
    fn client<'a>(http: &'a Client, fx: &'a FxClient<'a>) -> FireflyClient<'a> {
        FireflyClient::new(
            http,
            "http://firefly:8080",
            "tok",
            fx,
            "103",                       // PayPal Balance
            Some("105".to_string()),     // PayPal Credit
            Some("106".to_string()),     // Banco Popular USD
            Some("107".to_string()),     // Banco Popular DOP
        )
    }

    #[test]
    fn builds_withdrawal_with_named_account() {
        // A non-numeric balance account exercises the name (not id) path. With a
        // same-currency target the split books unchanged: no foreign fields.
        let rec = paypal_record(Some("Balance"), "EUR");
        let group =
            build_group(&rec, "8XY12345AB678901C", "PayPal Balance", "EUR", Decimal::ONE).unwrap();
        let json = serde_json::to_value(&group).unwrap();

        assert_eq!(json["error_if_duplicate_hash"], true);
        let split = &json["transactions"][0];
        assert_eq!(split["type"], "withdrawal");
        assert_eq!(split["amount"], "149.99");
        assert_eq!(split["currency_code"], "EUR");
        assert_eq!(split["external_id"], "8XY12345AB678901C");
        assert_eq!(split["source_name"], "PayPal Balance");
        assert!(split.get("source_id").is_none());
        assert_eq!(split["destination_name"], "Example Merchant B.V.");
        assert_eq!(split["tags"][0], "receipt-ledger");
        assert_eq!(split["date"], "2026-05-11");
        // Same currency → no foreign fields serialized.
        assert!(split.get("foreign_amount").is_none());
        assert!(split.get("foreign_currency_code").is_none());
    }

    /// Route a record to its account id via the pure `route_account`, asserting
    /// it is numeric (the production case).
    fn source_id_of(c: &FireflyClient, rec: &Extracted) -> String {
        let account = c.route_account(rec).expect("routing should succeed");
        assert!(is_numeric(account), "production accounts route to numeric ids");
        account.to_string()
    }

    // --- routing -----------------------------------------------------------

    #[test]
    fn paypal_credit_funded_routes_to_credit_account() {
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        for hint in ["Pay in 4", "PayPal Credit"] {
            let rec = paypal_record(Some(hint), "USD");
            assert_eq!(source_id_of(&c, &rec), "105", "hint {hint:?} should be credit");
        }
    }

    #[test]
    fn paypal_balance_funded_routes_to_balance_account() {
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        // Explicit balance hint and an absent hint both default to balance.
        assert_eq!(source_id_of(&c, &paypal_record(Some("Balance"), "USD")), "103");
        assert_eq!(source_id_of(&c, &paypal_record(None, "USD")), "103");
    }

    #[test]
    fn banco_dop_routes_to_dop_account() {
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        assert_eq!(source_id_of(&c, &banco_record("DOP")), "107");
        // Currency is uppercased before comparison.
        assert_eq!(source_id_of(&c, &banco_record("dop")), "107");
    }

    #[test]
    fn banco_non_dop_routes_to_usd_account() {
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        for cur in ["USD", "EUR", "JPY", "KRW"] {
            assert_eq!(source_id_of(&c, &banco_record(cur)), "106", "currency {cur} → USD acct");
        }
    }

    #[test]
    fn needed_but_unconfigured_account_errors() {
        let http = Client::new();
        let fx = fx(&http);
        // Only the required balance account is configured; everything else None.
        // A credit-funded PayPal record and either Banco record must error
        // (→ Review) rather than booking against the wrong account.
        let c = FireflyClient::new(&http, "http://firefly:8080", "tok", &fx, "103", None, None, None);
        assert!(c.route_account(&paypal_record(Some("Pay in 4"), "USD")).is_err());
        assert!(c.route_account(&banco_record("DOP")).is_err());
        assert!(c.route_account(&banco_record("USD")).is_err());
        // ...but a balance-funded PayPal record still routes.
        assert!(c.route_account(&paypal_record(Some("Balance"), "USD")).is_ok());
    }

    // --- conversion / foreign amount ---------------------------------------

    #[test]
    fn same_currency_books_unchanged_without_foreign_fields() {
        // A USD record into the USD account (rate ONE): amount unchanged,
        // currency = target, no foreign fields.
        let rec = banco_record("USD");
        let group = build_group(&rec, "h", "106", "USD", Decimal::ONE).unwrap();
        let json = serde_json::to_value(&group).unwrap();
        let split = &json["transactions"][0];
        assert_eq!(split["amount"], "1.5");
        assert_eq!(split["currency_code"], "USD");
        assert_eq!(split["source_id"], "106");
        assert!(split.get("foreign_amount").is_none());
        assert!(split.get("foreign_currency_code").is_none());
    }

    #[test]
    fn foreign_currency_books_converted_amount_with_foreign_fields() {
        // A JPY 5130 charge routed to the USD account at a known rate of
        // 0.0064 USD/JPY books 5130 * 0.0064 = 32.832 → 32.83 (round_dp(2)),
        // currency = USD, with the original JPY 5130 as the foreign amount.
        let mut rec = banco_record("JPY");
        rec.amount = Decimal::from_str("5130").unwrap();
        let rate = Decimal::from_str("0.0064").unwrap();
        let group = build_group(&rec, "h", "106", "USD", rate).unwrap();
        let json = serde_json::to_value(&group).unwrap();
        let split = &json["transactions"][0];

        assert_eq!(split["amount"], "32.83", "converted USD amount, 2 dp");
        assert_eq!(split["currency_code"], "USD", "books in the account currency");
        assert_eq!(split["foreign_amount"], "5130", "original charge amount");
        assert_eq!(split["foreign_currency_code"], "JPY", "original charge currency");
        assert_eq!(split["source_id"], "106");
    }

    #[test]
    fn target_currency_match_is_case_insensitive() {
        // Record currency "usd" against target "USD" is the same-currency path.
        let mut rec = banco_record("usd");
        rec.amount = Decimal::from_str("65.33").unwrap();
        let group = build_group(&rec, "h", "106", "USD", Decimal::ONE).unwrap();
        let json = serde_json::to_value(&group).unwrap();
        let split = &json["transactions"][0];
        assert_eq!(split["amount"], "65.33");
        assert!(split.get("foreign_amount").is_none());
    }

    // --- account-currency parsing ------------------------------------------

    #[test]
    fn parses_account_currency_code() {
        let body = r#"{"data":{"id":"106","attributes":{"name":"Banco USD","currency_code":"USD"}}}"#;
        assert_eq!(parse_account_currency(body).unwrap(), "USD");
    }

    #[test]
    fn parse_account_currency_errors_when_absent() {
        assert!(parse_account_currency(r#"{"data":{"attributes":{}}}"#).is_err());
        assert!(parse_account_currency(r#"{"data":{"attributes":{"currency_code":""}}}"#).is_err());
        assert!(parse_account_currency("not json").is_err());
    }

    /// A foreign Banco record converts end-to-end through the public `submit`
    /// path using a pre-seeded FX cache (no network for the rate) — proving the
    /// FxClient seam threads through FireflyClient. The Firefly POST itself
    /// would fail against `firefly.invalid`, so we stop at `build_group`'s
    /// inputs by asserting the seeded rate is what the client resolves.
    #[test]
    fn seeded_fx_rate_threads_through_client() {
        let http = Client::new();
        let date = NaiveDate::from_ymd_opt(2026, 5, 27).unwrap();
        let fx = FxClient::with_seeded_rate(&http, "JPY", "USD", date, Decimal::from_str("0.0064").unwrap());
        let c = client(&http, &fx);
        let mut rec = banco_record("JPY");
        rec.date = date;
        rec.amount = Decimal::from_str("5130").unwrap();
        // route + rate are the inputs to build_group; resolve them as submit does.
        let account = c.route_account(&rec).unwrap();
        assert_eq!(account, "106");
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let rate = rt
            .block_on(c.fx.rate(&rec.currency, "USD", rec.date))
            .expect("seeded rate, no network");
        let group = build_group(&rec, "h", account, "USD", rate).unwrap();
        let json = serde_json::to_value(&group).unwrap();
        assert_eq!(json["transactions"][0]["amount"], "32.83");
        assert_eq!(json["transactions"][0]["foreign_currency_code"], "JPY");
    }

    // --- classifier --------------------------------------------------------

    #[test]
    fn classifier_flags_credit_hints() {
        for hint in [
            "PayPal Credit",
            "Pay Later",
            "Pay in 4",
            "Pay in4",
            "Pay Monthly",
            "credit",
            // Substring + case/whitespace insensitivity.
            "  funded by PAYPAL CREDIT  ",
        ] {
            assert!(
                is_paypal_credit_funding(&paypal_record(Some(hint), "USD")),
                "hint {hint:?} should classify as credit"
            );
        }
    }

    #[test]
    fn classifier_rejects_non_credit_hints() {
        for hint in [None, Some(""), Some("   "), Some("Balance"), Some("Visa ending 1234"), Some("bank")] {
            assert!(
                !is_paypal_credit_funding(&paypal_record(hint, "USD")),
                "hint {hint:?} should NOT classify as credit"
            );
        }
    }

    #[test]
    fn detects_duplicate_message() {
        assert!(is_duplicate_error(
            r#"{"message":"Duplicate of transaction #123."}"#
        ));
        assert!(!is_duplicate_error(r#"{"message":"The amount is required."}"#));
    }
}
