//! Statement ingestion pipeline: detect → decrypt → parse → reconcile → book.
//!
//! This is the I/O glue that joins the pure pieces ([`super::pdf`],
//! [`super::parse`], [`super::reconcile`]) to the live mailbox + Firefly. It is
//! invoked from [`crate::run`] for messages [`classify_message`] tags as a
//! statement (a PDF attachment from the configured forwarder / a `Cuenta:`
//! subject), and returns a [`StatementReport`] the caller turns into a
//! Processed/Review disposition.
//!
//! Money safety is unchanged from the notification path: charges book through
//! `to_extracted` → [`crate::validate::validate`] → the USD ceiling →
//! `firefly.submit`; payments through [`crate::validate::validate_transfer`] →
//! `firefly.submit_transfer`. Anything not confidently bookable counts toward
//! review, and any flag (mismatch, unmatched journal, review) sends the whole
//! statement to the Review mailbox.

use anyhow::{Result, anyhow};
use chrono::Duration;
use rust_decimal::Decimal;
use tracing::{Instrument, debug, info, warn};

use super::reconcile::{ChargeOutcome, ReconcileParams, reconcile};
use super::{SectionCurrency, StatementTxn, parse, pdf};
use crate::config::Config;
use crate::firefly::{CorrectionOutcome, FireflyClient, SubmitOutcome, correction_decision};
use crate::fx::FxClient;
use crate::jmap::{FetchedMessage, Mailbox};
use crate::schema::Direction;
use crate::validate::{TransferVerdict, Verdict, validate, validate_transfer};

/// How a message should be ingested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ingest {
    /// A per-transaction notification (the existing adapter path).
    Notification,
    /// A monthly statement PDF (this module).
    Statement,
}

/// Classify a message by attachment + sender/subject. Pure. A statement is a
/// message carrying a PDF attachment that also looks like a Banco Popular
/// statement — either forwarded by the configured `sender_hint`, or with a
/// `Cuenta:`/`Estado de Cuenta` subject. Everything else is a notification.
#[must_use]
pub fn classify_message(msg: &FetchedMessage, sender_hint: Option<&str>) -> Ingest {
    let has_pdf = msg.attachments.iter().any(crate::jmap::Attachment::is_pdf);
    if !has_pdf {
        return Ingest::Notification;
    }
    let subject = msg.subject.as_deref().unwrap_or("").to_lowercase();
    let subject_marks_statement = subject.contains("cuenta"); // "Cuenta:" / "Estado de Cuenta"
    let from_matches = match (sender_hint, msg.from.as_deref()) {
        (Some(hint), Some(from)) => from.to_lowercase().contains(&hint.to_lowercase()),
        _ => false,
    };
    if subject_marks_statement || from_matches {
        Ingest::Statement
    } else {
        Ingest::Notification
    }
}

/// Tallies for one processed statement.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StatementReport {
    /// Charges already booked from a consumo (or a prior statement run) and
    /// confirmed — incl. Firefly duplicate responses on re-book.
    pub reconciled: usize,
    /// Matched but the statement amount differs from the booked estimate, and it
    /// was NOT auto-corrected — reported + routed to Review (Phase-1 behavior, or
    /// a Phase-2 correction blocked by the TOCTOU / bounded-delta guard).
    pub amount_mismatch: usize,
    /// Phase-2 amount auto-corrections applied (or already-correct no-ops) — the
    /// journal's booked estimate was rewritten to the statement's billed figure.
    /// A clean outcome (does not keep the statement out of Processed).
    pub corrected: usize,
    /// Charges the notifications missed, newly booked from the statement.
    pub booked_new: usize,
    /// Payments booked as transfers (paying account → card).
    pub payments_booked: usize,
    /// Journals in the window with no matching statement charge (audit signal).
    pub unmatched_booked: usize,
    /// Sections whose `BALANCE ANTERIOR + Σcharges − Σpayments` did not reconcile
    /// to `BALANCE TOTAL` within tolerance (a parse-completeness / unmodeled
    /// fee-or-interest signal).
    pub balance_mismatch: usize,
    /// Rows that could not be confidently handled → human review.
    pub review: usize,
    /// Rows skipped this run because the rate provider was transiently down (5xx
    /// / network). The row is NOT booked and NOT a review; the caller leaves the
    /// whole message in INBOX so the next run retries it (already-booked rows
    /// dedup via their `bpstmt:` external_id).
    pub deferred: usize,
    /// Correctness signal: the worst-case closing-balance delta across the
    /// statement's sections (`|BALANCE ANTERIOR + Σcharges − Σpayments − BALANCE
    /// TOTAL|`). `None` means the check did not run (a section lacked the balances)
    /// — distinct from `Some(0)` (checked and reconciled), so a "booked but doesn't
    /// reconcile" alert never mistakes "not checked" for "fine".
    pub balance_delta: Option<Decimal>,
}

impl StatementReport {
    /// Whether the statement is fully clean (→ Processed). Any mismatch,
    /// unmatched journal, balance discrepancy, review, or deferral keeps it out of
    /// Processed. (A deferral is handled before this is consulted — it holds the
    /// message in INBOX — but is included here for safety.)
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.amount_mismatch == 0
            && self.unmatched_booked == 0
            && self.balance_mismatch == 0
            && self.review == 0
            && self.deferred == 0
    }
}

/// Decrypt, parse, reconcile, and book one statement message. A fatal error
/// (no password, no PDF, decrypt/parse failure) is returned as `Err` so the
/// caller routes the whole message to Review; per-row problems are counted in
/// the [`StatementReport`] and the run continues.
///
/// Instrumented as a `statement` child span of the run, with `decrypt`, `parse`,
/// and `reconcile_book` sub-spans for the stages. **No-PII span allowlist:** the
/// only span attributes are `stage` and section counts — never a merchant,
/// amount, last-4, reference, or any statement content.
#[tracing::instrument(name = "statement", skip_all, fields(stage = "statement"))]
pub async fn process_statement(
    msg: &FetchedMessage,
    mailbox: &Mailbox,
    firefly: &FireflyClient<'_>,
    fx: &FxClient<'_>,
    cfg: &Config,
) -> Result<StatementReport> {
    let password = cfg.bp_statement_password.as_deref().ok_or_else(|| {
        anyhow!("statement password (RECEIPT_BP_STATEMENT_PASSWORD) not configured")
    })?;
    let pdf_att = msg
        .attachments
        .iter()
        .find(|a| a.is_pdf())
        .ok_or_else(|| anyhow!("message classified as statement but has no PDF attachment"))?;

    // `decrypt` stage: download + RC4-decrypt + positioned-text extraction.
    let bytes = mailbox
        .download(&pdf_att.blob_id)
        .instrument(tracing::info_span!("decrypt", stage = "decrypt"))
        .await?;
    let rows = {
        let _g = tracing::info_span!("decrypt", stage = "decrypt").entered();
        pdf::extract_rows(bytes, password)?
    };
    // `parse` stage: reassemble columns into typed statement rows (pure, sync).
    let parsed = {
        let _g = tracing::info_span!("parse", stage = "parse").entered();
        parse::parse_statement(&rows)?
    };
    info!(
        sections = parsed.sections.len(),
        txns = parsed.txns.len(),
        "parsed statement"
    );

    let mut report = StatementReport::default();
    let params = ReconcileParams::default();

    // Merchant alias map from a Firefly rule-group (canonicalizes both sides
    // before fuzzy matching). Missing/failed lookup degrades to no aliases.
    let aliases = match &cfg.bp_alias_rule_group {
        Some(group) => firefly.fetch_alias_map(group).await.unwrap_or_else(|e| {
            warn!(error = ?e, "alias map fetch failed; proceeding without aliases");
            Vec::new()
        }),
        None => Vec::new(),
    };

    // `reconcile_book` stage: per-section reconcile of statement rows against
    // booked journals, then book/correct/transfer. Wrapped in an instrumented
    // async block so the whole stage is one child span. `?` inside propagates a
    // fatal Firefly error out to the caller (→ Review) exactly as before.
    async {
    for section in &parsed.sections {
        let Some(account) = (match section.currency {
            SectionCurrency::Usd => cfg.banco_popular_usd_account.as_ref(),
            SectionCurrency::Dop => cfg.banco_popular_dop_account.as_ref(),
        }) else {
            let rows = parsed
                .txns
                .iter()
                .filter(|t| t.section == section.currency)
                .count();
            warn!(currency = ?section.currency, rows, "no card account configured; section → review");
            report.review += rows;
            continue;
        };

        // Pull this account's prior bookings over the cycle window (auth dates
        // precede the cut by up to ~a month; pad both ends).
        let start = section.cut_date - Duration::days(45);
        let end = section.cut_date + Duration::days(5);
        let journals = firefly.list_transactions(account, start, end).await?;

        let charges: Vec<StatementTxn> = parsed
            .txns
            .iter()
            .filter(|t| t.section == section.currency && t.direction == Direction::Out)
            .cloned()
            .collect();
        let payments: Vec<&StatementTxn> = parsed
            .txns
            .iter()
            .filter(|t| t.section == section.currency && t.direction == Direction::In)
            .collect();

        let recon = reconcile(&charges, &journals, &params, &aliases);
        report.unmatched_booked += recon.unmatched_journals.len();
        for j in &recon.unmatched_journals {
            // `merchant` is PII — gate it on RECEIPT_LOG_PII (the journal id is a
            // non-PII Firefly handle, always safe).
            if cfg.log_pii {
                info!(
                    journal = j.id,
                    merchant = j.merchant,
                    "unmatched Firefly journal (audit) → review"
                );
            } else {
                info!(journal = j.id, "unmatched Firefly journal (audit) → review");
            }
        }

        // recon.charges is positional with `charges`.
        for ((_, outcome), charge) in recon.charges.iter().zip(&charges) {
            // Per-row financial detail (merchant + amount) is PII — emit it only
            // when RECEIPT_LOG_PII is on (independent of dry-run / RUST_LOG), so a
            // misconfigured prod run can't ship it to Loki. Off → reference + the
            // bare outcome kind only (no merchant, no amount). The dry-run/`info!`
            // path is gated too (the operator opts into PII to observe a cycle).
            if cfg.log_pii {
                let line = format!(
                    "{} | {} {} | {:?}",
                    charge.reference.as_str(),
                    charge.money.amount.value().normalize(),
                    charge.money.currency.as_str(),
                    outcome
                );
                if cfg.dry_run {
                    info!(merchant = charge.merchant, plan = %line, "charge plan");
                } else {
                    debug!(merchant = charge.merchant, plan = %line, "charge");
                }
            } else {
                let kind = charge_outcome_kind(outcome);
                if cfg.dry_run {
                    info!(
                        reference = charge.reference.as_str(),
                        outcome = kind,
                        "charge plan"
                    );
                } else {
                    debug!(
                        reference = charge.reference.as_str(),
                        outcome = kind,
                        "charge"
                    );
                }
            }

            match outcome {
                ChargeOutcome::Confirmed { .. } => report.reconciled += 1,
                ChargeOutcome::AmountMismatch {
                    journal_id,
                    statement,
                    booked,
                } => {
                    correct_amount_mismatch(
                        journal_id,
                        *booked,
                        *statement,
                        firefly,
                        cfg,
                        &mut report,
                    )
                    .await;
                }
                ChargeOutcome::Review { .. } => report.review += 1,
                ChargeOutcome::BookNew => {
                    book_charge(charge, section, firefly, fx, cfg, &mut report).await;
                }
            }
        }

        for &payment in &payments {
            book_payment(payment, firefly, fx, cfg, &mut report).await;
        }

        check_balance(section, &charges, &payments, &mut report);
    }
    Ok::<(), anyhow::Error>(())
    }
    .instrument(tracing::info_span!("reconcile_book", stage = "reconcile_book"))
    .await?;

    // Canonical structured statement event — explicit named fields (NOT `?report`,
    // which under JSON logging serializes as one opaque Debug string LogQL can't
    // field-query). `balance_checked` distinguishes Some(0) from "not checked".
    let balance_delta_str = report
        .balance_delta
        .map(|d| d.normalize().to_string())
        .unwrap_or_else(|| "absent".to_string());
    info!(
        reconciled = report.reconciled,
        booked_new = report.booked_new,
        payments_booked = report.payments_booked,
        amount_mismatch = report.amount_mismatch,
        corrected = report.corrected,
        unmatched_booked = report.unmatched_booked,
        balance_mismatch = report.balance_mismatch,
        deferred = report.deferred,
        review = report.review,
        balance_checked = report.balance_delta.is_some(),
        balance_delta = %balance_delta_str,
        "statement reconciliation complete"
    );
    Ok(report)
}

/// Closing-balance internal consistency: `BALANCE ANTERIOR + Σcharges −
/// Σpayments` should reconcile to `BALANCE TOTAL`. Logs the full breakdown
/// (always — it's the most useful dry-run signal) and flags a mismatch beyond
/// tolerance. Note: interest/fee rows are not yet modeled, so a non-zero delta
/// is a *signal to read*, not necessarily a parse bug. Skipped when either
/// balance is absent.
fn check_balance(
    section: &super::Section,
    charges: &[StatementTxn],
    payments: &[&StatementTxn],
    report: &mut StatementReport,
) {
    let (Some(anterior), Some(stated)) = (section.balance_anterior, section.balance_total) else {
        debug!(currency = ?section.currency, "closing-balance check skipped (missing anterior/total)");
        return;
    };
    let sum_charges: Decimal = charges.iter().map(|c| c.money.amount.value()).sum();
    let sum_payments: Decimal = payments.iter().map(|p| p.money.amount.value()).sum();
    let computed = anterior + sum_charges - sum_payments;
    let delta = (computed - stated).abs();
    // Record the worst-case delta across sections (None → Some(delta), else max),
    // so the correctness signal reflects the least-reconciled section.
    report.balance_delta = Some(report.balance_delta.map_or(delta, |d| d.max(delta)));
    // Tight tolerance: surface any real discrepancy (missed row / unmodeled
    // fee or interest) for a human to read, especially during dry-run.
    let tolerance = Decimal::new(1, 2); // 0.01
    info!(
        currency = ?section.currency,
        anterior = %anterior,
        charges = %sum_charges,
        payments = %sum_payments,
        computed = %computed,
        stated = %stated,
        delta = %delta,
        "closing-balance check"
    );
    if delta > tolerance {
        report.balance_mismatch += 1;
        warn!(
            currency = ?section.currency,
            delta = %delta,
            "closing balance does not reconcile (missed rows, or unmodeled fees/interest) → review"
        );
    }
}

/// A PII-free label for a charge's reconcile outcome (no journal amounts / reason
/// strings) — used in the non-PII charge log path.
fn charge_outcome_kind(o: &ChargeOutcome) -> &'static str {
    match o {
        ChargeOutcome::Confirmed { .. } => "confirmed",
        ChargeOutcome::AmountMismatch { .. } => "amount_mismatch",
        ChargeOutcome::BookNew => "book_new",
        ChargeOutcome::Review { .. } => "review",
    }
}

/// Handle a matched charge whose statement (billed) amount differs from the
/// booked (ECB-estimate) amount. Phase-1 (autocorrect off) → report + Review.
/// Phase-2 (autocorrect on) → auto-correct the journal to the billed figure,
/// guarded by the TOCTOU + bounded-delta checks in
/// [`FireflyClient::correct_amount`]; a blocked correction or a Firefly write
/// failure falls back to Review (`amount_mismatch`), never aborting the statement.
async fn correct_amount_mismatch(
    journal_id: &str,
    booked: Decimal,
    billed: Decimal,
    firefly: &FireflyClient<'_>,
    cfg: &Config,
    report: &mut StatementReport,
) {
    if !cfg.bp_autocorrect_amounts {
        report.amount_mismatch += 1;
        return;
    }
    if cfg.dry_run {
        // Preview without the live GET/PUT: model `current == booked` (no drift
        // to show in a plan), so this exercises the bounded-delta guard only.
        match correction_decision(booked, booked, billed, cfg.bp_max_correction_pct) {
            CorrectionOutcome::Review { reason } => {
                report.amount_mismatch += 1;
                info!(journal = journal_id, %reason, "DRY RUN: would NOT auto-correct → review");
            }
            _ => {
                report.corrected += 1;
                info!(journal = journal_id, from = %booked.normalize(), to = %billed.normalize(), "DRY RUN: would auto-correct amount");
            }
        }
        return;
    }
    match firefly
        .correct_amount(journal_id, booked, billed, cfg.bp_max_correction_pct)
        .await
    {
        Ok(CorrectionOutcome::Corrected | CorrectionOutcome::NoOp) => report.corrected += 1,
        Ok(CorrectionOutcome::Review { reason }) => {
            report.amount_mismatch += 1;
            warn!(journal = journal_id, %reason, "amount auto-correct blocked → review");
        }
        Err(e) => {
            report.amount_mismatch += 1;
            warn!(journal = journal_id, error = ?e, "amount auto-correct failed → review");
        }
    }
}

/// Book a not-yet-present charge via the canonical gate, tallying the outcome.
/// A per-row failure is logged + counted as review, never aborts the statement.
async fn book_charge(
    charge: &StatementTxn,
    section: &super::Section,
    firefly: &FireflyClient<'_>,
    fx: &FxClient<'_>,
    cfg: &Config,
    report: &mut StatementReport,
) {
    let extracted = charge.to_extracted(&section.primary_last4);
    let validated = match validate(extracted) {
        Verdict::Booked(v) => v,
        Verdict::Review { reason } => {
            info!(reference = charge.reference.as_str(), %reason, "BookNew failed validate → review");
            report.review += 1;
            return;
        }
    };
    let ext = validated.as_extracted();
    match crate::usd_ceiling_review(
        fx,
        &cfg.validation,
        ext.currency().as_str(),
        ext.amount().value(),
        ext.date,
    )
    .await
    {
        Ok(Some(reason)) => {
            info!(reference = charge.reference.as_str(), %reason, "BookNew over ceiling → review");
            report.review += 1;
            return;
        }
        Ok(None) => {}
        Err(e) if crate::fx::is_transient(&e) => {
            // Rate provider down — defer this row (keep the statement in INBOX,
            // retry next run). Don't book, don't review.
            info!(reference = charge.reference.as_str(), error = ?e, "ceiling rate unavailable → deferred (retry next run)");
            report.deferred += 1;
            return;
        }
        Err(e) => {
            warn!(reference = charge.reference.as_str(), error = ?e, "ceiling FX failed → review");
            report.review += 1;
            return;
        }
    }
    let external_id = crate::dedup::external_id(validated.as_extracted());
    if cfg.dry_run {
        info!(reference = charge.reference.as_str(), %external_id, "DRY RUN: would book new charge");
        report.booked_new += 1;
        return;
    }
    // Phase-2 (c): map the charge's MCC to a Firefly category, if configured.
    let category = charge
        .mcc
        .as_ref()
        .and_then(|m| cfg.bp_mcc_category.get(m.as_str()))
        .map(String::as_str);
    match firefly
        .submit_with_category(&validated, &external_id, category)
        .await
    {
        Ok(SubmitOutcome::Created) => report.booked_new += 1,
        Ok(SubmitOutcome::Duplicate) => report.reconciled += 1,
        Err(e) => {
            warn!(reference = charge.reference.as_str(), error = ?e, "BookNew submit failed → review");
            report.review += 1;
        }
    }
}

/// Book a statement payment as a transfer (paying account → card), tallying the
/// outcome. Per-row failures are counted as review, never abort the statement.
async fn book_payment(
    payment: &StatementTxn,
    firefly: &FireflyClient<'_>,
    fx: &FxClient<'_>,
    cfg: &Config,
    report: &mut StatementReport,
) {
    let external_id = format!("bpstmt:{}", payment.reference.as_str());
    // Plausibility ceiling applies to transfers too (a crafted huge payment must
    // not silently move money out of the savings account).
    match crate::usd_ceiling_review(
        fx,
        &cfg.validation,
        payment.money.currency.as_str(),
        payment.money.amount.value(),
        payment.auth_date,
    )
    .await
    {
        Ok(Some(reason)) => {
            info!(reference = payment.reference.as_str(), %reason, "payment over ceiling → review");
            report.review += 1;
            return;
        }
        Ok(None) => {}
        Err(e) if crate::fx::is_transient(&e) => {
            info!(reference = payment.reference.as_str(), error = ?e, "payment ceiling rate unavailable → deferred (retry next run)");
            report.deferred += 1;
            return;
        }
        Err(e) => {
            warn!(reference = payment.reference.as_str(), error = ?e, "payment ceiling FX failed → review");
            report.review += 1;
            return;
        }
    }
    let transfer = match validate_transfer(
        payment.money.clone(),
        payment.auth_date,
        payment.merchant.clone(),
        external_id,
    ) {
        TransferVerdict::Booked(t) => t,
        TransferVerdict::Review { reason } => {
            info!(reference = payment.reference.as_str(), %reason, "payment failed transfer gate → review");
            report.review += 1;
            return;
        }
    };
    if cfg.dry_run {
        info!(
            reference = payment.reference.as_str(),
            "DRY RUN: would book payment transfer"
        );
        report.payments_booked += 1;
        return;
    }
    match firefly.submit_transfer(&transfer).await {
        Ok(SubmitOutcome::Created) => report.payments_booked += 1,
        Ok(SubmitOutcome::Duplicate) => report.reconciled += 1,
        Err(e) => {
            warn!(reference = payment.reference.as_str(), error = ?e, "payment submit failed → review");
            report.review += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jmap::Attachment;

    fn msg(subject: &str, from: &str, attachments: Vec<Attachment>) -> FetchedMessage {
        FetchedMessage {
            id: "m1".into(),
            subject: Some(subject.into()),
            from: Some(from.into()),
            text: String::new(),
            attachments,
        }
    }
    fn pdf_att() -> Attachment {
        Attachment {
            blob_id: "b".into(),
            content_type: Some("application/pdf".into()),
            name: Some("4173-XXXX-XXXX-7524.pdf".into()),
            size: 42715,
        }
    }
    fn other_att() -> Attachment {
        Attachment {
            blob_id: "b".into(),
            content_type: Some("image/png".into()),
            name: Some("logo.png".into()),
            size: 10,
        }
    }

    #[test]
    fn pdf_with_cuenta_subject_is_statement() {
        let m = msg(
            "Fwd: Cuenta: ****-****-****-7524 | Fecha: 22/05/2026",
            "rhansen@kitsd.com",
            vec![pdf_att()],
        );
        assert_eq!(classify_message(&m, None), Ingest::Statement);
    }

    #[test]
    fn pdf_from_configured_sender_is_statement() {
        let m = msg("monthly", "rhansen@kitsd.com", vec![pdf_att()]);
        assert_eq!(classify_message(&m, Some("kitsd.com")), Ingest::Statement);
    }

    #[test]
    fn pdf_without_marker_is_notification() {
        let m = msg("here is a receipt", "someone@example.com", vec![pdf_att()]);
        assert_eq!(
            classify_message(&m, Some("kitsd.com")),
            Ingest::Notification
        );
    }

    #[test]
    fn no_pdf_is_notification_even_with_cuenta_subject() {
        let m = msg("Cuenta: 123", "rhansen@kitsd.com", vec![other_att()]);
        assert_eq!(
            classify_message(&m, Some("kitsd.com")),
            Ingest::Notification
        );
    }

    #[test]
    fn report_clean_only_without_flags() {
        let clean = StatementReport {
            reconciled: 5,
            booked_new: 2,
            payments_booked: 1,
            ..Default::default()
        };
        assert!(clean.is_clean());
        assert!(
            !StatementReport {
                amount_mismatch: 1,
                ..Default::default()
            }
            .is_clean()
        );
        assert!(
            !StatementReport {
                unmatched_booked: 1,
                ..Default::default()
            }
            .is_clean()
        );
        assert!(
            !StatementReport {
                balance_mismatch: 1,
                ..Default::default()
            }
            .is_clean()
        );
        assert!(
            !StatementReport {
                review: 1,
                ..Default::default()
            }
            .is_clean()
        );
        assert!(
            !StatementReport {
                deferred: 1,
                ..Default::default()
            }
            .is_clean()
        );
    }

    #[test]
    fn check_balance_records_delta_and_distinguishes_absent() {
        use crate::statement::{Last4, Section};
        use rust_decimal::Decimal;
        use std::str::FromStr;

        fn section(anterior: Option<&str>, total: Option<&str>) -> Section {
            Section {
                currency: SectionCurrency::Usd,
                primary_last4: Last4::parse("7524").unwrap(),
                cut_date: chrono::NaiveDate::from_ymd_opt(2026, 5, 22).unwrap(),
                balance_anterior: anterior.map(|s| Decimal::from_str(s).unwrap()),
                balance_total: total.map(|s| Decimal::from_str(s).unwrap()),
            }
        }

        // Reconciling (anterior == total, no charges) → Some(0), no mismatch flag.
        let mut r = StatementReport::default();
        check_balance(&section(Some("100.00"), Some("100.00")), &[], &[], &mut r);
        assert_eq!(r.balance_delta, Some(Decimal::ZERO));
        assert_eq!(r.balance_mismatch, 0);

        // Not reconciling (100 → stated 130) → Some(30) + the mismatch flag fires.
        let mut r2 = StatementReport::default();
        check_balance(&section(Some("100.00"), Some("130.00")), &[], &[], &mut r2);
        assert_eq!(r2.balance_delta, Some(Decimal::from(30)));
        assert_eq!(r2.balance_mismatch, 1);

        // Balances absent → delta stays None (checked ≠ reconciled).
        let mut r3 = StatementReport::default();
        check_balance(&section(None, None), &[], &[], &mut r3);
        assert_eq!(r3.balance_delta, None);
        assert_eq!(r3.balance_mismatch, 0);
    }
}
