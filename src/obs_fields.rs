//! Stable observability field-name contract — single source of truth.
//!
//! Cluster-side LogQL recording/alert rules (see `docs/observability/README.md`)
//! key on the **field names** of the structured JSON events this crate emits.
//! Renaming a field at an emission site silently breaks those rules because the
//! Rust compiler has no idea Loki cares. This module pins every canonical field
//! name (and every event *message* string) as a `pub const`, and the contract
//! test (`tests/obs_field_contract.rs`) asserts that what the emit-helpers
//! actually log equals these constants — so a rename now fails CI.
//!
//! ## Why these are not interpolated into the `info!`/`warn!` macros
//!
//! `tracing` field keys are **compile-time identifiers baked into callsite
//! metadata**, not runtime strings — `info!(MY_CONST = x)` would record a field
//! literally named `MY_CONST`, not the const's value. So these constants do
//! **not** feed the macros. They are the *documented, testable* mirror of the
//! identifiers written literally at the emission sites; the contract test is what
//! keeps the two in lockstep.
//!
//! Grouped by event. Each event's `MSG_*` is the `tracing` message; the `*`
//! field constants are the structured fields the LogQL rules query. Fields like
//! `id` that the rules do **not** key on are intentionally omitted (they are not
//! part of the queryable contract), with one exception noted per event.

/// `run complete` event — emitted once per successful run (`src/main.rs`,
/// via [`crate::log_run_complete`]). All fields are numeric tallies.
pub mod run_complete {
    /// `tracing` message identifying the event.
    pub const MSG: &str = "run complete";

    pub const PROCESSED: &str = "processed";
    pub const BOOKED: &str = "booked";
    pub const DUPLICATES: &str = "duplicates";
    pub const REVIEW: &str = "review";
    pub const SKIPPED: &str = "skipped";
    pub const STATEMENTS: &str = "statements";
    pub const CORRECTED: &str = "corrected";
    pub const DEFERRED: &str = "deferred";

    /// Every contract field this event emits.
    pub const ALL: &[&str] = &[
        PROCESSED, BOOKED, DUPLICATES, REVIEW, SKIPPED, STATEMENTS, CORRECTED, DEFERRED,
    ];
}

/// `message outcome` event — one per processed message (`src/lib.rs`, via
/// [`crate::log_message_outcome`]). The LogQL source×disposition metric keys on
/// `source` + `disposition`; `review_reason_category` is present only on a review
/// (bounded, PII-free). `id` is emitted for correlation but is not a queried
/// contract field, so it is excluded from [`ALL`] and the test asserts only that
/// the contract fields are present.
pub mod message_outcome {
    /// `tracing` message identifying the event.
    pub const MSG: &str = "message outcome";

    pub const SOURCE: &str = "source";
    pub const DISPOSITION: &str = "disposition";
    /// Only emitted (non-`None`) on a `review` disposition.
    pub const REVIEW_REASON_CATEGORY: &str = "review_reason_category";

    /// Fields always emitted on every `message outcome`.
    pub const ALWAYS: &[&str] = &[SOURCE, DISPOSITION];
    /// Field emitted additionally when the disposition is `review`.
    pub const ON_REVIEW: &[&str] = &[REVIEW_REASON_CATEGORY];
}

/// `statement reconciliation complete` event — one per processed statement
/// (`src/statement/pipeline.rs`, via [`crate::log_statement_reconcile`]).
pub mod statement_reconcile {
    /// `tracing` message identifying the event.
    pub const MSG: &str = "statement reconciliation complete";

    pub const RECONCILED: &str = "reconciled";
    pub const BOOKED_NEW: &str = "booked_new";
    pub const PAYMENTS_BOOKED: &str = "payments_booked";
    pub const AMOUNT_MISMATCH: &str = "amount_mismatch";
    pub const CORRECTED: &str = "corrected";
    pub const UNMATCHED_BOOKED: &str = "unmatched_booked";
    pub const BALANCE_MISMATCH: &str = "balance_mismatch";
    pub const DEFERRED: &str = "deferred";
    pub const REVIEW: &str = "review";
    pub const BALANCE_CHECKED: &str = "balance_checked";
    pub const BALANCE_DELTA: &str = "balance_delta";

    /// Every contract field this event emits.
    pub const ALL: &[&str] = &[
        RECONCILED,
        BOOKED_NEW,
        PAYMENTS_BOOKED,
        AMOUNT_MISMATCH,
        CORRECTED,
        UNMATCHED_BOOKED,
        BALANCE_MISMATCH,
        DEFERRED,
        REVIEW,
        BALANCE_CHECKED,
        BALANCE_DELTA,
    ];
}

/// `model selection failed` event — emitted before the run aborts with a
/// non-zero exit (`src/lib.rs`, via [`crate::log_model_selection_failed`]). The
/// `stage` field lets a log-derived alert key on it directly; `error` carries the
/// failure (redacted of secrets upstream).
pub mod model_selection_failed {
    /// `tracing` message identifying the event.
    pub const MSG: &str = "model selection failed";

    pub const STAGE: &str = "stage";
    pub const ERROR: &str = "error";

    /// The fixed value of the `stage` field for this event.
    pub const STAGE_VALUE: &str = "model_selection";

    /// Every contract field this event emits.
    pub const ALL: &[&str] = &[STAGE, ERROR];
}
