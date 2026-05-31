//! Banco Popular monthly statement (*estado de cuenta*) ingestion.
//!
//! Unlike the per-transaction consumo notifications (which arrive as one charge
//! per email and are handled by the sender-routed [`crate::adapters`]), a
//! statement is a **password-protected PDF** carrying the whole billing cycle —
//! often dozens of charges, most of which were never notified. It is the
//! authoritative, complete list for the cycle, so the job here is *reconcile*,
//! not just *book*: confirm charges already present, book the ones the
//! notifications missed, and surface discrepancies for review.
//!
//! The module splits along a hard testability boundary:
//! - [`pdf`] owns the PDF-specific work (decrypt + positioned-text extraction)
//!   and exposes the result as plain [`TextRow`]s — geometry, no PDF types.
//! - [`parse`] turns [`TextRow`]s into a typed [`ParsedStatement`] with no PDF
//!   dependency at all, so the row grammar, section segmentation, year
//!   inference, and sign→direction logic are exercised by unit tests against
//!   synthetic rows.
//!
//! `reconcile` (matching against Firefly journals) lands in a sibling module.
//! Its inputs are shaped to that consumer: a [`StatementTxn`] projects to the
//! canonical [`Extracted`] via [`StatementTxn::to_extracted`], so the existing
//! `dedup` / `validate` / `firefly` stack books statement rows with no parallel
//! implementation.

pub mod parse;
pub mod pdf;
pub mod pipeline;
pub mod reconcile;

use chrono::NaiveDate;
use rust_decimal::Decimal;

use crate::schema::{Currency, Direction, Extracted, Money, Source};

/// One positioned text fragment recovered from the PDF: its device-space
/// coordinates and the decoded string. `y` increases up the page (PDF user
/// space), so rows sort by *descending* `y`.
#[derive(Debug, Clone, PartialEq)]
pub struct Run {
    pub x: f32,
    pub y: f32,
    pub text: String,
}

/// A single cell within a row: an x-position and its text. Cells are ordered
/// left-to-right by `x`.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub x: f32,
    pub text: String,
}

/// A line of the statement: the [`Run`]s sharing (approximately) one `y`,
/// collapsed into x-ordered [`Cell`]s. This is the boundary type between the
/// PDF layer and the pure parser — [`parse`] never sees a PDF type.
#[derive(Debug, Clone, PartialEq)]
pub struct TextRow {
    pub y: f32,
    pub cells: Vec<Cell>,
}

impl TextRow {
    /// The row's cells joined by single spaces — for header/marker detection.
    #[must_use]
    pub fn joined(&self) -> String {
        self.cells
            .iter()
            .map(|c| c.text.trim())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Define a digit-string newtype: trimmed, all ASCII digits, with a length
/// predicate over the bound name `len`. Collapses the otherwise-identical
/// `parse`/`as_str` shells so the four reference/card/MCC/auth types can't drift
/// apart — only the length rule varies, and it is declared inline.
macro_rules! digit_newtype {
    ($(#[$meta:meta])* $name:ident, $len:ident => $pred:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub struct $name(String);

        impl $name {
            /// Parse from untrusted text: trimmed, all ASCII digits, length
            /// satisfying this type's rule. `None` otherwise.
            #[must_use]
            pub fn parse(raw: &str) -> Option<Self> {
                let t = raw.trim();
                let $len = t.len();
                ($pred && t.bytes().all(|b| b.is_ascii_digit())).then(|| $name(t.to_string()))
            }

            /// The validated digit string.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

digit_newtype! {
    /// A statement row reference (`NO. DE REFERENCIA`): all ASCII digits, ≥10
    /// long (payments ~10, purchases ~23). Unique within a statement, so it is
    /// the stable `bpstmt:<ref>` dedup anchor — downstream never re-checks it.
    Reference, len => len >= 10
}

digit_newtype! {
    /// A card last-4 (`****-****-****-NNNN` header, or `terminada en NNNN`):
    /// exactly four ASCII digits.
    Last4, len => len == 4
}

digit_newtype! {
    /// A merchant-category code from a transaction's continuation line: exactly
    /// four ASCII digits.
    Mcc, len => len == 4
}

digit_newtype! {
    /// An authorization code from a transaction's continuation line: non-empty
    /// ASCII digits.
    AuthCode, len => len >= 1
}

/// Which card balance a statement section covers. Maps to the Firefly account:
/// DOP → the DOP liability (107), USD → the USD liability (106). A genuinely
/// closed two-variant domain (the card bills only these two), kept distinct
/// from [`Currency`] so routing is an exhaustive `match`, not a 180-code lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionCurrency {
    Dop,
    Usd,
}

impl SectionCurrency {
    /// The ISO-4217 code the section bills in. Built from a compile-time
    /// constant via [`Currency::from_static`] — no parse, no panic path.
    #[must_use]
    pub fn currency(self) -> Currency {
        match self {
            SectionCurrency::Dop => Currency::from_static("DOP"),
            SectionCurrency::Usd => Currency::from_static("USD"),
        }
    }
}

/// The header + footer facts of one statement section (one card currency).
#[derive(Debug, Clone, PartialEq)]
pub struct Section {
    pub currency: SectionCurrency,
    /// Last-4 of the *primary* card as printed in the section header. NOTE: the
    /// statement never prints a per-row last-4, and additional-cardholder rows
    /// belong to a different card, so this is **not** a reliable reconciliation
    /// key — see the module/plan notes. Kept for provenance + `account_hint`.
    pub primary_last4: Last4,
    /// `FECHA DE CORTE` — the cycle cut date. Anchors year inference for the
    /// year-less `DD/MM` transaction dates.
    pub cut_date: NaiveDate,
    /// `BALANCE ANTERIOR` (header) — the prior-cycle closing balance. With
    /// `balance_total` it gives the internal-consistency check
    /// `anterior + Σcharges − Σpayments ≈ total`. Signed [`Decimal`] (a balance
    /// can be negative), never booked.
    pub balance_anterior: Option<Decimal>,
    /// `BALANCE TOTAL` (footer) — the authoritative closing balance, used by the
    /// Phase-1 closing-balance check. Deliberately a bare (signed) [`Decimal`]
    /// and not a [`Money`]: a statement balance can be negative, which the
    /// non-negative [`crate::schema::Amount`] inside `Money` cannot represent.
    /// It is never booked, only compared.
    pub balance_total: Option<Decimal>,
}

/// One parsed statement transaction (a charge or a payment/credit).
///
/// Richer than [`Extracted`] — it keeps statement-only provenance
/// (`posting_date`, `mcc`, `auth_code`) — but projects to it losslessly for the
/// fields the booking stack needs (see [`StatementTxn::to_extracted`]).
#[derive(Debug, Clone, PartialEq)]
pub struct StatementTxn {
    pub section: SectionCurrency,
    /// `FECHAS DE ENTRADA` — posting date.
    pub posting_date: NaiveDate,
    /// `TRANSAC` — authorization date; the reconciliation anchor (matches the
    /// consumo notification's `Fecha`) and the date a booked journal carries.
    pub auth_date: NaiveDate,
    /// `NO. DE REFERENCIA` — stable per-row reference; the `bpstmt:<ref>` key.
    pub reference: Reference,
    /// `DESCRIPTION [LOCATION]` — the merchant string (statement rendering).
    pub merchant: String,
    /// Billed amount in the section currency, paired with that currency.
    pub money: Money,
    /// Charge (`Out`) or payment/credit (`In`, the `(-)`-signed rows).
    pub direction: Direction,
    /// MCC code from the continuation line, when present.
    pub mcc: Option<Mcc>,
    /// Authorization code from the continuation line, when present.
    pub auth_code: Option<AuthCode>,
}

impl StatementTxn {
    /// Project to the canonical [`Extracted`] the `validate` / `dedup` /
    /// `firefly` stack consumes, so statement rows book through the *same* gate
    /// as consumo notifications rather than a parallel path.
    ///
    /// - `auth_date` becomes `date` (the booking/anchor date).
    /// - `reference` drives a stable `external_id` of `bpstmt:<ref>`, which
    ///   [`crate::dedup::external_id`] returns verbatim — no composite hash.
    /// - `account_hint` carries the section's primary card last-4 (passed in,
    ///   since it lives on the [`Section`], not the row).
    /// - `status` is the constant `"posted"`: every statement row already
    ///   posted, and that token classifies as approved in [`crate::validate`].
    ///
    /// Note: `(-)`-signed payments are `Direction::In`, which the charge
    /// `validate` gate routes to Review — those book via the transfer path, not
    /// this projection.
    #[must_use]
    pub fn to_extracted(&self, primary_last4: &Last4) -> Extracted {
        Extracted {
            source: Source::BancoPopular,
            external_id: Some(format!("bpstmt:{}", self.reference.as_str())),
            money: self.money.clone(),
            direction: self.direction,
            date: self.auth_date,
            merchant: self.merchant.clone(),
            account_hint: Some(primary_last4.as_str().to_string()),
            status: "posted".to_string(),
            raw_ref: self.reference.as_str().to_string(),
        }
    }
}

/// The fully parsed statement: its sections and all transactions.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParsedStatement {
    pub sections: Vec<Section>,
    pub txns: Vec<StatementTxn>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_parse_rules() {
        assert!(Reference::parse("0601324353").is_some()); // 10 digits
        assert!(Reference::parse("24492166114100057344389").is_some());
        assert!(Reference::parse("8398").is_none()); // too short (an MCC)
        assert!(Reference::parse("06013243xx").is_none()); // non-digit
        assert_eq!(
            Reference::parse("  0601324353  ").unwrap().as_str(),
            "0601324353"
        );
    }

    #[test]
    fn last4_and_mcc_are_four_digits() {
        assert_eq!(Last4::parse("7524").unwrap().as_str(), "7524");
        assert!(Last4::parse("752").is_none());
        assert!(Last4::parse("75244").is_none());
        assert!(Mcc::parse("8398").is_some());
        assert!(Mcc::parse("83").is_none());
    }

    #[test]
    fn to_extracted_sets_dedup_anchor_and_posted_status() {
        use crate::schema::{Amount, Money};
        let txn = StatementTxn {
            section: SectionCurrency::Usd,
            posting_date: NaiveDate::from_ymd_opt(2026, 5, 14).unwrap(),
            auth_date: NaiveDate::from_ymd_opt(2026, 4, 17).unwrap(),
            reference: Reference::parse("74987506133002256024229").unwrap(),
            merchant: "7-Eleven B315 Kastrup".to_string(),
            money: Money::new(
                Amount::parse("7.28").unwrap(),
                SectionCurrency::Usd.currency(),
            ),
            direction: Direction::Out,
            mcc: Mcc::parse("5499"),
            auth_code: AuthCode::parse("020509"),
        };
        let e = txn.to_extracted(&Last4::parse("7524").unwrap());
        assert_eq!(
            e.external_id.as_deref(),
            Some("bpstmt:74987506133002256024229")
        );
        assert_eq!(
            e.date,
            NaiveDate::from_ymd_opt(2026, 4, 17).unwrap(),
            "anchor = auth date"
        );
        assert_eq!(e.status, "posted");
        assert_eq!(e.account_hint.as_deref(), Some("7524"));
        assert_eq!(e.source, Source::BancoPopular);
        // The dedup key is the verbatim external_id (no composite hash).
        assert_eq!(
            crate::dedup::external_id(&e),
            "bpstmt:74987506133002256024229"
        );
    }
}
