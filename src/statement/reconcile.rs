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
//!
//! Resolution is **two-pass and order-independent**: pass 1 scores every charge
//! against every journal (no consumption); pass 2 resolves claims globally so a
//! charge can never be pushed to [`ChargeOutcome::BookNew`] merely because an
//! earlier charge consumed its match (which would double-book). Two charges that
//! contend for one journal route to Review, not to a duplicate booking.

use std::collections::{HashMap, HashSet};

use chrono::NaiveDate;
use rust_decimal::Decimal;
use strsim::jaro_winkler;

use super::{Reference, StatementTxn};
use crate::schema::Money;

/// A `receipt-ledger`-tagged Banco Popular transaction already in Firefly, for a
/// single account, reduced to what matching needs. `amount` is the booked value
/// as a [`Money`] (magnitude + currency), in the account currency — so it is
/// directly comparable to a statement charge in that section, and a currency
/// cross-up is a value mismatch the matcher can detect rather than a silent bug.
#[derive(Debug, Clone, PartialEq)]
pub struct ExistingJournal {
    pub id: String,
    pub date: NaiveDate,
    pub amount: Money,
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
    /// Similarity gap below which two confident options are "too close to call".
    /// Governs **two** comparisons: intra-charge (a charge's two best journals)
    /// and inter-charge (the two strongest claimants of one journal). Both →
    /// Review when within `score_epsilon`. (Kept as one knob deliberately; split
    /// if calibration ever needs them tuned independently.)
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
    /// Not confidently decidable (ambiguous, a gray-zone near-duplicate that must
    /// not be double-booked, or contention with another charge) → human review.
    Review { reason: String },
}

/// The full per-cycle reconciliation outcome for one account/section.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Reconciliation {
    /// One decision per input charge, **in input order** — `charges[i]`
    /// corresponds to the `charges[i]` passed to [`reconcile`], so the caller
    /// zips positionally to recover the full [`StatementTxn`] needed to book a
    /// [`ChargeOutcome::BookNew`]. The [`Reference`] is included for logging.
    pub charges: Vec<(Reference, ChargeOutcome)>,
    /// Journals with no matching statement charge — booked from a consumo that
    /// did not post this cycle (released hold / late decline / wrong cycle). An
    /// audit signal → the statement routes to Review. Carries the full journal so
    /// a reviewer sees merchant/amount/date, not a bare id.
    pub unmatched_journals: Vec<ExistingJournal>,
}

/// Per-charge intent decided in pass 1, before global conflict resolution.
enum Intent {
    /// No plausible journal at all → book.
    Book,
    /// Decided locally (gray-zone guard or ambiguous) → review.
    Review(String),
    /// Confidently wants this journal; contention is resolved in pass 2.
    Claim { journal: usize, score: f64 },
}

/// Reconcile one section's **charges** (callers pass `Direction::Out` rows;
/// payments book via the transfer path) against that account's existing
/// journals. Pure, deterministic, and order-independent.
#[must_use]
pub fn reconcile(
    charges: &[StatementTxn],
    journals: &[ExistingJournal],
    p: &ReconcileParams,
    aliases: &[(String, String)],
) -> Reconciliation {
    debug_assert!(
        p.merchant_gray <= p.merchant_threshold,
        "merchant_gray must not exceed merchant_threshold"
    );
    debug_assert!(p.score_epsilon >= 0.0, "score_epsilon must be non-negative");

    let n = charges.len();
    let m = journals.len();
    let mut outcome: Vec<Option<ChargeOutcome>> = (0..n).map(|_| None).collect();
    let mut taken = vec![false; m];

    // --- 1. exact `bpstmt:<ref>` matches (prior statement booking) ---------
    // References are unique within a statement, so these are unambiguous 1:1.
    for (ci, charge) in charges.iter().enumerate() {
        let want = format!("bpstmt:{}", charge.reference.as_str());
        if let Some(ji) =
            (0..m).find(|&j| !taken[j] && journals[j].external_id.as_deref() == Some(want.as_str()))
        {
            taken[ji] = true;
            outcome[ci] = Some(amount_verdict(charge, &journals[ji], p.amount_tolerance));
        }
    }

    // --- 2a. compute each undecided charge's intent over the FREE journals --
    //         (free = after the exact-`bpstmt` pass above consumed its journals;
    //          no further consumption here — that is what makes 2 order-free)
    let mut intents: Vec<(usize, Intent)> = Vec::new();
    for (ci, charge) in charges.iter().enumerate() {
        if outcome[ci].is_some() {
            continue;
        }
        intents.push((ci, intent_for(charge, journals, &taken, p, aliases)));
    }

    // --- 2b. resolve Claims globally: group by journal, pick a winner -------
    let mut claims: HashMap<usize, Vec<(usize, f64)>> = HashMap::new();
    for (ci, intent) in &intents {
        if let Intent::Claim { journal, score } = intent {
            claims.entry(*journal).or_default().push((*ci, *score));
        }
    }
    let mut winner: HashMap<usize, usize> = HashMap::new(); // charge → journal
    let mut conflict: HashMap<usize, String> = HashMap::new(); // charge → review reason
    let mut book: HashSet<usize> = HashSet::new(); // cluster losers that are genuinely new
    for (ji, claimants) in claims {
        if claimants.len() == 1 {
            winner.insert(claimants[0].0, ji);
            continue;
        }
        // Multiple charges of the same merchant claim one journal (the journal is
        // one charge; the others are separate purchases at the same merchant).
        // Disambiguate by AMOUNT: the claimant whose amount is *clearly* nearest
        // the journal's is the match; the rest are different charges.
        resolve_cluster(
            ji,
            &claimants,
            charges,
            journals,
            p,
            aliases,
            &mut winner,
            &mut conflict,
            &mut book,
        );
    }

    // --- 2c. apply -------------------------------------------------------
    for (ci, intent) in intents {
        let decided = if let Some(&ji) = winner.get(&ci) {
            taken[ji] = true;
            amount_verdict(&charges[ci], &journals[ji], p.amount_tolerance)
        } else if let Some(reason) = conflict.remove(&ci) {
            ChargeOutcome::Review { reason }
        } else if book.contains(&ci) {
            ChargeOutcome::BookNew
        } else {
            match intent {
                Intent::Book => ChargeOutcome::BookNew,
                Intent::Review(reason) => ChargeOutcome::Review { reason },
                Intent::Claim { .. } => {
                    unreachable!("every Claim is resolved into winner/conflict/book")
                }
            }
        };
        outcome[ci] = Some(decided);
    }

    let charges_out = charges
        .iter()
        .zip(outcome)
        // Passes 1–2c are expected to decide every charge; a leftover `None`
        // would be a reconciler logic bug. Rather than panic and sink the whole
        // statement, route the undecided charge to Review so the run continues
        // and a human sees it.
        .map(|(c, o)| {
            let decided = o.unwrap_or_else(|| ChargeOutcome::Review {
                reason: "reconciler left charge undecided (internal invariant violated)"
                    .to_string(),
            });
            (c.reference.clone(), decided)
        })
        .collect();
    let unmatched_journals = journals
        .iter()
        .enumerate()
        .filter(|(i, _)| !taken[*i])
        .map(|(_, j)| j.clone())
        .collect();

    Reconciliation {
        charges: charges_out,
        unmatched_journals,
    }
}

/// Decide one charge's intent against the currently-free journals (pass 1).
fn intent_for(
    charge: &StatementTxn,
    journals: &[ExistingJournal],
    taken: &[bool],
    p: &ReconcileParams,
    aliases: &[(String, String)],
) -> Intent {
    let mut cands: Vec<(usize, f64)> = journals
        .iter()
        .enumerate()
        .filter(|&(i, j)| {
            !taken[i]
                && same_currency(charge, j)
                && within_window(charge.auth_date, j.date, p.date_window_days)
        })
        .map(|(i, j)| {
            (
                i,
                merchant_similarity(&charge.merchant, &j.merchant, aliases),
            )
        })
        .filter(|&(_, s)| s >= p.merchant_gray)
        .collect();
    // Deterministic order: score desc, then journal id asc (so equal-score ties
    // resolve the same way across runs regardless of pagination order).
    cands.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| journals[a.0].id.cmp(&journals[b.0].id))
    });

    let Some(&(best_j, best_s)) = cands.first() else {
        return Intent::Book; // no journal even close → genuinely missing
    };
    if best_s < p.merchant_threshold {
        // Gray-zone near-match: could be the same charge described differently —
        // forbid booking a duplicate.
        return Intent::Review(format!(
            "possible duplicate of journal {} (merchant similarity {:.2} below {:.2}); not booked",
            journals[best_j].id, best_s, p.merchant_threshold
        ));
    }
    if let Some(&(_, second_s)) = cands.get(1)
        && second_s >= p.merchant_threshold
        && (best_s - second_s) < p.score_epsilon
    {
        return Intent::Review(format!(
            "ambiguous: multiple journals match within {:.2} (top {best_s:.2}/{second_s:.2})",
            p.score_epsilon
        ));
    }
    Intent::Claim {
        journal: best_j,
        score: best_s,
    }
}

/// Disambiguate several same-merchant charges that all claim one journal, by
/// amount. The journal is exactly one charge; the claimant whose amount is
/// *clearly* nearest the journal's is that charge (winner), and the rest are
/// separate purchases at the same merchant — booked as new when they have no
/// other plausible journal, else routed to review (the double-book guard). If no
/// claimant is clearly nearest, the whole cluster routes to review.
#[allow(clippy::too_many_arguments)]
fn resolve_cluster(
    journal: usize,
    claimants: &[(usize, f64)],
    charges: &[StatementTxn],
    journals: &[ExistingJournal],
    p: &ReconcileParams,
    aliases: &[(String, String)],
    winner: &mut HashMap<usize, usize>,
    conflict: &mut HashMap<usize, String>,
    book: &mut HashSet<usize>,
) {
    let jamt = journals[journal].amount.amount.value();
    let mut by_dist: Vec<(usize, Decimal)> = claimants
        .iter()
        .map(|&(ci, _)| (ci, (charges[ci].money.amount.value() - jamt).abs()))
        .collect();
    by_dist.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));

    let (near_ci, near_d) = by_dist[0];
    let (_, second_d) = by_dist[1];
    // Strictly within half the runner-up's distance, and a sane fraction of the
    // journal amount (absorbs the ECB-estimate vs billed gap without confirming a
    // far-off amount).
    let clearly_nearest = near_d * Decimal::from(2) < second_d;
    let within_sanity =
        jamt > Decimal::ZERO && near_d * Decimal::from(100) <= jamt * Decimal::from(15);

    if clearly_nearest && within_sanity {
        winner.insert(near_ci, journal);
        for &(ci, _) in claimants.iter().filter(|&&(c, _)| c != near_ci) {
            if has_other_candidate(&charges[ci], journals, journal, p, aliases) {
                conflict.insert(
                    ci,
                    format!(
                        "same-merchant cluster loser with another plausible journal {}; not booked",
                        journals[journal].id
                    ),
                );
            } else {
                book.insert(ci);
            }
        }
    } else {
        for &(ci, _) in claimants {
            conflict.insert(
                ci,
                format!(
                    "multiple charges contend for journal {} (amount-ambiguous); not booked",
                    journals[journal].id
                ),
            );
        }
    }
}

/// Whether `charge` has *another* plausible journal besides `exclude` (same
/// currency, within the date window, merchant similarity ≥ gray) — so a cluster
/// loser is only booked-new when nothing else could be its duplicate.
fn has_other_candidate(
    charge: &StatementTxn,
    journals: &[ExistingJournal],
    exclude: usize,
    p: &ReconcileParams,
    aliases: &[(String, String)],
) -> bool {
    journals.iter().enumerate().any(|(i, j)| {
        i != exclude
            && same_currency(charge, j)
            && within_window(charge.auth_date, j.date, p.date_window_days)
            && merchant_similarity(&charge.merchant, &j.merchant, aliases) >= p.merchant_gray
    })
}

/// Symmetric double-book probe (consumo side, Phase 2): does any **statement
/// booking** (`bpstmt:`-keyed journal) in `journals` plausibly already represent
/// a charge with this `merchant` and authorization `date`? Returns the first such
/// journal.
///
/// Uses the matcher's own merchant + date-window signals (similarity ≥
/// `merchant_gray`, within `date_window_days`) — the same conservatism as the
/// forward guard. Two deliberate choices:
/// - **`bpstmt:` only.** A prior *consumo* journal (`composite_hash`) is NOT a
///   double-book risk (an identical re-send dedups; a genuine second charge is
///   new), so restricting to statement bookings is what makes this a no-op in the
///   normal consumo-first flow — it fires only once a statement has booked ahead.
/// - **Amount is not compared.** A foreign charge's consumo estimate and the
///   statement's billed figure legitimately differ (§2.1), and the consumo's
///   currency here is its *original* (pre-FX) currency — not the account currency
///   the journal carries — so an amount/currency equality test would miss real
///   duplicates. Merchant + date is the reliable cross-source signal; the cost is
///   that a recurring same-merchant charge within the window is conservatively
///   sent to Review rather than double-booked (fail-closed, by design).
///
/// Pure — no I/O; the caller supplies the in-window journals.
#[must_use]
pub fn statement_already_booked<'j>(
    merchant: &str,
    date: NaiveDate,
    journals: &'j [ExistingJournal],
    p: &ReconcileParams,
    aliases: &[(String, String)],
) -> Option<&'j ExistingJournal> {
    journals.iter().find(|j| {
        j.external_id
            .as_deref()
            .is_some_and(|e| e.starts_with("bpstmt:"))
            && within_window(date, j.date, p.date_window_days)
            && merchant_similarity(merchant, &j.merchant, aliases) >= p.merchant_gray
    })
}

/// Confirmed vs AmountMismatch, by the tolerance band (from [`ReconcileParams`]).
fn amount_verdict(
    charge: &StatementTxn,
    journal: &ExistingJournal,
    tolerance: Decimal,
) -> ChargeOutcome {
    let statement = charge.money.amount.value();
    let booked = journal.amount.amount.value();
    if (statement - booked).abs() <= tolerance {
        ChargeOutcome::Confirmed {
            journal_id: journal.id.clone(),
        }
    } else {
        ChargeOutcome::AmountMismatch {
            journal_id: journal.id.clone(),
            statement,
            booked,
        }
    }
}

/// Whether the charge and journal are denominated in the same currency. A
/// matcher only ever compares same-currency amounts (the caller scopes journals
/// per account, but this makes the invariant explicit rather than assumed).
fn same_currency(charge: &StatementTxn, journal: &ExistingJournal) -> bool {
    charge.money.currency == journal.amount.currency
}

/// Merchant similarity in `[0,1]`. Both names are first **canonicalized** via the
/// alias map (the same Firefly `description_contains → set destination account`
/// rules, applied here to both sides before scoring), then scored as the **max**
/// of Jaro–Winkler (good for shared prefixes / typos) and asymmetric
/// token-containment (good when one name is contained in the other, e.g.
/// "jompeame" inside "donacion jompeame jompeame.com" — which Jaro–Winkler alone
/// scores too low).
fn merchant_similarity(a: &str, b: &str, aliases: &[(String, String)]) -> f64 {
    let na = normalize_merchant(&canonicalize(a, aliases));
    let nb = normalize_merchant(&canonicalize(b, aliases));
    jaro_winkler(&na, &nb).max(token_containment(&na, &nb))
}

/// Apply the merchant alias map: if `name` (case-insensitive) contains a known
/// trigger substring, return its canonical form. The map mirrors a Firefly
/// rule-group, so the canonicalization Firefly applies to bookings is applied
/// here to *both* the statement merchant and the journal before matching.
/// Triggers are expected pre-lowercased.
fn canonicalize(name: &str, aliases: &[(String, String)]) -> String {
    let lower = name.to_lowercase();
    for (trigger, canonical) in aliases {
        if !trigger.is_empty() && lower.contains(trigger.as_str()) {
            return canonical.clone();
        }
    }
    name.to_string()
}

/// Asymmetric token overlap: the fraction of the *shorter* name's tokens present
/// in the other. 1.0 when one name's tokens are a subset of the other's.
fn token_containment(a: &str, b: &str) -> f64 {
    let ta = tokens(a);
    let tb = tokens(b);
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let (small, big) = if ta.len() <= tb.len() {
        (&ta, &tb)
    } else {
        (&tb, &ta)
    };
    let hits = small.iter().filter(|t| big.contains(*t)).count();
    hits as f64 / small.len() as f64
}

/// Split into alphanumeric tokens (≥2 chars), lowercased — drops punctuation and
/// single-char noise.
fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(str::to_lowercase)
        .collect()
}

/// Normalise a merchant string for comparison. Distinct from
/// [`crate::validate`]'s status normaliser (which also collapses punctuation);
/// kept separate on purpose — different domain, different rules.
fn normalize_merchant(s: &str) -> String {
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
    use crate::schema::{Amount, Currency, Direction, Money};
    use crate::statement::SectionCurrency;

    fn charge(reference: &str, merchant: &str, amount: &str, day: u32) -> StatementTxn {
        let d = NaiveDate::from_ymd_opt(2026, 4, day).unwrap();
        StatementTxn {
            section: SectionCurrency::Usd,
            posting_date: d,
            auth_date: d,
            reference: Reference::parse(reference).unwrap(),
            merchant: merchant.to_string(),
            money: Money::new(
                Amount::parse(amount).unwrap(),
                SectionCurrency::Usd.currency(),
            ),
            direction: Direction::Out,
            mcc: None,
            auth_code: None,
        }
    }

    fn usd(amount: &str) -> Money {
        Money::new(
            Amount::parse(amount).unwrap(),
            Currency::parse("USD").unwrap(),
        )
    }

    fn journal(id: &str, merchant: &str, amount: &str, day: u32) -> ExistingJournal {
        ExistingJournal {
            id: id.to_string(),
            date: NaiveDate::from_ymd_opt(2026, 4, day).unwrap(),
            amount: usd(amount),
            merchant: merchant.to_string(),
            external_id: None,
        }
    }

    // --- Phase-2 (d): symmetric double-book probe (consumo side) -----------

    #[test]
    fn double_book_probe_flags_only_statement_bookings_in_window() {
        let p = ReconcileParams::default();
        let day = NaiveDate::from_ymd_opt(2026, 4, 21).unwrap();
        let stmt = ExistingJournal {
            external_id: Some("bpstmt:74542016112077674827117".to_string()),
            ..journal("1001", "JR EAST SIBUYAKU", "33.33", 21)
        };

        // A consumo "JR EAST" on the same day matches the statement booking
        // (token containment) → flagged (would double-book).
        assert!(
            statement_already_booked("JR EAST", day, std::slice::from_ref(&stmt), &p, &[])
                .is_some()
        );

        // A CONSUMO-booked journal (non-`bpstmt:` external_id, or none) is NOT a
        // double-book risk — the probe ignores it.
        let consumo = ExistingJournal {
            external_id: Some("8XY12345AB".to_string()),
            ..stmt.clone()
        };
        assert!(
            statement_already_booked("JR EAST", day, std::slice::from_ref(&consumo), &p, &[])
                .is_none()
        );
        let untagged = ExistingJournal {
            external_id: None,
            ..stmt.clone()
        };
        assert!(
            statement_already_booked("JR EAST", day, std::slice::from_ref(&untagged), &p, &[])
                .is_none()
        );

        // Outside the date window (9 days > W=5) → no match.
        let far = NaiveDate::from_ymd_opt(2026, 4, 30).unwrap();
        assert!(
            statement_already_booked("JR EAST", far, std::slice::from_ref(&stmt), &p, &[])
                .is_none()
        );

        // A different merchant → no match.
        assert!(
            statement_already_booked("STARBUCKS TOKYO", day, std::slice::from_ref(&stmt), &p, &[])
                .is_none()
        );
    }

    /// Ids of the outcomes, in order, as a coarse fingerprint for permutation tests.
    fn kinds(r: &Reconciliation) -> Vec<&'static str> {
        r.charges
            .iter()
            .map(|(_, o)| match o {
                ChargeOutcome::Confirmed { .. } => "confirmed",
                ChargeOutcome::AmountMismatch { .. } => "mismatch",
                ChargeOutcome::BookNew => "book",
                ChargeOutcome::Review { .. } => "review",
            })
            .collect()
    }

    #[test]
    fn confirms_close_merchant_same_amount() {
        let charges = [charge("0601324353", "JR EAST SIBUYAKU", "50.93", 21)];
        let journals = [journal("J1", "JR EAST", "50.93", 21)];
        let r = reconcile(&charges, &journals, &ReconcileParams::default(), &[]);
        assert!(matches!(r.charges[0].1, ChargeOutcome::Confirmed { .. }));
        assert!(r.unmatched_journals.is_empty());
    }

    #[test]
    fn amount_mismatch_when_estimate_differs() {
        let charges = [charge("0601324353", "JR EAST SIBUYAKU", "50.93", 21)];
        let journals = [journal("J1", "JR EAST SIBUYAKU", "51.10", 21)];
        let r = reconcile(&charges, &journals, &ReconcileParams::default(), &[]);
        match &r.charges[0].1 {
            ChargeOutcome::AmountMismatch {
                journal_id,
                statement,
                booked,
            } => {
                assert_eq!(journal_id, "J1");
                assert_eq!(*statement, Decimal::from_str_exact("50.93").unwrap());
                assert_eq!(*booked, Decimal::from_str_exact("51.10").unwrap());
            }
            other => panic!("expected AmountMismatch, got {other:?}"),
        }
    }

    #[test]
    fn amount_tolerance_param_is_honored() {
        // H2 regression: a custom tolerance must actually change the verdict.
        let charges = [charge("0601324353", "JR EAST SIBUYAKU", "50.93", 21)];
        let journals = [journal("J1", "JR EAST SIBUYAKU", "51.10", 21)];
        let wide = ReconcileParams {
            amount_tolerance: Decimal::new(50, 2),
            ..Default::default()
        }; // 0.50
        let r = reconcile(&charges, &journals, &wide, &[]);
        assert!(
            matches!(r.charges[0].1, ChargeOutcome::Confirmed { .. }),
            "a 0.50 tolerance should absorb a 0.17 gap → Confirmed, got {:?}",
            r.charges[0].1
        );
    }

    #[test]
    fn unmatched_charge_books_new() {
        let charges = [charge("0601324353", "TOTALLY NEW MERCHANT", "9.99", 10)];
        let journals = [journal("J1", "SOMETHING ELSE", "1.00", 28)];
        let r = reconcile(&charges, &journals, &ReconcileParams::default(), &[]);
        assert_eq!(r.charges[0].1, ChargeOutcome::BookNew);
        assert_eq!(r.unmatched_journals.len(), 1);
        assert_eq!(r.unmatched_journals[0].id, "J1");
    }

    #[test]
    fn gray_zone_near_match_is_reviewed_not_booked() {
        let charges = [charge("0601324353", "NAGANO DENTETSU", "6.66", 23)];
        let journals = [journal("J1", "NAGANO DENX", "6.66", 23)];
        let p = ReconcileParams {
            merchant_threshold: 0.99,
            ..Default::default()
        };
        let r = reconcile(&charges, &journals, &p, &[]);
        assert!(matches!(r.charges[0].1, ChargeOutcome::Review { .. }));
        assert_eq!(r.unmatched_journals.len(), 1);
    }

    #[test]
    fn ambiguous_two_equal_candidates_reviewed() {
        let charges = [charge("0601324353", "7-ELEVEN", "7.28", 17)];
        let journals = [
            journal("J1", "7-ELEVEN", "7.28", 17),
            journal("J2", "7-ELEVEN", "7.28", 17),
        ];
        let r = reconcile(&charges, &journals, &ReconcileParams::default(), &[]);
        assert!(matches!(r.charges[0].1, ChargeOutcome::Review { .. }));
        assert_eq!(r.unmatched_journals.len(), 2);
    }

    #[test]
    fn prior_bpstmt_booking_confirmed_by_external_id() {
        let charges = [charge(
            "74987506133002256024229",
            "7-Eleven B315 Kastrup",
            "7.28",
            17,
        )];
        let journals = [ExistingJournal {
            id: "J9".to_string(),
            date: NaiveDate::from_ymd_opt(2026, 4, 17).unwrap(),
            amount: usd("7.28"),
            merchant: "7-Eleven B315 Kastrup".to_string(),
            external_id: Some("bpstmt:74987506133002256024229".to_string()),
        }];
        let r = reconcile(&charges, &journals, &ReconcileParams::default(), &[]);
        assert_eq!(
            r.charges[0].1,
            ChargeOutcome::Confirmed {
                journal_id: "J9".to_string()
            }
        );
    }

    #[test]
    fn date_outside_window_does_not_match() {
        let charges = [charge("0601324353", "JR EAST", "50.93", 1)];
        let journals = [journal("J1", "JR EAST", "50.93", 28)];
        let r = reconcile(&charges, &journals, &ReconcileParams::default(), &[]);
        assert_eq!(r.charges[0].1, ChargeOutcome::BookNew);
    }

    #[test]
    fn different_currency_does_not_match() {
        let charges = [charge("0601324353", "JR EAST", "50.93", 21)];
        let dop = ExistingJournal {
            id: "J1".into(),
            date: NaiveDate::from_ymd_opt(2026, 4, 21).unwrap(),
            amount: Money::new(
                Amount::parse("50.93").unwrap(),
                Currency::parse("DOP").unwrap(),
            ),
            merchant: "JR EAST".into(),
            external_id: None,
        };
        let r = reconcile(&charges, &[dop], &ReconcileParams::default(), &[]);
        assert_eq!(
            r.charges[0].1,
            ChargeOutcome::BookNew,
            "USD charge must not match a DOP journal"
        );
    }

    /// H1 regression: two charges both match one journal; whichever is processed
    /// second must NOT fall through to BookNew (a double-book). Test both orders.
    #[test]
    fn two_charges_one_journal_loser_reviews_not_books() {
        let journals = [journal("J1", "JR EAST SIBUYAKU", "50.93", 21)];
        // A is the better match (exact merchant); B is also confident but weaker.
        let a = charge("0601000001", "JR EAST SIBUYAKU", "50.93", 21);
        let b = charge("0601000002", "JR EAST SIBUYAKU TOKYO", "50.93", 21);

        for order in [[a.clone(), b.clone()], [b.clone(), a.clone()]] {
            let r = reconcile(&order, &journals, &ReconcileParams::default(), &[]);
            let books = kinds(&r).iter().filter(|k| **k == "book").count();
            assert_eq!(
                books,
                0,
                "no charge may BookNew against a contended journal: {:?}",
                kinds(&r)
            );
            // Exactly one confirmed (the winner) or both reviewed; never a dup booking.
            let confirmed = kinds(&r).iter().filter(|k| **k == "confirmed").count();
            assert!(confirmed <= 1);
        }
    }

    /// Contention safety: when several similar charges and journals all match
    /// each other (token-containment saturates), the matcher cannot confidently
    /// assign, so it routes to Review — the one invariant that matters for money
    /// is that **no charge BookNews** (which would double-book) and no journal is
    /// consumed twice. Pinned so it can't silently drift.
    #[test]
    fn contended_charges_review_never_book() {
        let journals = [
            journal("J1", "JR EAST SIBUYAKU", "50.93", 21),
            journal("J2", "JR EAST", "50.93", 21),
        ];
        let a = charge("0601000001", "JR EAST SIBUYAKU", "50.93", 21);
        let b = charge("0601000002", "JR EAST SIBUYAKU TOKYO", "50.93", 21);
        let r = reconcile(&[a, b], &journals, &ReconcileParams::default(), &[]);

        assert!(
            !kinds(&r).contains(&"book"),
            "contended charges must not BookNew: {:?}",
            kinds(&r)
        );
        assert!(matches!(r.charges[0].1, ChargeOutcome::Review { .. }));
        assert!(matches!(r.charges[1].1, ChargeOutcome::Review { .. }));
    }

    /// Permutation invariance: the multiset of outcomes is independent of input
    /// order (the property that makes the greedy double-book impossible).
    #[test]
    fn outcomes_are_permutation_invariant() {
        let journals = [
            journal("J1", "JR EAST SIBUYAKU", "50.93", 21),
            journal("J2", "NAGANO DENTETSU", "6.66", 23),
        ];
        let c1 = charge("0601000001", "JR EAST SIBUYAKU", "50.93", 21);
        let c2 = charge("0601000002", "NAGANO DENTETSU", "6.66", 23);
        let c3 = charge("0601000003", "BRAND NEW CAFE", "3.00", 19);

        let mut sorted_a = {
            let r = reconcile(
                &[c1.clone(), c2.clone(), c3.clone()],
                &journals,
                &ReconcileParams::default(),
                &[],
            );
            let mut k = kinds(&r);
            k.sort_unstable();
            k
        };
        let sorted_b = {
            let r = reconcile(&[c3, c1, c2], &journals, &ReconcileParams::default(), &[]);
            let mut k = kinds(&r);
            k.sort_unstable();
            k
        };
        sorted_a.sort_unstable();
        assert_eq!(sorted_a, sorted_b);
    }

    // --- merchant matching improvements (point 1) --------------------------

    #[test]
    fn token_containment_basics() {
        assert_eq!(
            token_containment("jompeame", "donacion jompeame jompeame.com"),
            1.0
        );
        assert_eq!(
            token_containment("nagano dentetsu", "nagano dentetsu nagano"),
            1.0
        );
        assert_eq!(
            token_containment("starbuckskorea", "starbuckscoffee tokyo"),
            0.0
        );
        assert_eq!(token_containment("", "anything"), 0.0);
    }

    #[test]
    fn canonicalize_via_alias_map() {
        let aliases = vec![("jompeame".to_string(), "Jompeame".to_string())];
        assert_eq!(
            canonicalize("DONACION JOMPEAME JOMPEAME.COM", &aliases),
            "Jompeame"
        );
        assert_eq!(canonicalize("JOMPEAME", &aliases), "Jompeame");
        assert_eq!(canonicalize("Unrelated Cafe", &aliases), "Unrelated Cafe");
    }

    /// The dry-run's real false-negative: a consumo `JOMPEAME` journal vs the
    /// statement's `DONACION JOMPEAME JOMPEAME.COM`. Jaro-Winkler alone scored it
    /// too low → BookNew (a double-book). Token-containment now confirms it even
    /// with NO alias rule configured.
    #[test]
    fn contained_merchant_confirms_without_alias() {
        let charges = [charge(
            "0601324353",
            "DONACION JOMPEAME JOMPEAME.COM",
            "1000.00",
            24,
        )];
        let journals = [journal("J1", "JOMPEAME", "1000.00", 24)];
        let r = reconcile(&charges, &journals, &ReconcileParams::default(), &[]);
        assert!(
            matches!(r.charges[0].1, ChargeOutcome::Confirmed { .. }),
            "containment should confirm, not BookNew: {:?}",
            r.charges[0].1
        );
        assert!(r.unmatched_journals.is_empty());
    }

    /// The real NAGANO case: 3 same-merchant statement rows claim 1 consumo
    /// journal (an ECB estimate near the 33.33 billed). Amount disambiguates:
    /// the 33.33 row matches the journal (amount differs → mismatch, to correct),
    /// the other two are separate purchases → booked new. Previously all 3 → Review.
    #[test]
    fn cluster_disambiguates_by_amount() {
        let journals = [journal("J964", "NAGANO DENTETSU", "32.27", 21)];
        let charges = [
            charge("0601000001", "NAGANO DENTETSU NAGANO", "33.33", 21),
            charge("0601000002", "NAGANO DENTETSU NAGANO", "6.66", 23),
            charge("0601000003", "NAGANO DENTETSU NAGANO", "28.45", 22),
        ];
        let r = reconcile(&charges, &journals, &ReconcileParams::default(), &[]);
        assert!(
            matches!(r.charges[0].1, ChargeOutcome::AmountMismatch { .. }),
            "nearest-amount row matches the journal: {:?}",
            r.charges[0].1
        );
        assert_eq!(r.charges[1].1, ChargeOutcome::BookNew);
        assert_eq!(r.charges[2].1, ChargeOutcome::BookNew);
        assert!(
            r.unmatched_journals.is_empty(),
            "journal consumed by the amount match"
        );
    }

    /// Cluster where two charges are equidistant from the journal — no clear
    /// nearest → safe fallback to Review (never an arbitrary confirm/book).
    #[test]
    fn cluster_amount_ambiguous_all_review() {
        let journals = [journal("J1", "CAFE MOCHA", "10.00", 21)];
        let charges = [
            charge("0601000001", "CAFE MOCHA TOKYO", "9.00", 21),
            charge("0601000002", "CAFE MOCHA TOKYO", "11.00", 21),
        ];
        let r = reconcile(&charges, &journals, &ReconcileParams::default(), &[]);
        assert!(matches!(r.charges[0].1, ChargeOutcome::Review { .. }));
        assert!(matches!(r.charges[1].1, ChargeOutcome::Review { .. }));
        assert!(
            !kinds(&r).contains(&"book"),
            "ambiguous cluster never books"
        );
    }

    /// With an alias rule, even names that share *no* tokens canonicalize to the
    /// same payee and match exactly.
    #[test]
    fn alias_map_bridges_disjoint_names() {
        let aliases = vec![
            ("youtube".to_string(), "Google".to_string()),
            ("goog".to_string(), "Google".to_string()),
        ];
        let charges = [charge("0601324353", "GOOG*YOUTUBEPREMIUM", "11.99", 21)];
        let journals = [journal("J1", "YOUTUBE", "11.99", 21)];
        let r = reconcile(&charges, &journals, &ReconcileParams::default(), &aliases);
        assert!(
            matches!(r.charges[0].1, ChargeOutcome::Confirmed { .. }),
            "alias canonicalization should confirm: {:?}",
            r.charges[0].1
        );
    }
}
