//! Shared JSON-parsing helpers for adapters.
//!
//! Small models emit loose JSON; every adapter must turn that into well-typed
//! [`Extracted`](crate::schema::Extracted) fields. These helpers are the common
//! primitives — container-shape normalisation, string fields, amount-as-string-
//! or-number, and date parsing against an adapter-supplied format list. Each
//! adapter still owns its own `parse_one` (its field set differs); only the
//! field-level primitives live here.

use anyhow::{Result, anyhow};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr;

/// Normalise the various container shapes a model might emit into a flat list
/// of candidate objects: a bare object, an array, or `{"transactions":[...]}`.
pub fn collect_objects(json: &Value) -> Vec<Value> {
    match json {
        Value::Array(items) => items.clone(),
        Value::Object(map) => match map.get("transactions") {
            Some(Value::Array(items)) => items.clone(),
            _ => vec![json.clone()],
        },
        _ => Vec::new(),
    }
}

/// A present, non-blank string field, trimmed. Absent/blank/non-string → `None`.
pub fn string_field(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    match map.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// Like [`string_field`] but named for genuinely optional fields, where an
/// absent value is expected rather than an error.
pub fn opt_string(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    string_field(map, key)
}

/// Accept the amount as a JSON string ("149.99") or number (149.99).
pub fn parse_amount(v: Option<&Value>) -> Result<Decimal> {
    match v {
        Some(Value::String(s)) => Decimal::from_str(s.trim())
            .map_err(|e| anyhow!("amount string {s:?} is not a decimal: {e}")),
        Some(Value::Number(n)) => Decimal::from_str(&n.to_string())
            .map_err(|e| anyhow!("amount number {n} is not a decimal: {e}")),
        other => Err(anyhow!("amount missing or wrong type: {other:?}")),
    }
}

/// Parse a date string against an ordered list of `chrono` format strings,
/// returning the first that matches. The caller supplies the formats because
/// they are source-specific (PayPal: US `%m/%d/%Y`; Banco Popular: `%d/%m/%Y`).
pub fn parse_date_with(v: Option<&Value>, formats: &[&str]) -> Result<NaiveDate> {
    let s = v
        .and_then(Value::as_str)
        .map(str::trim)
        .ok_or_else(|| anyhow!("date missing or not a string"))?;

    for fmt in formats {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Ok(d);
        }
    }
    Err(anyhow!("unrecognised date format: {s:?}"))
}
