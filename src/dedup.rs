//! Deduplication keys.
//!
//! Each booked transaction needs a stable identity so re-running the job (or a
//! re-forwarded email) does not double-book. PayPal gives us a real
//! `Transaction ID`, which is the strongest possible key. Sources without an
//! id (e.g. Banco Popular, v2) fall back to a composite hash over the salient
//! fields. Firefly's own external-id duplicate detection is the final backstop.

use sha2::{Digest, Sha256};

use crate::schema::Extracted;

/// The dedup key Firefly receives as `external_id`.
///
/// - If the record carries an `external_id` (PayPal Transaction ID), use it
///   verbatim — it is globally unique and stable.
/// - Otherwise derive the composite hash so id-less sources still dedup.
pub fn external_id(record: &Extracted) -> String {
    match &record.external_id {
        Some(id) if !id.trim().is_empty() => id.trim().to_string(),
        _ => composite_hash(record),
    }
}

/// `sha256(date|amount|merchant|last4|status)`, hex-encoded.
///
/// Deterministic and order-stable: the same record always yields the same
/// hash, and two materially different records differ in at least one field.
pub fn composite_hash(record: &Extracted) -> String {
    let last4 = record.account_hint.as_deref().unwrap_or("");
    let material = format!(
        "{date}|{amount}|{merchant}|{last4}|{status}",
        date = record.date,
        amount = record.amount.normalize(),
        merchant = record.merchant.trim(),
        last4 = last4.trim(),
        status = record.status.trim().to_ascii_lowercase(),
    );
    let digest = Sha256::digest(material.as_bytes());
    hex(&digest)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    s
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
    fn paypal_uses_transaction_id() {
        assert_eq!(external_id(&record()), "8XY12345AB678901C");
    }

    #[test]
    fn falls_back_to_composite_when_no_id() {
        let mut r = record();
        r.external_id = None;
        let id = external_id(&r);
        assert_eq!(id.len(), 64, "sha256 hex is 64 chars");
        // hex only
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn composite_is_deterministic() {
        let mut r = record();
        r.external_id = None;
        assert_eq!(composite_hash(&r), composite_hash(&r));
    }

    #[test]
    fn composite_changes_with_amount() {
        let mut a = record();
        a.external_id = None;
        let mut b = a.clone();
        b.amount = Decimal::from_str("202.00").unwrap();
        assert_ne!(composite_hash(&a), composite_hash(&b));
    }
}
