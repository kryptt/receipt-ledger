//! Deterministic validation gates — the safety net between the LLM and the
//! ledger.
//!
//! A record is **booked** only if every gate passes; otherwise it becomes a
//! [`Verdict::Review`] carrying a human-readable reason and is *never* booked.
//! The LLM extracts; this module decides. All logic here is pure and unit
//! tested.
//!
//! Two type-level guarantees come out of this module:
//!
//! - [`Validated`] is a newtype that ONLY [`validate`] can mint. Downstream code
//!   ([`crate::firefly::submit`]) requires a `&Validated`, so an unvalidated
//!   [`Extracted`] is uncompilable to book — the gate cannot be skipped.
//! - [`Status`] is a *closed* classification (Approved / Declined / Other).
//!   Only [`Status::Approved`] books. There is no substring soup: each status
//!   string is normalized and matched against exact tokens, with an explicit
//!   reject veto.

use chrono::NaiveDate;

use crate::schema::{Direction, Extracted, Money};

/// A record that has cleared every validation gate.
///
/// The wrapped [`Extracted`] is identical to the input; the *type* is the
/// proof. Construction is private to this module, so the only way to obtain a
/// `Validated` is through [`validate`]. [`crate::firefly::submit`] takes a
/// `&Validated`, which makes "book an unvalidated record" a compile error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validated(Extracted);

impl Validated {
    /// Borrow the underlying record for read-only use (dedup, payload shaping).
    #[must_use]
    pub fn as_extracted(&self) -> &Extracted {
        &self.0
    }
}

/// A statement card **payment** that has cleared the transfer gate — booked as a
/// Firefly `transfer` (paying bank account → card liability), not a withdrawal.
///
/// The withdrawal [`validate`] gate deliberately routes `Direction::In` to
/// Review (a refund/deposit notice must not auto-book). Statement payments are a
/// *different, trusted* shape — the bank's own statement says the card was paid —
/// so they get their own gate + token. Like [`Validated`], construction is
/// private to this module; [`crate::firefly::submit_transfer`] requires a
/// `&ValidatedTransfer`, so the gate cannot be skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedTransfer {
    money: Money,
    date: NaiveDate,
    description: String,
    external_id: String,
}

impl ValidatedTransfer {
    /// The transfer amount + currency (same currency on both legs).
    #[must_use]
    pub fn money(&self) -> &Money {
        &self.money
    }
    #[must_use]
    pub fn date(&self) -> NaiveDate {
        self.date
    }
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }
    #[must_use]
    pub fn external_id(&self) -> &str {
        &self.external_id
    }
}

/// Outcome of the transfer gate — mirrors [`Verdict`] so a caller handling both
/// the withdrawal and transfer paths writes symmetric `match` arms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferVerdict {
    /// Cleared the gate — safe to `submit_transfer`.
    Booked(ValidatedTransfer),
    /// Failed a gate — route to review, do not book.
    Review { reason: String },
}

/// Gate a statement payment into a [`ValidatedTransfer`]. Mirrors [`validate`]'s
/// money checks (positive amount, currency we book in) plus a non-empty
/// description and dedup key. Pure.
#[must_use]
pub fn validate_transfer(
    money: Money,
    date: NaiveDate,
    description: String,
    external_id: String,
) -> TransferVerdict {
    let reason = if !money.amount.is_positive() {
        Some(format!("transfer amount not positive: {}", money.amount))
    } else if !is_known_currency(money.currency.as_str()) {
        Some(format!("unknown transfer currency: {}", money.currency))
    } else if description.trim().is_empty() {
        Some("transfer description is empty".to_string())
    } else if external_id.trim().is_empty() {
        Some("transfer external_id is empty".to_string())
    } else {
        None
    };
    match reason {
        Some(reason) => TransferVerdict::Review { reason },
        None => TransferVerdict::Booked(ValidatedTransfer { money, date, description, external_id }),
    }
}

/// Outcome of running the gates over an [`Extracted`] record.
///
/// Exhaustive by construction: a caller must handle both arms, so a new gate
/// that introduces a third disposition cannot be silently ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Cleared every gate — safe to submit to Firefly.
    Booked(Validated),
    /// Failed a gate — route to the review mailbox, do not book.
    Review { reason: String },
}

/// Run all sync gates. Returns [`Verdict::Booked`] only when the record is
/// fully trustworthy for automatic submission *on the deterministic axes*
/// (status / direction / positive amount / known currency / non-empty
/// merchant). The FX-dependent USD-equivalent ceiling (`RECEIPT_MAX_AMOUNT`) is
/// applied separately in [`crate::process_message`] after this gate, because it
/// needs a live conversion rate and must therefore stay out of this pure path.
pub fn validate(record: Extracted) -> Verdict {
    // Status must classify as Approved — never Declined, never the ambiguous
    // Other bucket (pending/processing/refund/reversal/etc.). Fail closed.
    match Status::classify(&record.status) {
        Status::Approved => {}
        Status::Declined => {
            return review(format!("status declined/rejected: {:?}", record.status));
        }
        Status::Other => {
            return review(format!("status not clearly approved: {:?}", record.status));
        }
    }

    // Direction policy: this service ingests expense/charge notifications only.
    // A deposit/refund (`In`) must get human eyes — never auto-book money
    // arriving, which would otherwise inflate balances from a refund or reversal
    // notice that slipped past the status gate.
    if record.direction == Direction::In {
        return review("incoming (deposit/refund) transaction routed to review".to_string());
    }

    // Amount must be strictly positive. The schema's `Amount::parse` already
    // rejected absurd hallucinations (scientific notation, huge scale), so this
    // sync gate carries no *magnitude* ceiling: a meaningful ceiling is a USD
    // threshold, which is inherently FX-dependent (₩100,000 ≈ $72 must NOT flag
    // while $100,001 must). That conversion needs a live rate, so it lives in
    // the async pipeline (`crate::process_message`), AFTER this gate mints a
    // `Validated`. See `RECEIPT_MAX_AMOUNT` (documented as a USD ceiling).
    let amount = record.amount();
    if !amount.is_positive() {
        return review(format!("amount not positive: {amount}"));
    }

    // Currency is already a parsed `Currency` (3-letter ISO at construction); we
    // still gate on it being one we actually book in.
    if !is_known_currency(record.currency().as_str()) {
        return review(format!("unknown currency: {}", record.currency()));
    }

    if record.merchant.trim().is_empty() {
        return review("merchant is empty".to_string());
    }

    // `date` is already a parsed `NaiveDate` (the schema enforces this at
    // deserialization), so there is no separate date-parse gate to run.
    Verdict::Booked(Validated(record))
}

fn review(reason: String) -> Verdict {
    Verdict::Review { reason }
}

/// Closed classification of a raw status string.
///
/// Replaces fuzzy substring matching: each status is normalized (trimmed,
/// lower-cased, non-alphanumerics collapsed to single spaces) and matched
/// against *exact* tokens. Only [`Status::Approved`] books; everything else —
/// declines, refunds, reversals, pending/processing, holds, disputes — is
/// [`Status::Declined`] (a hard negative) or [`Status::Other`] (ambiguous /
/// unknown), and neither books.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// A completed, successful charge — the only bookable status.
    Approved,
    /// A clear negative: declined, refused, reversed, refunded, chargeback,
    /// dispute, void, cancelled, etc. Never books.
    Declined,
    /// Anything else — pending, processing, on hold, expired, or simply
    /// unrecognized. Fail closed: never books.
    Other,
}

impl Status {
    /// Classify a raw status string. Reject tokens veto approve tokens, so a
    /// string carrying both ("successfully refunded", "approved then reversed")
    /// classifies as [`Status::Declined`]. In-flight states
    /// (pending/processing/hold/...) also veto approve tokens but classify as
    /// the softer [`Status::Other`] (not a hard negative event — just not final).
    /// All three precede the approve check so an ambiguous status never books.
    #[must_use]
    pub fn classify(raw: &str) -> Status {
        let norm = normalize(raw);
        let tokens: Vec<&str> = norm.split(' ').filter(|t| !t.is_empty()).collect();

        // Hard-negative veto: any reject token makes the whole status Declined,
        // regardless of approve tokens.
        if tokens.iter().any(|t| REJECT_TOKENS.contains(t)) {
            return Status::Declined;
        }
        // In-flight veto: a not-yet-final state is ambiguous → Other, and still
        // takes precedence over any approve token in the same string.
        if tokens.iter().any(|t| IN_FLIGHT_TOKENS.contains(t)) {
            return Status::Other;
        }
        if tokens.iter().any(|t| APPROVE_TOKENS.contains(t)) {
            return Status::Approved;
        }
        Status::Other
    }
}

/// Normalize a status: trim, lower-case, and replace every run of
/// non-alphanumeric characters with a single ASCII space. So `"Pre-paid"`,
/// `"PRE PAID"`, and `"pre_paid"` all normalize to the same token stream.
fn normalize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_space = true; // suppress leading spaces
    for c in raw.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_space = false;
        } else if !prev_space {
            out.push(' ');
            prev_space = true;
        }
    }
    out.trim_end().to_string()
}

/// Exact tokens that mean "completed successful charge". Matched as whole
/// normalized tokens, never substrings — so `"prepaid"` (a single token) does
/// NOT match `"paid"`, and `"unpaid"` does NOT match `"paid"`.
const APPROVE_TOKENS: &[&str] = &[
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
    "completado",
];

/// Exact tokens that hard-veto a booking — declines, reversals, refunds,
/// disputes, holds, pending/processing states, and their es/en variants. Any
/// one present classifies the status as [`Status::Declined`].
const REJECT_TOKENS: &[&str] = &[
    // declines / failures / refusals
    "declined",
    "declinada",
    "declinado",
    "failed",
    "failure",
    "refused",
    "denied",
    "rechazada",
    "rechazado",
    "incomplete",
    "void",
    "voided",
    "cancelled",
    "canceled",
    "cancelada",
    "cancelado",
    // refunds / reversals / chargebacks / disputes
    "refund",
    "refunded",
    "reembolsado",
    "reembolso",
    "reversal",
    "reversed",
    "contracargo",
    "chargeback",
    "disputa",
    "dispute",
    "disputed",
    "devuelto",
    "unpaid",
];

/// Exact tokens for in-flight / not-final states. These are not hard negatives
/// (no money was lost), but a transaction in one of these states is not yet a
/// completed charge, so it classifies as [`Status::Other`] and never books.
const IN_FLIGHT_TOKENS: &[&str] = &[
    "pending",
    "pendiente",
    "processing",
    "procesando",
    "hold",
    "review",
    "expired",
    "expirada",
];

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
    use crate::schema::{Amount, Currency, Direction, Money, Source};
    use chrono::NaiveDate;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn base() -> Extracted {
        Extracted {
            source: Source::Paypal,
            external_id: Some("8XY12345AB678901C".to_string()),
            money: Money::new(
                Amount::parse("149.99").unwrap(),
                Currency::parse("EUR").unwrap(),
            ),
            direction: Direction::Out,
            date: NaiveDate::from_ymd_opt(2026, 5, 11).unwrap(),
            merchant: "Example Merchant B.V.".to_string(),
            account_hint: None,
            status: "approved".to_string(),
            raw_ref: "TESTORDER0123456".to_string(),
        }
    }

    fn with_status(s: &str) -> Extracted {
        let mut r = base();
        r.status = s.to_string();
        r
    }

    fn is_review(r: Extracted) -> bool {
        matches!(validate(r), Verdict::Review { .. })
    }

    fn is_booked(r: Extracted) -> bool {
        matches!(validate(r), Verdict::Booked(_))
    }

    #[test]
    fn approved_record_books() {
        match validate(base()) {
            Verdict::Booked(v) => {
                assert_eq!(
                    v.as_extracted().amount().value(),
                    Decimal::from_str("149.99").unwrap()
                )
            }
            Verdict::Review { reason } => panic!("expected booked, got review: {reason}"),
        }
    }

    // --- C1 / L4: closed status classification ---------------------------

    #[test]
    fn approve_tokens_classify_approved() {
        for s in [
            "approved",
            "Approved",
            "completed",
            "complete",
            "success",
            "successful",
            "paid",
            "sent",
            "posted",
            "settled",
            "Aprobada",
            "aprobado",
            "completada",
        ] {
            assert_eq!(
                Status::classify(s),
                Status::Approved,
                "{s:?} should approve"
            );
        }
    }

    #[test]
    fn reject_tokens_classify_declined() {
        for s in [
            "declined",
            "Declinada",
            "failed",
            "reversed",
            "refunded",
            "reembolsado",
            "contracargo",
            "disputa",
            "devuelto",
            "chargeback",
            "unpaid",
            "cancelled",
            "void",
        ] {
            assert_eq!(
                Status::classify(s),
                Status::Declined,
                "{s:?} should decline"
            );
        }
    }

    #[test]
    fn ambiguous_states_classify_other() {
        for s in [
            "pending",
            "processing",
            "on hold",
            "expired",
            "weird-new-status",
            "",
        ] {
            assert_eq!(Status::classify(s), Status::Other, "{s:?} should be Other");
        }
    }

    /// The must-Review battery from the audit: each MUST NOT book.
    #[test]
    fn must_review_statuses_do_not_book() {
        for s in ["successfully refunded", "reembolsado", "unpaid", "prepaid"] {
            assert!(
                is_review(with_status(s)),
                "status {s:?} must route to review"
            );
        }
    }

    #[test]
    fn prepaid_is_not_paid() {
        // "prepaid" is a single token, NOT the approve token "paid" — Other.
        assert_eq!(Status::classify("prepaid"), Status::Other);
        // "successfully refunded" carries both success + refund: refund vetoes.
        assert_eq!(Status::classify("successfully refunded"), Status::Declined);
    }

    #[test]
    fn declined_routes_to_review() {
        assert!(is_review(with_status("Declinada")));
    }

    #[test]
    fn pending_routes_to_review() {
        assert!(is_review(with_status("Pending")));
    }

    // --- C2: amount positivity -------------------------------------------
    //
    // The *magnitude* ceiling moved to the async pipeline as a USD-equivalent
    // gate (it needs a live FX rate); see `crate::usd_ceiling` for its pure
    // unit tests. Here we only gate positivity, which is FX-independent.

    #[test]
    fn non_positive_amount_routes_to_review() {
        let mut r = base();
        r.money = Money::new(Amount::parse("0").unwrap(), Currency::parse("EUR").unwrap());
        assert!(is_review(r));
    }

    // --- M5: direction policy --------------------------------------------

    #[test]
    fn incoming_direction_routes_to_review() {
        let mut r = base();
        r.direction = Direction::In;
        assert!(is_review(r));
    }

    #[test]
    fn outgoing_direction_books() {
        assert!(is_booked(base()));
    }

    // --- currency / merchant ---------------------------------------------

    #[test]
    fn unknown_currency_routes_to_review() {
        let mut r = base();
        // XYZ parses as a Currency (3 letters) but is not in our booked set.
        r.money = Money::new(
            Amount::parse("1.00").unwrap(),
            Currency::parse("XYZ").unwrap(),
        );
        assert!(is_review(r));
    }

    #[test]
    fn empty_merchant_routes_to_review() {
        let mut r = base();
        r.merchant = "   ".to_string();
        assert!(is_review(r));
    }

    // --- transfer gate (statement payments) ------------------------------

    fn money(amount: &str, currency: &str) -> Money {
        Money::new(Amount::parse(amount).unwrap(), Currency::parse(currency).unwrap())
    }
    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 4, 28).unwrap()
    }

    fn is_transfer_review(money: Money, desc: &str, ext: &str) -> bool {
        matches!(
            validate_transfer(money, day(), desc.into(), ext.into()),
            TransferVerdict::Review { .. }
        )
    }

    #[test]
    fn valid_payment_mints_transfer() {
        match validate_transfer(money("60999.81", "DOP"), day(), "Pago Via App".into(), "bpstmt:1".into()) {
            TransferVerdict::Booked(t) => {
                assert_eq!(t.money().currency.as_str(), "DOP");
                assert_eq!(t.external_id(), "bpstmt:1");
                assert_eq!(t.date(), day());
            }
            TransferVerdict::Review { reason } => panic!("expected booked, got review: {reason}"),
        }
    }

    #[test]
    fn transfer_gate_rejects_bad_inputs() {
        assert!(is_transfer_review(money("0", "USD"), "x", "id"));
        assert!(is_transfer_review(money("1.00", "XYZ"), "x", "id"));
        assert!(is_transfer_review(money("1.00", "USD"), "  ", "id"));
        assert!(is_transfer_review(money("1.00", "USD"), "x", ""));
    }
}
