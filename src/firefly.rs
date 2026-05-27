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

use anyhow::{Context, Result};
use reqwest::{Client, StatusCode};
use serde::Serialize;
use tracing::{info, warn};

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
}

impl<'a> FireflyClient<'a> {
    pub fn new(
        http: &'a Client,
        base_url: impl Into<String>,
        token: impl Into<String>,
        paypal_balance_account: impl Into<String>,
        paypal_credit_account: Option<String>,
        banco_popular_usd_account: Option<String>,
        banco_popular_dop_account: Option<String>,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            token: token.into(),
            paypal_balance_account: paypal_balance_account.into(),
            paypal_credit_account,
            banco_popular_usd_account,
            banco_popular_dop_account,
        }
    }

    /// Submit one validated, deduped record as a withdrawal.
    ///
    /// `external_id` is the dedup key computed by [`crate::dedup`].
    pub async fn submit(&self, record: &Extracted, external_id: &str) -> Result<SubmitOutcome> {
        let url = format!("{}/api/v1/transactions", self.base_url.trim_end_matches('/'));
        let group = self.build_group(record, external_id)?;

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

    fn build_group<'b>(
        &'b self,
        record: &'b Extracted,
        external_id: &'b str,
    ) -> Result<TransactionGroup<'b>> {
        // Route to the Firefly account for this record. Exhaustive over
        // `Source` so a new variant forces a routing decision here rather than
        // silently booking against the wrong account. Within each source the
        // funding/currency rules pick balance-vs-credit (PayPal) or
        // USD-vs-DOP (Banco Popular). A needed-but-unconfigured Option account
        // is an `Err`, which the pipeline turns into a per-message Review.
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

        let (source_id, source_name) = if is_numeric(account) {
            (Some(account), None)
        } else {
            (None, Some(account))
        };

        let kind = match record.direction {
            Direction::Out => "withdrawal",
            Direction::In => "deposit",
        };

        Ok(TransactionGroup {
            error_if_duplicate_hash: true,
            // Let Firefly rules (e.g. the future transit-account rewrite) fire.
            apply_rules: true,
            transactions: vec![Split {
                kind,
                date: record.date.to_string(),
                amount: record.amount.normalize().to_string(),
                currency_code: &record.currency,
                description: &record.merchant,
                external_id,
                tags: vec![IMPORT_TAG],
                source_id,
                source_name,
                destination_name: Some(&record.merchant),
            }],
        })
    }
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

    /// A client wired with the four production account ids.
    fn client<'a>(http: &'a Client) -> FireflyClient<'a> {
        FireflyClient::new(
            http,
            "http://firefly:8080",
            "tok",
            "103",                       // PayPal Balance
            Some("105".to_string()),     // PayPal Credit
            Some("106".to_string()),     // Banco Popular USD
            Some("107".to_string()),     // Banco Popular DOP
        )
    }

    #[test]
    fn builds_withdrawal_with_named_account() {
        let http = Client::new();
        // A non-numeric balance account exercises the name (not id) path.
        let c = FireflyClient::new(
            &http,
            "http://firefly:8080",
            "tok",
            "PayPal Balance",
            None,
            None,
            None,
        );
        let rec = paypal_record(Some("Balance"), "EUR");
        let group = c.build_group(&rec, "8XY12345AB678901C").unwrap();
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
    }

    fn source_id_of(c: &FireflyClient, rec: &Extracted) -> String {
        let group = c.build_group(rec, "hash").unwrap();
        let json = serde_json::to_value(&group).unwrap();
        assert!(
            json["transactions"][0].get("source_name").is_none(),
            "numeric account must use source_id, not source_name"
        );
        json["transactions"][0]["source_id"]
            .as_str()
            .expect("numeric account → source_id string")
            .to_string()
    }

    // --- routing -----------------------------------------------------------

    #[test]
    fn paypal_credit_funded_routes_to_credit_account() {
        let http = Client::new();
        let c = client(&http);
        for hint in ["Pay in 4", "PayPal Credit"] {
            let rec = paypal_record(Some(hint), "USD");
            assert_eq!(source_id_of(&c, &rec), "105", "hint {hint:?} should be credit");
        }
    }

    #[test]
    fn paypal_balance_funded_routes_to_balance_account() {
        let http = Client::new();
        let c = client(&http);
        // Explicit balance hint and an absent hint both default to balance.
        assert_eq!(source_id_of(&c, &paypal_record(Some("Balance"), "USD")), "103");
        assert_eq!(source_id_of(&c, &paypal_record(None, "USD")), "103");
    }

    #[test]
    fn banco_dop_routes_to_dop_account() {
        let http = Client::new();
        let c = client(&http);
        assert_eq!(source_id_of(&c, &banco_record("DOP")), "107");
        // Currency is uppercased before comparison.
        assert_eq!(source_id_of(&c, &banco_record("dop")), "107");
    }

    #[test]
    fn banco_non_dop_routes_to_usd_account() {
        let http = Client::new();
        let c = client(&http);
        for cur in ["USD", "EUR"] {
            assert_eq!(source_id_of(&c, &banco_record(cur)), "106", "currency {cur} → USD acct");
        }
    }

    #[test]
    fn needed_but_unconfigured_account_errors() {
        let http = Client::new();
        // Only the required balance account is configured; everything else None.
        // A credit-funded PayPal record and either Banco record must error
        // (→ Review) rather than booking against the wrong account.
        let c = FireflyClient::new(&http, "http://firefly:8080", "tok", "103", None, None, None);
        assert!(c.build_group(&paypal_record(Some("Pay in 4"), "USD"), "h").is_err());
        assert!(c.build_group(&banco_record("DOP"), "h").is_err());
        assert!(c.build_group(&banco_record("USD"), "h").is_err());
        // ...but a balance-funded PayPal record still books.
        assert!(c.build_group(&paypal_record(Some("Balance"), "USD"), "h").is_ok());
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
