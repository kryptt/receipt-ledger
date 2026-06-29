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
pub mod swift;

use std::sync::OnceLock;

use anyhow::Result;
use chrono::NaiveDate;
use serde_json::Value;

use crate::schema::{Extracted, Money};

/// A payment booked as a Firefly **transfer** (funding bank account → a card /
/// credit liability, or → the user's own foreign account), as extracted from a
/// payment-receipt or wire-confirmation email.
///
/// Distinct from [`Extracted`] (a withdrawal/deposit candidate): a transfer
/// moves money *between two own accounts*, so it carries no merchant and no
/// direction — both legs are the same currency. The *account ids* are
/// deliberately absent from the record: the pipeline supplies them from config
/// (mirroring how withdrawal account routing lives outside the adapter). The
/// source is resolved from [`source`](TransferRecord::source) (which names BOTH
/// the last-4 *and* the config map it resolves against); the destination is
/// chosen by [`dest`](TransferRecord::dest).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRecord {
    /// The transfer amount + currency (same currency on both legs).
    pub money: Money,
    /// The transaction date.
    pub date: NaiveDate,
    /// Human-readable description for the Firefly split.
    pub description: String,
    /// Dedup key (Firefly `external_id`), e.g. `pp-payment:<transaction id>` or
    /// `swift:<uetr>`.
    pub external_id: String,
    /// How the pipeline should resolve the *source* (funding/debtor) leg's
    /// account id. A closed set that names BOTH the instrument's last-4 and the
    /// config map it resolves against, so a PayPal funding card and a SWIFT
    /// debtor IBAN with a colliding last-4 cannot resolve against the same map.
    pub source: SourceHint,
    /// How the pipeline should resolve the *destination* leg's account id. A
    /// closed set so a new transfer source forces a routing decision rather than
    /// silently defaulting.
    pub dest: DestHint,
}

/// How the pipeline resolves a [`TransferRecord`]'s *source* (funding/debtor)
/// account id.
///
/// A sum type rather than a bare last-4 string, so the last-4 always carries the
/// *map* it should resolve against. This makes a last-4 collision between two
/// unrelated instruments (a PayPal funding card vs. a BPD IBAN) unrepresentable:
/// the PayPal path resolves against `RECEIPT_PAYING_ACCOUNT_BY_LAST4` and the
/// SWIFT path against the dedicated `RECEIPT_SWIFT_DEBTOR_BY_LAST4`, never the
/// same map. [`crate::book_transfer`] must `match` exhaustively.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceHint {
    /// A PayPal-payment funding card, resolved from its last-4 against
    /// `RECEIPT_PAYING_ACCOUNT_BY_LAST4`.
    PayPalFundingLast4(String),
    /// A SWIFT outbound wire's debtor IBAN, resolved from its last-4 against the
    /// dedicated `RECEIPT_SWIFT_DEBTOR_BY_LAST4` map (kept separate from the
    /// PayPal funding map so a colliding last-4 cannot mis-route).
    SwiftDebtorLast4(String),
}

/// How the pipeline resolves a [`TransferRecord`]'s destination account id.
///
/// A sum type rather than a stringly-typed field, so [`crate::book_transfer`]
/// must `match` exhaustively — adding a new transfer source forces a destination
/// routing decision instead of silently mis-booking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestHint {
    /// The configured PayPal Credit account (`RECEIPT_PAYPAL_CREDIT_ACCOUNT`).
    /// Used by the PayPal-payment receipt path, which carries no BIC.
    PayPalCredit,
    /// A SWIFT outbound wire: the destination is the user's own foreign account,
    /// resolved from the creditor institution's normalized 8-char BIC against
    /// the configured `BIC → account id` map (`RECEIPT_SWIFT_DEST_BY_BIC`).
    CreditorBic(String),
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

/// Test-only helpers shared between unit and integration tests.
///
/// Not `#[cfg(test)]` because integration tests in `tests/` are separate
/// crates and cannot see cfg(test) items from the library crate.  The crate
/// is `publish = false` and its public surface is not a SemVer contract, so
/// an unconditional (but `#[doc(hidden)]`) module is the pragmatic choice.
#[doc(hidden)]
pub mod test_support {
    use super::{Outcome, TransferRecord};
    use crate::schema::Extracted;

    /// Unwrap a `Transaction` outcome into its records, panicking with a clear
    /// message when the outcome is a different variant. Shared by
    /// [`single_transaction`] and [`assert_transaction_count`].
    fn unwrap_transactions(outcome: Outcome) -> Vec<Extracted> {
        match outcome {
            Outcome::Transaction(v) => v,
            Outcome::Transfer(_) => panic!("expected transaction, got transfer"),
            Outcome::NotATransaction { reason } => {
                panic!("expected transaction, got skip: {reason}")
            }
        }
    }

    /// Extract the single record from a `Transaction` outcome, panicking
    /// if the outcome is not a single-element transaction.
    pub fn single_transaction(outcome: Outcome) -> Extracted {
        let mut v = unwrap_transactions(outcome);
        assert_eq!(v.len(), 1);
        v.pop().unwrap()
    }

    /// Extract the transfer from a `Transfer` outcome, panicking if the
    /// outcome is not a transfer.
    pub fn single_transfer(outcome: Outcome) -> TransferRecord {
        match outcome {
            Outcome::Transfer(t) => t,
            Outcome::Transaction(_) => panic!("expected transfer, got transaction"),
            Outcome::NotATransaction { reason } => {
                panic!("expected transfer, got skip: {reason}")
            }
        }
    }

    /// Assert that the outcome is a `Transaction` with exactly `expected`
    /// records. Returns the records for further assertions.
    pub fn assert_transaction_count(outcome: Outcome, expected: usize) -> Vec<Extracted> {
        let v = unwrap_transactions(outcome);
        assert_eq!(v.len(), expected);
        v
    }

    /// Assert that the outcome is a `NotATransaction` (a clean skip). Returns
    /// the skip reason for optional further inspection.
    pub fn assert_not_a_transaction(outcome: Outcome) -> String {
        match outcome {
            Outcome::NotATransaction { reason } => reason,
            Outcome::Transaction(v) => {
                panic!("expected not-a-transaction, got {} transaction(s)", v.len())
            }
            Outcome::Transfer(_) => panic!("expected not-a-transaction, got transfer"),
        }
    }

    /// Validate an extracted record and assert it books cleanly (no Review).
    /// Delegates to [`crate::test_support::assert_booked`] after calling
    /// [`crate::validate::validate`].
    pub fn assert_books_clean(e: Extracted) -> crate::validate::Validated {
        crate::test_support::assert_booked(crate::validate::validate(e))
    }

    /// Validate an extracted record and assert it routes to Review (not Booked).
    /// Returns the review reason for further assertions.
    pub fn assert_reviews(e: Extracted) -> String {
        match crate::validate::validate(e) {
            crate::validate::Verdict::Review { reason } => reason,
            crate::validate::Verdict::Booked(_) => {
                panic!("expected Review, got Booked")
            }
        }
    }

    /// Assert amount + currency on an [`Extracted`] in one call. Eliminates the
    /// repeated `assert_eq!(e.amount().value(), dec(...)); assert_eq!(e.currency().as_str(), ...)`
    /// pair across adapter tests.
    pub fn assert_money(e: &Extracted, expected_amount: &str, expected_currency: &str) {
        assert_eq!(e.amount().value(), crate::test_support::dec(expected_amount));
        assert_eq!(e.currency().as_str(), expected_currency);
    }

    /// Postprocess JSON through an adapter and extract the single transaction
    /// record. Panics if postprocess fails or the outcome is not a single
    /// transaction. Shared by integration and unit tests.
    pub fn postprocess_one(
        adapter: &dyn super::Adapter,
        json: &serde_json::Value,
    ) -> Extracted {
        single_transaction(adapter.postprocess(json).expect("postprocess succeeds"))
    }

    /// Postprocess JSON with body refinement and extract the single transaction
    /// record. Panics if postprocess_with_body fails or the outcome is not a
    /// single transaction. Used by cross-currency and promo integration tests.
    pub fn postprocess_with_body_one(
        adapter: &dyn super::Adapter,
        model_json: &serde_json::Value,
        body: &str,
    ) -> Extracted {
        single_transaction(
            adapter
                .postprocess_with_body(model_json, body)
                .expect("postprocess_with_body succeeds"),
        )
    }
}
