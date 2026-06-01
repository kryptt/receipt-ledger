//! Field-name contract test for the structured observability events.
//!
//! Cluster-side LogQL recording/alert rules (`docs/observability/README.md`) key
//! on the **field names** of the JSON events this crate emits. A field rename at
//! an emission site would silently break those rules — the Rust compiler can't
//! see Loki. This test closes that gap: it installs a `tracing` capture layer,
//! drives each `log_*` emit-helper with sample data, and asserts the captured
//! field-name set for each event **equals** the canonical set declared in
//! [`receipt_ledger::obs_fields`]. Rename a field at an emission site and one of
//! these assertions fails in CI.
//!
//! The emit-helpers are pure refactors of the inline emission sites (same field
//! identifiers, same values, same messages), so pinning the helpers pins the
//! real sites.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use receipt_ledger::Summary;
use receipt_ledger::obs_fields::{
    message_outcome, model_selection_failed, run_complete, statement_reconcile,
};
use receipt_ledger::statement::pipeline::{StatementReport, log_statement_reconcile};
use receipt_ledger::{log_message_outcome, log_model_selection_failed, log_run_complete};
use tracing::field::{Field, Visit};
use tracing::subscriber::with_default;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry};

/// Per-event capture: maps a `tracing` message string to the set of field names
/// observed on events carrying that message. Shared behind a `Mutex` so the
/// layer (held by the subscriber) and the test body can both reach it.
type Captured = Arc<Mutex<HashMap<String, HashSet<String>>>>;

/// A `tracing` layer that records, for each event, the field names it emits —
/// keyed by the event's `message` field. `tracing` records an event's message as
/// a field literally named `message`, so we capture every field, pull `message`
/// out as the key, and store the remaining field names as the contract set.
struct CaptureLayer {
    captured: Captured,
}

/// Visits an event's fields, collecting the message separately from the rest.
#[derive(Default)]
struct FieldVisitor {
    message: Option<String>,
    fields: HashSet<String>,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        } else {
            self.fields.insert(field.name().to_string());
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.fields.insert(field.name().to_string());
        }
    }

    // The numeric/bool primitives bypass `record_debug`, so capture them too —
    // otherwise the `run complete` / statement tallies (all `usize`/`bool`)
    // would be silently dropped from the field set.
    fn record_u64(&mut self, field: &Field, _value: u64) {
        self.fields.insert(field.name().to_string());
    }

    fn record_i64(&mut self, field: &Field, _value: i64) {
        self.fields.insert(field.name().to_string());
    }

    fn record_u128(&mut self, field: &Field, _value: u128) {
        self.fields.insert(field.name().to_string());
    }

    fn record_i128(&mut self, field: &Field, _value: i128) {
        self.fields.insert(field.name().to_string());
    }

    fn record_f64(&mut self, field: &Field, _value: f64) {
        self.fields.insert(field.name().to_string());
    }

    fn record_bool(&mut self, field: &Field, _value: bool) {
        self.fields.insert(field.name().to_string());
    }
}

impl<S> Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        if let Some(message) = visitor.message {
            // A `None` Option field records nothing, so a non-review
            // `message outcome` legitimately has no `review_reason_category`
            // here — exactly the contract.
            self.captured
                .lock()
                .expect("capture mutex poisoned")
                .entry(message)
                .or_default()
                .extend(visitor.fields);
        }
    }
}

/// Run `body` with the capture layer installed as the (scoped) default
/// subscriber, then return what it captured, keyed by event message.
fn capture<F: FnOnce()>(body: F) -> HashMap<String, HashSet<String>> {
    let captured: Captured = Arc::new(Mutex::new(HashMap::new()));
    let subscriber = Registry::default().with(CaptureLayer {
        captured: Arc::clone(&captured),
    });
    with_default(subscriber, body);
    Arc::try_unwrap(captured)
        .expect("layer outlived capture scope")
        .into_inner()
        .expect("capture mutex poisoned")
}

/// The field-name set captured for `message`, or a clear failure if the event
/// was never emitted.
fn fields_for<'a>(
    captured: &'a HashMap<String, HashSet<String>>,
    message: &str,
) -> &'a HashSet<String> {
    captured
        .get(message)
        .unwrap_or_else(|| panic!("event {message:?} was never emitted under the capture layer"))
}

/// `&[&str]` → owned `HashSet<String>` for set comparison.
fn expected(names: &[&str]) -> HashSet<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

#[test]
fn run_complete_emits_exactly_the_contract_fields() {
    let captured = capture(|| log_run_complete(&Summary::default()));
    assert_eq!(
        fields_for(&captured, run_complete::MSG),
        &expected(run_complete::ALL),
        "run complete field set drifted from obs_fields::run_complete::ALL"
    );
}

#[test]
fn message_outcome_non_review_omits_review_category() {
    let captured = capture(|| log_message_outcome("msg-1", "paypal", "booked", None));
    let fields = fields_for(&captured, message_outcome::MSG);
    // EXACT field-set equality: the always-present contract fields PLUS `id`
    // (always emitted for correlation; intentionally not in the queried contract).
    // A non-review outcome carries NO review category (a `None` Option records
    // nothing — the documented contract). Equality (not subset) means a stray
    // field — e.g. an accidental PII field — added to `log_message_outcome` now
    // fails CI instead of slipping through.
    let mut want = expected(message_outcome::ALWAYS);
    want.insert("id".to_string());
    assert_eq!(
        fields, &want,
        "non-review message outcome field set drifted; want exactly ALWAYS+id"
    );
}

#[test]
fn message_outcome_review_includes_review_category() {
    let captured =
        capture(|| log_message_outcome("msg-2", "paypal", "review", Some("over_ceiling")));
    let fields = fields_for(&captured, message_outcome::MSG);
    // EXACT field-set equality: ALWAYS + ON_REVIEW (review_reason_category) + `id`.
    // Any unlisted field added to `log_message_outcome` now fails CI.
    let mut want = expected(message_outcome::ALWAYS);
    want.extend(expected(message_outcome::ON_REVIEW));
    want.insert("id".to_string());
    assert_eq!(
        fields, &want,
        "review message outcome field set drifted; want exactly ALWAYS+ON_REVIEW+id"
    );
}

#[test]
fn statement_reconcile_emits_exactly_the_contract_fields() {
    let captured = capture(|| log_statement_reconcile(&StatementReport::default()));
    assert_eq!(
        fields_for(&captured, statement_reconcile::MSG),
        &expected(statement_reconcile::ALL),
        "statement reconciliation field set drifted from obs_fields::statement_reconcile::ALL"
    );
}

/// Per-event value capture: maps a message to its field name→stringified value
/// map. Used to assert not just the field SET but specific serialized values.
type CapturedValues = Arc<Mutex<HashMap<String, HashMap<String, String>>>>;

/// A field visitor that records every field's stringified value (and pulls out
/// `message` as the event key).
#[derive(Default)]
struct ValueVisitor {
    message: Option<String>,
    values: HashMap<String, String>,
}

impl Visit for ValueVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let v = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(v);
        } else {
            self.values.insert(field.name().to_string(), v);
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.values
                .insert(field.name().to_string(), value.to_string());
        }
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.values
            .insert(field.name().to_string(), value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.values
            .insert(field.name().to_string(), value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.values
            .insert(field.name().to_string(), value.to_string());
    }
}

struct ValueCaptureLayer {
    captured: CapturedValues,
}

impl<S> Layer<S> for ValueCaptureLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = ValueVisitor::default();
        event.record(&mut visitor);
        if let Some(message) = visitor.message {
            self.captured
                .lock()
                .expect("capture mutex poisoned")
                .insert(message, visitor.values);
        }
    }
}

fn capture_values<F: FnOnce()>(body: F) -> HashMap<String, HashMap<String, String>> {
    let captured: CapturedValues = Arc::new(Mutex::new(HashMap::new()));
    let subscriber = Registry::default().with(ValueCaptureLayer {
        captured: Arc::clone(&captured),
    });
    with_default(subscriber, body);
    Arc::try_unwrap(captured)
        .expect("layer outlived capture scope")
        .into_inner()
        .expect("capture mutex poisoned")
}

#[test]
fn statement_reconcile_with_balance_delta_present_emits_contract_and_values() {
    // The default-report test drives balance_delta=None → "absent". This case
    // exercises the `Some(_)` branch: the SAME contract field set, plus
    // `balance_checked` must serialize `true` and `balance_delta` the numeric
    // string (NOT "absent").
    use rust_decimal::Decimal;
    let report = StatementReport {
        balance_delta: Some(Decimal::new(1234, 2)), // 12.34
        ..Default::default()
    };
    let captured = capture_values(|| log_statement_reconcile(&report));
    let values = captured
        .get(statement_reconcile::MSG)
        .expect("statement reconciliation event was emitted");

    // Same field-name set as the contract (Some(_) does not add/drop a field).
    let names: HashSet<String> = values.keys().cloned().collect();
    assert_eq!(
        names,
        expected(statement_reconcile::ALL),
        "Some(balance_delta) must emit exactly the contract field set"
    );
    assert_eq!(
        values
            .get(statement_reconcile::BALANCE_CHECKED)
            .map(String::as_str),
        Some("true"),
        "balance_checked must serialize `true` when the check ran"
    );
    assert_eq!(
        values
            .get(statement_reconcile::BALANCE_DELTA)
            .map(String::as_str),
        Some("12.34"),
        "balance_delta must be the numeric string, not \"absent\""
    );
}

#[test]
fn model_selection_failed_emits_exactly_the_contract_fields() {
    let captured = capture(|| log_model_selection_failed(&anyhow::anyhow!("boom")));
    assert_eq!(
        fields_for(&captured, model_selection_failed::MSG),
        &expected(model_selection_failed::ALL),
        "model selection failed field set drifted from obs_fields::model_selection_failed::ALL"
    );
}
