//! PayPal Credit *payment-receipt* adapter.
//!
//! Distinct from the PayPal *purchase* adapter ([`super::paypal`], sender
//! `service@paypal.com`, which books withdrawals): this one matches sender
//! `customercare@paypal.com` and ingests the "Receipt for your payment to
//! PayPal Credit" notice — money the user paid FROM a funding bank account INTO
//! their PayPal Credit line. That is a movement between two of the user's own
//! accounts, so it books as a Firefly **transfer**, not a withdrawal.
//!
//! The receipt is a rigid, machine-generated form (fixed labels, fixed field
//! order), so this adapter extracts it **deterministically** — no LLM call.
//! [`PaypalPaymentAdapter::deterministic_extract`] parses the body and returns
//! an [`Outcome::Transfer`]; the LLM `prompt`/`postprocess` are unreachable
//! stubs that satisfy the trait. The destination (the PayPal Credit account)
//! and the source (resolved from the funding last-4) are supplied by the
//! pipeline from config, mirroring how withdrawal routing lives outside the
//! adapter.

use anyhow::{Result, anyhow};
use chrono::NaiveDate;
use serde_json::Value;

// PayPal Credit payment-receipt: deterministic parse, no LLM.
use super::parse::strip_thousands_commas;
use super::{Adapter, DestHint, Outcome, SourceHint, TransferRecord};
use crate::schema::{Amount, Currency, Money};

/// Sender substring that identifies a PayPal Credit payment receipt. Note this
/// is `customercare@`, NOT the purchase adapter's `service@`.
const PAYMENT_SENDER: &str = "customercare@paypal.com";

/// The user-paid phrasing every real receipt carries. A *necessary* marker,
/// but not sufficient on its own — a dispute/quoting mail can echo "you paid"
/// without being a payment confirmation, so [`is_payment_receipt`] also requires
/// a structural marker from [`STRUCTURAL_MARKERS`].
const USER_PAID_MARKER: &str = "you paid";

/// Structural markers a genuine payment *confirmation* always carries but a
/// mail merely quoting "you paid" (a dispute, a customer-care reply) would not:
/// the `PayPal Credit will receive` confirmation line, or the
/// `Receipt for your payment to PayPal Credit` subject phrasing. Matched
/// case-insensitively; presence of ANY one (alongside [`USER_PAID_MARKER`]) is
/// what distinguishes a receipt from a quote. Tightening this to an AND of the
/// user-paid phrase + a structural marker (rather than the old weak OR) stops a
/// non-receipt that merely contains "you paid" from booking a spurious transfer.
const STRUCTURAL_MARKERS: &[&str] = &[
    "paypal credit will receive",
    "receipt for your payment to paypal credit",
];

/// The fixed description booked for the transfer split.
const PAYMENT_DESCRIPTION: &str = "Payment to PayPal Credit";

pub struct PaypalPaymentAdapter;

impl Adapter for PaypalPaymentAdapter {
    fn name(&self) -> &'static str {
        "paypal_payment"
    }

    fn matches(&self, sender: &str) -> bool {
        // PayPal Credit payment receipts: customercare@ (not service@).
        sender.contains(PAYMENT_SENDER)
    }

    fn is_transaction(&self, email_body: &str) -> bool {
        // Deterministic: receipt structure + user-paid marker (not just a quote).
        is_payment_receipt(email_body)
    }

    /// Parse the fixed-format receipt directly, bypassing the LLM. Returns
    /// `Some(Ok(Outcome::Transfer(..)))` on a well-formed receipt,
    /// `Some(Ok(Outcome::NotATransaction { .. }))` when the body does not look
    /// like a payment receipt at all, or `Some(Err(..))` when it *does* look
    /// like one but a required field cannot be parsed (→ Review, never a silent
    /// skip). Always `Some` — this source never falls through to the LLM.
    fn deterministic_extract(&self, body: &str) -> Option<Result<Outcome>> {
        if !self.is_transaction(body) {
            return Some(Ok(Outcome::NotATransaction {
                reason: "customercare@paypal.com mail did not look like a payment receipt"
                    .to_string(),
            }));
        }
        Some(parse_payment(body).map(Outcome::Transfer))
    }

    /// Unreachable in the live pipeline: the deterministic path above always
    /// returns `Some`, so the LLM is never invoked for this source. A short,
    /// honest placeholder satisfies the trait.
    fn prompt(&self, _email_text: &str) -> String {
        "PayPal Credit payment receipts are parsed deterministically; no LLM prompt is used."
            .to_string()
    }

    /// Unreachable in the live pipeline (see [`prompt`](Self::prompt)). Bails so
    /// a hypothetical caller that reached it cannot silently mis-book.
    fn postprocess(&self, _json: &Value) -> Result<Outcome> {
        Err(anyhow!(
            "paypal_payment is a deterministic adapter; use deterministic_extract"
        ))
    }
}

/// Whether `body` has the payment-confirmation *structure*, not just a generic
/// phrase. Requires BOTH the user-paid phrasing ([`USER_PAID_MARKER`]) AND at
/// least one [`STRUCTURAL_MARKERS`] line — so a mail that merely quotes "you
/// paid" (a dispute, a customer-care reply) without the confirmation structure
/// is NotATransaction and never books a spurious transfer. Case-insensitive.
fn is_payment_receipt(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains(USER_PAID_MARKER) && STRUCTURAL_MARKERS.iter().any(|s| lower.contains(s))
}

/// Parse the receipt body into a [`TransferRecord`]. Pure — no I/O — so the
/// fixed-format parsing is unit-testable. Returns `Err` only when the body
/// looked like a receipt (the caller already checked the markers) but a
/// required field is absent or malformed.
fn parse_payment(body: &str) -> Result<TransferRecord> {
    let (amount, currency) = parse_amount_currency(body)?;
    let date = parse_payment_date(body)?;
    let transaction_id = parse_transaction_id(body)?;
    let funding_last4 = parse_funding_last4(body)?;

    // Build the PayPal Credit transfer record from the parsed receipt fields.
    let transfer_money = Money::new(amount, currency);
    Ok(TransferRecord {
        money: transfer_money,
        date,
        description: PAYMENT_DESCRIPTION.to_string(),
        external_id: format!("pp-payment:{transaction_id}"),
        // The funding card's last-4 resolves against the PayPal funding map
        // (RECEIPT_PAYING_ACCOUNT_BY_LAST4), never the SWIFT debtor map.
        source: SourceHint::PayPalFundingLast4(funding_last4),
        // A PayPal Credit payment's destination is the configured PayPal Credit
        // account; the pipeline resolves it (this path carries no BIC).
        dest: DestHint::PayPalCredit,
    })
}

/// Extract the `(amount, currency)` from the `You paid $1,300.00 USD` line: the
/// first decimal after the `$` (thousands commas stripped) and the 3-letter ISO
/// code that follows it. Scans line by line for the first `you paid` line that
/// carries a `$`, so the leading "You paid $1,300.00 USD on May 29, 2026"
/// headline (or the body restatement) both work.
fn parse_amount_currency(body: &str) -> Result<(Amount, Currency)> {
    let line = body
        .lines()
        .find(|l| {
            let lower = l.to_ascii_lowercase();
            lower.contains("you paid") && l.contains('$')
        })
        .ok_or_else(|| anyhow!("no `You paid $...` line found"))?;

    let after_dollar = line
        .split_once('$')
        .map(|(_, rest)| rest)
        .ok_or_else(|| anyhow!("`You paid` line has no `$` amount: {line:?}"))?;

    // The amount token runs to the first space; the currency is the next token.
    let mut tokens = after_dollar.split_whitespace();
    let amount_token = tokens
        .next()
        .ok_or_else(|| anyhow!("no amount after `$` in {line:?}"))?;
    let currency_token = tokens
        .next()
        .ok_or_else(|| anyhow!("no currency after the amount in {line:?}"))?;

    let amount = Amount::parse(&strip_thousands_commas(amount_token))
        .map_err(|e| anyhow!("amount {amount_token:?} rejected: {e}"))?;
    let currency =
        Currency::parse(currency_token).map_err(|e| anyhow!("currency {currency_token:?}: {e}"))?;
    Ok((amount, currency))
}

/// Extract the payment date. Prefer the value on the line *after* a bare `Date`
/// label (the Transaction Details block), then fall back to the `on May 29,
/// 2026` phrase **from the `You paid … on …` headline only** (not anywhere in
/// the body, so a prose "Logged on March 01, 2026 …" line can't mis-date the
/// transfer). Either way the value is the English `Month DD, YYYY` form parsed
/// with chrono `%B %d, %Y`.
fn parse_payment_date(body: &str) -> Result<NaiveDate> {
    if let Some(date) = date_after_label(body).or_else(|| date_after_on_headline(body)) {
        return Ok(date);
    }
    Err(anyhow!(
        "no parseable payment date (`Date` block or `on <Month DD, YYYY>`)"
    ))
}

/// Find the line whose trimmed content matches `label` (case-insensitive) and
/// return a sub-iterator starting at the first line AFTER it. The shared
/// primitive behind `value_after_label` and `parse_funding_last4`.
fn skip_to_label<'a>(lines: &mut impl Iterator<Item = &'a str>, label: &str) -> bool {
    lines.any(|line| line.trim().eq_ignore_ascii_case(label))
}

/// The trimmed text on the line immediately following a line whose trimmed
/// content matches `label` (case-insensitive). The common structure behind
/// `date_after_label` and `parse_transaction_id`. Returns `None` when the
/// label is absent or the following line is blank.
fn value_after_label<'a>(body: &'a str, label: &str) -> Option<&'a str> {
    let mut lines = body.lines();
    if !skip_to_label(&mut lines, label) {
        return None;
    }
    let value = lines.next()?.trim();
    if !value.is_empty() { Some(value) } else { None }
}

/// The value on the line following a line that is exactly `Date` (the
/// Transaction Details label), parsed as `%B %d, %Y`.
fn date_after_label(body: &str) -> Option<NaiveDate> {
    let value = value_after_label(body, "date")?;
    NaiveDate::parse_from_str(value, "%B %d, %Y").ok()
}

/// The date from the `You paid $… on May 29, 2026` headline — the three tokens
/// after a standalone `on`, parsed as `%B %d, %Y`. Anchored to the headline (the
/// first line containing "you paid") rather than the whole body, so a stray
/// prose "on <date>" elsewhere cannot mis-date the transfer (which also sets the
/// FX-rate date). Returns `None` when no headline line carries a parseable
/// `on <Month DD, YYYY>`.
fn date_after_on_headline(body: &str) -> Option<NaiveDate> {
    body.lines()
        .filter(|l| l.to_ascii_lowercase().contains(USER_PAID_MARKER))
        .find_map(date_after_on_in_line)
}

/// The date from an `... on May 29, 2026` phrase within a single `line`, parsed
/// as `%B %d, %Y`. Reads the three tokens following the first standalone `on`.
fn date_after_on_in_line(line: &str) -> Option<NaiveDate> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    tokens.iter().enumerate().find_map(|(i, t)| {
        if !t.eq_ignore_ascii_case("on") {
            return None;
        }
        let phrase = tokens.get(i + 1..i + 4)?.join(" ");
        NaiveDate::parse_from_str(&phrase, "%B %d, %Y").ok()
    })
}

/// Extract the value under the `Transaction ID` label — the line following a
/// line that is exactly `Transaction ID`. Errors when the label is absent or
/// the following value is blank.
fn parse_transaction_id(body: &str) -> Result<String> {
    value_after_label(body, "transaction id")
        .map(str::to_string)
        .ok_or_else(|| anyhow!("no `Transaction ID` value found"))
}

/// Extract the funding last-4 from the value line under the exact `Paid with`
/// label (the funding instrument, e.g. `JPMORGAN CHASE x-0130` → `"0130"`),
/// using the same "value on the line after an exact label" approach as
/// [`parse_transaction_id`]/[`date_after_label`].
///
/// Deliberately NOT a whole-body scan: the previous `body.lines().find_map(..)`
/// took the FIRST `x-NNNN` anywhere, so an unrelated earlier masked token
/// (`fx-…`, `tax-…`, a URL) whose digits happened to be a mapped last-4 would
/// book the transfer from the WRONG source account. Anchoring to the `Paid with`
/// value makes the funding instrument the only source of the last-4. Errors when
/// there is no `Paid with` block or its value carries no `x-NNNN`.
fn parse_funding_last4(body: &str) -> Result<String> {
    let mut lines = body.lines();
    if !skip_to_label(&mut lines, "paid with") {
        return Err(anyhow!("no `Paid with` funding `x-NNNN` last-4 found"));
    }
    // The funding instrument value sits on the following line(s); read forward
    // until a line yields an `x-NNNN`, stopping at a blank line.
    for value in lines {
        if value.trim().is_empty() {
            break;
        }
        if let Some(last4) = four_digits_after_x(value) {
            return Ok(last4);
        }
    }
    Err(anyhow!("no `Paid with` funding `x-NNNN` last-4 found"))
}

/// The 4 digits right after an `x-` masking marker in `line`, if present and
/// exactly four digits long. `"JPMORGAN CHASE x-0130"` → `"0130"`.
///
/// Robust on the marker boundary: the `x-` must start the line or follow a
/// non-alphanumeric byte, so the `x-` inside `fx-`, `tax-`, or a URL token is
/// NOT matched. Case-insensitive on the `x`. Returns `None` when no boundary
/// `x-` is present or the following run is not exactly four digits.
fn four_digits_after_x(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = line[search_from..].to_ascii_lowercase().find("x-") {
        let idx = search_from + rel;
        // The `x` must be at a word boundary: line start, or preceded by a
        // non-alphanumeric byte. Otherwise it is the tail of `fx`, `tax`, etc.
        let boundary = idx == 0 || !bytes[idx - 1].is_ascii_alphanumeric();
        if boundary {
            let tail = &line[idx + "x-".len()..];
            let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
            if digits.len() == 4 {
                return Some(digits);
            }
        }
        search_from = idx + "x-".len();
    }
    None
}

// -- adapters-paypal-payment unit tests --
#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::single_transfer as transfer;
    use crate::test_support::dec;

    /// The exact forwarded-receipt body shape (after the Gmail forward is
    /// unwrapped: the inner body, original sender recovered separately).
    fn sample_body() -> &'static str {
        "You paid $1,300.00 USD on May 29, 2026\n\
         Hello, Rodolfo Hansen\n\
         You paid $1,300.00 USD to PayPal Credit.\n\
         Thanks for making your payment.\n\
         Transaction Details\n\
         Transaction ID\n\
         49R50555FK9130709\n\
         Date\n\
         May 29, 2026\n\
         You paid\n\
         $1,300.00 USD\n\
         PayPal Credit will receive\n\
         $1,300.00 USD\n\
         Paid with\n\
         JPMORGAN CHASE x-0130\n\
         Payment from\n\
         Email\n\
         kryptt@gmail.com"
    }

    #[test]
    fn matches_payment_sender() {
        assert!(PaypalPaymentAdapter.matches("customercare@paypal.com"));
        assert!(PaypalPaymentAdapter.matches("PayPal Credit <customercare@paypal.com>"));
        // The purchase adapter's sender must NOT match this one.
        assert!(!PaypalPaymentAdapter.matches("service@paypal.com"));
    }

    #[test]
    fn deterministic_extract_yields_transfer_from_sample() {
        let outcome = PaypalPaymentAdapter
            .deterministic_extract(sample_body())
            .expect("deterministic adapter always takes over")
            .expect("the sample receipt parses");
        let t = transfer(outcome);
        assert_eq!(t.money.amount.value(), dec("1300.00"));
        assert_eq!(t.money.currency.as_str(), "USD");
        assert_eq!(t.date, NaiveDate::from_ymd_opt(2026, 5, 29).unwrap());
        assert_eq!(t.external_id, "pp-payment:49R50555FK9130709");
        assert_eq!(t.source, SourceHint::PayPalFundingLast4("0130".to_string()));
        assert_eq!(t.description, "Payment to PayPal Credit");
    }

    #[test]
    fn non_payment_body_is_not_a_transaction() {
        let outcome = PaypalPaymentAdapter
            .deterministic_extract("Your security settings were updated.")
            .expect("deterministic adapter always takes over")
            .expect("a clean non-match is Ok(NotATransaction)");
        assert!(matches!(outcome, Outcome::NotATransaction { .. }));
    }

    #[test]
    fn is_transaction_requires_paid_phrase_and_structure() {
        // Fix 2: a real receipt has BOTH the user-paid phrasing AND a structural
        // confirmation marker (`PayPal Credit will receive` here).
        assert!(PaypalPaymentAdapter.is_transaction(sample_body()));
        // The subject-phrase structural marker also suffices alongside "you paid".
        assert!(
            PaypalPaymentAdapter
                .is_transaction("Receipt for your payment to PayPal Credit\nYou paid $1.00 USD.",)
        );
        // "you paid" alone (a dispute/quote echoing the phrase) is NOT a receipt:
        // no `will receive`/`receipt for your payment` structure → no spurious book.
        assert!(
            !PaypalPaymentAdapter
                .is_transaction("Regarding your dispute: you paid $1.00 USD, but we disagree.")
        );
        // The bare structural line without "you paid" is also not a receipt.
        assert!(!PaypalPaymentAdapter.is_transaction("PayPal Credit will receive your statement."));
        assert!(!PaypalPaymentAdapter.is_transaction("Your statement is ready to view."));
    }

    #[test]
    fn date_parses_english_month_day_year() {
        let expected = NaiveDate::from_ymd_opt(2026, 5, 29).unwrap();
        assert_eq!(
            NaiveDate::parse_from_str("May 29, 2026", "%B %d, %Y").unwrap(),
            expected,
        );
        // The adapter's two date paths both land on the same date.
        assert_eq!(parse_payment_date(sample_body()).unwrap(), expected);
    }

    #[test]
    fn funding_last4_reads_digits_after_x() {
        assert_eq!(
            four_digits_after_x("JPMORGAN CHASE x-0130").as_deref(),
            Some("0130")
        );
        assert_eq!(
            four_digits_after_x("Visa x-1234 ending").as_deref(),
            Some("1234")
        );
        assert_eq!(
            four_digits_after_x("x-5678 at line start").as_deref(),
            Some("5678")
        );
        // Not four digits, or no `x-`, → None.
        assert_eq!(four_digits_after_x("x-12"), None);
        assert_eq!(four_digits_after_x("no card here"), None);
    }

    #[test]
    fn four_digits_after_x_ignores_non_boundary_x() {
        // Fix 1: `x-` glued to a preceding alphanumeric (the tail of `fx`, `tax`,
        // a URL) is NOT a masking marker, so its digits are not a funding last-4.
        assert_eq!(four_digits_after_x("rate fx-0130 quoted"), None);
        assert_eq!(four_digits_after_x("tax-4242 line item"), None);
        assert_eq!(four_digits_after_x("https://ex.com/tx-9999/page"), None);
        // A real boundary marker later in a line with an earlier non-boundary one
        // is still found.
        assert_eq!(
            four_digits_after_x("fx-0001 then CHASE x-0130").as_deref(),
            Some("0130"),
        );
    }

    #[test]
    fn funding_last4_anchors_to_paid_with_block() {
        // Fix 1: the funding last-4 comes ONLY from the `Paid with` value, never
        // an unrelated earlier `x-NNNN`. Here a stray `tax-9999` and a bogus
        // `fx-1111` appear before the block but must be ignored.
        let body = "You paid $10.00 USD\n\
            PayPal Credit will receive\n\
            tax-9999 reference fx-1111 here\n\
            Paid with\n\
            JPMORGAN CHASE x-0130";
        assert_eq!(parse_funding_last4(body).unwrap(), "0130");
    }

    #[test]
    fn funding_last4_errors_without_paid_with_block() {
        // No `Paid with` label at all → Err (→ Review), even though an `x-NNNN`
        // exists elsewhere: we never source the funding from an unanchored token.
        let body = "You paid $10.00 USD\n\
            Some Bank x-0130 mentioned in prose";
        assert!(parse_funding_last4(body).is_err());
    }

    #[test]
    fn date_fallback_only_reads_the_you_paid_headline() {
        // Fix 3: with no `Date` block, the fallback reads the `on <date>` ONLY
        // from the headline (the "you paid" line), not a stray prose line.
        let body = "Logged on March 01, 2026 by the system\n\
            You paid $10.00 USD on May 29, 2026\n\
            PayPal Credit will receive\n$10.00 USD";
        assert_eq!(
            parse_payment_date(body).unwrap(),
            NaiveDate::from_ymd_opt(2026, 5, 29).unwrap(),
            "headline date, not the prose `on March 01` line",
        );
    }

    #[test]
    fn date_fallback_ignores_prose_on_date_when_headline_lacks_one() {
        // A stray "on <date>" prose line elsewhere is NOT picked when neither the
        // `Date` block nor a headline `on <date>` is present → Err (→ Review).
        let body = "Logged on March 01, 2026 by the system\n\
            You paid $10.00 USD to PayPal Credit.\n\
            PayPal Credit will receive\n$10.00 USD";
        assert!(
            parse_payment_date(body).is_err(),
            "no Date block and no headline `on <date>` → no date guessed from prose",
        );
    }

    #[test]
    fn amount_strips_thousands_comma_and_reads_currency() {
        // PayPal Credit receipt: `$1,300.00 USD` line parsed with comma stripping.
        let parsed = parse_amount_currency(sample_body()).unwrap();
        assert_eq!(parsed.0.value(), dec("1300.00"));
        assert_eq!(parsed.1.as_str(), "USD");
    }

    #[test]
    fn missing_required_field_is_an_error() {
        // Looks like a receipt (carries the user-paid phrase AND the structural
        // `will receive` confirmation) but has no `Paid with` funding block → a
        // hard parse error (→ Review), not a clean skip.
        let body = "You paid $1,300.00 USD to PayPal Credit.\n\
            PayPal Credit will receive\n$1,300.00 USD\n\
            Transaction ID\n49R50555FK9130709\n\
            Date\nMay 29, 2026";
        let result = PaypalPaymentAdapter
            .deterministic_extract(body)
            .expect("always takes over");
        assert!(result.is_err(), "missing funding last-4 must be an Err");
    }
}
