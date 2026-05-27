//! Canonical extraction schema — the typed target the LLM must produce.
//!
//! `Extracted` is what an adapter's `postprocess` returns: a fully typed,
//! still-untrusted record. Nothing here is "booked"; the validation gates in
//! [`crate::validate`] decide that. Parsing the LLM's loose JSON into these
//! types is the first boundary where we reject malformed input.

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Which upstream notification source a record came from.
///
/// Closed set — adding a variant forces every `match` to be revisited, which
/// is exactly what we want when a new adapter lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Paypal,
    BancoPopular,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Paypal => "paypal",
            Source::BancoPopular => "banco_popular",
        }
    }
}

/// Money flow relative to the account owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Money leaving the account (a purchase / withdrawal).
    Out,
    /// Money arriving (a refund / deposit).
    In,
}

/// A single extracted transaction candidate.
///
/// `amount` is parsed straight into a [`Decimal`] and `date` into a
/// [`NaiveDate`] at deserialization, so a record that reaches Rust code is
/// already well-formed in those fields. Domain validity (positive amount,
/// known currency, approved status) is still checked downstream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extracted {
    pub source: Source,
    /// PayPal Transaction ID; `None` for sources without a stable id.
    #[serde(default)]
    pub external_id: Option<String>,
    pub amount: Decimal,
    /// ISO-4217 currency code as produced by the LLM (validated downstream).
    pub currency: String,
    pub direction: Direction,
    pub date: NaiveDate,
    pub merchant: String,
    /// Card last-4 / funding-source hint, when present.
    #[serde(default)]
    pub account_hint: Option<String>,
    /// Raw status text exactly as the source rendered it.
    pub status: String,
    /// Order id / reference for human traceability back to the email.
    pub raw_ref: String,
}
