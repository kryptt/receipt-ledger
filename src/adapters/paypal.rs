//! PayPal adapter.
//!
//! PayPal sends a rich English receipt with a stable `Transaction ID`. The
//! adapter asks the LLM to extract the canonical fields as JSON, then parses
//! that JSON into [`Extracted`]. It is deliberately liberal in what JSON shapes
//! it accepts (single object, array, or `{"transactions":[...]}`) and in how
//! amounts arrive (JSON number or string), because small models are not
//! perfectly consistent — but it is strict about producing well-typed output.

use anyhow::{Context, Result, anyhow};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr;

use super::Adapter;
use crate::schema::{Direction, Extracted, Source};

/// Sender substring that identifies a PayPal notification.
const PAYPAL_SENDER: &str = "service@paypal.com";

pub struct PaypalAdapter;

impl Adapter for PaypalAdapter {
    fn name(&self) -> &'static str {
        "paypal"
    }

    fn matches(&self, original_sender: &str) -> bool {
        original_sender.contains(PAYPAL_SENDER)
    }

    fn prompt(&self, email_text: &str) -> String {
        format!(
            r#"You extract a single financial transaction from a PayPal receipt email.
Return ONLY a JSON object (no prose, no markdown fences) with EXACTLY these keys:

{{
  "external_id": string,   // the "Transaction ID" value
  "amount": string,        // the TOTAL amount as a decimal string, e.g. "149.99"
  "currency": string,      // ISO-4217 code of the total, e.g. "EUR"
  "direction": "out" | "in", // "out" if the user paid/sent money, "in" if received
  "date": string,          // the transaction date as ISO YYYY-MM-DD
  "merchant": string,      // the merchant / payee name
  "account_hint": string,  // funding source / card last4 if stated, else ""
  "status": string,        // the payment status text, e.g. "approved" / "completed"
  "raw_ref": string        // the Order ID if present, else the Transaction ID
}}

Rules:
- Use the purchase TOTAL and its currency, not any converted funding amount.
- "amount" must be a positive decimal string with a dot separator.
- If the payment clearly succeeded, set "status" to "approved".
- Do not invent values; if a field is genuinely absent use "".

PayPal receipt:
---
{email_text}
---"#
        )
    }

    fn postprocess(&self, json: &Value) -> Result<Vec<Extracted>> {
        let objects = collect_objects(json);
        if objects.is_empty() {
            return Err(anyhow!("LLM JSON contained no transaction object"));
        }
        objects.iter().map(parse_one).collect()
    }
}

/// Normalise the various container shapes a model might emit into a flat list
/// of candidate objects.
fn collect_objects(json: &Value) -> Vec<Value> {
    match json {
        Value::Array(items) => items.clone(),
        Value::Object(map) => match map.get("transactions") {
            Some(Value::Array(items)) => items.clone(),
            _ => vec![json.clone()],
        },
        _ => Vec::new(),
    }
}

/// Parse one JSON object into a typed [`Extracted`].
fn parse_one(obj: &Value) -> Result<Extracted> {
    let map = obj
        .as_object()
        .ok_or_else(|| anyhow!("expected JSON object, got {obj}"))?;

    let amount = parse_amount(map.get("amount")).context("parsing `amount`")?;
    let currency = string_field(map, "currency")
        .map(|s| s.trim().to_ascii_uppercase())
        .ok_or_else(|| anyhow!("missing `currency`"))?;
    let direction = parse_direction(map.get("direction"));
    let date = parse_date(map.get("date")).context("parsing `date`")?;
    let merchant = string_field(map, "merchant").ok_or_else(|| anyhow!("missing `merchant`"))?;
    let status = string_field(map, "status").unwrap_or_default();

    let external_id = opt_string(map, "external_id");
    let raw_ref = opt_string(map, "raw_ref")
        .or_else(|| external_id.clone())
        .unwrap_or_default();
    let account_hint = opt_string(map, "account_hint");

    Ok(Extracted {
        source: Source::Paypal,
        external_id,
        amount,
        currency,
        direction,
        date,
        merchant,
        account_hint,
        status,
        raw_ref,
    })
}

fn string_field(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    match map.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// Like [`string_field`] but keeps the field absent (`None`) when empty rather
/// than failing — used for genuinely optional fields.
fn opt_string(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    string_field(map, key)
}

/// Accept the amount as a JSON string ("149.99") or number (149.99).
fn parse_amount(v: Option<&Value>) -> Result<Decimal> {
    match v {
        Some(Value::String(s)) => Decimal::from_str(s.trim())
            .map_err(|e| anyhow!("amount string {s:?} is not a decimal: {e}")),
        Some(Value::Number(n)) => Decimal::from_str(&n.to_string())
            .map_err(|e| anyhow!("amount number {n} is not a decimal: {e}")),
        other => Err(anyhow!("amount missing or wrong type: {other:?}")),
    }
}

/// Default to `out` (a purchase) when the model omits or garbles direction —
/// PayPal receipts are overwhelmingly outgoing payments, and validation will
/// still gate on status.
fn parse_direction(v: Option<&Value>) -> Direction {
    match v.and_then(Value::as_str).map(|s| s.trim().to_ascii_lowercase()) {
        Some(s) if s == "in" => Direction::In,
        _ => Direction::Out,
    }
}

/// Accept ISO `YYYY-MM-DD` first, then a couple of human formats PayPal uses
/// ("May 11, 2026").
fn parse_date(v: Option<&Value>) -> Result<NaiveDate> {
    let s = v
        .and_then(Value::as_str)
        .map(str::trim)
        .ok_or_else(|| anyhow!("date missing or not a string"))?;

    const FORMATS: &[&str] = &["%Y-%m-%d", "%B %e, %Y", "%b %e, %Y", "%m/%d/%Y"];
    for fmt in FORMATS {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Ok(d);
        }
    }
    Err(anyhow!("unrecognised date format: {s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate::{Verdict, validate};
    use serde_json::json;

    /// The JSON a correctly-behaving model produces for the fixture receipt.
    fn fixture_json() -> Value {
        json!({
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

    #[test]
    fn matches_paypal_sender() {
        assert!(PaypalAdapter.matches("service@paypal.com"));
        assert!(PaypalAdapter.matches("paypal <service@paypal.com>"));
        assert!(!PaypalAdapter.matches("notificaciones@popularenlinea.com"));
    }

    #[test]
    fn postprocess_then_validate_books_the_fixture() {
        let extracted = PaypalAdapter
            .postprocess(&fixture_json())
            .expect("postprocess should succeed");
        assert_eq!(extracted.len(), 1);
        let e = &extracted[0];

        assert_eq!(e.external_id.as_deref(), Some("8XY12345AB678901C"));
        assert_eq!(e.amount, Decimal::from_str("149.99").unwrap());
        assert_eq!(e.currency, "EUR");
        assert_eq!(e.direction, Direction::Out);
        assert_eq!(e.date, NaiveDate::from_ymd_opt(2026, 5, 11).unwrap());
        assert!(e.merchant.contains("Example Merchant"));

        match validate(e.clone()) {
            Verdict::Booked(b) => {
                assert_eq!(b.external_id.as_deref(), Some("8XY12345AB678901C"))
            }
            Verdict::Review { reason } => panic!("fixture should book, got review: {reason}"),
        }
    }

    #[test]
    fn accepts_numeric_amount_and_human_date() {
        let v = json!({
            "external_id": "X",
            "amount": 12.50,
            "currency": "usd",
            "direction": "out",
            "date": "May 11, 2026",
            "merchant": "Shop",
            "status": "completed",
            "raw_ref": "X"
        });
        let e = &PaypalAdapter.postprocess(&v).unwrap()[0];
        assert_eq!(e.amount, Decimal::from_str("12.50").unwrap());
        assert_eq!(e.currency, "USD");
        assert_eq!(e.date, NaiveDate::from_ymd_opt(2026, 5, 11).unwrap());
    }

    #[test]
    fn declined_status_postprocesses_but_validation_reviews() {
        let mut v = fixture_json();
        v["status"] = json!("Declined");
        let e = &PaypalAdapter.postprocess(&v).unwrap()[0];
        assert!(matches!(validate(e.clone()), Verdict::Review { .. }));
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
        assert_eq!(PaypalAdapter.postprocess(&v).unwrap().len(), 1);
    }
}
