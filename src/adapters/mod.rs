//! Per-source adapters.
//!
//! An [`Adapter`] owns everything source-specific: how to recognise the
//! original sender, how to phrase the extraction prompt for the LLM, and how
//! to turn the LLM's JSON answer into typed [`Extracted`] records. The
//! money-touching steps (validate, dedup, submit) live outside the adapter and
//! treat its output as untrusted.

pub mod banco_popular;
pub mod parse;
pub mod paypal;

use std::sync::OnceLock;

use anyhow::Result;
use serde_json::Value;

use crate::schema::Extracted;

/// What an adapter made of an email.
///
/// A *clean skip* (non-receipt mail — PayPal "your order is on its way",
/// "Pay in 4 plan", surveys) is a first-class [`Outcome::NotATransaction`],
/// distinct from a parse `Err`. The pipeline routes it to a clean disposition
/// (Processed/Ignored), NOT to Review — it never was a transaction, so it does
/// not deserve human eyes the way a *failed* extraction does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Zero or more transaction candidates were extracted.
    Transaction(Vec<Extracted>),
    /// This mail is not a transaction notification at all — skip it cleanly.
    NotATransaction { reason: String },
}

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

    /// A deterministic, pre-LLM check: does this body look like a real
    /// transaction notification from this source? When it clearly does not
    /// (e.g. a PayPal shipping update lacking "Transaction ID"/"You paid"), the
    /// adapter can short-circuit to [`Outcome::NotATransaction`] without
    /// spending an LLM call. Default: assume it is a transaction (let the LLM
    /// and validation gates decide).
    fn is_transaction(&self, _body: &str) -> bool {
        true
    }

    /// Build the extraction prompt for the LLM from the original email text.
    /// The prompt instructs the model to emit JSON matching [`Extracted`].
    fn prompt(&self, email_text: &str) -> String;

    /// Parse the LLM's JSON answer into an [`Outcome`].
    ///
    /// One email may carry several transactions, hence `Vec` inside
    /// [`Outcome::Transaction`]. A model that reports the mail is not a
    /// transaction (via the `kind` discriminant) yields
    /// [`Outcome::NotATransaction`]. Returns `Err` only when the JSON is
    /// structurally unusable.
    fn postprocess(&self, json: &Value) -> Result<Outcome>;
}

/// The registry of all enabled adapters, tried in order. Built once and reused
/// for the process lifetime — adapters are zero-sized and stateless.
fn registry() -> &'static [&'static (dyn Adapter + 'static)] {
    static REGISTRY: OnceLock<Vec<&'static (dyn Adapter + 'static)>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            vec![
                &paypal::PaypalAdapter as &(dyn Adapter + 'static),
                &banco_popular::BancoPopularAdapter as &(dyn Adapter + 'static),
            ]
        })
        .as_slice()
}

/// Select the adapter whose `matches` accepts `original_sender`, if any.
#[must_use]
pub fn select(original_sender: &str) -> Option<&'static dyn Adapter> {
    registry()
        .iter()
        .copied()
        .find(|a| a.matches(original_sender))
}
