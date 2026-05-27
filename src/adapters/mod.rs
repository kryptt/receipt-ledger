//! Per-source adapters.
//!
//! An [`Adapter`] owns everything source-specific: how to recognise the
//! original sender, how to phrase the extraction prompt for the LLM, and how
//! to turn the LLM's JSON answer into typed [`Extracted`] records. The
//! money-touching steps (validate, dedup, submit) live outside the adapter and
//! treat its output as untrusted.

pub mod banco_popular;
mod parse;
pub mod paypal;

use anyhow::Result;
use serde_json::Value;

use crate::schema::Extracted;

/// A source-specific extraction strategy.
///
/// Implementors are deterministic apart from `postprocess`, which only parses
/// already-received JSON — no I/O. This keeps the whole adapter layer unit
/// testable.
pub trait Adapter: Send + Sync {
    /// Stable identifier for logs.
    fn name(&self) -> &'static str;

    /// Does this adapter handle mail from `original_sender`? The argument is
    /// the lower-cased original sender recovered from the forwarded body.
    fn matches(&self, original_sender: &str) -> bool;

    /// Build the extraction prompt for the LLM from the original email text.
    /// The prompt instructs the model to emit JSON matching [`Extracted`].
    fn prompt(&self, email_text: &str) -> String;

    /// Parse the LLM's JSON answer into zero or more [`Extracted`] records.
    ///
    /// One email may carry several transactions, hence `Vec`. Returns `Err`
    /// only when the JSON is structurally unusable; an empty `Vec` is a valid
    /// "nothing extractable" result.
    fn postprocess(&self, json: &Value) -> Result<Vec<Extracted>>;
}

/// The registry of all enabled adapters, tried in order.
pub fn adapters() -> Vec<Box<dyn Adapter>> {
    vec![
        Box::new(paypal::PaypalAdapter),
        Box::new(banco_popular::BancoPopularAdapter),
    ]
}

/// Select the adapter whose `matches` accepts `original_sender`, if any.
pub fn select(original_sender: &str) -> Option<Box<dyn Adapter>> {
    adapters()
        .into_iter()
        .find(|a| a.matches(original_sender))
}
