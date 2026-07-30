//! PayPal adapter.
//!
//! PayPal sends a rich English receipt with a stable `Transaction ID`. The
//! adapter asks the LLM to extract the canonical fields as JSON, then parses
//! that JSON into [`Extracted`]. It is deliberately liberal in what JSON shapes
//! it accepts (single object, array, or `{"transactions":[...]}`) and in how
//! amounts arrive (JSON number or string), because small models are not
//! perfectly consistent — but it is strict about producing well-typed output.
//!
//! Not every mail from `service@paypal.com` is a receipt: shipping updates
//! ("your order is on its way"), "Pay in 4 plan" reminders, and surveys also
//! arrive. Those are a clean [`Outcome::NotATransaction`], detected both
//! deterministically ([`PaypalAdapter::is_transaction`]) and via a `kind`
//! discriminant the model fills in — never a date-parse error polluting Review.

use std::borrow::Cow;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde_json::Value;

// PayPal purchase adapter: LLM-based extraction with cross-currency refinement.
use super::parse::{
    extract_common_fields, parse_amount, parse_date_with, postprocess_transactions, string_field,
};
use super::{Adapter, Outcome};
use crate::schema::{Amount, Direction, Extracted, Money, Source};

/// Sender substring that identifies a PayPal notification.
const PAYPAL_SENDER: &str = "service@paypal.com";

/// Deterministic markers that a PayPal mail is an actual payment receipt rather
/// than a shipping/marketing/survey notice. Matched case-insensitively. A real
/// receipt always carries the transaction id label and the "you paid" phrasing.
const RECEIPT_MARKERS: &[&str] = &["transaction id", "you paid"];

/// Phrases that mark a mail as a **Pay-in-4 installment payment** — paying DOWN
/// an existing plan whose purchase was already booked at checkout — rather than
/// a fresh purchase. An installment says "You made a $X payment for your Pay in
/// 4 plan" (no merchant, just a plan + a funding instrument it was charged to).
/// Matched case-insensitively as substrings of the body. A real Pay-in-4
/// *purchase* never says "made a payment for your Pay in 4 plan"; it says
/// "You paid $X to <merchant>" and carries a Transaction ID. See [`P2`].
///
/// [`P2`]: PaypalAdapter::is_installment_payment
const INSTALLMENT_MARKERS: &[&str] = &[
    "payment for your pay in 4 plan",
    "payment for your pay-in-4 plan",
    "pay in 4 payment went through",
    "pay-in-4 payment went through",
];

/// The label PayPal uses for the authoritative USD figure on a cross-currency
/// receipt: `Total amount of this Transaction: $54.50 USD`. When present, this
/// is what the user's funding instrument was actually charged, so it — not the
/// merchant's foreign-currency total — is the booked amount (policy P1).
const USD_TXN_TOTAL_LABEL: &str = "total amount of this transaction";

pub struct PaypalAdapter;

impl Adapter for PaypalAdapter {
    fn name(&self) -> &'static str {
        "paypal"
    }

    fn matches(&self, original_sender: &str) -> bool {
        // PayPal purchase receipts come from service@paypal.com.
        original_sender.contains(PAYPAL_SENDER)
    }

    fn is_transaction(&self, body: &str) -> bool {
        let lower = body.to_ascii_lowercase();
        // P2: a Pay-in-4 installment payment is NOT a transaction — it pays down
        // a plan whose purchase was already booked. Veto it even if it happens
        // to carry a receipt marker (a Transaction ID line), so the installment
        // never spends an LLM call or gets mistaken for a fresh purchase.
        if is_installment_payment(&lower) {
            return false;
        }
        // P2 passed: check for receipt markers (Transaction ID, You paid).
        RECEIPT_MARKERS.iter().any(|m| lower.contains(m))
    }

    fn prompt(&self, email_text: &str) -> String {
        let email_text = trim_paypal_noise(email_text);
        format!(
            r#"You classify and extract a single financial transaction from a PayPal email.
Return ONLY a JSON object (no prose, no markdown fences) with EXACTLY these keys:

{{
  "kind": "transaction" | "other",
  "external_id": string,   // the "Transaction ID" value, else ""
  "amount": string,        // the purchase TOTAL as a decimal string, e.g. "149.99"
  "currency": string,      // ISO-4217 code of the TOTAL, e.g. "EUR" / "USD" / "GBP"
  "direction": "out" | "in",
  "date": string,          // the transaction/refund date as ISO YYYY-MM-DD
  "merchant": string,      // the merchant / payee name
  "account_hint": string,  // the FUNDING METHOD, one of the values listed below
  "status": string,        // one of: "approved", "refunded", "declined", "pending"
  "raw_ref": string        // the Order ID if present, else the Transaction ID
}}

KIND — is this a real, NEW money movement (a purchase or a refund)?
THE ONE TEST: does the email say "You paid $X to <MERCHANT>" (a purchase) or
"<merchant> sent you a refund"? If yes -> "transaction". Otherwise -> "other".
- "transaction": an actual completed PURCHASE ("You paid $X to <merchant>", with
  a "Transaction ID" and a merchant name) or a refund. A Pay-in-4 PURCHASE is
  STILL a transaction even when it shows a "Down payment today" / "3 remaining
  payments" schedule — the schedule is just HOW it is funded; it was still paid
  to a merchant today. Book it (account_hint "Pay in 4").
- "other": NO new money movement to book. Set every other field to "". Use
  "other" ONLY when there is NO "You paid $X to <merchant>" purchase line:
  * Pay-in-4 INSTALLMENT payments — "You made a $X payment for your Pay in 4
    plan" / "Your Pay in 4 payment went through". This pays DOWN an existing
    plan; it names NO merchant and is charged to a funding instrument, NOT paid
    "to" a merchant. (If the mail says "You paid ... to <merchant>", it is NOT
    an installment — it is a purchase.)
  * "Pay in 4 plan created" / "See your new Pay in 4 plan" / payment-schedule
    reminders (upcoming / "due" dates, amounts "due today" — NOT a charge to a
    merchant today).
  * shipping/delivery updates ("your order is on its way", tracking numbers),
    surveys, marketing, security notices.

STATUS — pick exactly one token, in this priority order:
- "refunded"  if this is a refund / money returned to the buyer (words like
              "refund", "refunded", "sent you a refund", "reversed"). A refund
              email often ALSO mentions the original successful payment — ignore
              that; if money is being returned, status is "refunded".
- "declined"  if the payment failed / was declined / cancelled / disputed.
- "pending"   if the payment is pending / processing / on hold.
- "approved"  ONLY for a normal completed outgoing payment with none of the above.

DIRECTION — "out" if the user paid/sent money; "in" if the user RECEIVED money
(a refund is "in").

AMOUNT & CURRENCY:
- CROSS-CURRENCY RULE (P1): if the email shows a merchant total in a FOREIGN
  currency AND a line "Total amount of this Transaction: $54.50 USD", BOOK THE
  USD FIGURE: amount="54.50", currency="USD". That USD line is what the funding
  instrument was actually charged. Only when NO "Total amount of this
  Transaction" USD line is present, fall back to the merchant-currency total.
- Otherwise use the purchase TOTAL (the "Total ..." line, NOT a "Subtotal") and
  its own currency. Ignore any converted funding amount shown only in
  parentheses ("PayPal's conversion rate"). For a refund use the refunded amount.
- "amount" is a positive decimal with a dot separator and NO thousands commas
  and NO currency symbol, e.g. "149.99". "currency" is the 3-letter ISO code of
  that total: € -> EUR, $ -> USD (unless clearly another dollar), £ -> GBP.

ACCOUNT_HINT — classify HOW it was funded (P3). Output EXACTLY one of these:
- "Pay in 4"      if funded by Pay in 4 (lines like "Paid with Pay in 4", or a
                  "Pay in 4" down-payment / installment breakdown on a PURCHASE).
- "Pay Later"     if funded by Pay Later / Pay Monthly.
- "PayPal Credit" if funded by PayPal Credit.
- "Balance"       for ANY other funding — the PayPal balance, a Bank Account, or
                  a linked card (VISA / Mastercard) — OR if NO funding line is
                  stated at all (Balance is the default).
IGNORE PROMOTIONAL NOISE: a line like "Earn 3% cash back with PayPal Cashback
Mastercard®…" (often with an "[image: ...Mastercard...]" line) is MARKETING, not
the funding instrument. Do NOT let that promo flip the account_hint to a card.
Read only the actual "Payment method" / "Paid with" value. A genuine Pay-in-4
PURCHASE is still credit — the Pay-in-4 line wins over any card shown.
When in doubt, use "Balance".

DATE — normalize to ISO YYYY-MM-DD. PayPal writes dates like
"May 11, 2026 10:10:38 AM PDT" -> "2026-05-11"; "March 3, 2026" -> "2026-03-03".

Examples (illustrative):
- "Paid with PayPal balance €58.40 EUR" + "You paid €58.40 EUR to Tulip Press"
  -> {{"kind":"transaction","amount":"58.40","currency":"EUR","direction":"out","account_hint":"Balance","status":"approved", ...}}
- "Paid with Pay in 4" + "You paid $212.00 USD to Northwind Outfitters"
  -> {{"kind":"transaction","amount":"212.00","currency":"USD","direction":"out","account_hint":"Pay in 4","status":"approved", ...}}
- "Maple & Stone Goods sent you a refund of €72.30 EUR"
  -> {{"kind":"transaction","amount":"72.30","currency":"EUR","direction":"in","status":"refunded","account_hint":"", ...}}
- "Your Pay in 4 plan has been created. Payment 2 of 4 €53.00 due May 23"
  -> {{"kind":"other", ...everything else ""}}
- P1 cross-currency: "You paid €44.80 EUR to Shop" + "Total €44.80 EUR" +
  "Total amount of this Transaction: $54.50 USD" + "Payment method: Visa ..."
  -> {{"kind":"transaction","amount":"54.50","currency":"USD","direction":"out","account_hint":"Balance","status":"approved", ...}} (USD line wins; a linked card is Balance)
- P3 promo ignored: "You paid $12.10 USD to Shop" + "Subtotal $12.10" + "VISA" +
  "Earn 3% cash back with PayPal Cashback Mastercard®…"
  -> {{"kind":"transaction","amount":"12.10","currency":"USD","direction":"out","account_hint":"Balance","status":"approved", ...}} (VISA → Balance; ignore the Mastercard promo)
- P2 installment: "Your Pay in 4 payment went through" + "You made a $62.00 USD
  payment for your Pay in 4 plan ... charged to the Bank Account ending in x-0142"
  -> {{"kind":"other", ...everything else ""}} (paying down a plan, NOT a purchase)

Do not invent values; if a field is genuinely absent use "".

PayPal email:
---
{email_text}
---"#
        )
    }

    fn postprocess(&self, json: &Value) -> Result<Outcome> {
        // Honour an explicit non-transaction classification from the model.
        if let Some(reason) = not_a_transaction_reason(json) {
            return Ok(Outcome::NotATransaction { reason });
        }
        postprocess_transactions(json, parse_one)
    }

    /// P1: after the model's extraction, deterministically override the booked
    /// amount/currency with the receipt's `Total amount of this Transaction: $X
    /// USD` line when present — that USD figure is what the funding instrument
    /// was actually charged on a cross-currency purchase. A non-transaction
    /// outcome is passed through untouched.
    fn postprocess_with_body(&self, json: &Value, body: &str) -> Result<Outcome> {
        let outcome = self.postprocess(json)?;
        let Outcome::Transaction(records) = outcome else {
            return Ok(outcome);
        };
        // No authoritative USD line → keep the merchant-currency total (the model
        // already extracted it); downstream FX converts it at booking time.
        let Some(usd) = usd_transaction_total(body) else {
            return Ok(Outcome::Transaction(records));
        };
        let usd_currency = crate::schema::Currency::from_static("USD");
        // Transform-and-return: rebuild each record with the USD money rather
        // than mutating in place.
        let refined = records
            .into_iter()
            .map(|r| Extracted {
                money: Money::new(usd, usd_currency.clone()),
                ..r
            })
            .collect();
        Ok(Outcome::Transaction(refined))
    }
}

/// If the model classified the top-level object as `kind: "other"`, return the
/// skip reason; otherwise `None`. Only a bare object carries a top-level
/// `kind` — array/`transactions` shapes are treated as transactions.
fn not_a_transaction_reason(json: &Value) -> Option<String> {
    let kind = json.as_object()?.get("kind")?.as_str()?.trim();
    if kind.eq_ignore_ascii_case("other") || kind.eq_ignore_ascii_case("not_a_transaction") {
        Some("model classified PayPal mail as non-transaction (kind=other)".to_string())
    } else {
        None
    }
}

/// Parse one JSON object into a typed [`Extracted`].
fn parse_one(obj: &Value) -> Result<Extracted> {
    let c = extract_common_fields(
        obj,
        |map| parse_amount(map.get("amount")).context("parsing `amount`"),
        |map| parse_date(map.get("date")).context("parsing `date`"),
    )?;

    let direction = parse_direction(c.map.get("direction"));
    let external_id = string_field(c.map, "external_id");
    // Fall back to external_id when raw_ref is absent (PayPal-specific policy).
    let raw_ref = if c.raw_ref.is_empty() {
        external_id.clone().unwrap_or_default()
    } else {
        c.raw_ref
    };

    Ok(Extracted {
        source: Source::Paypal,
        external_id,
        money: Money::new(c.amount, c.currency),
        direction,
        date: c.date,
        merchant: c.merchant,
        account_hint: c.account_hint,
        status: c.status,
        raw_ref,
    })
}

/// Default to `out` (a purchase) when the model omits or garbles direction —
/// PayPal receipts are overwhelmingly outgoing payments, and validation will
/// still gate on status.
fn parse_direction(v: Option<&Value>) -> Direction {
    match v
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase())
    {
        Some(s) if s == "in" => Direction::In,
        _ => Direction::Out,
    }
}

/// Accept ISO `YYYY-MM-DD` first, then a couple of human formats PayPal uses
/// ("May 11, 2026") and US `%m/%d/%Y`.
fn parse_date(v: Option<&Value>) -> Result<NaiveDate> {
    const FORMATS: &[&str] = &["%Y-%m-%d", "%B %e, %Y", "%b %e, %Y", "%m/%d/%Y"];
    parse_date_with(v, FORMATS)
}

/// P2: whether a PayPal body is a Pay-in-4 **installment payment** — paying down
/// an existing plan — rather than a fresh purchase. Pure; the caller passes an
/// already-lower-cased body. A purchase ("You paid $X to <merchant>") never
/// contains these phrases, so a `true` here is a confident non-transaction.
#[must_use]
fn is_installment_payment(lower_body: &str) -> bool {
    INSTALLMENT_MARKERS.iter().any(|m| lower_body.contains(m))
}

/// P1: the authoritative USD figure from a cross-currency receipt's
/// `Total amount of this Transaction: $54.50 USD` line, if present. Pure — no
/// I/O — so it is unit tested under `./test.sh`.
///
/// Scans line by line for the [`USD_TXN_TOTAL_LABEL`] prefix (case-insensitive),
/// then reads the first plain decimal that follows on that line through the
/// sanitizing [`Amount::parse`] gate. Returns `None` when the label is absent or
/// the value after it is not a clean decimal (fail closed — the caller then
/// falls back to the merchant-currency total + downstream FX). The currency is
/// always USD by definition of the label, so it is implied rather than returned.
#[must_use]
pub fn usd_transaction_total(body: &str) -> Option<Amount> {
    body.lines().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        let idx = lower.find(USD_TXN_TOTAL_LABEL)?;
        // The remainder of the line after the label holds "...: $54.50 USD".
        let tail = &line[idx + USD_TXN_TOTAL_LABEL.len()..];
        first_decimal(tail).and_then(|d| Amount::parse(&d).ok())
    })
}

/// Strip noise from a Gmail-forwarded PayPal receipt that wastes LLM context
/// without carrying extraction signal: tracking-URL blocks (`<https://...>`,
/// often line-wrapped), inline image alt-text markers (`[image: ...]`), and
/// the marketing footer (everything from `Help & Contact` onward). A real
/// forwarded receipt is ~10 kB of which ~88% is this noise — enough to blow
/// past a small model's 8 K context window and crash the request with a 400
/// before generation even starts.
///
/// Idempotent: when none of the markers are present (the case for the
/// scrubbed test fixtures and for short native bank alerts), the original
/// slice is returned without allocation. Pure: no I/O, deterministic.
///
/// The full original body is still passed to [`postprocess_with_body`], so
/// the P1 cross-currency `Total amount of this Transaction: $X USD` check
/// runs against untrimmed input — trimming only the LLM input cannot change
/// the booked amount.
fn trim_paypal_noise(body: &str) -> Cow<'_, str> {
    let has_url = body.contains("<http://") || body.contains("<https://");
    let has_image = body.contains("[image:");
    let footer_at = body.find("Help & Contact");
    if !has_url && !has_image && footer_at.is_none() {
        return Cow::Borrowed(body);
    }

    // Cut the marketing footer first; everything after `Help & Contact` is
    // social-media links and phishing-awareness boilerplate the LLM does not
    // need to see.
    let core = match footer_at {
        Some(i) => &body[..i],
        None => body,
    };

    let mut out = String::with_capacity(core.len());
    let mut rest = core;
    while !rest.is_empty() {
        let url_at = rest
            .find("<http")
            .filter(|&i| rest[i..].starts_with("<http://") || rest[i..].starts_with("<https://"));
        let image_at = rest.find("[image:");
        let (cut, after_open, close) = match (url_at, image_at) {
            (Some(u), Some(i)) if u <= i => (u, u + 1, '>'),
            (Some(u), None) => (u, u + 1, '>'),
            (_, Some(i)) => (i, i + "[image:".len(), ']'),
            (None, None) => {
                out.push_str(rest);
                break;
            }
        };
        out.push_str(&rest[..cut]);
        match rest[after_open..].find(close) {
            Some(rel) => {
                rest = &rest[after_open + rel + 1..];
            }
            None => {
                // Unterminated marker (defensive — real PayPal mails always
                // close `<…>` and `[image:…]`). Drop the tail rather than
                // emitting half-stripped noise.
                break;
            }
        }
    }
    Cow::Owned(collapse_blank_runs(&out))
}

/// Collapse runs of 3+ consecutive newlines down to 2. Stripping `<URL>` and
/// `[image:…]` blocks routinely leaves behind 4–5 blank lines in a row; the
/// LLM does not care, but it costs tokens.
fn collapse_blank_runs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut newlines = 0usize;
    for c in s.chars() {
        if c == '\n' {
            newlines += 1;
            if newlines <= 2 {
                out.push('\n');
            }
        } else {
            newlines = 0;
            out.push(c);
        }
    }
    out
}

/// Extract the first plain decimal token from `s`, ignoring a leading currency
/// symbol/`$`/`:` and stopping at the first non-`[0-9.]` after digits begin.
/// `": $54.50 USD"` -> `"54.50"`. Returns `None` if no digit is found. Thousands
/// commas are not expected on PayPal's USD line; a comma simply terminates the
/// token (so a stray grouped value fails closed rather than misparsing).
fn first_decimal(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut started = false;
    for c in s.chars() {
        match c {
            '0'..='9' | '.' => {
                out.push(c);
                started = true;
            }
            _ if started => break,
            _ => {}
        }
    }
    if started { Some(out) } else { None }
}

/// PayPal JSON fixtures shared between unit and integration tests.
#[doc(hidden)]
pub mod fixtures {
    use serde_json::{Value, json};

    /// The JSON a correctly-behaving model produces for the PayPal fixture receipt.
    pub fn fixture_json() -> Value {
        json!({
            "kind": "transaction",
            "external_id": "8XY12345AB678901C",
            "amount": "149.99",
            "currency": "EUR",
            "direction": "out",
            "date": "2026-05-11",
            "merchant": "Example Merchant B.V.",
            "account_hint": "Pay in 4",
            "status": "approved",
            "raw_ref": "TESTORDER0123456"
        })
    }

    /// Model JSON for a cross-currency receipt where the EUR merchant total
    /// should be overridden to USD by the deterministic body refinement.
    pub fn cross_currency_model_json(raw_ref: &str) -> Value {
        json!({
            "kind": "transaction",
            "external_id": "7AA11122BB333444C",
            "amount": "44.80",
            "currency": "EUR",
            "direction": "out",
            "date": "2026-05-12",
            "merchant": "Northwind Outfitters",
            "account_hint": "Visa ending x-9981",
            "status": "approved",
            "raw_ref": raw_ref
        })
    }
}

// -- adapters-paypal unit tests --
#[cfg(test)]
mod tests {
    use super::fixtures::{cross_currency_model_json, fixture_json};
    use super::*;
    use crate::adapters::test_support::{
        assert_books_clean, assert_money, assert_not_a_transaction, assert_reviews,
        assert_transaction_count, single_transaction as one,
    };
    use serde_json::json;

    #[test]
    fn matches_paypal_sender() {
        assert!(PaypalAdapter.matches("service@paypal.com"));
        assert!(PaypalAdapter.matches("paypal <service@paypal.com>"));
        assert!(!PaypalAdapter.matches("notificaciones@popularenlinea.com"));
    }

    #[test]
    fn is_transaction_detects_receipt_markers() {
        assert!(PaypalAdapter.is_transaction("... Transaction ID: 8XY ..."));
        assert!(PaypalAdapter.is_transaction("You paid EUR 1.00 to Shop"));
        // Non-receipt mail: shipping, plan reminders, surveys.
        assert!(!PaypalAdapter.is_transaction("Your order is on its way!"));
        assert!(!PaypalAdapter.is_transaction("Your Pay in 4 plan: next payment due soon"));
        assert!(!PaypalAdapter.is_transaction("How did we do? Take our survey."));
    }

    #[test]
    fn kind_other_is_clean_skip_not_review() {
        let v = json!({"kind": "other"});
        assert_not_a_transaction(PaypalAdapter.postprocess(&v).unwrap());
    }

    #[test]
    fn postprocess_then_validate_books_the_fixture() {
        let e = one(PaypalAdapter.postprocess(&fixture_json()).unwrap());

        assert_eq!(e.external_id.as_deref(), Some("8XY12345AB678901C"));
        assert_money(&e, "149.99", "EUR");
        assert_eq!(e.direction, Direction::Out);
        assert_eq!(e.date, NaiveDate::from_ymd_opt(2026, 5, 11).unwrap());
        assert!(e.merchant.contains("Example Merchant"));

        let b = assert_books_clean(e);
        assert_eq!(
            b.as_extracted().external_id.as_deref(),
            Some("8XY12345AB678901C")
        );
    }

    #[test]
    fn accepts_numeric_amount_and_human_date() {
        let v = json!({
            "kind": "transaction",
            "external_id": "X",
            "amount": 12.50,
            "currency": "usd",
            "direction": "out",
            "date": "May 11, 2026",
            "merchant": "Shop",
            "status": "completed",
            "raw_ref": "X"
        });
        let e = one(PaypalAdapter.postprocess(&v).unwrap());
        assert_money(&e, "12.50", "USD");
        assert_eq!(e.date, NaiveDate::from_ymd_opt(2026, 5, 11).unwrap());
    }

    #[test]
    fn declined_status_postprocesses_but_validation_reviews() {
        let mut v = fixture_json();
        v["status"] = json!("Declined");
        let e = one(PaypalAdapter.postprocess(&v).unwrap());
        assert_reviews(e);
    }

    #[test]
    fn missing_amount_is_an_error() {
        let mut v = fixture_json();
        v.as_object_mut().unwrap().remove("amount");
        assert!(PaypalAdapter.postprocess(&v).is_err());
    }

    #[test]
    fn accepts_transactions_wrapper() {
        let v = json!({ "transactions": [fixture_json()] });
        assert_transaction_count(PaypalAdapter.postprocess(&v).unwrap(), 1);
    }

    // --- P2: installment detection / is_transaction prefilter --------------

    #[test]
    fn installment_payment_is_not_a_transaction() {
        // Real production shape: "You made a $X payment for your Pay in 4 plan".
        let body = "Your Pay in 4 payment went through\n\
            You made a $62.00 USD payment for your Pay in 4 plan. The payment \
            was charged to the Bank Account ending in x-0142.\n\
            Payment method\nBank Account";
        assert!(
            !PaypalAdapter.is_transaction(body),
            "an installment payment must not be treated as a transaction"
        );
    }

    #[test]
    fn installment_marker_vetoes_even_with_receipt_markers() {
        // Even if an installment mail also carried a Transaction ID line, the
        // installment veto wins -- it is paying down an already-booked plan.
        let body = "You made a $62.00 USD payment for your Pay in 4 plan.\n\
            Transaction ID: 9ZZ00011AA222333B";
        assert!(!PaypalAdapter.is_transaction(body));
    }

    #[test]
    fn real_payin4_purchase_is_still_a_transaction() {
        // A genuine Pay-in-4 PURCHASE ("You paid $X to <merchant>") is a
        // transaction -- only the installment-payment phrasing is vetoed.
        let body = "You paid $212.00 USD to Northwind Outfitters\n\
            Paid with Pay in 4\nTransaction ID: 6RT90034LM778820B";
        assert!(PaypalAdapter.is_transaction(body));
    }

    #[test]
    fn is_installment_payment_phrases() {
        assert!(is_installment_payment(
            "you made a $62.00 usd payment for your pay in 4 plan"
        ));
        assert!(is_installment_payment("your pay in 4 payment went through"));
        // A purchase does not match.
        assert!(!is_installment_payment("you paid $12.10 usd to a shop"));
    }

    // --- P1: cross-currency USD total --------------------------------------

    /// A cross-currency receipt body carrying the authoritative USD total
    /// line. Shared across P1, trim, and postprocess_with_body tests.
    fn cross_currency_body() -> String {
        "You paid EUR 44.80 EUR to Northwind Outfitters\n\
            Total EUR 44.80 EUR\n\
            Total amount of this Transaction: $54.50 USD\n\
            Payment method: Visa ending x-9981\n\
            Transaction ID: 7AA11122BB333444C"
            .to_string()
    }

    #[test]
    fn usd_transaction_total_extracts_the_usd_figure() {
        assert_eq!(
            usd_transaction_total(&cross_currency_body()),
            Amount::parse("54.50").ok()
        );
    }

    #[test]
    fn usd_transaction_total_absent_when_no_label() {
        let body = "You paid EUR 44.80 EUR to Shop\nTotal EUR 44.80 EUR";
        assert_eq!(usd_transaction_total(body), None);
    }

    #[test]
    fn first_decimal_strips_symbol_and_currency_suffix() {
        assert_eq!(first_decimal(": $54.50 USD").as_deref(), Some("54.50"));
        assert_eq!(first_decimal("  12 USD").as_deref(), Some("12"));
        assert_eq!(first_decimal("no digits here"), None);
    }

    #[test]
    fn postprocess_with_body_overrides_to_usd_on_cross_currency() {
        // P1 unit test: the EUR merchant total gets replaced by the USD figure.
        let xcur_model = cross_currency_model_json("NW-XCUR-01");
        let xcur_body = cross_currency_body();
        let refined = one(PaypalAdapter
            .postprocess_with_body(&xcur_model, &xcur_body)
            .unwrap());
        assert_money(&refined, "54.50", "USD");
        // Everything else is preserved from the model's extraction.
        assert_eq!(refined.merchant, "Northwind Outfitters");
        assert_eq!(refined.external_id.as_deref(), Some("7AA11122BB333444C"));
    }

    #[test]
    fn postprocess_with_body_keeps_merchant_total_when_no_usd_line() {
        // No "Total amount of this Transaction" line -- fall back to the model's
        // merchant-currency total (downstream FX handles conversion).
        let model_eur = fixture_json(); // EUR 149.99
        let body = "You paid EUR 149.99 EUR to Example Merchant B.V.\nTotal EUR 149.99 EUR";
        let kept = one(PaypalAdapter
            .postprocess_with_body(&model_eur, body)
            .unwrap());
        assert_money(&kept, "149.99", "EUR");
    }

    #[test]
    fn postprocess_with_body_passes_through_non_transaction() {
        // Even with a USD-total line present, a non-transaction stays skipped.
        let other = json!({"kind": "other"});
        assert_not_a_transaction(
            PaypalAdapter
                .postprocess_with_body(&other, "Total amount of this Transaction: $1.00 USD")
                .unwrap(),
        );
    }

    // --- LLM-input trimming (8K-ctx guard) ---------------------------------

    #[test]
    fn trim_is_idempotent_on_clean_fixture_body() {
        // The scrubbed dataset fixtures never contain <URL>, [image:...], or
        // a "Help & Contact" footer. The trim must pass them through with no
        // allocation (Borrowed) and no content change.
        let body = format!(
            "{}\nYour payment was sent from buyer@example.com",
            cross_currency_body()
        );
        match trim_paypal_noise(&body) {
            Cow::Borrowed(s) => assert_eq!(s, body.as_str()),
            Cow::Owned(_) => panic!("clean body must be returned borrowed"),
        }
    }

    #[test]
    fn trim_strips_url_blocks_and_image_markers() {
        let body = "You paid $10.65 USD to DigitalOcean\n\
            [image: PayPal]\n\
            View Payment Details\n\
            <https://www.paypal.com/very/long/tracking?\n\
            spanning=multiple&lines=true>\n\
            Transaction ID: 6VJ47975E7611463Y";
        let trimmed = trim_paypal_noise(body);
        assert!(matches!(trimmed, Cow::Owned(_)));
        let t = trimmed.as_ref();
        assert!(!t.contains("[image:"), "[image: marker remained: {t}");
        assert!(!t.contains("paypal.com/very"), "URL block remained: {t}");
        // The meaningful content survives.
        assert!(t.contains("You paid $10.65 USD to DigitalOcean"));
        assert!(t.contains("Transaction ID: 6VJ47975E7611463Y"));
    }

    #[test]
    fn trim_cuts_at_help_and_contact_footer() {
        let body = "You paid $1.00 USD to Shop\nTransaction ID: ABC123\n\
            ------------------------------\n\
            Help & Contact | Security | Apps\n\
            PayPal is committed to preventing fraudulent emails...";
        let trimmed = trim_paypal_noise(body);
        let t = trimmed.as_ref();
        assert!(t.contains("Transaction ID: ABC123"));
        assert!(!t.contains("Help & Contact"));
        assert!(!t.contains("preventing fraudulent emails"));
    }

    #[test]
    fn trim_preserves_p1_cross_currency_signal() {
        // The USD-line check runs on the UNTRIMMED body in the pipeline, so
        // this is belt-and-suspenders, but the trim must still leave it.
        let body = "You paid EUR 44.80 EUR to Shop\n\
            <https://www.paypal.com/track/foo>\n\
            [image: PayPal]\n\
            Total EUR 44.80 EUR\n\
            Total amount of this Transaction: $54.50 USD\n\
            Payment method: Visa";
        let t = trim_paypal_noise(body);
        assert!(t.contains("Total amount of this Transaction: $54.50 USD"));
        assert!(t.contains("Payment method: Visa"));
    }

    #[test]
    fn trim_handles_unterminated_marker_safely() {
        // An unterminated <http://... (no closing '>') is defensive-dropped.
        // The meaningful prefix survives; we just lose the open-ended tail.
        let body = "You paid $1.00 USD to Shop\nTransaction ID: ABC\n\
            <https://truncated.example.com/no/close";
        let t = trim_paypal_noise(body);
        assert!(t.contains("Transaction ID: ABC"));
        assert!(!t.contains("truncated.example.com"));
    }

    #[test]
    fn trim_real_world_paypal_forward_shrinks_dramatically() {
        // A 5x repetition of the noisy-block pattern observed in the failing
        // production emails (DigitalOcean / Drakenrijk). The trimmed body
        // must end up well under the original size while keeping every field
        // the prompt cares about.
        let noisy_block = "View Payment Details\n\
            <https://www.paypal.com/mobile-app/myaccount/activities/\n\
            details/6VJ47975E7611463Y?source=RT001736&pp_web_dl=custom&\n\
            link_ref=view-payment-status-btn-RT001736&v=1&utm_source=unp&\n\
            utm_medium=email&utm_campaign=RT001736>\n\
            [image: A dark blue PayPal Cashback Mastercard card image with\n\
            a 3% cash back icon.]\n";
        let body = format!(
            "You paid $10.65 USD to DigitalOcean\n\
             Transaction ID: 6VJ47975E7611463Y\n\
             Paid DigitalOcean with PayPal Credit $10.65 USD\n\
             {repeat}\
             Your payment was sent from kryptt@gmail.com\n\
             ------------------------------\n\
             Help & Contact | Security | Apps\n",
            repeat = noisy_block.repeat(5),
        );
        let t = trim_paypal_noise(&body);
        assert!(t.len() < body.len() / 2, "expected substantial shrink");
        assert!(t.contains("You paid $10.65 USD to DigitalOcean"));
        assert!(t.contains("Transaction ID: 6VJ47975E7611463Y"));
        assert!(t.contains("PayPal Credit"));
        assert!(!t.contains("Help & Contact"));
        assert!(!t.contains("[image:"));
    }

    // --- P3: promo-Mastercard ignored -- funding from the real method -------

    #[test]
    fn promo_mastercard_does_not_become_the_funding_hint() {
        // The model, following the prompt, reads the real VISA payment method
        // and ignores the cashback-Mastercard promo. A "Balance"-class hint
        // (a linked card) routes to PayPal Balance, never credit.
        use crate::firefly::paypal_is_credit_funded;
        let visa_funded = one(PaypalAdapter
            .postprocess(&json!({
                "kind": "transaction",
                "external_id": "PROMO-TEST-1",
                "amount": "9.95",
                "currency": "USD",
                "direction": "out",
                "date": "2026-05-12",
                "merchant": "Card Shop",
                "account_hint": "VISA ending x-7781",
                "status": "approved",
                "raw_ref": "PROMO-TEST-1"
            }))
            .unwrap());
        assert!(
            !paypal_is_credit_funded(&visa_funded),
            "a linked VISA card funds the PayPal balance, not credit"
        );
    }
}
