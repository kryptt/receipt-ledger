//! End-to-end test of the deterministic core against the Banco Popular
//! fixtures.
//!
//! Mirrors `paypal_pipeline.rs`: exercises everything that does NOT require
//! network I/O — unwrap (auto- or manual-forward) -> sender detection -> adapter
//! selection -> adapter postprocess (on the JSON a correct model would return)
//! -> validation gates -> dedup key.
//!
//! Two delivery shapes are covered:
//!   - an APPROVED consumo delivered via Gmail *auto*-forward (no marker; the
//!     envelope From is the bank), which must book; and
//!   - a DECLINED consumo delivered as a manual "Fwd:" (with marker; the
//!     envelope From is a human), where unwrap must recover the INNER bank
//!     sender and validation must route to Review.

use chrono::NaiveDate;
use serde_json::json;

use receipt_ledger::adapters;
use receipt_ledger::adapters::banco_popular::fixtures::approved_json;
use receipt_ledger::adapters::test_support::{assert_money, postprocess_one};
use receipt_ledger::dedup;
use receipt_ledger::schema::{Direction, Source};
use receipt_ledger::test_support::{assert_booked, assert_review};
use receipt_ledger::unwrap::unwrap_message;
use receipt_ledger::validate::validate;

const AUTOFORWARD: &str = include_str!("fixtures/banco_popular_autoforward.txt");
const MANUAL_FWD: &str = include_str!("fixtures/banco_popular_manual_fwd.txt");

#[test]
fn autoforwarded_consumo_books_with_expected_fields() {
    // 1. Auto-forward: no marker, so unwrap_message falls back to the envelope
    //    From the bank set as the original sender.
    let unwrapped = unwrap_message(Some("<notificaciones@popularenlinea.com>"), AUTOFORWARD)
        .expect("auto-forward resolves via envelope From");
    assert_eq!(
        unwrapped.original_sender,
        "notificaciones@popularenlinea.com"
    );
    // The body is the original message verbatim.
    assert!(
        unwrapped.body.contains("Notificacion de Consumo")
            || unwrapped.body.contains("Notificación de Consumo")
    );
    assert!(unwrapped.body.contains("Example Cafe Amsterdam"));

    // 2. Sender detection selects the Banco Popular adapter.
    let adapter = adapters::select(&unwrapped.original_sender).expect("banco adapter selected");
    assert_eq!(adapter.name(), "banco_popular");
    assert!(adapter.matches(&unwrapped.original_sender));

    // The prompt embeds the consumo so the model sees the figures.
    let prompt = adapter.prompt(&unwrapped.body);
    assert!(prompt.contains("Example Cafe Amsterdam"));

    // 3-4. Postprocess the JSON a correct model returns -> typed record.
    let record = postprocess_one(adapter, &approved_json());

    assert_eq!(record.source, Source::BancoPopular);
    assert_eq!(record.external_id, None);
    assert_money(&record, "1.50", "EUR");
    assert_eq!(record.direction, Direction::Out);
    // DD/MM/YYYY, not US m/d: 27 May 2026.
    assert_eq!(record.date, NaiveDate::from_ymd_opt(2026, 5, 27).unwrap());
    assert_eq!(record.merchant, "Example Cafe Amsterdam");
    assert_eq!(record.account_hint.as_deref(), Some("1234"));

    // 5. Consumo passes the validation gate.
    let validated = assert_booked(validate(record));
    assert_eq!(validated.as_extracted().source, Source::BancoPopular);

    // 6. No transaction id -> dedup falls back to a composite hash (64 hex).
    let id = dedup::external_id(validated.as_extracted());
    assert_eq!(id.len(), 64);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn manual_forward_recovers_inner_bank_sender_and_declined_reviews() {
    // 1. Manual "Fwd:": the marker block carries the real bank sender; the
    //    envelope From is the human forwarder and must be ignored.
    let unwrapped = unwrap_message(Some("Jane Doe <jane@example.com>"), MANUAL_FWD)
        .expect("manual forward is unwrapped via the marker");
    assert_eq!(
        unwrapped.original_sender,
        "notificaciones@popularenlinea.com"
    );

    // 2. Manual-forward still resolves to the banco adapter.
    let banco = adapters::select(&unwrapped.original_sender).expect("banco adapter selected");
    assert_eq!(banco.name(), "banco_popular");

    // 3. Build a declined variant from the shared fixture.
    let mut model_json = approved_json();
    model_json["amount"] = json!("49.08");
    model_json["merchant"] = json!("Example Shop B.V.");
    model_json["status"] = json!("Declinada");

    let record = postprocess_one(banco, &model_json);
    assert_eq!(record.status, "Declinada");

    // 4. A declined consumo never books.
    assert_review(validate(record));
}
