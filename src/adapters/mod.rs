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
pub mod paypal_payment;

use std::sync::OnceLock;

use anyhow::Result;
use chrono::NaiveDate;
use serde_json::Value;

use crate::schema::{Extracted, Money};

/// A payment booked as a Firefly **transfer** (funding bank account → a card /
/// credit liability), as extracted from a payment-receipt email.
///
/// Distinct from [`Extracted`] (a withdrawal/deposit candidate): a transfer
/// moves money *between two own accounts*, so it carries no merchant and no
/// direction — both legs are the same currency. The destination is deliberately
/// absent: the pipeline supplies it from config (mirroring how withdrawal
/// account routing lives outside the adapter), and the source is resolved from
/// [`funding_last4`](TransferRecord::funding_last4) against a config map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRecord {
    /// The transfer amount + currency (same currency on both legs).
    pub money: Money,
    /// The transaction date.
    pub date: NaiveDate,
    /// Human-readable description for the Firefly split.
    pub description: String,
    /// Dedup key (Firefly `external_id`), e.g. `pp-payment:<transaction id>`.
    pub external_id: String,
    /// Last-4 of the funding instrument (e.g. `0130`), resolved by the pipeline
    /// against the configured `last4 → account id` map to pick the source.
    pub funding_last4: String,
}

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
    /// A payment to book as a transfer (funding account → credit liability).
    Transfer(TransferRecord),
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

    /// A deterministic, pre-LLM extraction for fixed-format sources. When a
    /// source's mail is a rigid, machine-generated receipt (no free text to
    /// interpret), the adapter can parse it directly and bypass the LLM
    /// entirely. Returns `Some(result)` to take over extraction (the pipeline
    /// uses that [`Outcome`] instead of calling the model), or `None` (the
    /// default) to fall through to the LLM path. An inner `Err` is reserved for
    /// a body that *looks* like this source's receipt but fails to parse a
    /// required field — never for a clean non-match (that is
    /// `Some(Ok(Outcome::NotATransaction { .. }))`). Pure: no I/O.
    fn deterministic_extract(&self, _body: &str) -> Option<Result<Outcome>> {
        None
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

    /// Parse the LLM's JSON, then apply any deterministic, *body-derived*
    /// refinement the source needs — overrides that must come from the email
    /// text itself, not the model's reading of it. The default is plain
    /// [`postprocess`](Adapter::postprocess) (no refinement).
    ///
    /// PayPal overrides this to enforce policy P1: when a cross-currency receipt
    /// carries an authoritative `Total amount of this Transaction: $X USD` line,
    /// the booked amount/currency is that USD figure regardless of what the
    /// model extracted — a deterministic guarantee, not a prompt hope.
    ///
    /// Pure (no I/O): both the live pipeline and the eval harness call this so
    /// they share one extraction path.
    fn postprocess_with_body(&self, json: &Value, _body: &str) -> Result<Outcome> {
        self.postprocess(json)
    }
}

/// The registry of all enabled adapters, tried in order. Built once and reused
/// for the process lifetime — adapters are zero-sized and stateless.
fn registry() -> &'static [&'static (dyn Adapter + 'static)] {
    static REGISTRY: OnceLock<Vec<&'static (dyn Adapter + 'static)>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            vec![
                // The PayPal *payment* adapter (customercare@paypal.com) is
                // tried before the *purchase* one (service@paypal.com); their
                // `matches` senders are disjoint, so order is not load-bearing.
                &paypal_payment::PaypalPaymentAdapter as &(dyn Adapter + 'static),
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
