//! Transient vs. permanent failure classification, shared by every provider
//! (FX rate lookups, LLM extraction, etc.).
//!
//! HTTP providers can fail in two ways: *transiently* (5xx, 408, 429, network
//! timeout — retry later) or *permanently* (4xx auth, bad request, parse
//! failure — will not improve). The shared helpers here classify an HTTP status
//! and walk an `anyhow` chain looking for a typed transient marker, so every
//! provider module delegates to the same logic instead of duplicating it.

/// Whether an HTTP status represents a transient (retryable) or permanent
/// failure.
pub enum Transience {
    Transient,
    Permanent,
}

/// Classify an HTTP status: server errors plus 408/429 are transient; every
/// other non-success is permanent.
pub fn classify_http_status(status: reqwest::StatusCode) -> Transience {
    if status.is_server_error()
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
    {
        Transience::Transient
    } else {
        Transience::Permanent
    }
}

/// Walk an `anyhow` error chain looking for a downcastable `T` whose
/// `is_transient` predicate returns `true`.
///
/// Both `fx::is_transient` and `llm::is_transient` delegate here so the
/// chain-walking logic lives in one place.
pub fn has_transient_in_chain<T: std::error::Error + Send + Sync + 'static>(
    err: &anyhow::Error,
    is_transient: fn(&T) -> bool,
) -> bool {
    err.chain()
        .any(|e| e.downcast_ref::<T>().is_some_and(is_transient))
}

/// Generate a provider error enum with `Transient(String)` and `Permanent(String)`
/// variants, `Display`, `std::error::Error`, `is_transient`, and `classify_status`.
///
/// Usage:
/// ```ignore
/// crate::transient::define_provider_error!(RateError, "rate");
/// ```
/// produces:
/// - `pub enum RateError { Transient(String), Permanent(String) }`
/// - `Display`: "transient rate failure: …" / "permanent rate failure: …"
/// - `std::error::Error` impl
/// - `#[must_use] pub fn is_transient(err: &anyhow::Error) -> bool`
/// - `fn classify_status(status: reqwest::StatusCode, msg: String) -> $name`
macro_rules! define_provider_error {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone)]
        pub enum $name {
            Transient(String),
            Permanent(String),
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    $name::Transient(m) => {
                        write!(f, concat!("transient ", $prefix, " failure: {}"), m)
                    }
                    $name::Permanent(m) => {
                        write!(f, concat!("permanent ", $prefix, " failure: {}"), m)
                    }
                }
            }
        }

        impl std::error::Error for $name {}

        #[must_use]
        pub fn is_transient(err: &anyhow::Error) -> bool {
            crate::transient::has_transient_in_chain::<$name>(err, |e| {
                matches!(e, $name::Transient(_))
            })
        }

        fn classify_status(status: reqwest::StatusCode, msg: String) -> $name {
            match crate::transient::classify_http_status(status) {
                crate::transient::Transience::Transient => $name::Transient(msg),
                crate::transient::Transience::Permanent => $name::Permanent(msg),
            }
        }
    };
}

pub(crate) use define_provider_error;

/// Generate `assert_transient` / `assert_permanent` test helpers for a
/// `classify_status` function that returns an error type with
/// `Transient(_)` and `Permanent(_)` variants.
///
/// Usage: `define_classify_assertions!(classify_status, RateError);`
/// produces `fn assert_transient(code)` and `fn assert_permanent(code)`.
#[cfg(test)]
macro_rules! define_classify_assertions {
    ($classify_fn:path, $error_ty:ident) => {
        fn assert_transient(code: reqwest::StatusCode) {
            assert!(
                matches!($classify_fn(code, "x".into()), $error_ty::Transient(_)),
                "{code} should be transient"
            );
        }

        fn assert_permanent(code: reqwest::StatusCode) {
            assert!(
                matches!($classify_fn(code, "x".into()), $error_ty::Permanent(_)),
                "{code} should be permanent"
            );
        }
    };
}

/// Generate the `is_transient_walks_the_context_chain` test body for a
/// provider's `is_transient` function and error type. Asserts the three
/// canonical cases: transient survives `.context()`, permanent does not,
/// and an unrelated error does not.
///
/// Usage: `assert_transient_chain!(is_transient, RateError);`
#[cfg(test)]
macro_rules! assert_transient_chain {
    ($is_transient:path, $error_ty:ident) => {
        let transient =
            anyhow::anyhow!($error_ty::Transient("test".into())).context("wrapped context");
        assert!($is_transient(&transient));
        let permanent =
            anyhow::anyhow!($error_ty::Permanent("test".into())).context("wrapped context");
        assert!(!$is_transient(&permanent));
        assert!(!$is_transient(&anyhow::anyhow!("an unrelated error")));
    };
}

#[cfg(test)]
pub(crate) use assert_transient_chain;
#[cfg(test)]
pub(crate) use define_classify_assertions;
