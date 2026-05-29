//! Reconciling a parsed statement against what Firefly already holds.
//!
//! The statement is the authoritative, complete list for the cycle, but most of
//! its charges may already be booked from consumo notifications — described
//! *differently* (the notification's merchant string and an ECB-estimated USD
//! amount vs. the statement's billed amount), so neither `external_id` nor
//! Firefly's content-hash dedup recognises them as the same charge. Matching is
//! therefore fuzzy, on the only shared signals: the **auth date** (`TRANSAC` ==
//! the notification's `Fecha`), **merchant similarity**, and the
//! account-currency amount as a *tolerance-band corroborator* — never an exact
//! key, because a foreign charge's booked amount is an estimate and the
//! statement's is the real billed figure (see the module/plan notes).
//!
//! This module is **pure**: it takes the parsed charges and the existing Firefly
//! journals (as plain [`ExistingJournal`]s — no Firefly client types) and
//! returns a [`Reconciliation`] of per-charge decisions plus the journals that
//! went unmatched. The I/O (fetching journals, booking the new rows) lives in
//! the pipeline; the *decisions* are decided here and exhaustively tested.

use chrono::NaiveDate;
use rust_decimal::Decimal;
use strsim::jaro_winkler;

use super::{Reference, StatementTxn};

/// A `receipt-ledger`-tagged Banco Popular transaction already in Firefly, for a
/// single account, reduced to what matching needs. `amount` is the booked value
/// in the **account currency** (positive magnitude), same currency as a
/// statement charge in that section — so the two are directly comparable.
#[derive(Debug, Clone, PartialEq)]
pub struct ExistingJournal {
    pub id: String,
    pub date: NaiveDate,
    pub amount: Decimal,
    pub merchant: String,
    /// The journal's `external_id`, if any. A `bpstmt:<ref>` value is a prior
    /// statement booking and matches its charge exactly (re-run idempotency).
    pub external_id: Option<String>,
}

/// Tunable thresholds. Defaults are deliberately conservative (Phase-0 could not
/// calibrate against this cycle): a non-confident match routes to Review rather
/// than auto-booking or silently confirming.
#[derive(Debug, Clone, Copy)]
pub struct ReconcileParams {
    /// Max `|charge.auth_date − journal.date|` (days) for a candidate.
    pub date_window_days: i64,
    /// Merchant similarity at or above this is a confident match.
    pub merchant_threshold: f64,
    /// Similarity in `[merchant_gray, merchant_threshold)` is a *possible*
    /// duplicate — not confident enough to confirm, but enough to forbid booking
    /// a new row (the cross-path double-book guard) → Review.
    pub merchant_gray: f64,
    /// `|statement − booked|` at or below this counts the amounts as equal
    /// (absorbs the ECB-estimate vs billed gap for foreign charges).
    pub amount_tolerance: Decimal,
    /// If a charge's two best candidates are within this similarity of each
    /// other, the match is ambiguous → Review.
    pub score_epsilon: f64,
}

impl Default for ReconcileParams {
    fn default() -> Self {
        ReconcileParams {
            date_window_days: 5,
            // 0.85 confidently matches the common "statement appends a location"
            // shape (Jaro–Winkler of "JR EAST" vs "JR EAST SIBUYAKU" ≈ 0.89)
            // while keeping distinct merchants apart. A *starting* value — the
            // plan calls for calibrating it on a cycle with overlapping consumos.
            merchant_threshold: 0.85,
            merchant_gray: 0.70,
            amount_tolerance: Decimal::new(1, 2), // 0.01
            score_epsilon: 0.03,
        }
    }
}

/// What reconciliation decided for a single statement charge.
#[derive(Debug, Clone, PartialEq)]
pub enum ChargeOutcome {
    /// Matched a journal and the amounts agree (within tolerance).
    Confirmed { journal_id: String },
    /// Matched a journal but the amounts differ — the statement's billed figure
    /// is authoritative; Phase 2 may auto-correct, Phase 1 reports + reviews.
    AmountMismatch {
        journal_id: String,
        statement: Decimal,
        booked: Decimal,
    },
    /// No plausible existing journal — a charge the notifications missed. Book it.
    BookNew,
    /// Not confidently decidable (ambiguous, or a gray-zone near-duplicate that
    /// must not be double-booked) → human review.
    Review { reason: String },
}

/// The full per-cycle reconciliation outcome for one account/section.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Reconciliation {
    /// One decision per input charge, paired with its reference.
    pub charges: Vec<(Reference, ChargeOutcome)>,
    /// Journal ids with no matching statement charge — booked from a consumo
    /// that did not post this cycle (released hold / late decline / wrong
    /// cycle). An audit signal → the statement routes to Review.
    pub unmatched_journals: Vec<String>,
}

/// Reconcile one section's **charges** (callers pass `Direction::Out` rows;
/// payments book via the transfer path) against that account's existing
/// journals. Pure and deterministic.
#[must_use]
pub fn reconcile(
    charges: &[StatementTxn],
    journals: &[ExistingJournal],
    params: &ReconcileParams,
) -> Reconciliation {
    let mut taken = vec![false; journals.len()];
    let mut out = Reconciliation::default();

    for charge in charges {
        let outcome = decide(charge, journals, &mut taken, params);
        out.charges.push((charge.reference.clone(), outcome));
    }

    out.unmatched_journals = journals
        .iter()
        .enumerate()
        .filter(|(i, _)| !taken[*i])
        .map(|(_, j)| j.id.clone())
        .collect();
    out
}

/// Decide one charge, marking any consumed journal in `taken`.
fn decide(
    charge: &StatementTxn,
    journals: &[ExistingJournal],
    taken: &mut [bool],
    p: &ReconcileParams,
) -> ChargeOutcome {
    // 1. Exact prior-booking match by our own `bpstmt:<ref>` external_id — a
    //    re-run that already booked this row. Idempotent confirm.
    let want = format!("bpstmt:{}", charge.reference.as_str());
    if let Some(i) = journals
        .iter()
        .enumerate()
        .position(|(i, j)| !taken[i] && j.external_id.as_deref() == Some(want.as_str()))
    {
        taken[i] = true;
        return amount_verdict(charge, &journals[i]);
    }

    // 2. Score every free candidate within the date window; keep those above the
    //    gray floor, best first.
    let mut scored: Vec<(usize, f64)> = journals
        .iter()
        .enumerate()
        .filter(|&(i, j)| !taken[i] && within_window(charge.auth_date, j.date, p.date_window_days))
        .map(|(i, j)| (i, merchant_similarity(&charge.merchant, &j.merchant)))
        .filter(|&(_, s)| s >= p.merchant_gray)
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let Some(&(best_i, best_s)) = scored.first() else {
        // No journal even close → genuinely missing → book it.
        return ChargeOutcome::BookNew;
    };

    // 3. Gray zone: a near-match below the confident threshold. Could be the same
    //    charge described differently — forbid booking a duplicate → Review.
    if best_s < p.merchant_threshold {
        return ChargeOutcome::Review {
            reason: format!(
                "possible duplicate of journal {} (merchant similarity {:.2} below {:.2}); not booked",
                journals[best_i].id, best_s, p.merchant_threshold
            ),
        };
    }

    // 4. Ambiguous: two confident candidates too close to choose between.
    if let Some(&(_, second_s)) = scored.get(1)
        && second_s >= p.merchant_threshold
        && (best_s - second_s) < p.score_epsilon
    {
        return ChargeOutcome::Review {
            reason: format!(
                "ambiguous: {} journals match within {:.2} (top {:.2}/{:.2})",
                scored.iter().filter(|(_, s)| *s >= p.merchant_threshold).count(),
                p.score_epsilon,
                best_s,
                second_s
            ),
        };
    }

    // 5. Confident, unambiguous match.
    taken[best_i] = true;
    amount_verdict(charge, &journals[best_i])
}

/// Confirmed vs AmountMismatch, by the tolerance band.
fn amount_verdict(charge: &StatementTxn, journal: &ExistingJournal) -> ChargeOutcome {
    let statement = charge.money.amount.value();
    let booked = journal.amount;
    if (statement - booked).abs() <= amount_tol(charge) {
        ChargeOutcome::Confirmed { journal_id: journal.id.clone() }
    } else {
        ChargeOutcome::AmountMismatch {
            journal_id: journal.id.clone(),
            statement,
            booked,
        }
    }
}

/// The amount tolerance for a charge. A free function so the (currently
/// constant) policy has one home; kept tiny on purpose.
fn amount_tol(_charge: &StatementTxn) -> Decimal {
    ReconcileParams::default().amount_tolerance
}

/// Merchant similarity in `[0,1]`, on normalised strings (lowercased,
/// whitespace-collapsed). Jaro–Winkler rewards a shared prefix, which fits
/// "JR EAST" vs "JR EAST SIBUYAKU".
fn merchant_similarity(a: &str, b: &str) -> f64 {
    jaro_winkler(&normalize(a), &normalize(b))
}

fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn within_window(a: NaiveDate, b: NaiveDate, days: i64) -> bool {
    (a - b).num_days().abs() <= days
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Amount, Direction, Money};
    use crate::statement::SectionCurrency;

    fn charge(reference: &str, merchant: &str, amount: &str, day: u32) -> StatementTxn {
        let d = NaiveDate::from_ymd_opt(2026, 4, day).unwrap();
        StatementTxn {
            section: SectionCurrency::Usd,
            posting_date: d,
            auth_date: d,
            reference: Reference::parse(reference).unwrap(),
            merchant: merchant.to_string(),
            money: Money::new(Amount::parse(amount).unwrap(), SectionCurrency::Usd.currency()),
            direction: Direction::Out,
            mcc: None,
            auth_code: None,
        }
    }

    fn journal(id: &str, merchant: &str, amount: &str, day: u32) -> ExistingJournal {
        ExistingJournal {
            id: id.to_string(),
            date: NaiveDate::from_ymd_opt(2026, 4, day).unwrap(),
            amount: Decimal::from_str_exact(amount).unwrap(),
            merchant: merchant.to_string(),
            external_id: None,
        }
    }

    #[test]
    fn confirms_close_merchant_same_amount() {
        let charges = [charge("0601324353", "JR EAST SIBUYAKU", "50.93", 21)];
        // Notification booked the same charge, same USD amount, merchant slightly different.
        let journals = [journal("J1", "JR EAST", "50.93", 21)];
        let r = reconcile(&charges, &journals, &ReconcileParams::default());
        assert!(matches!(r.charges[0].1, ChargeOutcome::Confirmed { .. }));
        assert!(r.unmatched_journals.is_empty());
    }

    #[test]
    fn amount_mismatch_when_estimate_differs() {
        // Foreign charge: notification booked an ECB estimate (51.10), statement billed 50.93.
        let charges = [charge("0601324353", "JR EAST SIBUYAKU", "50.93", 21)];
        let journals = [journal("J1", "JR EAST SIBUYAKU", "51.10", 21)];
        let r = reconcile(&charges, &journals, &ReconcileParams::default());
        match &r.charges[0].1 {
            ChargeOutcome::AmountMismatch { journal_id, statement, booked } => {
                assert_eq!(journal_id, "J1");
                assert_eq!(*statement, Decimal::from_str_exact("50.93").unwrap());
                assert_eq!(*booked, Decimal::from_str_exact("51.10").unwrap());
            }
            other => panic!("expected AmountMismatch, got {other:?}"),
        }
    }

    #[test]
    fn unmatched_charge_books_new() {
        let charges = [charge("0601324353", "TOTALLY NEW MERCHANT", "9.99", 10)];
        let journals = [journal("J1", "SOMETHING ELSE", "1.00", 28)];
        let r = reconcile(&charges, &journals, &ReconcileParams::default());
        assert_eq!(r.charges[0].1, ChargeOutcome::BookNew);
        // The journal had no charge → audit signal.
        assert_eq!(r.unmatched_journals, vec!["J1".to_string()]);
    }

    #[test]
    fn gray_zone_near_match_is_reviewed_not_booked() {
        // Similar enough to be suspicious, below the confident threshold → must
        // NOT book a duplicate.
        let charges = [charge("0601324353", "NAGANO DENTETSU", "6.66", 23)];
        let journals = [journal("J1", "NAGANO DENX", "6.66", 23)];
        let p = ReconcileParams { merchant_threshold: 0.99, ..Default::default() };
        let r = reconcile(&charges, &journals, &p);
        assert!(
            matches!(r.charges[0].1, ChargeOutcome::Review { .. }),
            "gray-zone near-match must Review, not BookNew: {:?}",
            r.charges[0].1
        );
        // Not consumed → also surfaces as an unmatched journal (audit).
        assert_eq!(r.unmatched_journals, vec!["J1".to_string()]);
    }

    #[test]
    fn ambiguous_two_equal_candidates_reviewed() {
        // Two identical journals both match the one charge → can't choose.
        let charges = [charge("0601324353", "7-ELEVEN", "7.28", 17)];
        let journals = [
            journal("J1", "7-ELEVEN", "7.28", 17),
            journal("J2", "7-ELEVEN", "7.28", 17),
        ];
        let r = reconcile(&charges, &journals, &ReconcileParams::default());
        assert!(matches!(r.charges[0].1, ChargeOutcome::Review { .. }));
        // Neither consumed.
        assert_eq!(r.unmatched_journals.len(), 2);
    }

    #[test]
    fn prior_bpstmt_booking_confirmed_by_external_id() {
        // A re-run: the row was already booked by a previous statement run.
        let charges = [charge("74987506133002256024229", "7-Eleven B315 Kastrup", "7.28", 17)];
        let journals = [ExistingJournal {
            id: "J9".to_string(),
            date: NaiveDate::from_ymd_opt(2026, 4, 17).unwrap(),
            amount: Decimal::from_str_exact("7.28").unwrap(),
            merchant: "7-Eleven B315 Kastrup".to_string(),
            external_id: Some("bpstmt:74987506133002256024229".to_string()),
        }];
        let r = reconcile(&charges, &journals, &ReconcileParams::default());
        assert_eq!(
            r.charges[0].1,
            ChargeOutcome::Confirmed { journal_id: "J9".to_string() }
        );
    }

    #[test]
    fn date_outside_window_does_not_match() {
        let charges = [charge("0601324353", "JR EAST", "50.93", 1)];
        let journals = [journal("J1", "JR EAST", "50.93", 28)]; // 27 days apart
        let r = reconcile(&charges, &journals, &ReconcileParams::default());
        assert_eq!(r.charges[0].1, ChargeOutcome::BookNew);
        assert_eq!(r.unmatched_journals, vec!["J1".to_string()]);
    }

    #[test]
    fn one_journal_consumed_only_once() {
        // Two distinct charges, one journal: only the better match takes it; the
        // other books new (greedy 1:1, no double-assignment).
        let charges = [
            charge("0601324353", "JR EAST SIBUYAKU", "50.93", 21),
            charge("0601324999", "JR EAST SIBUYAKU", "50.93", 21),
        ];
        let journals = [journal("J1", "JR EAST SIBUYAKU", "50.93", 21)];
        let r = reconcile(&charges, &journals, &ReconcileParams::default());
        let confirmed = r
            .charges
            .iter()
            .filter(|(_, o)| matches!(o, ChargeOutcome::Confirmed { .. }))
            .count();
        // First charge confirms against J1; second has no free journal.
        // (Both charges are identical-merchant, so the second sees J1 taken.)
        assert_eq!(confirmed, 1);
        assert!(r.unmatched_journals.is_empty(), "J1 was consumed exactly once");
    }
}
