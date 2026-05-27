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
    /// PayPal asset account — name or numeric id.
    paypal_account: String,
    /// Banco Popular asset account — name or numeric id. `None` when
    /// unconfigured; a Banco Popular record then errors out (→ Review).
    banco_popular_account: Option<String>,
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
    /// Source account by id (numeric `paypal_account`) ...
    #[serde(skip_serializing_if = "Option::is_none")]
    source_id: Option<&'a str>,
    /// ... or by name (non-numeric `paypal_account`).
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
        paypal_account: impl Into<String>,
        banco_popular_account: Option<String>,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            token: token.into(),
            paypal_account: paypal_account.into(),
            banco_popular_account,
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
        // Route to the asset account for this record's source. Exhaustive over
        // `Source` so a new variant forces a routing decision here rather than
        // silently booking against PayPal.
        let account: &str = match record.source {
            Source::Paypal => &self.paypal_account,
            Source::BancoPopular => self
                .banco_popular_account
                .as_deref()
                .context("no Firefly account configured for Banco Popular")?,
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

    fn record() -> Extracted {
        Extracted {
            source: Source::Paypal,
            external_id: Some("8XY12345AB678901C".to_string()),
            amount: Decimal::from_str("149.99").unwrap(),
            currency: "EUR".to_string(),
            direction: Direction::Out,
            date: NaiveDate::from_ymd_opt(2026, 5, 11).unwrap(),
            merchant: "Example Merchant B.V.".to_string(),
            account_hint: None,
            status: "approved".to_string(),
            raw_ref: "TESTORDER0123456".to_string(),
        }
    }

    #[test]
    fn builds_withdrawal_with_named_account() {
        let http = Client::new();
        let c = FireflyClient::new(&http, "http://firefly:8080", "tok", "PayPal", None);
        let rec = record();
        let group = c.build_group(&rec, "8XY12345AB678901C").unwrap();
        let json = serde_json::to_value(&group).unwrap();

        assert_eq!(json["error_if_duplicate_hash"], true);
        let split = &json["transactions"][0];
        assert_eq!(split["type"], "withdrawal");
        assert_eq!(split["amount"], "149.99");
        assert_eq!(split["currency_code"], "EUR");
        assert_eq!(split["external_id"], "8XY12345AB678901C");
        assert_eq!(split["source_name"], "PayPal");
        assert!(split.get("source_id").is_none());
        assert_eq!(split["destination_name"], "Example Merchant B.V.");
        assert_eq!(split["tags"][0], "receipt-ledger");
        assert_eq!(split["date"], "2026-05-11");
    }

    #[test]
    fn numeric_account_uses_source_id() {
        let http = Client::new();
        let c = FireflyClient::new(&http, "http://firefly:8080", "tok", "42", None);
        let rec = record();
        let group = c.build_group(&rec, "id").unwrap();
        let json = serde_json::to_value(&group).unwrap();
        assert_eq!(json["transactions"][0]["source_id"], "42");
        assert!(json["transactions"][0].get("source_name").is_none());
    }

    fn banco_record() -> Extracted {
        Extracted {
            source: Source::BancoPopular,
            external_id: None,
            amount: Decimal::from_str("1.50").unwrap(),
            currency: "EUR".to_string(),
            direction: Direction::Out,
            date: NaiveDate::from_ymd_opt(2026, 5, 27).unwrap(),
            merchant: "Example Cafe Amsterdam".to_string(),
            account_hint: Some("1234".to_string()),
            status: "Aprobada".to_string(),
            raw_ref: String::new(),
        }
    }

    #[test]
    fn banco_record_routes_to_its_configured_account() {
        let http = Client::new();
        let c = FireflyClient::new(
            &http,
            "http://firefly:8080",
            "tok",
            "42", // PayPal account — must NOT be used for a Banco record.
            Some("104".to_string()),
        );
        let rec = banco_record();
        let group = c.build_group(&rec, "hash").unwrap();
        let json = serde_json::to_value(&group).unwrap();
        // Numeric Banco account → source_id "104", not the PayPal "42".
        assert_eq!(json["transactions"][0]["source_id"], "104");
        assert!(json["transactions"][0].get("source_name").is_none());
    }

    #[test]
    fn banco_record_without_account_errors() {
        let http = Client::new();
        let c = FireflyClient::new(&http, "http://firefly:8080", "tok", "42", None);
        // No Banco account configured → build_group errors, which the pipeline
        // turns into a per-message Review rather than a panic.
        assert!(c.build_group(&banco_record(), "hash").is_err());
    }

    #[test]
    fn detects_duplicate_message() {
        assert!(is_duplicate_error(
            r#"{"message":"Duplicate of transaction #123."}"#
        ));
        assert!(!is_duplicate_error(r#"{"message":"The amount is required."}"#));
    }
}
