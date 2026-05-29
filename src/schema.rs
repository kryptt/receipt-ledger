//! Canonical extraction schema — the typed target the LLM must produce.
//!
//! `Extracted` is what an adapter's `postprocess` returns: a fully typed,
//! still-untrusted record. Nothing here is "booked"; the validation gates in
//! [`crate::validate`] decide that. Parsing the LLM's loose JSON into these
//! types is the first boundary where we reject malformed input.
//!
//! The money-touching invariants are encoded in the types themselves so they
//! cannot drift apart downstream:
//!
//! - [`Currency`] is a 3-letter ISO-4217 code, uppercase, validated at
//!   construction — "currency is a 3-letter code" is a parse-time invariant.
//! - [`Amount`] is a sanitized non-negative [`Decimal`] with a bounded scale —
//!   "amount is a plausible number" is a parse-time invariant (see
//!   [`crate::adapters::parse::parse_amount`], which is the only public way to
//!   mint one from untrusted input).
//! - [`Money`] pairs an [`Amount`] with the [`Currency`] it is denominated in,
//!   so an amount can never be read in the wrong currency.

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
    #[must_use]
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

/// Error minting a [`Currency`] from untrusted input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurrencyError {
    /// Not exactly three ASCII alphabetic characters.
    NotThreeLetters(String),
}

impl std::fmt::Display for CurrencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CurrencyError::NotThreeLetters(s) => {
                write!(f, "currency {s:?} is not a 3-letter ISO-4217 code")
            }
        }
    }
}

impl std::error::Error for CurrencyError {}

/// A validated ISO-4217 alphabetic currency code.
///
/// Invariant: exactly three ASCII letters, stored uppercase. The only way to
/// mint one from untrusted input is [`Currency::parse`], which is the parse
/// boundary that makes "currency is a 3-letter code" uncheckable downstream
/// (it is already true). Whether the *particular* code is one we book in is a
/// separate, looser question answered by [`crate::validate`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Currency(String);

impl Currency {
    /// Parse a currency code: trimmed, must be exactly three ASCII letters,
    /// stored uppercase. Anything else is a [`CurrencyError`].
    pub fn parse(raw: &str) -> Result<Self, CurrencyError> {
        let t = raw.trim();
        if t.len() == 3 && t.chars().all(|c| c.is_ascii_alphabetic()) {
            Ok(Currency(t.to_ascii_uppercase()))
        } else {
            Err(CurrencyError::NotThreeLetters(raw.to_string()))
        }
    }

    /// Mint a currency from a statically-known-valid code, skipping the parse
    /// path. For *compile-time* constants only (e.g. an account's fixed booking
    /// currency) — never for untrusted input, which must go through
    /// [`Currency::parse`]. Debug-asserts the invariant so a typo'd literal
    /// fails fast in tests.
    #[must_use]
    pub(crate) fn from_static(code: &'static str) -> Self {
        debug_assert!(
            code.len() == 3 && code.bytes().all(|b| b.is_ascii_uppercase()),
            "from_static given a non-canonical code: {code:?}"
        );
        Currency(code.to_string())
    }

    /// The uppercase ISO code, e.g. `"EUR"`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Currency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Currency {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Currency {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Currency::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// Error minting an [`Amount`] from untrusted input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmountError {
    /// The source string was not a plain non-negative decimal
    /// (`^[0-9]+(\.[0-9]+)?$`). Scientific notation, underscores, thousands
    /// separators, signs, and currency symbols are all rejected here.
    NotPlainDecimal(String),
    /// The value parsed but carries more fractional digits than a real currency
    /// minor unit could (see [`Amount::MAX_SCALE`]).
    ScaleTooLarge { value: String, scale: u32 },
}

impl std::fmt::Display for AmountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AmountError::NotPlainDecimal(s) => {
                write!(f, "amount {s:?} is not a plain non-negative decimal")
            }
            AmountError::ScaleTooLarge { value, scale } => {
                write!(
                    f,
                    "amount {value:?} has scale {scale} > {}",
                    Amount::MAX_SCALE
                )
            }
        }
    }
}

impl std::error::Error for AmountError {}

/// A sanitized non-negative monetary magnitude.
///
/// Invariants enforced at construction by [`Amount::parse`]:
/// - the *source string* matched `^[0-9]+(\.[0-9]+)?$` — no scientific
///   notation (`1e10`), digit separators (`9_999_999`), thousands commas
///   (`5,130.00`), signs, or symbols ever reach [`Decimal::from_str`];
/// - the scale (fractional digits) is at most [`MAX_SCALE`](Self::MAX_SCALE),
///   so a model that emits absurd precision cannot smuggle in a tiny rounding
///   ghost.
///
/// Plausibility *bounds* (a configurable maximum) are a policy gate applied in
/// [`crate::validate`], not here — this type only guarantees the shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Amount(Decimal);

impl Amount {
    /// Maximum accepted fractional-digit count. Real currencies top out at 3
    /// minor digits (e.g. KWD, BHD); we allow a little slack for pre-rounding
    /// FX conversions while still rejecting model noise like `1.0000000001`.
    pub const MAX_SCALE: u32 = 4;

    /// Parse a *source string* into an `Amount`. Rejects anything that is not a
    /// plain non-negative decimal before it ever touches [`Decimal::from_str`],
    /// then caps the scale. This is the single sanitization gate for amounts.
    pub fn parse(raw: &str) -> Result<Self, AmountError> {
        let t = raw.trim();
        if !is_plain_decimal(t) {
            return Err(AmountError::NotPlainDecimal(raw.to_string()));
        }
        // `is_plain_decimal` guarantees `from_str` cannot fail, but treat a
        // parse error as "not plain" rather than panicking — fail closed.
        let d = Decimal::from_str_exact(t)
            .map_err(|_| AmountError::NotPlainDecimal(raw.to_string()))?;
        let scale = d.scale();
        if scale > Self::MAX_SCALE {
            return Err(AmountError::ScaleTooLarge {
                value: raw.to_string(),
                scale,
            });
        }
        Ok(Amount(d))
    }

    /// The underlying decimal value.
    #[must_use]
    pub fn value(self) -> Decimal {
        self.0
    }

    /// Whether the magnitude is strictly positive (the booking precondition).
    #[must_use]
    pub fn is_positive(self) -> bool {
        self.0 > Decimal::ZERO
    }
}

impl std::fmt::Display for Amount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Whether `s` matches `^[0-9]+(\.[0-9]+)?$`: one or more digits, optionally a
/// single dot followed by one or more digits. No regex dependency — a tiny hand
/// scan. Rejects empty, leading/trailing dots, signs, `e`/`E`, `_`, `,`, and
/// whitespace inside.
fn is_plain_decimal(s: &str) -> bool {
    let mut chars = s.chars();
    // Integer part: at least one digit.
    let mut saw_int_digit = false;
    let mut seen_dot = false;
    let mut saw_frac_digit = false;
    for c in chars.by_ref() {
        match c {
            '0'..='9' => {
                if seen_dot {
                    saw_frac_digit = true;
                } else {
                    saw_int_digit = true;
                }
            }
            '.' if !seen_dot => {
                seen_dot = true;
            }
            _ => return false,
        }
    }
    // Need integer digits; if there is a dot, need fractional digits too.
    saw_int_digit && (!seen_dot || saw_frac_digit)
}

/// A monetary value: a sanitized [`Amount`] paired with the [`Currency`] it is
/// denominated in. Bundling them means an amount can never be booked,
/// converted, or compared against the wrong currency by accident.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Money {
    pub amount: Amount,
    pub currency: Currency,
}

impl Money {
    #[must_use]
    pub fn new(amount: Amount, currency: Currency) -> Self {
        Self { amount, currency }
    }
}

// `Amount` (de)serializes as the bare decimal string, matching the prior
// `amount: Decimal` wire shape so fixtures and the `Extracted` JSON contract
// are unchanged.
impl Serialize for Amount {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.normalize().to_string())
    }
}

impl<'de> Deserialize<'de> for Amount {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // Accept a JSON string or number, both reduced to their textual form
        // and run through the same plain-decimal gate as untrusted input.
        let v = serde_json::Value::deserialize(d)?;
        let raw = match v {
            serde_json::Value::String(s) => s,
            serde_json::Value::Number(n) => n.to_string(),
            other => {
                return Err(serde::de::Error::custom(format!(
                    "amount wrong type: {other}"
                )));
            }
        };
        Amount::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// A single extracted transaction candidate.
///
/// `money` is a parsed [`Money`] (sanitized [`Amount`] + validated
/// [`Currency`]) and `date` a [`NaiveDate`], so a record that reaches Rust code
/// is already well-formed in those fields. Domain *policy* (positive amount
/// within bounds, bookable status, expense direction) is still checked in
/// [`crate::validate`], which alone can promote an `Extracted` to a
/// [`crate::validate::Validated`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extracted {
    pub source: Source,
    /// PayPal Transaction ID; `None` for sources without a stable id.
    #[serde(default)]
    pub external_id: Option<String>,
    /// The transaction value, amount and currency bound together.
    #[serde(flatten)]
    pub money: Money,
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

impl Extracted {
    /// The sanitized amount magnitude.
    #[must_use]
    pub fn amount(&self) -> Amount {
        self.money.amount
    }

    /// The currency the amount is denominated in.
    #[must_use]
    pub fn currency(&self) -> &Currency {
        &self.money.currency
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn currency_parses_three_letters_uppercase() {
        assert_eq!(Currency::parse("eur").unwrap().as_str(), "EUR");
        assert_eq!(Currency::parse(" usd ").unwrap().as_str(), "USD");
    }

    #[test]
    fn currency_rejects_wrong_length_or_non_alpha() {
        assert!(Currency::parse("US").is_err());
        assert!(Currency::parse("USDX").is_err());
        assert!(Currency::parse("US1").is_err());
        assert!(Currency::parse("").is_err());
    }

    #[test]
    fn amount_accepts_plain_decimals() {
        assert_eq!(
            Amount::parse("149.99").unwrap().value().to_string(),
            "149.99"
        );
        assert_eq!(Amount::parse("0").unwrap().value().to_string(), "0");
        assert_eq!(Amount::parse("5130").unwrap().value().to_string(), "5130");
    }

    #[test]
    fn amount_rejects_scientific_underscore_comma_and_signs() {
        assert!(Amount::parse("1e10").is_err());
        assert!(Amount::parse("9_999_999").is_err());
        assert!(Amount::parse("5,130.00").is_err());
        assert!(Amount::parse("-1.00").is_err());
        assert!(Amount::parse("+1.00").is_err());
        assert!(Amount::parse(".5").is_err());
        assert!(Amount::parse("1.").is_err());
        assert!(Amount::parse("1.2.3").is_err());
        assert!(Amount::parse("EUR$1.50").is_err());
    }

    #[test]
    fn amount_caps_scale() {
        assert!(Amount::parse("1.00001").is_err());
        assert!(Amount::parse("1.0001").is_ok());
    }

    #[test]
    fn plain_decimal_scanner_edge_cases() {
        assert!(is_plain_decimal("0"));
        assert!(is_plain_decimal("0.0"));
        assert!(is_plain_decimal("12345"));
        assert!(!is_plain_decimal(""));
        assert!(!is_plain_decimal("."));
        assert!(!is_plain_decimal("1."));
        assert!(!is_plain_decimal(".1"));
        assert!(!is_plain_decimal("1 2"));
    }

    // --- property tests --------------------------------------------------

    use proptest::prelude::*;

    proptest! {
        /// Any plain decimal with up to 2 fractional digits is accepted, and
        /// round-trips to the same numeric value.
        #[test]
        fn prop_plain_decimals_scale_le_2_accepted(int in 0u64..1_000_000, frac in 0u32..100) {
            let s = format!("{int}.{frac:02}");
            let amt = Amount::parse(&s).expect("plain 2-dp decimal must parse");
            prop_assert_eq!(amt.value(), Decimal::from_str(&s).unwrap());
        }

        /// Scientific notation, underscores, and thousands commas are always
        /// rejected, no matter the surrounding digits.
        #[test]
        fn prop_scientific_underscore_comma_rejected(a in 1u32..9999, b in 1u32..9999) {
            let sci_e = format!("{a}e{b}");
            let sci_e_upper = format!("{a}E{b}");
            let underscore = format!("{a}_{b}");
            let comma = format!("{a},{b:03}");
            prop_assert!(Amount::parse(&sci_e).is_err());
            prop_assert!(Amount::parse(&sci_e_upper).is_err());
            prop_assert!(Amount::parse(&underscore).is_err());
            prop_assert!(Amount::parse(&comma).is_err());
        }

        /// A leading sign is never accepted (amounts are unsigned magnitudes).
        #[test]
        fn prop_signed_rejected(n in 1u64..1_000_000) {
            let neg = format!("-{n}");
            let pos = format!("+{n}");
            prop_assert!(Amount::parse(&neg).is_err());
            prop_assert!(Amount::parse(&pos).is_err());
        }
    }
}
