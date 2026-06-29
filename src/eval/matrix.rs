//! Aggregation of per-example [`FieldScores`] into a per-model × per-field
//! accuracy matrix.
//!
//! Pure: the harness feeds each example's [`FieldScores`] in via
//! [`ModelScores::record`], then asks for accuracies and a rendered table. No
//! I/O lives here, so the aggregation + rendering is unit tested under
//! `./test.sh` even though the model calls that produce the scores are not.

use std::collections::BTreeMap;

use serde::Serialize;

use super::scorer::{FieldScore, FieldScores};

/// Ratio of `correct / applicable`, or `None` when `applicable` is zero.
fn ratio(correct: u32, applicable: u32) -> Option<f64> {
    (applicable > 0).then(|| f64::from(correct) / f64::from(applicable))
}

/// The eight scored fields, in display order. Mirrors [`FieldScores::iter`].
pub const FIELDS: [&str; 8] = [
    "kind",
    "amount",
    "currency",
    "direction",
    "date",
    "merchant",
    "status",
    "account",
];

/// A running (correct, applicable) tally for one field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Tally {
    /// Number of applicable examples that scored [`FieldScore::Correct`].
    pub correct: u32,
    /// Number of applicable examples (denominator); excludes N/A.
    pub applicable: u32,
}

impl Tally {
    fn observe(&mut self, score: FieldScore) {
        match score {
            FieldScore::Correct => {
                self.correct += 1;
                self.applicable += 1;
            }
            FieldScore::Wrong => {
                self.applicable += 1;
            }
            FieldScore::NotApplicable => {}
        }
    }

    /// Accuracy in `[0,1]`, or `None` when no example was applicable (so it is
    /// rendered as `-` rather than a misleading 0% or 100%).
    #[must_use]
    pub fn accuracy(&self) -> Option<f64> {
        ratio(self.correct, self.applicable)
    }
}

/// Per-field tallies for a single model, plus the example count it saw.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelScores {
    /// Field name → tally. A `BTreeMap` keeps a stable order in JSON output.
    pub fields: BTreeMap<String, Tally>,
    /// Number of examples scored against this model.
    pub examples: u32,
}

impl ModelScores {
    /// Fold one example's field scores into this model's running tallies.
    pub fn record(&mut self, scores: &FieldScores) {
        self.examples += 1;
        for (name, fs) in scores.iter() {
            self.fields.entry(name.to_string()).or_default().observe(fs);
        }
    }

    /// The per-field accuracy for `field`, if any example was applicable.
    #[must_use]
    pub fn field_accuracy(&self, field: &str) -> Option<f64> {
        self.fields.get(field).and_then(Tally::accuracy)
    }

    /// Overall accuracy: total correct over total applicable across all fields.
    /// `None` when nothing was applicable.
    #[must_use]
    pub fn overall_accuracy(&self) -> Option<f64> {
        let mut correct = 0u32;
        let mut applicable = 0u32;
        for t in self.fields.values() {
            correct += t.correct;
            applicable += t.applicable;
        }
        ratio(correct, applicable)
    }
}

/// The full matrix: model name → its [`ModelScores`]. `BTreeMap` for a stable
/// row order in both the table and JSON.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Matrix {
    pub models: BTreeMap<String, ModelScores>,
}

impl Matrix {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one (model, example) score into the matrix.
    pub fn record(&mut self, model: &str, scores: &FieldScores) {
        self.models
            .entry(model.to_string())
            .or_default()
            .record(scores);
    }

    /// Render a fixed-width text table: one row per model, one column per field,
    /// plus an `OVERALL` column. Cells are percentages (or `-` for N/A). Pure —
    /// returns the table as a `String` so the binary can print it and tests can
    /// assert on it.
    #[must_use]
    pub fn render_table(&self) -> String {
        // Column width: enough for "100%" / "-" plus the field header.
        let model_w = self
            .models
            .keys()
            .map(String::len)
            .max()
            .unwrap_or(5)
            .max("model".len())
            + 2;
        // Wide enough for the longest header ("direction" = 9) plus a leading
        // space so columns never run together.
        let cell_w = 11usize;

        let mut out = String::new();

        // Header row.
        out.push_str(&pad("model", model_w));
        for f in FIELDS {
            out.push_str(&lpad(f, cell_w));
        }
        out.push_str(&lpad("OVERALL", cell_w + 1));
        out.push('\n');

        // Separator.
        out.push_str(&"-".repeat(model_w + cell_w * FIELDS.len() + cell_w + 1));
        out.push('\n');

        // One row per model.
        for (name, scores) in &self.models {
            out.push_str(&pad(name, model_w));
            for f in FIELDS {
                out.push_str(&lpad(&fmt_pct(scores.field_accuracy(f)), cell_w));
            }
            out.push_str(&lpad(&fmt_pct(scores.overall_accuracy()), cell_w + 1));
            out.push('\n');
        }
        out
    }
}

/// Format an optional accuracy as a percentage cell: `"92%"`, or `"-"` for N/A.
fn fmt_pct(acc: Option<f64>) -> String {
    match acc {
        Some(a) => format!("{}%", (a * 100.0).round() as i64),
        None => "-".to_string(),
    }
}

/// Left-justify `s` in a field of width `w` (header / model names).
fn pad(s: &str, w: usize) -> String {
    format!("{s:<w$}")
}

/// Right-justify `s` in a field of width `w` (numeric cells).
fn lpad(s: &str, w: usize) -> String {
    format!("{s:>w$}")
}

// -- eval-matrix unit tests --
#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::scorer::FieldScore::{Correct, NotApplicable, Wrong};

    /// Build a `FieldScores` from eight explicit scores in FIELDS order.
    fn fs(scores: [FieldScore; 8]) -> FieldScores {
        FieldScores {
            kind: scores[0],
            amount: scores[1],
            currency: scores[2],
            direction: scores[3],
            date: scores[4],
            merchant: scores[5],
            status: scores[6],
            routed_account: scores[7],
        }
    }

    #[test]
    fn tally_accuracy_excludes_not_applicable() {
        let mut t = Tally::default();
        t.observe(Correct);
        t.observe(Wrong);
        t.observe(NotApplicable);
        // 1 correct of 2 applicable (the N/A does not count).
        assert_eq!(t.accuracy(), Some(0.5));
    }

    #[test]
    fn tally_all_not_applicable_is_none() {
        let mut t = Tally::default();
        t.observe(NotApplicable);
        assert_eq!(t.accuracy(), None);
    }

    #[test]
    fn matrix_aggregates_per_field_and_overall() {
        let mut m = Matrix::new();
        // Example 1: everything correct.
        m.record("modelA", &fs([Correct; 8]));
        // Example 2: amount wrong, the rest correct.
        m.record(
            "modelA",
            &fs([
                Correct, Wrong, Correct, Correct, Correct, Correct, Correct, Correct,
            ]),
        );

        let a = &m.models["modelA"];
        assert_eq!(a.examples, 2);
        // amount: 1/2 correct.
        assert_eq!(a.field_accuracy("amount"), Some(0.5));
        // kind: 2/2.
        assert_eq!(a.field_accuracy("kind"), Some(1.0));
        // overall: 15 correct of 16 applicable.
        assert_eq!(a.overall_accuracy(), Some(15.0 / 16.0));
    }

    #[test]
    fn not_applicable_field_renders_dash() {
        let mut m = Matrix::new();
        // A non-transaction example: kind correct, all tx fields N/A.
        m.record(
            "modelB",
            &fs([
                Correct,
                NotApplicable,
                NotApplicable,
                NotApplicable,
                NotApplicable,
                NotApplicable,
                NotApplicable,
                NotApplicable,
            ]),
        );
        let b = &m.models["modelB"];
        assert_eq!(b.field_accuracy("amount"), None);
        let table = m.render_table();
        assert!(table.contains("modelB"));
        // The amount column for the only (N/A) example renders as a dash.
        assert!(table.contains('-'));
    }

    #[test]
    fn table_has_a_row_per_model_and_overall_column() {
        let mut m = Matrix::new();
        m.record("alpha", &fs([Correct; 8]));
        m.record("beta", &fs([Wrong; 8]));
        let table = m.render_table();
        assert!(table.contains("OVERALL"));
        assert!(table.contains("alpha"));
        assert!(table.contains("beta"));
        // alpha all-correct → 100%, beta all-wrong → 0%.
        assert!(table.contains("100%"));
        assert!(table.contains("0%"));
    }
}
