//! End-to-end test of the deterministic core against the real PayPal fixture.
//!
//! This exercises everything that does NOT require network I/O:
//!   unwrap (Gmail forward) → sender detection → adapter selection →
//!   adapter postprocess (on the JSON the fixture implies) → validation gates
//!   → dedup key.
//!
//! The LLM step is represented by the JSON a correct model would return for the
//! fixture, since `postprocess` consumes JSON, not raw text.

use std::str::FromStr;

use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde_json::json;

use receipt_ledger::adapters;
use receipt_ledger::dedup;
use receipt_ledger::schema::{Direction, Source};
use receipt_ledger::unwrap::unwrap_forward;
use receipt_ledger::validate::{Verdict, validate};

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

    // The adapter builds a prompt from the unwrapped body (sanity: it embeds
    // the receipt so the model sees the figures).
    let prompt = adapter.prompt(&unwrapped.body);
    assert!(prompt.contains("8XY12345AB678901C"));
    assert!(prompt.contains("Example Merchant"));

    // 3. The JSON a correct model returns for this receipt.
    let model_json = json!({
        "external_id": "8XY12345AB678901C",
        "amount": "149.99",
        "currency": "EUR",
        "direction": "out",
        "date": "2026-05-11",
        "merchant": "Example Merchant B.V.",
        "account_hint": "Pay in 4",
        "status": "approved",
        "raw_ref": "TESTORDER0123456"
    });

    // 4. postprocess → typed record.
    let records = adapter
        .postprocess(&model_json)
        .expect("postprocess succeeds");
    assert_eq!(records.len(), 1);
    let record = records.into_iter().next().unwrap();

    assert_eq!(record.source, Source::Paypal);
    assert_eq!(record.external_id.as_deref(), Some("8XY12345AB678901C"));
    assert_eq!(record.amount, Decimal::from_str("149.99").unwrap());
    assert_eq!(record.currency, "EUR");
    assert_eq!(record.direction, Direction::Out);
    assert_eq!(record.date, NaiveDate::from_ymd_opt(2026, 5, 11).unwrap());
    assert!(record.merchant.contains("Example Merchant"));

    // 5. Validation gates → booked.
    let booked = match validate(record) {
        Verdict::Booked(b) => b,
        Verdict::Review { reason } => panic!("expected booked, got review: {reason}"),
    };

    // 6. Dedup uses the PayPal Transaction ID verbatim.
    assert_eq!(dedup::external_id(&booked), "8XY12345AB678901C");
}

#[test]
fn declined_paypal_goes_to_review() {
    let unwrapped = unwrap_forward(FIXTURE).unwrap();
    let adapter = adapters::select(&unwrapped.original_sender).unwrap();

    // Same receipt, but the model reports a declined payment.
    let model_json = json!({
        "external_id": "8XY12345AB678901C",
        "amount": "149.99",
        "currency": "EUR",
        "direction": "out",
        "date": "2026-05-11",
        "merchant": "Example Merchant B.V.",
        "status": "declined",
        "raw_ref": "TESTORDER0123456"
    });

    let record = adapter
        .postprocess(&model_json)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    assert!(matches!(validate(record), Verdict::Review { .. }));
}

#[test]
fn non_forward_is_not_unwrapped() {
    assert!(unwrap_forward("a plain message, no forward marker").is_none());
}
