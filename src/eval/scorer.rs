//! Pure scoring for the eval harness.
//!
//! Both the ground-truth label and the model's end-to-end output are reduced to
//! a [`Produced`] value; [`score`] then compares them field by field. Every
//! comparison is value-based, not string-identical, so a model that emits a
//! semantically-correct-but-differently-spelled value (`"1.5"` vs `"1.50"`,
//! `"eur"` vs `"EUR"`) still scores a match — we judge extraction accuracy, not
//! formatting.

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::firefly::paypal_is_credit_funded;
use crate::schema::{Direction, Extracted, Source};
use crate::validate::Status;

/// Whether the example is a real transaction or a non-transaction notification.
///
/// Closed: a third classification would force every match here to be revisited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A real money-moved transaction the pipeline should extract + route.
    Transaction,
    /// Not a transaction (shipping update, plan-created, survey, ...). The
    /// pipeline should classify it as a clean skip, never extract fields.
    NotATransaction,
}

/// The logical Firefly routing target for a transaction, source-and-funding
/// derived and account-id-agnostic (the dataset must not hard-code numeric
/// ids). Mirrors the four-way routing in [`crate::firefly`].
///
/// Closed over every routing destination the pipeline can choose, so a new
/// account class forces this enum — and every match on it — to be revisited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutedAccount {
    /// PayPal balance asset account (the safe default funding).
    PaypalBalance,
    /// PayPal Credit / Pay in 4 / Pay Later / Pay Monthly liability account.
    PaypalCredit,
    /// Banco Popular VISA USD (non-DOP) liability account.
    BancoPopularUsd,
    /// Banco Popular VISA DOP liability account.
    BancoPopularDop,
}

/// The closed status classification the *ledger* acts on. We score on this, not
/// the raw status text, because a record only ever books when its status
/// classifies [`Status::Approved`]; the exact wording is immaterial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusClass {
    Approved,
    Declined,
    Other,
}

impl StatusClass {
    /// Project the validation [`Status`] classifier onto the eval enum.
    #[must_use]
    pub fn from_status(s: Status) -> Self {
        match s {
            Status::Approved => StatusClass::Approved,
            Status::Declined => StatusClass::Declined,
            Status::Other => StatusClass::Other,
        }
    }

    /// Classify a raw status string exactly as the validation gate does.
    #[must_use]
    pub fn classify(raw: &str) -> Self {
        StatusClass::from_status(Status::classify(raw))
    }
}

/// The ground-truth label for one dataset example, as stored in its `.json`.
///
/// `from` is the envelope sender the harness feeds to
/// [`crate::unwrap::unwrap_message`] (needed for auto-forwards that carry no
/// marker). The transaction fields are present only when `kind` is
/// [`Kind::Transaction`]; for a non-transaction they are absent.
#[derive(Debug, Clone, Deserialize)]
pub struct Expected {
    /// Envelope `From:` for `unwrap_message` (auto-forwards have no marker).
    pub from: String,
    /// Transaction vs not.
    pub kind: Kind,
    /// Expected amount as a decimal string (only meaningful for a transaction).
    #[serde(default)]
    pub amount: Option<String>,
    /// Expected ISO-4217 currency code (only for a transaction).
    #[serde(default)]
    pub currency: Option<String>,
    /// Expected direction (only for a transaction).
    #[serde(default)]
    pub direction: Option<Direction>,
    /// Expected ISO date (only for a transaction).
    #[serde(default)]
    pub date: Option<NaiveDate>,
    /// Expected merchant (only for a transaction).
    #[serde(default)]
    pub merchant: Option<String>,
    /// Expected status classification (only for a transaction).
    #[serde(default)]
    pub status: Option<StatusClass>,
    /// Expected routed account (only for a transaction that should book).
    #[serde(default)]
    pub routed_account: Option<RoutedAccount>,
}

/// The flat, comparable projection of either the ground truth or the model's
/// end-to-end output. `score` compares two of these.
///
/// All transaction fields are `Option`: a non-transaction (or a pipeline that
/// failed to extract one) leaves them `None`. Amounts are parsed to [`Decimal`]
/// and currencies upper-cased at projection time, so [`score`]'s comparisons
/// are plain value equality.
#[derive(Debug, Clone, PartialEq)]
pub struct Produced {
    pub kind: Kind,
    pub amount: Option<Decimal>,
    pub currency: Option<String>,
    pub direction: Option<Direction>,
    pub date: Option<NaiveDate>,
    pub merchant: Option<String>,
    pub status: Option<StatusClass>,
    pub routed_account: Option<RoutedAccount>,
}

impl Produced {
    /// Project a ground-truth [`Expected`] into the comparable shape, applying
    /// the same canonicalization the model-output projection uses (amount →
    /// `Decimal`, currency upper-cased, merchant trimmed). A malformed label
    /// (e.g. a non-decimal expected amount) is a dataset bug surfaced as an
    /// `Err` so it cannot silently score everything wrong.
    pub fn from_expected(e: &Expected) -> anyhow::Result<Self> {
        let amount = match &e.amount {
            Some(s) => Some(parse_amount(s)?),
            None => None,
        };
        Ok(Produced {
            kind: e.kind,
            amount,
            currency: e.currency.as_deref().map(canon_currency),
            direction: e.direction,
            date: e.date,
            merchant: e.merchant.as_deref().map(canon_merchant),
            status: e.status,
            routed_account: e.routed_account,
        })
    }

    /// A non-transaction projection: every transaction field `None`.
    #[must_use]
    pub fn not_a_transaction() -> Self {
        Produced {
            kind: Kind::NotATransaction,
            amount: None,
            currency: None,
            direction: None,
            date: None,
            merchant: None,
            status: None,
            routed_account: None,
        }
    }

    /// Project a real, typed [`Extracted`] record (the pipeline's own output,
    /// post-`postprocess`) into the comparable shape. The `status` is reduced to
    /// its closed classification and the `routed_account` is derived from the
    /// *real* routing rules ([`routed_account_of`]), so the eval scores exactly
    /// what the pipeline would do — currency/funding included.
    #[must_use]
    pub fn from_record(record: &Extracted) -> Self {
        Produced {
            kind: Kind::Transaction,
            amount: Some(record.amount().value()),
            currency: Some(canon_currency(record.currency().as_str())),
            direction: Some(record.direction),
            date: Some(record.date),
            merchant: Some(canon_merchant(&record.merchant)),
            status: Some(StatusClass::classify(&record.status)),
            routed_account: Some(routed_account_of(record)),
        }
    }
}

/// Derive the logical [`RoutedAccount`] for a record using the SAME rules the
/// Firefly client uses to pick a source account. Pure and exhaustive over
/// [`Source`], so a new source forces a routing decision here too.
///
/// PayPal: credit-funded → [`RoutedAccount::PaypalCredit`], else
/// [`RoutedAccount::PaypalBalance`]. Banco Popular: DOP →
/// [`RoutedAccount::BancoPopularDop`], else [`RoutedAccount::BancoPopularUsd`].
#[must_use]
pub fn routed_account_of(record: &Extracted) -> RoutedAccount {
    match record.source {
        Source::Paypal => {
            if paypal_is_credit_funded(record) {
                RoutedAccount::PaypalCredit
            } else {
                RoutedAccount::PaypalBalance
            }
        }
        Source::BancoPopular => {
            if record.currency().as_str() == "DOP" {
                RoutedAccount::BancoPopularDop
            } else {
                RoutedAccount::BancoPopularUsd
            }
        }
    }
}

/// Per-field exact-match outcomes for one (expected, produced) pair.
///
/// Each field is scored only when it is *applicable*: transaction fields on a
/// non-transaction example score `None` (not applicable), so they neither help
/// nor hurt the accuracy denominator. `kind` is always applicable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldScores {
    pub kind: FieldScore,
    pub amount: FieldScore,
    pub currency: FieldScore,
    pub direction: FieldScore,
    pub date: FieldScore,
    pub merchant: FieldScore,
    pub status: FieldScore,
    pub routed_account: FieldScore,
}

/// The score of a single field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldScore {
    /// The field applied and the produced value matched the expected.
    Correct,
    /// The field applied and the produced value did not match.
    Wrong,
    /// The field did not apply to this example (e.g. a transaction field on a
    /// non-transaction, or a routed-account on an example with no expected
    /// route). Excluded from the accuracy denominator.
    NotApplicable,
}

impl FieldScores {
    /// Iterate the eight fields as `(name, score)` for tabular aggregation.
    #[must_use]
    pub fn iter(&self) -> [(&'static str, FieldScore); 8] {
        [
            ("kind", self.kind),
            ("amount", self.amount),
            ("currency", self.currency),
            ("direction", self.direction),
            ("date", self.date),
            ("merchant", self.merchant),
            ("status", self.status),
            ("account", self.routed_account),
        ]
    }
}

/// Score the model's `produced` projection against the `expected` projection,
/// field by field. Pure — the whole point — so it is unit tested under
/// `./test.sh`.
///
/// `kind` is always scored. The transaction fields are scored only when the
/// *expected* example is a transaction; on a non-transaction they are
/// [`FieldScore::NotApplicable`] (we only care that the pipeline did not
/// hallucinate a transaction, which the `kind` field already captures).
/// `routed_account` additionally requires an *expected* route to be applicable.
#[must_use]
pub fn score(expected: &Produced, produced: &Produced) -> FieldScores {
    let kind = bool_score(expected.kind == produced.kind);

    // Transaction fields apply only when the ground truth is a transaction.
    if expected.kind != Kind::Transaction {
        return FieldScores {
            kind,
            amount: FieldScore::NotApplicable,
            currency: FieldScore::NotApplicable,
            direction: FieldScore::NotApplicable,
            date: FieldScore::NotApplicable,
            merchant: FieldScore::NotApplicable,
            status: FieldScore::NotApplicable,
            routed_account: FieldScore::NotApplicable,
        };
    }

    FieldScores {
        kind,
        amount: opt_score(&expected.amount, &produced.amount),
        currency: opt_score(&expected.currency, &produced.currency),
        direction: opt_score(&expected.direction, &produced.direction),
        date: opt_score(&expected.date, &produced.date),
        merchant: opt_score(&expected.merchant, &produced.merchant),
        status: opt_score(&expected.status, &produced.status),
        // Routed account is applicable only when the label specifies one.
        routed_account: match &expected.routed_account {
            Some(_) => opt_score(&expected.routed_account, &produced.routed_account),
            None => FieldScore::NotApplicable,
        },
    }
}

/// Score an `Option` field: not applicable when the expected value is absent,
/// otherwise correct iff the produced value equals it (a missing produced value
/// is Wrong).
fn opt_score<T: PartialEq>(expected: &Option<T>, produced: &Option<T>) -> FieldScore {
    match expected {
        None => FieldScore::NotApplicable,
        Some(_) => bool_score(expected == produced),
    }
}

fn bool_score(b: bool) -> FieldScore {
    if b {
        FieldScore::Correct
    } else {
        FieldScore::Wrong
    }
}

/// Canonicalize a currency code for comparison: trim + upper-case. (Validity is
/// the schema's job; here we only normalize for equality.)
fn canon_currency(s: &str) -> String {
    s.trim().to_ascii_uppercase()
}

/// Canonicalize a merchant for comparison: trim + lower-case + collapse internal
/// whitespace runs. Models vary on casing and spacing of the same name; the
/// extraction is "correct" if it names the same merchant.
pub(crate) fn canon_merchant(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Parse an expected/produced amount string into a [`Decimal`] for value
/// comparison (so `"1.50"` and `"1.5"` compare equal). Tolerant of a leading
/// currency symbol/code and thousands commas, since this compares *values*, not
/// the strict schema gate.
pub(crate) fn parse_amount(s: &str) -> anyhow::Result<Decimal> {
    let cleaned: String = s
        .trim()
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    cleaned
        .parse::<Decimal>()
        .map_err(|e| anyhow::anyhow!("amount {s:?} not a decimal: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    /// A fully-correct transaction projection.
    fn tx() -> Produced {
        Produced {
            kind: Kind::Transaction,
            amount: Some(dec("149.99")),
            currency: Some("EUR".to_string()),
            direction: Some(Direction::Out),
            date: Some(NaiveDate::from_ymd_opt(2026, 5, 11).unwrap()),
            merchant: Some(canon_merchant("Example Merchant B.V.")),
            status: Some(StatusClass::Approved),
            routed_account: Some(RoutedAccount::PaypalBalance),
        }
    }

    #[test]
    fn identical_projections_score_all_correct() {
        let s = score(&tx(), &tx());
        for (name, fs) in s.iter() {
            assert_eq!(fs, FieldScore::Correct, "{name} should be Correct");
        }
    }

    #[test]
    fn amount_value_equality_ignores_trailing_zero() {
        // "1.50" vs "1.5" must score Correct: value equality, not string.
        let mut a = tx();
        a.amount = Some(dec("1.50"));
        let mut b = tx();
        b.amount = Some(dec("1.5"));
        assert_eq!(score(&a, &b).amount, FieldScore::Correct);
    }

    #[test]
    fn wrong_currency_scores_wrong_but_others_correct() {
        let expected = tx();
        let mut produced = tx();
        produced.currency = Some("USD".to_string());
        let s = score(&expected, &produced);
        assert_eq!(s.currency, FieldScore::Wrong);
        assert_eq!(s.amount, FieldScore::Correct);
        assert_eq!(s.kind, FieldScore::Correct);
    }

    #[test]
    fn missing_produced_field_is_wrong() {
        let expected = tx();
        let mut produced = tx();
        produced.merchant = None;
        assert_eq!(score(&expected, &produced).merchant, FieldScore::Wrong);
    }

    #[test]
    fn non_transaction_expected_makes_tx_fields_not_applicable() {
        let expected = Produced::not_a_transaction();
        let produced = Produced::not_a_transaction();
        let s = score(&expected, &produced);
        assert_eq!(s.kind, FieldScore::Correct);
        for (name, fs) in s.iter() {
            if name != "kind" {
                assert_eq!(fs, FieldScore::NotApplicable, "{name} should be N/A");
            }
        }
    }

    #[test]
    fn pipeline_hallucinated_a_transaction_fails_kind_only() {
        // Expected: not a transaction. Produced: extracted one anyway.
        let expected = Produced::not_a_transaction();
        let produced = tx();
        let s = score(&expected, &produced);
        assert_eq!(
            s.kind,
            FieldScore::Wrong,
            "kind must catch the hallucination"
        );
        // Transaction fields stay N/A — kind already captures the error.
        assert_eq!(s.amount, FieldScore::NotApplicable);
    }

    #[test]
    fn pipeline_missed_a_transaction_fails_kind() {
        // Expected: a transaction. Produced: classified as non-transaction.
        let expected = tx();
        let produced = Produced::not_a_transaction();
        let s = score(&expected, &produced);
        assert_eq!(s.kind, FieldScore::Wrong);
        // The expected transaction fields are present, produced are None → Wrong.
        assert_eq!(s.amount, FieldScore::Wrong);
        assert_eq!(s.currency, FieldScore::Wrong);
    }

    #[test]
    fn routed_account_not_applicable_when_label_omits_it() {
        let mut expected = tx();
        expected.routed_account = None;
        let produced = tx();
        assert_eq!(
            score(&expected, &produced).routed_account,
            FieldScore::NotApplicable
        );
    }

    #[test]
    fn wrong_routed_account_scores_wrong() {
        let expected = tx();
        let mut produced = tx();
        produced.routed_account = Some(RoutedAccount::PaypalCredit);
        assert_eq!(
            score(&expected, &produced).routed_account,
            FieldScore::Wrong
        );
    }

    #[test]
    fn status_compared_by_classification() {
        // Both classify Approved despite different raw words.
        assert_eq!(StatusClass::classify("Aprobada"), StatusClass::Approved);
        assert_eq!(StatusClass::classify("completed"), StatusClass::Approved);
        assert_eq!(StatusClass::classify("Declinada"), StatusClass::Declined);
        assert_eq!(StatusClass::classify("pending"), StatusClass::Other);
    }

    #[test]
    fn merchant_canon_is_case_and_whitespace_insensitive() {
        assert_eq!(
            canon_merchant("  Example   Merchant B.V. "),
            canon_merchant("example merchant b.v.")
        );
    }

    #[test]
    fn amount_parse_tolerates_symbol_and_commas() {
        assert_eq!(parse_amount("EUR$5,130.00").unwrap(), dec("5130.00"));
        assert_eq!(parse_amount("  149.99 ").unwrap(), dec("149.99"));
    }

    use crate::schema::{Amount, Currency, Money};

    fn paypal_record(hint: Option<&str>, currency: &str) -> Extracted {
        Extracted {
            source: Source::Paypal,
            external_id: Some("X".to_string()),
            money: Money::new(
                Amount::parse("10.00").unwrap(),
                Currency::parse(currency).unwrap(),
            ),
            direction: Direction::Out,
            date: NaiveDate::from_ymd_opt(2026, 5, 11).unwrap(),
            merchant: "Shop".to_string(),
            account_hint: hint.map(str::to_string),
            status: "approved".to_string(),
            raw_ref: "X".to_string(),
        }
    }

    fn banco_record(currency: &str) -> Extracted {
        Extracted {
            source: Source::BancoPopular,
            external_id: None,
            money: Money::new(
                Amount::parse("10.00").unwrap(),
                Currency::parse(currency).unwrap(),
            ),
            direction: Direction::Out,
            date: NaiveDate::from_ymd_opt(2026, 5, 27).unwrap(),
            merchant: "Tienda".to_string(),
            account_hint: Some("4417".to_string()),
            status: "Aprobada".to_string(),
            raw_ref: String::new(),
        }
    }

    #[test]
    fn routed_account_matches_firefly_rules() {
        assert_eq!(
            routed_account_of(&paypal_record(Some("Balance"), "USD")),
            RoutedAccount::PaypalBalance
        );
        assert_eq!(
            routed_account_of(&paypal_record(None, "EUR")),
            RoutedAccount::PaypalBalance
        );
        assert_eq!(
            routed_account_of(&paypal_record(Some("Pay in 4"), "USD")),
            RoutedAccount::PaypalCredit
        );
        assert_eq!(
            routed_account_of(&paypal_record(Some("PayPal Credit"), "EUR")),
            RoutedAccount::PaypalCredit
        );
        for cur in ["USD", "EUR", "JPY", "KRW", "GBP"] {
            assert_eq!(
                routed_account_of(&banco_record(cur)),
                RoutedAccount::BancoPopularUsd,
                "{cur} → USD account"
            );
        }
        assert_eq!(
            routed_account_of(&banco_record("DOP")),
            RoutedAccount::BancoPopularDop
        );
    }

    #[test]
    fn from_record_projects_and_classifies() {
        let p = Produced::from_record(&banco_record("EUR"));
        assert_eq!(p.kind, Kind::Transaction);
        assert_eq!(p.currency.as_deref(), Some("EUR"));
        assert_eq!(p.status, Some(StatusClass::Approved)); // "Aprobada" → Approved
        assert_eq!(p.routed_account, Some(RoutedAccount::BancoPopularUsd));
        assert_eq!(p.merchant.as_deref(), Some("tienda"));
    }

    #[test]
    fn from_record_then_score_against_matching_label_is_all_correct() {
        let rec = paypal_record(Some("Pay in 4"), "USD");
        let produced = Produced::from_record(&rec);
        let expected = Expected {
            from: "x".to_string(),
            kind: Kind::Transaction,
            amount: Some("10.00".to_string()),
            currency: Some("USD".to_string()),
            direction: Some(Direction::Out),
            date: NaiveDate::from_ymd_opt(2026, 5, 11),
            merchant: Some("Shop".to_string()),
            status: Some(StatusClass::Approved),
            routed_account: Some(RoutedAccount::PaypalCredit),
        };
        let expected = Produced::from_expected(&expected).unwrap();
        let s = score(&expected, &produced);
        for (name, fs) in s.iter() {
            assert_eq!(fs, FieldScore::Correct, "{name} should be Correct");
        }
    }

    #[test]
    fn from_expected_canonicalizes() {
        let e = Expected {
            from: "x".to_string(),
            kind: Kind::Transaction,
            amount: Some("1.50".to_string()),
            currency: Some("eur".to_string()),
            direction: Some(Direction::Out),
            date: NaiveDate::from_ymd_opt(2026, 5, 11),
            merchant: Some("  Shop  Name ".to_string()),
            status: Some(StatusClass::Approved),
            routed_account: Some(RoutedAccount::PaypalBalance),
        };
        let p = Produced::from_expected(&e).unwrap();
        assert_eq!(p.amount, Some(dec("1.5")));
        assert_eq!(p.currency.as_deref(), Some("EUR"));
        assert_eq!(p.merchant.as_deref(), Some("shop name"));
    }
}
