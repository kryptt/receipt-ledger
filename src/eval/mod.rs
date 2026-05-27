//! Extraction-accuracy evaluation: the objective judge.
//!
//! This module is the *pure*, network-free core of the eval harness: the
//! ground-truth schema ([`Expected`]), the produced-fields projection
//! ([`Produced`]), and the field-by-field scorer ([`score`]). It is compiled
//! into the library and unit tested under `./test.sh`, so the scoring logic is
//! covered without ever touching a model.
//!
//! The *network* part — actually calling each model, running the real
//! extraction path, and aggregating a per-model × per-field matrix — lives in
//! the `eval` binary (`src/bin/eval.rs`) and is NOT part of `./test.sh`.
//!
//! Design: the harness reduces both the ground-truth label and the model's
//! end-to-end output to the same flat [`Produced`] shape, then [`score`]
//! compares them field by exact-match field. "Exact match" is defined per
//! field with the minimum normalization that does not let the model cheat:
//! currencies are upper-cased, dates compared as `NaiveDate`, amounts as
//! `Decimal` (so `"1.50"` == `"1.5"`), merchant trimmed + case-insensitive,
//! status compared by its closed [`crate::validate::Status`] *classification*
//! (Approved/Declined/Other) rather than raw text — because the ledger only
//! ever acts on the classification.

pub mod matrix;
pub mod scorer;

pub use matrix::{Matrix, ModelScores};
pub use scorer::{
    Expected, FieldScore, FieldScores, Kind, Produced, RoutedAccount, StatusClass,
    routed_account_of, score,
};
