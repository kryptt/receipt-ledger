//! Firefly III submission.
//!
//! Posts a single-split withdrawal transaction group to
//! `POST {base}/api/v1/transactions` with `error_if_duplicate_hash: true`.
//! Firefly answers a duplicate import with HTTP 422 — we parse the body and
//! treat *only* the duplicate-hash shape as success (the transaction is already
//! in the ledger), which makes re-runs idempotent. Any other 422 is a real
//! validation failure and routes to Review.
//!
//! Payload shape confirmed against the Firefly III v1 API docs: a transaction
//! group with a `transactions` array of splits; the group-level
//! `error_if_duplicate_hash` flag guards double-imports.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use reqwest::{Client, StatusCode};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::AccountId;
use crate::fx::FxClient;
use crate::schema::{Direction, Extracted, Source};
use crate::validate::Validated;

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

/// The authoritative booking target for an account, as Firefly reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    /// ISO-4217 currency code the account books in.
    currency: String,
    /// The currency's real minor-unit precision (e.g. 2 for USD/EUR, 0 for
    /// JPY/KRW). Conversions round to exactly this many places.
    decimals: u32,
}

pub struct FireflyClient<'a> {
    http: &'a Client,
    base_url: String,
    token: String,
    /// FX-rate resolver, used to convert a charge into the target account's
    /// currency before booking. An FX failure propagates as `Err` so the
    /// message routes to Review rather than booking at the wrong amount.
    fx: &'a FxClient<'a>,
    /// PayPal Balance account id. The safe default for any PayPal record whose
    /// funding is not a credit product, so always present.
    paypal_balance_account: AccountId,
    /// PayPal Credit account id. `None` when unconfigured; a credit-funded
    /// PayPal record then errors out (→ Review).
    paypal_credit_account: Option<AccountId>,
    /// Banco Popular VISA USD account id. `None` when unconfigured; a non-DOP
    /// Banco Popular record then errors out (→ Review).
    banco_popular_usd_account: Option<AccountId>,
    /// Banco Popular VISA DOP account id. `None` when unconfigured; a DOP Banco
    /// Popular record then errors out (→ Review).
    banco_popular_dop_account: Option<AccountId>,
    /// Per-account-id target cache: numeric account id → its Firefly
    /// `currency_code` + `decimal_places`. Authoritative source of the
    /// conversion target so we book in the account's real currency at its real
    /// precision. Populated lazily on first use.
    account_target: Mutex<HashMap<String, Target>>,
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
    /// Source account by numeric id.
    source_id: &'a str,
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
        paypal_balance_account: AccountId,
        paypal_credit_account: Option<AccountId>,
        banco_popular_usd_account: Option<AccountId>,
        banco_popular_dop_account: Option<AccountId>,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            token: token.into(),
            fx,
            paypal_balance_account,
            paypal_credit_account,
            banco_popular_usd_account,
            banco_popular_dop_account,
            account_target: Mutex::new(HashMap::new()),
        }
    }

    /// Submit one validated, deduped record as a withdrawal.
    ///
    /// Requires a [`Validated`] — an unvalidated [`Extracted`] cannot reach this
    /// call, so the validation gate is impossible to skip. `external_id` is the
    /// dedup key computed by [`crate::dedup`].
    ///
    /// Resolves the target account, its authoritative currency + precision, and
    /// the FX rate from the charge currency to that target — then builds and
    /// posts the split. Any of those resolutions failing is an `Err`, which the
    /// pipeline turns into a per-message Review (never a mis-booked amount).
    pub async fn submit(&self, record: &Validated, external_id: &str) -> Result<SubmitOutcome> {
        let record = record.as_extracted();
        let url = format!(
            "{}/api/v1/transactions",
            self.base_url.trim_end_matches('/')
        );

        let account = self.route_account(record)?;
        let target = self.account_target(account.as_str()).await?;
        let rate = self
            .fx
            .rate(record.currency().as_str(), &target.currency, record.date)
            .await
            .context("resolving FX rate for conversion")?;

        let group = build_group(record, external_id, account.as_str(), &target, rate);

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
        // parse the body as JSON and match the specific duplicate-hash shape;
        // any OTHER 422 is a real validation failure (→ Review), never silently
        // treated as Processed.
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

    /// Route to the Firefly account id for this record. Exhaustive over
    /// `Source` so a new variant forces a routing decision here rather than
    /// silently booking against the wrong account. Within each source the
    /// funding/currency rules pick balance-vs-credit (PayPal) or USD-vs-DOP
    /// (Banco Popular). A needed-but-unconfigured `Option` account is an `Err`,
    /// which the pipeline turns into a per-message Review. Pure: depends only on
    /// the record and the configured account ids.
    fn route_account(&self, record: &Extracted) -> Result<&AccountId> {
        let account = match record.source {
            Source::Paypal => {
                if is_paypal_credit_funding(record) {
                    self.paypal_credit_account
                        .as_ref()
                        .context("no Firefly account configured for PayPal Credit")?
                } else {
                    &self.paypal_balance_account
                }
            }
            Source::BancoPopular => {
                if record.currency().as_str() == "DOP" {
                    self.banco_popular_dop_account
                        .as_ref()
                        .context("no Firefly account configured for Banco Popular DOP")?
                } else {
                    self.banco_popular_usd_account
                        .as_ref()
                        .context("no Firefly account configured for Banco Popular USD")?
                }
            }
        };
        Ok(account)
    }

    /// The authoritative booking [`Target`] (currency + precision) of an
    /// account id, as Firefly reports it.
    ///
    /// Does `GET {base}/api/v1/accounts/{id}` and reads
    /// `data.attributes.currency_code` + `currency_decimal_places`, caching the
    /// result per id so a batch hits the network at most once per account.
    async fn account_target(&self, account: &str) -> Result<Target> {
        // Recover a poisoned cache rather than panicking the whole batch.
        if let Some(t) = self
            .account_target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account)
        {
            return Ok(t.clone());
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

        let target = parse_account_target(&body)
            .with_context(|| format!("reading currency for account {account}"))?;

        self.account_target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(account.to_string(), target.clone());
        Ok(target)
    }
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
    /// Firefly reports the currency's minor-unit precision here. Absent on some
    /// account kinds; we default to 2 (the overwhelming common case) only when
    /// the currency_code is present but this field is not.
    currency_decimal_places: Option<u32>,
}

/// Extract the booking [`Target`] (currency + precision) from a Firefly account
/// response. Pure — no I/O — so it is unit-testable. Errors when the currency
/// is absent or blank so the caller never converts against an unknown target.
fn parse_account_target(body: &str) -> Result<Target> {
    let env: AccountEnvelope =
        serde_json::from_str(body).context("decoding Firefly account JSON")?;
    let currency = match env.data.attributes.currency_code {
        Some(code) if !code.trim().is_empty() => code.trim().to_string(),
        _ => return Err(anyhow!("account response missing currency_code")),
    };
    let decimals = env.data.attributes.currency_decimal_places.unwrap_or(2);
    Ok(Target { currency, decimals })
}

/// Build the transaction group for a record, given the resolved `account` id,
/// its `target` (currency + precision), and the conversion `rate` (multiply the
/// record amount by it to get the target-currency amount). Pure — no I/O — so
/// the conversion and foreign-amount shaping are unit-testable without a live
/// Firefly or FX API.
///
/// When the record currency already equals the target currency the split books
/// unchanged: `amount` is the record amount, no foreign fields. Otherwise the
/// split books the converted `amount` (rounded to the TARGET currency's real
/// `decimals`) in the target currency and carries the original as Firefly's
/// `foreign_amount` + `foreign_currency_code`.
fn build_group<'b>(
    record: &'b Extracted,
    external_id: &'b str,
    account: &'b str,
    target: &'b Target,
    rate: Decimal,
) -> TransactionGroup<'b> {
    let kind = match record.direction {
        Direction::Out => "withdrawal",
        Direction::In => "deposit",
    };

    let amount = record.amount().value();
    let same_currency = record
        .currency()
        .as_str()
        .eq_ignore_ascii_case(&target.currency);
    let (amount, foreign_amount, foreign_currency_code) = if same_currency {
        // No conversion: book the record amount in the target currency.
        (amount.normalize().to_string(), None, None)
    } else {
        // Convert to the account currency, rounded to the target currency's
        // REAL minor-unit precision (2 for USD/EUR, 0 for JPY/KRW). We use
        // banker's rounding (MidpointNearestEven): over a large batch it does
        // not bias the ledger high or low, which matters for a feed that books
        // many small conversions.
        let converted = (amount * rate)
            .round_dp_with_strategy(target.decimals, RoundingStrategy::MidpointNearestEven);
        (
            converted.normalize().to_string(),
            Some(amount.normalize().to_string()),
            Some(record.currency().as_str()),
        )
    };

    TransactionGroup {
        error_if_duplicate_hash: true,
        // Let Firefly rules fire (e.g. a planned transit-account rewrite).
        // Firefly's own rule engine is part of the trusted base: rules are
        // operator-authored, so applying them here is deliberate, not a risk.
        apply_rules: true,
        transactions: vec![Split {
            kind,
            date: record.date.to_string(),
            amount,
            currency_code: &target.currency,
            description: &record.merchant,
            external_id,
            tags: vec![IMPORT_TAG],
            source_id: account,
            destination_name: Some(&record.merchant),
            foreign_amount,
            foreign_currency_code,
        }],
    }
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

/// Public, pure view of the PayPal funding classification, for the eval harness
/// (and anything that needs to predict routing without the live client). `true`
/// → PayPal Credit liability account; `false` → PayPal Balance asset account.
/// Identical logic to the private [`is_paypal_credit_funding`] the submit path
/// uses, so the eval scores the *real* routing decision.
#[must_use]
pub fn paypal_is_credit_funded(record: &Extracted) -> bool {
    is_paypal_credit_funding(record)
}

/// Classify a PayPal record's funding method from its `account_hint`.
///
/// Returns `true` when the hint names a PayPal credit product, so the record
/// books against the PayPal Credit liability account; `false` (the default)
/// routes it to the PayPal Balance account. Pure — depends only on the record.
fn is_paypal_credit_funding(record: &Extracted) -> bool {
    match record.account_hint.as_deref() {
        Some(hint) => {
            let hint = hint.trim().to_ascii_lowercase();
            PAYPAL_CREDIT_HINTS
                .iter()
                .any(|needle| hint.contains(needle))
        }
        None => false,
    }
}

/// The Firefly 422 validation-error envelope. Firefly returns
/// `{"message": "...", "errors": {"field": ["msg", ...], ...}}`. A duplicate
/// import surfaces as an error message about a duplicate transaction hash.
#[derive(Deserialize)]
struct ValidationError {
    #[serde(default)]
    message: String,
    #[serde(default)]
    errors: HashMap<String, Vec<String>>,
}

/// Whether a 422 body is Firefly's duplicate-hash rejection specifically.
///
/// We parse the JSON and look for the duplicate phrasing in either the
/// top-level `message` or any field error (Firefly phrases it as "Duplicate of
/// transaction #N." and attaches it under a `transactions.N.description`-style
/// key). Any other 422 shape is NOT a duplicate — it is a real validation
/// failure the caller must surface. A body that does not parse as the expected
/// envelope is conservatively treated as not-a-duplicate.
fn is_duplicate_error(body: &str) -> bool {
    let Ok(parsed) = serde_json::from_str::<ValidationError>(body) else {
        return false;
    };
    let mentions_duplicate = |s: &str| {
        let l = s.to_ascii_lowercase();
        l.contains("duplicate") && l.contains("transaction")
    };
    if mentions_duplicate(&parsed.message) {
        return true;
    }
    parsed
        .errors
        .values()
        .flatten()
        .any(|msg| mentions_duplicate(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::FxClient;
    use crate::schema::{Amount, Currency, Direction, Money, Source};
    use chrono::NaiveDate;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn acct(id: &str) -> AccountId {
        AccountId::parse(id).unwrap()
    }

    fn usd_target() -> Target {
        Target {
            currency: "USD".to_string(),
            decimals: 2,
        }
    }

    /// A PayPal record with a configurable funding hint and currency. Defaults
    /// to a balance-funded EUR purchase.
    fn paypal_record(account_hint: Option<&str>, currency: &str) -> Extracted {
        Extracted {
            source: Source::Paypal,
            external_id: Some("8XY12345AB678901C".to_string()),
            money: Money::new(
                Amount::parse("149.99").unwrap(),
                Currency::parse(currency).unwrap(),
            ),
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
            money: Money::new(
                Amount::parse("1.50").unwrap(),
                Currency::parse(currency).unwrap(),
            ),
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
            acct("103"),       // PayPal Balance
            Some(acct("105")), // PayPal Credit
            Some(acct("106")), // Banco Popular USD
            Some(acct("107")), // Banco Popular DOP
        )
    }

    #[test]
    fn builds_withdrawal_same_currency_without_foreign_fields() {
        // A same-currency target books unchanged: no foreign fields.
        let rec = paypal_record(Some("Balance"), "EUR");
        let target = Target {
            currency: "EUR".to_string(),
            decimals: 2,
        };
        let group = build_group(&rec, "8XY12345AB678901C", "103", &target, Decimal::ONE);
        let json = serde_json::to_value(&group).unwrap();

        assert_eq!(json["error_if_duplicate_hash"], true);
        let split = &json["transactions"][0];
        assert_eq!(split["type"], "withdrawal");
        assert_eq!(split["amount"], "149.99");
        assert_eq!(split["currency_code"], "EUR");
        assert_eq!(split["external_id"], "8XY12345AB678901C");
        assert_eq!(split["source_id"], "103");
        assert_eq!(split["destination_name"], "Example Merchant B.V.");
        assert_eq!(split["tags"][0], "receipt-ledger");
        assert_eq!(split["date"], "2026-05-11");
        assert!(split.get("foreign_amount").is_none());
        assert!(split.get("foreign_currency_code").is_none());
    }

    /// Route a record to its account id via the pure `route_account`.
    fn source_id_of(c: &FireflyClient, rec: &Extracted) -> String {
        c.route_account(rec)
            .expect("routing should succeed")
            .as_str()
            .to_string()
    }

    // --- routing -----------------------------------------------------------

    #[test]
    fn paypal_credit_funded_routes_to_credit_account() {
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        for hint in ["Pay in 4", "PayPal Credit"] {
            let rec = paypal_record(Some(hint), "USD");
            assert_eq!(
                source_id_of(&c, &rec),
                "105",
                "hint {hint:?} should be credit"
            );
        }
    }

    #[test]
    fn paypal_balance_funded_routes_to_balance_account() {
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        assert_eq!(
            source_id_of(&c, &paypal_record(Some("Balance"), "USD")),
            "103"
        );
        assert_eq!(source_id_of(&c, &paypal_record(None, "USD")), "103");
    }

    #[test]
    fn banco_dop_routes_to_dop_account() {
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        assert_eq!(source_id_of(&c, &banco_record("DOP")), "107");
    }

    #[test]
    fn banco_non_dop_routes_to_usd_account() {
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        for cur in ["USD", "EUR", "JPY", "KRW"] {
            assert_eq!(
                source_id_of(&c, &banco_record(cur)),
                "106",
                "currency {cur} → USD acct"
            );
        }
    }

    #[test]
    fn needed_but_unconfigured_account_errors() {
        let http = Client::new();
        let fx = fx(&http);
        // Only the required balance account is configured; everything else None.
        let c = FireflyClient::new(
            &http,
            "http://firefly:8080",
            "tok",
            &fx,
            acct("103"),
            None,
            None,
            None,
        );
        assert!(
            c.route_account(&paypal_record(Some("Pay in 4"), "USD"))
                .is_err()
        );
        assert!(c.route_account(&banco_record("DOP")).is_err());
        assert!(c.route_account(&banco_record("USD")).is_err());
        assert!(
            c.route_account(&paypal_record(Some("Balance"), "USD"))
                .is_ok()
        );
    }

    // --- conversion / foreign amount ---------------------------------------

    #[test]
    fn same_currency_books_unchanged_without_foreign_fields() {
        let rec = banco_record("USD");
        let target = usd_target();
        let group = build_group(&rec, "h", "106", &target, Decimal::ONE);
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
        // JPY 5130 → USD at 0.0064 = 32.832 → 32.83 (2 dp, MidpointNearestEven).
        let mut rec = banco_record("JPY");
        rec.money = Money::new(
            Amount::parse("5130").unwrap(),
            Currency::parse("JPY").unwrap(),
        );
        let rate = Decimal::from_str("0.0064").unwrap();
        let target = usd_target();
        let group = build_group(&rec, "h", "106", &target, rate);
        let json = serde_json::to_value(&group).unwrap();
        let split = &json["transactions"][0];

        assert_eq!(split["amount"], "32.83", "converted USD amount, 2 dp");
        assert_eq!(
            split["currency_code"], "USD",
            "books in the account currency"
        );
        assert_eq!(split["foreign_amount"], "5130", "original charge amount");
        assert_eq!(
            split["foreign_currency_code"], "JPY",
            "original charge currency"
        );
        assert_eq!(split["source_id"], "106");
    }

    /// H1/H2: a 0-decimal target currency (JPY) rounds to whole units.
    #[test]
    fn zero_decimal_target_rounds_to_whole_units() {
        // USD 65.33 → JPY at rate 156.0 = 10191.48 → 10191 (0 dp).
        let mut rec = banco_record("USD");
        rec.money = Money::new(
            Amount::parse("65.33").unwrap(),
            Currency::parse("USD").unwrap(),
        );
        let jpy = Target {
            currency: "JPY".to_string(),
            decimals: 0,
        };
        let rate = Decimal::from_str("156.0").unwrap();
        let group = build_group(&rec, "h", "999", &jpy, rate);
        let json = serde_json::to_value(&group).unwrap();
        let split = &json["transactions"][0];
        assert_eq!(split["amount"], "10191", "0-dp target → whole units");
        assert_eq!(split["currency_code"], "JPY");
        assert_eq!(split["foreign_currency_code"], "USD");
    }

    /// H1/H2: a `.xx5` midpoint rounds to the nearest EVEN last digit under
    /// MidpointNearestEven (banker's rounding), 2-dp target.
    #[test]
    fn midpoint_rounds_to_nearest_even() {
        // amount 1.005, rate 1 → 1.005 → 1.00 (round half to even: 0 is even).
        let mut rec = banco_record("EUR");
        rec.money = Money::new(
            Amount::parse("1.005").unwrap(),
            Currency::parse("EUR").unwrap(),
        );
        let target = usd_target();
        let group = build_group(&rec, "h", "106", &target, Decimal::ONE);
        let json = serde_json::to_value(&group).unwrap();
        assert_eq!(json["transactions"][0]["amount"], "1");

        // amount 1.015, rate 1 → 1.015 → 1.02 (round half to even: 2 is even).
        let mut rec2 = banco_record("EUR");
        rec2.money = Money::new(
            Amount::parse("1.015").unwrap(),
            Currency::parse("EUR").unwrap(),
        );
        let group2 = build_group(&rec2, "h", "106", &target, Decimal::ONE);
        let json2 = serde_json::to_value(&group2).unwrap();
        assert_eq!(json2["transactions"][0]["amount"], "1.02");
    }

    // --- account-target parsing --------------------------------------------

    #[test]
    fn parses_account_currency_and_decimals() {
        let body = r#"{"data":{"id":"106","attributes":{"name":"Banco USD","currency_code":"USD","currency_decimal_places":2}}}"#;
        let t = parse_account_target(body).unwrap();
        assert_eq!(t.currency, "USD");
        assert_eq!(t.decimals, 2);
    }

    #[test]
    fn parses_zero_decimal_currency() {
        let body = r#"{"data":{"attributes":{"currency_code":"JPY","currency_decimal_places":0}}}"#;
        let t = parse_account_target(body).unwrap();
        assert_eq!(t.currency, "JPY");
        assert_eq!(t.decimals, 0);
    }

    #[test]
    fn defaults_decimals_to_two_when_absent() {
        let body = r#"{"data":{"attributes":{"currency_code":"EUR"}}}"#;
        assert_eq!(parse_account_target(body).unwrap().decimals, 2);
    }

    #[test]
    fn parse_account_target_errors_when_currency_absent() {
        assert!(parse_account_target(r#"{"data":{"attributes":{}}}"#).is_err());
        assert!(parse_account_target(r#"{"data":{"attributes":{"currency_code":""}}}"#).is_err());
        assert!(parse_account_target("not json").is_err());
    }

    /// A foreign Banco record's rate threads through the public client via a
    /// pre-seeded FX cache (no network for the rate).
    #[test]
    fn seeded_fx_rate_threads_through_client() {
        let http = Client::new();
        let date = NaiveDate::from_ymd_opt(2026, 5, 27).unwrap();
        let fx = FxClient::with_seeded_rate(
            &http,
            "JPY",
            "USD",
            date,
            Decimal::from_str("0.0064").unwrap(),
        );
        let c = client(&http, &fx);
        let mut rec = banco_record("JPY");
        rec.date = date;
        rec.money = Money::new(
            Amount::parse("5130").unwrap(),
            Currency::parse("JPY").unwrap(),
        );
        let account = c.route_account(&rec).unwrap();
        assert_eq!(account.as_str(), "106");
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let rate = rt
            .block_on(c.fx.rate(rec.currency().as_str(), "USD", rec.date))
            .expect("seeded rate, no network");
        let target = usd_target();
        let group = build_group(&rec, "h", account.as_str(), &target, rate);
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
        for hint in [
            None,
            Some(""),
            Some("   "),
            Some("Balance"),
            Some("Visa ending 1234"),
            Some("bank"),
        ] {
            assert!(
                !is_paypal_credit_funding(&paypal_record(hint, "USD")),
                "hint {hint:?} should NOT classify as credit"
            );
        }
    }

    // --- M3: 422 duplicate detection ---------------------------------------

    #[test]
    fn detects_duplicate_message_in_top_level_message() {
        assert!(is_duplicate_error(
            r#"{"message":"Duplicate of transaction #123.","errors":{}}"#
        ));
    }

    #[test]
    fn detects_duplicate_message_in_field_errors() {
        let body = r#"{"message":"The given data was invalid.","errors":{"transactions.0.description":["Duplicate of transaction #99."]}}"#;
        assert!(is_duplicate_error(body));
    }

    #[test]
    fn non_duplicate_422_is_not_a_duplicate() {
        // A real validation failure must NOT be swallowed as a duplicate.
        assert!(!is_duplicate_error(
            r#"{"message":"The given data was invalid.","errors":{"transactions.0.amount":["The amount is required."]}}"#
        ));
        // A 422 whose body isn't the expected envelope is conservatively a
        // non-duplicate (→ real failure → Review).
        assert!(!is_duplicate_error("not json"));
        assert!(!is_duplicate_error(r#"{"unexpected":true}"#));
    }

    // --- property tests --------------------------------------------------

    use proptest::prelude::*;

    /// Ten-to-the-power for the minor-unit width of a currency, as a `Decimal`.
    fn minor_unit(decimals: u32) -> Decimal {
        // 10^-decimals: e.g. decimals=2 → 0.01, decimals=0 → 1.
        Decimal::new(1, decimals)
    }

    proptest! {
        /// FX rounding reconciliation: the booked (rounded) target amount is
        /// always within one minor unit of the exact (unrounded) conversion.
        #[test]
        fn prop_fx_rounding_within_one_minor_unit(
            // record amount in EUR (a non-USD source so conversion fires),
            amount_cents in 1u64..100_000_000,
            // rate scaled by 1e4, kept positive and bounded,
            rate_e4 in 1u64..2_000_000,
            decimals in 0u32..=2,
        ) {
            let amount = Decimal::new(amount_cents as i64, 2); // e.g. 1234 → 12.34
            let rate = Decimal::new(rate_e4 as i64, 4);        // e.g. 6400 → 0.6400

            let mut rec = banco_record("EUR");
            rec.money = Money::new(
                Amount::parse(&amount.to_string()).unwrap(),
                Currency::parse("EUR").unwrap(),
            );
            let target = Target { currency: "USD".to_string(), decimals };
            let group = build_group(&rec, "h", "106", &target, rate);
            let booked = Decimal::from_str(&group.transactions[0].amount).unwrap();

            let exact = amount * rate;
            let diff = (booked - exact).abs();
            // Strictly within one minor unit (round-half-even is at most half a
            // unit off, comfortably inside one unit).
            prop_assert!(
                diff < minor_unit(decimals),
                "booked {} vs exact {} differ by {} >= one minor unit",
                booked, exact, diff,
            );
            // And the booked amount carries no more than `decimals` places.
            prop_assert!(booked.scale() <= decimals);
        }
    }
}
