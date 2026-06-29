//! End-to-end test of the deterministic core against the real PayPal fixture.
//!
//! This exercises everything that does NOT require network I/O:
//!   unwrap (Gmail forward) -> sender detection -> adapter selection ->
//!   adapter postprocess (on the JSON the fixture implies) -> validation gates
//!   -> dedup key.
//!
//! The LLM step is represented by the JSON a correct model would return for the
//! fixture, since `postprocess` consumes JSON, not raw text.

use serde_json::json;

use receipt_ledger::adapters::paypal::fixtures::{
    cross_currency_model_json, fixture_json as fixture_model_json,
};
use receipt_ledger::adapters::test_support::{
    assert_money, postprocess_one, postprocess_with_body_one,
};
use receipt_ledger::adapters;
use receipt_ledger::schema::{Direction, Source};
use receipt_ledger::test_support::{assert_booked, assert_review};
use receipt_ledger::unwrap::unwrap_forward;
use receipt_ledger::validate::validate;

const FIXTURE: &str = include_str!("fixtures/paypal_accell.txt");

#[test]
fn paypal_fixture_books_with_expected_fields() {
    // 1. Unwrap the Gmail forward and recover the original sender.
    let unwrapped = unwrap_forward(FIXTURE).expect("fixture is a Gmail forward");
    assert_eq!(unwrapped.original_sender, "service@paypal.com");
    assert!(unwrapped.body.contains("Transaction ID: 8XY12345AB678901C"));

    // 2. Sender detection selects the PayPal adapter.
    let adapter = adapters::select(&unwrapped.original_sender).expect("paypal adapter selected");
    assert_eq!(adapter.name(), "paypal");

    // The deterministic pre-filter recognises this as a real receipt.
    assert!(adapter.is_transaction(&unwrapped.body));

    // The adapter builds a prompt from the unwrapped body (sanity: it embeds
    // the receipt so the model sees the figures).
    let prompt = adapter.prompt(&unwrapped.body);
    assert!(prompt.contains("8XY12345AB678901C"));
    assert!(prompt.contains("Example Merchant"));

    // 3-4. Postprocess the JSON a correct model returns -> typed record.
    let record = postprocess_one(adapter, &fixture_model_json());

    assert_eq!(record.source, Source::Paypal);
    assert_eq!(record.external_id.as_deref(), Some("8XY12345AB678901C"));
    assert_money(&record, "149.99", "EUR");
    assert_eq!(record.direction, Direction::Out);
    assert!(record.merchant.contains("Example Merchant"));

    // 5. Validation gates -> booked.
    let booked = assert_booked(validate(record));

    // 6. Dedup uses the PayPal Transaction ID verbatim.
    assert_eq!(
        receipt_ledger::dedup::external_id(booked.as_extracted()),
        "8XY12345AB678901C"
    );
}

#[test]
fn declined_paypal_goes_to_review() {
    let unwrapped = unwrap_forward(FIXTURE).unwrap();
    let adapter = adapters::select(&unwrapped.original_sender).unwrap();

    // Same receipt, but the model reports a declined payment.
    let mut model_json = fixture_model_json();
    model_json.as_object_mut().unwrap().insert("status".into(), json!("declined"));

    let record = postprocess_one(adapter, &model_json);
    assert_review(validate(record));
}

/// M1: non-receipt PayPal mail (a shipping update) is a clean skip via the
/// deterministic pre-filter -- never a Review.
#[test]
fn shipping_update_is_not_a_transaction() {
    let adapter = adapters::select("service@paypal.com").unwrap();
    assert!(!adapter.is_transaction("Good news! Your order is on its way."));
}

#[test]
fn non_forward_is_not_unwrapped() {
    assert!(unwrap_forward("a plain message, no forward marker").is_none());
}

// === P1/P2/P3 production-derived behaviors (end-to-end over fixtures) ========

const INSTALLMENT: &str = include_str!("../eval/dataset/17_paypal_payin4_installment_notatx.txt");
const CROSS_CURRENCY: &str = include_str!("../eval/dataset/15_paypal_crosscurrency_usd.txt");
const CARD_PROMO: &str = include_str!("../eval/dataset/18_paypal_card_promo_balance.txt");

/// P2: a Pay-in-4 INSTALLMENT payment ("You made a $X payment for your Pay in 4
/// plan") is a clean skip via the deterministic pre-filter -- no LLM call, never
/// a Review. It pays down a plan whose purchase was already booked.
#[test]
fn payin4_installment_is_not_a_transaction() {
    let unwrapped = unwrap_forward(INSTALLMENT).expect("fixture is a Gmail forward");
    assert_eq!(unwrapped.original_sender, "service@paypal.com");
    let adapter = adapters::select(&unwrapped.original_sender).unwrap();
    assert!(
        !adapter.is_transaction(&unwrapped.body),
        "installment payment must be skipped by the pre-filter"
    );
}

/// Unwrap a fixture forward, select its adapter, and assert the pre-filter
/// recognises the body as a transaction. Shared by P1/P3 tests.
fn unwrap_and_select(
    fixture: &str,
) -> (
    receipt_ledger::unwrap::Unwrapped,
    &'static dyn receipt_ledger::adapters::Adapter,
) {
    let unwrapped = unwrap_forward(fixture).expect("fixture is a Gmail forward");
    let adapter = adapters::select(&unwrapped.original_sender).expect("adapter selected");
    assert!(adapter.is_transaction(&unwrapped.body));
    (unwrapped, adapter)
}

/// P1: on a cross-currency receipt the booked amount is the authoritative
/// `Total amount of this Transaction: $X USD` figure, applied deterministically
/// by `postprocess_with_body` even if the model extracted the EUR merchant total.
#[test]
fn cross_currency_books_the_usd_total() {
    let (unwrapped, adapter) = unwrap_and_select(CROSS_CURRENCY);

    // The model dutifully reads the EUR merchant total (the wrong figure to
    // book); the deterministic body refinement must override it to USD.
    let model_json = cross_currency_model_json("NW-2026-7741");

    let record = postprocess_with_body_one(adapter, &model_json, &unwrapped.body);
    assert_money(&record, "54.50", "USD");

    // And it books (a linked card -> PayPal Balance, USD currency).
    assert_booked(validate(record));
}

/// P3: a cashback-Mastercard PROMO line is marketing noise, never the funding
/// instrument. Following the prompt, the model reads the real VISA payment
/// method; a linked card funds the PayPal Balance, not Credit.
#[test]
fn promo_mastercard_funds_balance_not_credit() {
    use receipt_ledger::firefly::paypal_is_credit_funded;

    let (unwrapped, adapter) = unwrap_and_select(CARD_PROMO);

    // Prompt-faithful model output: account_hint is the real VISA, not the
    // promo Mastercard.
    let model_json = json!({
        "kind": "transaction",
        "external_id": "3QW77410ZX556677A",
        "amount": "12.10",
        "currency": "USD",
        "direction": "out",
        "date": "2026-05-18",
        "merchant": "Lakeshore Coffee Roasters",
        "account_hint": "VISA ending x-7781",
        "status": "approved",
        "raw_ref": "LCR-55012"
    });

    let promo_record = postprocess_with_body_one(adapter, &model_json, &unwrapped.body);
    // E2E: the promo Mastercard line is ignored; VISA funds Balance, not Credit.
    assert!(
        !paypal_is_credit_funded(&promo_record),
        "VISA-funded purchase must route to PayPal Balance"
    );
    assert_money(&promo_record, "12.10", "USD");
}
