//! Deterministic validation gates — the safety net between the LLM and the
//! ledger.
//!
//! A record is **booked** only if every gate passes; otherwise it becomes a
//! [`Verdict::Review`] carrying a human-readable reason and is *never* booked.
//! The LLM extracts; this module decides. All logic here is pure and unit
//! tested.

use rust_decimal::Decimal;

use crate::schema::Extracted;

/// Outcome of running the gates over an [`Extracted`] record.
///
/// Exhaustive by construction: a caller must handle both arms, so a new gate
/// that introduces a third disposition cannot be silently ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Cleared every gate — safe to submit to Firefly.
    Booked(Extracted),
    /// Failed a gate — route to the review mailbox, do not book.
    Review { reason: String },
}

/// Run all gates. Returns [`Verdict::Booked`] only when the record is fully
/// trustworthy for automatic submission.
pub fn validate(record: Extracted) -> Verdict {
    if !is_approved(&record.status) {
        return review(format!("status not approved: {:?}", record.status));
    }
    if record.amount <= Decimal::ZERO {
        return review(format!("amount not positive: {}", record.amount));
    }
    if !is_known_currency(&record.currency) {
        return review(format!("unknown currency: {:?}", record.currency));
    }
    if record.merchant.trim().is_empty() {
        return review("merchant is empty".to_string());
    }
    // `date` is already a parsed `NaiveDate` (the schema enforces this at
    // deserialization), so there is no separate date-parse gate to run.
    Verdict::Booked(record)
}

fn review(reason: String) -> Verdict {
    Verdict::Review { reason }
}

/// Approve only statuses that *clearly* indicate a completed, successful
/// transaction. Anything ambiguous (pending, processing, on hold) or negative
/// (declined, failed, reversed, cancelled) is rejected — fail closed.
fn is_approved(status: &str) -> bool {
    let s = status.trim().to_ascii_lowercase();

    // Explicit rejects take precedence over fuzzy approves so a string like
    // "payment completed but later declined" never books.
    const REJECT_SUBSTRINGS: &[&str] = &[
        "declin",   // declined / declinada
        "fail",     // failed
        "pending",  // pending
        "process",  // processing / in process
        "hold",     // on hold
        "review",   // under review
        "cancel",   // cancelled / canceled
        "revers",   // reversed
        "refus",    // refused
        "denied",   // denied
        "incomplete",
        "void",
    ];
    if REJECT_SUBSTRINGS.iter().any(|r| s.contains(r)) {
        return false;
    }

    const APPROVE_VALUES: &[&str] = &[
        "approved",
        "completed",
        "complete",
        "success",
        "successful",
        "paid",
        "sent",
        "posted",
        "settled",
        "aprobada", // es: approved
        "aprobado",
        "completada",
    ];
    APPROVE_VALUES.iter().any(|a| s.contains(a))
}

/// Whether `code` is a known ISO-4217 alphabetic currency code. We carry a
/// curated subset covering the currencies this household actually sees plus
/// the major reserves; an unknown code routes to review rather than booking a
/// transaction in a currency Firefly may not recognise.
fn is_known_currency(code: &str) -> bool {
    let c = code.trim().to_ascii_uppercase();
    ISO_4217.contains(&c.as_str())
}

/// Curated ISO-4217 subset. Intentionally not exhaustive of all ~180 codes —
/// extend as new currencies appear in real mail.
const ISO_4217: &[&str] = &[
    "USD", "EUR", "GBP", "JPY", "CHF", "CAD", "AUD", "NZD", "CNY", "HKD", "SGD", "SEK", "NOK",
    "DKK", "PLN", "CZK", "HUF", "MXN", "BRL", "ARS", "CLP", "COP", "PEN", "DOP", "ZAR", "INR",
    "KRW", "TWD", "THB", "MYR", "IDR", "PHP", "TRY", "ILS", "AED", "SAR",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Direction, Source};
    use chrono::NaiveDate;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn base() -> Extracted {
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
    fn approved_record_books() {
        match validate(base()) {
            Verdict::Booked(e) => assert_eq!(e.amount, Decimal::from_str("149.99").unwrap()),
            Verdict::Review { reason } => panic!("expected booked, got review: {reason}"),
        }
    }

    #[test]
    fn declined_routes_to_review() {
        let mut r = base();
        r.status = "Declinada".to_string();
        assert!(matches!(validate(r), Verdict::Review { .. }));
    }

    #[test]
    fn pending_routes_to_review() {
        let mut r = base();
        r.status = "Pending".to_string();
        assert!(matches!(validate(r), Verdict::Review { .. }));
    }

    #[test]
    fn non_positive_amount_routes_to_review() {
        let mut r = base();
        r.amount = Decimal::ZERO;
        assert!(matches!(validate(r), Verdict::Review { .. }));
    }

    #[test]
    fn unknown_currency_routes_to_review() {
        let mut r = base();
        r.currency = "XYZ".to_string();
        assert!(matches!(validate(r), Verdict::Review { .. }));
    }

    #[test]
    fn empty_merchant_routes_to_review() {
        let mut r = base();
        r.merchant = "   ".to_string();
        assert!(matches!(validate(r), Verdict::Review { .. }));
    }
}
