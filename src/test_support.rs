//! Shared test helpers, available to every `#[cfg(test)]` module in the crate.
//!
//! Centralises small utilities that otherwise get copy-pasted across test files
//! (e.g. the `dec` decimal parser, single-threaded tokio runtime, record
//! factories).
//!
//! NOT `#[cfg(test)]`-gated: `tests/banco_popular_pipeline.rs` and
//! `tests/paypal_pipeline.rs` are integration tests, which link the crate built
//! *without* `cfg(test)`, so this module has to exist in that build for them to
//! use it. That in turn means `lib.rs`'s
//! `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]`
//! applies here, hence the module-level allow below. The panics are correct:
//! these are fixture builders, and a bad fixture literal should fail the test
//! loudly rather than be handled.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use rust_decimal::Decimal;
use std::str::FromStr;

/// Shorthand decimal parser for test literals. Panics on invalid input.
pub fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap() // test-only parse
}

/// Build a [`Money`] from amount and currency string literals. Panics on
/// invalid input (test code only).
pub fn money(amount: &str, currency: &str) -> crate::schema::Money {
    crate::schema::Money::new(
        crate::schema::Amount::parse(amount).unwrap(),
        crate::schema::Currency::parse(currency).unwrap(),
    )
}

/// Build a single-threaded tokio runtime for blocking-on async test code.
/// Avoids the four-line boilerplate repeated across fx / usd_ceiling / etc.
pub fn test_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
}

/// Assert a `Verdict` is `Booked` and return the `Validated` record.
/// Panics with the review reason if the verdict is `Review`.
pub fn assert_booked(v: crate::validate::Verdict) -> crate::validate::Validated {
    match v {
        crate::validate::Verdict::Booked(b) => b,
        crate::validate::Verdict::Review { reason } => {
            panic!("expected booked, got review: {reason}")
        }
    }
}

/// Assert a `Verdict` is `Review`. Panics if it is `Booked`.
pub fn assert_review(v: crate::validate::Verdict) {
    assert!(
        matches!(v, crate::validate::Verdict::Review { .. }),
        "expected review, got booked"
    );
}

/// Canonical PayPal [`Extracted`] record used across dedup, validate, and
/// eval tests. Returns the same field values every time so a single source
/// of truth lives here rather than being copy-pasted into each test module.
pub fn paypal_record() -> crate::schema::Extracted {
    use crate::schema::{Direction, Source};
    use chrono::NaiveDate;
    crate::schema::Extracted {
        source: Source::Paypal,
        external_id: Some("8XY12345AB678901C".to_string()),
        money: money("149.99", "EUR"), // canonical fixture amount
        direction: Direction::Out,
        date: NaiveDate::from_ymd_opt(2026, 5, 11).unwrap(),
        merchant: "Example Merchant B.V.".to_string(),
        account_hint: None,
        status: "approved".to_string(),
        raw_ref: "TESTORDER0123456".to_string(),
    }
}
