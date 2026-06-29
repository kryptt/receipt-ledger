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
#[must_use]
pub fn external_id(record: &Extracted) -> String {
    match &record.external_id {
        Some(id) if !id.trim().is_empty() => id.trim().to_string(),
        _ => composite_hash(record),
    }
}

/// `sha256(source|date|amount|currency|merchant|last4|status)`, hex-encoded.
///
/// Deterministic and order-stable: the same record always yields the same
/// hash, and two materially different records differ in at least one field.
/// `source` and `currency` are part of the material so two charges that share
/// date/amount/merchant but differ in originating source or currency (e.g. a
/// $5.00 and a €5.00 charge at the same merchant on the same day) hash
/// distinctly and are not collapsed into one.
#[must_use]
pub fn composite_hash(record: &Extracted) -> String {
    let last4 = record.account_hint.as_deref().unwrap_or("");
    let material = format!(
        "{source}|{date}|{amount}|{currency}|{merchant}|{last4}|{status}",
        source = record.source.as_str(),
        date = record.date,
        amount = record.amount().value().normalize(),
        currency = record.currency().as_str(),
        merchant = record.merchant.trim(),
        last4 = last4.trim(),
        status = record.status.trim().to_ascii_lowercase(),
    );
    let digest = Sha256::digest(material.as_bytes());
    hex(&digest)
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Each nibble is in 0..=15, so indexing HEX is always in bounds —
        // no fallible step to unwrap.
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

// -- dedup unit tests (external_id + composite hash) --
#[cfg(test)]
mod tests {
    use crate::schema::Source;
    use crate::test_support::money;
    use super::*;

    /// Base dedup fixture: PayPal record with transaction id (has external_id).
    fn record() -> Extracted { crate::test_support::paypal_record() }

    fn idless() -> Extracted {
        let mut r = record();
        r.external_id = None;
        r
    }

    /// Assert that mutating one field of an id-less record changes the composite
    /// hash (i.e. that field is discriminating).
    fn assert_field_discriminates(mutate: impl FnOnce(&mut Extracted)) {
        let a = idless();
        let mut b = a.clone();
        mutate(&mut b);
        assert_ne!(composite_hash(&a), composite_hash(&b));
    }

    #[test]
    fn paypal_uses_transaction_id() {
        assert_eq!(external_id(&record()), "8XY12345AB678901C");
    }

    #[test]
    fn falls_back_to_composite_when_no_id() {
        let hash_id = external_id(&idless()); // composite fallback
        assert_eq!(hash_id.len(), 64, "sha256 hex = 64 chars");
        assert!(
            hash_id.chars().all(|c| c.is_ascii_hexdigit()),
            "composite hash must be pure hex: {hash_id}"
        );
    }

    #[test]
    fn composite_is_deterministic() {
        let r = idless();
        assert_eq!(composite_hash(&r), composite_hash(&r));
    }

    #[test]
    fn composite_changes_with_amount() {
        assert_field_discriminates(|r| r.money = money("202.00", "EUR"));
    }

    /// H3: currency is part of the hash material -- same date/amount/merchant in
    /// a different currency must not collide.
    #[test]
    fn composite_changes_with_currency() {
        assert_field_discriminates(|r| r.money = money("149.99", "USD"));
    }

    /// H3: source is part of the hash material -- same fields from a different
    /// source must not collide.
    #[test]
    fn composite_changes_with_source() {
        assert_field_discriminates(|r| r.source = Source::BancoPopular);
    }

    // --- property tests --------------------------------------------------

    use proptest::prelude::*;

    /// Build an id-less record from salient fields, for hashing-invariant props.
    fn idless_with(
        source: Source, // dedup hash source discriminant
        amount: &str, currency: &str, merchant: &str,
        last4: &str,
        status: &str,
    ) -> Extracted {
        let mut r = idless();
        r.source = source;
        r.money = money(amount, currency);
        r.merchant = merchant.to_string();
        r.account_hint = Some(last4.to_string());
        r.status = status.to_string();
        r
    }

    proptest! {
        /// Determinism + format: the composite hash is always 64 lowercase hex
        /// chars and stable across repeated calls on the same record.
        #[test]
        fn prop_composite_is_stable_64_hex(
            amount in "[0-9]{1,6}\\.[0-9]{2}",
            merchant in "[A-Za-z ]{1,20}",
            last4 in "[0-9]{4}",
        ) {
            let r = idless_with(Source::Paypal, &amount, "USD", &merchant, &last4, "approved");
            let h1 = composite_hash(&r);
            let h2 = composite_hash(&r);
            prop_assert_eq!(&h1, &h2);
            prop_assert_eq!(h1.len(), 64);
            prop_assert!(h1.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        }

        /// Currency is discriminating: identical everything-else but a different
        /// currency yields a different hash (H3).
        #[test]
        fn prop_currency_discriminates(
            amount in "[0-9]{1,6}\\.[0-9]{2}",
            merchant in "[A-Za-z ]{1,20}",
        ) {
            let a = idless_with(Source::Paypal, &amount, "USD", &merchant, "1234", "approved");
            let b = idless_with(Source::Paypal, &amount, "EUR", &merchant, "1234", "approved");
            prop_assert_ne!(composite_hash(&a), composite_hash(&b)); // H3: currency discriminates
        }

        /// Status normalization: case and surrounding whitespace do not change
        /// the hash (status is lowercased + trimmed into the material).
        #[test]
        fn prop_status_case_and_whitespace_invariant(
            amount in "[0-9]{1,6}\\.[0-9]{2}",
            merchant in "[A-Za-z ]{1,20}",
        ) {
            let lower = idless_with(Source::Paypal, &amount, "USD", &merchant, "1234", "approved");
            let upper = idless_with(Source::Paypal, &amount, "USD", &merchant, "1234", "  APPROVED  ");
            prop_assert_eq!(composite_hash(&lower), composite_hash(&upper));
        }
    }
}
