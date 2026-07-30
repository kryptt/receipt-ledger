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
use serde_json::Value;

use super::Outcome;
use crate::schema::{Amount, Currency, Extracted};

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
///
/// This serves both the "required, error if absent" and the "genuinely
/// optional" call sites — the distinction is made by the caller (`ok_or_else`
/// vs. using the `Option` directly), so a single primitive suffices.
pub fn string_field(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    match map.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// Parse a [`Currency`] field: present, non-blank, exactly a 3-letter ISO code.
pub fn currency_field(map: &serde_json::Map<String, Value>, key: &str) -> Result<Currency> {
    let raw = string_field(map, key).ok_or_else(|| anyhow!("missing `{key}`"))?;
    Currency::parse(&raw).map_err(|e| anyhow!("parsing `{key}`: {e}"))
}

/// Accept the amount as a JSON string ("149.99") or number (149.99) and run it
/// through the sanitizing [`Amount::parse`] gate: only a plain non-negative
/// decimal of bounded scale survives. Scientific notation, digit separators,
/// thousands commas, signs, and currency symbols are all rejected at this
/// source-string boundary, before any [`rust_decimal::Decimal`] parsing.
pub fn parse_amount(v: Option<&Value>) -> Result<Amount> {
    parse_amount_with(v, |s| s.to_string())
}

/// Like [`parse_amount`] but applies `normalize` to the *source string* before
/// the gate. Adapters whose source legitimately embeds a thousands separator
/// (Banco Popular renders `5,130.00`) pass a stripper here so the gate sees a
/// clean decimal — the only place a comma is ever removed, and only from the
/// adapter that genuinely produces it.
pub fn parse_amount_with(v: Option<&Value>, normalize: impl Fn(&str) -> String) -> Result<Amount> {
    let raw = match v {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        other => return Err(anyhow!("amount missing or wrong type: {other:?}")),
    };
    let cleaned = normalize(&raw);
    Amount::parse(&cleaned).map_err(|e| anyhow!("amount {raw:?} rejected: {e}"))
}

/// Strip ASCII thousands separators (`,`) from a decimal source string, leaving
/// the dot as the decimal point. `"5,130.00"` → `"5130.00"`. Used only by the
/// Banco Popular adapter, whose notifications render grouped amounts.
pub fn strip_thousands_commas(s: &str) -> String {
    s.replace(',', "")
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

/// The fields shared by every transaction-producing adapter's `parse_one`. Each
/// adapter still controls *how* the amount and date are parsed (different formats
/// and normalizers), but the surrounding ceremony — extracting the JSON map,
/// reading merchant/status/account_hint/raw_ref — is identical and lives here.
pub struct CommonFields<'a> {
    /// The JSON map backing this object (borrowed from the input `Value`).
    pub map: &'a serde_json::Map<String, Value>,
    pub amount: Amount,
    pub currency: Currency,
    pub date: NaiveDate,
    pub merchant: String,
    pub status: String,
    pub account_hint: Option<String>,
    pub raw_ref: String,
}

/// Extract the fields that every adapter's `parse_one` shares: validates that
/// the value is a JSON object, reads amount/currency/date via caller-supplied
/// closures, and reads merchant/status/account_hint/raw_ref from fixed keys.
///
/// The two closures decouple the *shared* field extraction from the *source-
/// specific* parsing: Banco Popular strips thousands commas and uses `%d/%m/%Y`;
/// PayPal uses bare `parse_amount` and US date formats.
pub fn extract_common_fields<'a>(
    obj: &'a Value,
    parse_amt: impl FnOnce(&serde_json::Map<String, Value>) -> Result<Amount>,
    parse_dt: impl FnOnce(&serde_json::Map<String, Value>) -> Result<NaiveDate>,
) -> Result<CommonFields<'a>> {
    let map = obj
        .as_object()
        .ok_or_else(|| anyhow!("expected JSON object, got {obj}"))?;

    let amount = parse_amt(map)?;
    let currency = currency_field(map, "currency")?;
    let date = parse_dt(map)?;
    let merchant = string_field(map, "merchant").ok_or_else(|| anyhow!("missing `merchant`"))?;
    let status = string_field(map, "status").unwrap_or_default();
    let account_hint = string_field(map, "account_hint");
    let raw_ref = string_field(map, "raw_ref").unwrap_or_default();

    Ok(CommonFields {
        map,
        amount,
        currency,
        date,
        merchant,
        status,
        account_hint,
        raw_ref,
    })
}

/// Collect JSON objects from the LLM response, parse each with `parse_one`, and
/// wrap the results in [`Outcome::Transaction`]. The shared postprocess body for
/// adapters whose LLM path produces transaction records.
pub fn postprocess_transactions(
    json: &Value,
    parse_one: impl Fn(&Value) -> Result<Extracted>,
) -> Result<Outcome> {
    let objects = collect_objects(json);
    if objects.is_empty() {
        return Err(anyhow!("LLM JSON contained no transaction object"));
    }
    let records = objects.iter().map(parse_one).collect::<Result<Vec<_>>>()?;
    Ok(Outcome::Transaction(records))
}
