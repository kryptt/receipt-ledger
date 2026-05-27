//! PayPal adapter.
//!
//! PayPal sends a rich English receipt with a stable `Transaction ID`. The
//! adapter asks the LLM to extract the canonical fields as JSON, then parses
//! that JSON into [`Extracted`]. It is deliberately liberal in what JSON shapes
//! it accepts (single object, array, or `{"transactions":[...]}`) and in how
//! amounts arrive (JSON number or string), because small models are not
//! perfectly consistent — but it is strict about producing well-typed output.
//!
//! Not every mail from `service@paypal.com` is a receipt: shipping updates
//! ("your order is on its way"), "Pay in 4 plan" reminders, and surveys also
//! arrive. Those are a clean [`Outcome::NotATransaction`], detected both
//! deterministically ([`PaypalAdapter::is_transaction`]) and via a `kind`
//! discriminant the model fills in — never a date-parse error polluting Review.

use anyhow::{Context, Result, anyhow};
use chrono::NaiveDate;
use serde_json::Value;

use super::parse::{collect_objects, currency_field, parse_amount, parse_date_with, string_field};
use super::{Adapter, Outcome};
use crate::schema::{Direction, Extracted, Money, Source};

/// Sender substring that identifies a PayPal notification.
const PAYPAL_SENDER: &str = "service@paypal.com";

/// Deterministic markers that a PayPal mail is an actual payment receipt rather
/// than a shipping/marketing/survey notice. Matched case-insensitively. A real
/// receipt always carries the transaction id label and the "you paid" phrasing.
const RECEIPT_MARKERS: &[&str] = &["transaction id", "you paid"];

pub struct PaypalAdapter;

impl Adapter for PaypalAdapter {
    fn name(&self) -> &'static str {
        "paypal"
    }

    fn matches(&self, original_sender: &str) -> bool {
        original_sender.contains(PAYPAL_SENDER)
    }

    fn is_transaction(&self, body: &str) -> bool {
        let lower = body.to_ascii_lowercase();
        RECEIPT_MARKERS.iter().any(|m| lower.contains(m))
    }

    fn prompt(&self, email_text: &str) -> String {
        format!(
            r#"You classify and extract a single financial transaction from a PayPal email.
Return ONLY a JSON object (no prose, no markdown fences) with EXACTLY these keys:

{{
  "kind": "transaction" | "other", // "transaction" ONLY for an actual payment
                           // receipt (money moved). Use "other" for shipping
                           // updates ("your order is on its way"), plan/"Pay in
                           // 4" reminders, surveys, marketing, or anything where
                           // no payment occurred.
  "external_id": string,   // the "Transaction ID" value
  "amount": string,        // the TOTAL amount as a decimal string, e.g. "149.99"
  "currency": string,      // ISO-4217 code of the total, e.g. "EUR"
  "direction": "out" | "in", // "out" if the user paid/sent money, "in" if received
  "date": string,          // the transaction date as ISO YYYY-MM-DD
  "merchant": string,      // the merchant / payee name
  "account_hint": string,  // the FUNDING METHOD/SOURCE, stated plainly, else ""
  "status": string,        // the payment status text, e.g. "approved" / "completed"
  "raw_ref": string        // the Order ID if present, else the Transaction ID
}}

Rules:
- If this email is NOT a payment receipt, set "kind" to "other" and leave the
  other fields as "". Otherwise set "kind" to "transaction".
- Use the purchase TOTAL and its currency, not any converted funding amount.
- "amount" must be a positive decimal string with a dot separator.
- If the payment clearly succeeded, set "status" to "approved".
- For "account_hint", report HOW the payment was funded as clearly as the
  receipt states it. Use the PayPal funding product name when present —
  "PayPal Credit", "Pay in 4", "Pay Later", "Pay Monthly" — or "Balance" when
  funded from the PayPal balance, or the card/bank when funded that way, e.g.
  "Visa ending 1234" or "bank". If the funding source is not stated, use "".
- Do not invent values; if a field is genuinely absent use "".

PayPal email:
---
{email_text}
---"#
        )
    }

    fn postprocess(&self, json: &Value) -> Result<Outcome> {
        // Honour an explicit non-transaction classification from the model.
        if let Some(reason) = not_a_transaction_reason(json) {
            return Ok(Outcome::NotATransaction { reason });
        }
        let objects = collect_objects(json);
        if objects.is_empty() {
            return Err(anyhow!("LLM JSON contained no transaction object"));
        }
        let records = objects.iter().map(parse_one).collect::<Result<Vec<_>>>()?;
        Ok(Outcome::Transaction(records))
    }
}

/// If the model classified the top-level object as `kind: "other"`, return the
/// skip reason; otherwise `None`. Only a bare object carries a top-level
/// `kind` — array/`transactions` shapes are treated as transactions.
fn not_a_transaction_reason(json: &Value) -> Option<String> {
    let kind = json.as_object()?.get("kind")?.as_str()?.trim();
    if kind.eq_ignore_ascii_case("other") || kind.eq_ignore_ascii_case("not_a_transaction") {
        Some("model classified PayPal mail as non-transaction (kind=other)".to_string())
    } else {
        None
    }
}

/// Parse one JSON object into a typed [`Extracted`].
fn parse_one(obj: &Value) -> Result<Extracted> {
    let map = obj
        .as_object()
        .ok_or_else(|| anyhow!("expected JSON object, got {obj}"))?;

    let amount = parse_amount(map.get("amount")).context("parsing `amount`")?;
    let currency = currency_field(map, "currency")?;
    let direction = parse_direction(map.get("direction"));
    let date = parse_date(map.get("date")).context("parsing `date`")?;
    let merchant = string_field(map, "merchant").ok_or_else(|| anyhow!("missing `merchant`"))?;
    let status = string_field(map, "status").unwrap_or_default();

    let external_id = string_field(map, "external_id");
    let raw_ref = string_field(map, "raw_ref")
        .or_else(|| external_id.clone())
        .unwrap_or_default();
    let account_hint = string_field(map, "account_hint");

    Ok(Extracted {
        source: Source::Paypal,
        external_id,
        money: Money::new(amount, currency),
        direction,
        date,
        merchant,
        account_hint,
        status,
        raw_ref,
    })
}

/// Default to `out` (a purchase) when the model omits or garbles direction —
/// PayPal receipts are overwhelmingly outgoing payments, and validation will
/// still gate on status.
fn parse_direction(v: Option<&Value>) -> Direction {
    match v
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
    {
        Some(s) if s == "in" => Direction::In,
        _ => Direction::Out,
    }
}

/// Accept ISO `YYYY-MM-DD` first, then a couple of human formats PayPal uses
/// ("May 11, 2026") and US `%m/%d/%Y`.
fn parse_date(v: Option<&Value>) -> Result<NaiveDate> {
    const FORMATS: &[&str] = &["%Y-%m-%d", "%B %e, %Y", "%b %e, %Y", "%m/%d/%Y"];
    parse_date_with(v, FORMATS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ValidationPolicy;
    use crate::validate::{Verdict, validate};
    use rust_decimal::Decimal;
    use serde_json::json;
    use std::str::FromStr;

    /// No-ceiling validation policy for these adapter-level tests.
    fn policy() -> ValidationPolicy {
        ValidationPolicy { max_amount: None }
    }

    /// The JSON a correctly-behaving model produces for the fixture receipt.
    fn fixture_json() -> Value {
        json!({
            "kind": "transaction",
            "external_id": "8XY12345AB678901C",
            "amount": "149.99",
            "currency": "EUR",
            "direction": "out",
            "date": "2026-05-11",
            "merchant": "Example Merchant B.V.",
            "account_hint": "Pay in 4",
            "status": "approved",
            "raw_ref": "TESTORDER0123456"
        })
    }

    /// The single record from a `Transaction` outcome, or a panic.
    fn one(outcome: Outcome) -> Extracted {
        match outcome {
            Outcome::Transaction(mut v) => {
                assert_eq!(v.len(), 1);
                v.pop().unwrap()
            }
            Outcome::NotATransaction { reason } => {
                panic!("expected transaction, got skip: {reason}")
            }
        }
    }

    #[test]
    fn matches_paypal_sender() {
        assert!(PaypalAdapter.matches("service@paypal.com"));
        assert!(PaypalAdapter.matches("paypal <service@paypal.com>"));
        assert!(!PaypalAdapter.matches("notificaciones@popularenlinea.com"));
    }

    #[test]
    fn is_transaction_detects_receipt_markers() {
        assert!(PaypalAdapter.is_transaction("... Transaction ID: 8XY ..."));
        assert!(PaypalAdapter.is_transaction("You paid €1.00 to Shop"));
        // Non-receipt mail: shipping, plan reminders, surveys.
        assert!(!PaypalAdapter.is_transaction("Your order is on its way!"));
        assert!(!PaypalAdapter.is_transaction("Your Pay in 4 plan: next payment due soon"));
        assert!(!PaypalAdapter.is_transaction("How did we do? Take our survey."));
    }

    #[test]
    fn kind_other_is_clean_skip_not_review() {
        let v = json!({"kind": "other"});
        assert!(matches!(
            PaypalAdapter.postprocess(&v).unwrap(),
            Outcome::NotATransaction { .. }
        ));
    }

    #[test]
    fn postprocess_then_validate_books_the_fixture() {
        let e = one(PaypalAdapter.postprocess(&fixture_json()).unwrap());

        assert_eq!(e.external_id.as_deref(), Some("8XY12345AB678901C"));
        assert_eq!(e.amount().value(), Decimal::from_str("149.99").unwrap());
        assert_eq!(e.currency().as_str(), "EUR");
        assert_eq!(e.direction, Direction::Out);
        assert_eq!(e.date, NaiveDate::from_ymd_opt(2026, 5, 11).unwrap());
        assert!(e.merchant.contains("Example Merchant"));

        match validate(e, &policy()) {
            Verdict::Booked(b) => {
                assert_eq!(
                    b.as_extracted().external_id.as_deref(),
                    Some("8XY12345AB678901C")
                )
            }
            Verdict::Review { reason } => panic!("fixture should book, got review: {reason}"),
        }
    }

    #[test]
    fn accepts_numeric_amount_and_human_date() {
        let v = json!({
            "kind": "transaction",
            "external_id": "X",
            "amount": 12.50,
            "currency": "usd",
            "direction": "out",
            "date": "May 11, 2026",
            "merchant": "Shop",
            "status": "completed",
            "raw_ref": "X"
        });
        let e = one(PaypalAdapter.postprocess(&v).unwrap());
        assert_eq!(e.amount().value(), Decimal::from_str("12.50").unwrap());
        assert_eq!(e.currency().as_str(), "USD");
        assert_eq!(e.date, NaiveDate::from_ymd_opt(2026, 5, 11).unwrap());
    }

    #[test]
    fn declined_status_postprocesses_but_validation_reviews() {
        let mut v = fixture_json();
        v["status"] = json!("Declined");
        let e = one(PaypalAdapter.postprocess(&v).unwrap());
        assert!(matches!(validate(e, &policy()), Verdict::Review { .. }));
    }

    #[test]
    fn missing_amount_is_an_error() {
        let mut v = fixture_json();
        v.as_object_mut().unwrap().remove("amount");
        assert!(PaypalAdapter.postprocess(&v).is_err());
    }

    #[test]
    fn accepts_transactions_wrapper() {
        let v = json!({ "transactions": [fixture_json()] });
        match PaypalAdapter.postprocess(&v).unwrap() {
            Outcome::Transaction(v) => assert_eq!(v.len(), 1),
            Outcome::NotATransaction { reason } => panic!("unexpected skip: {reason}"),
        }
    }
}
