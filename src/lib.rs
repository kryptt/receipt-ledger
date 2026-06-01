//! receipt-ledger library crate.
//!
//! The binary (`main.rs`) is a thin shell over [`run`]; everything testable
//! lives here so integration tests in `tests/` can exercise the deterministic
//! core (schema, unwrap, adapters, validate, dedup) directly.

pub mod adapters;
pub mod config;
pub mod dedup;
pub mod eval;
pub mod firefly;
pub mod fx;
pub mod jmap;
pub mod llm;
pub mod model_selection;
pub mod schema;
pub mod statement;
pub mod telemetry;
pub mod unwrap;
pub mod usd_ceiling;
pub mod validate;

use anyhow::{Context, Result};
use reqwest::Client;
use tracing::{Instrument, error, info, warn};

use crate::adapters::{DestHint, Outcome, SourceHint, TransferRecord};
use crate::config::{Config, ValidationPolicy};
use crate::firefly::{FireflyClient, SubmitOutcome, TransferSubmit};
use crate::fx::FxClient;
use crate::jmap::{FetchedMessage, Mailbox};
use crate::llm::LlmClient;
use crate::statement::pipeline::{Ingest, classify_message, process_statement};
use crate::usd_ceiling::CeilingVerdict;
use crate::validate::{TransferVerdict, Verdict, validate, validate_transfer};

/// Tallies for the end-of-run summary.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub processed: usize,
    pub booked: usize,
    pub duplicates: usize,
    pub review: usize,
    /// Mail that was not a transaction at all (clean skip → Processed).
    pub skipped: usize,
    /// Statement PDFs processed (their per-row tallies fold into the fields
    /// above; this is just how many statement messages were handled).
    pub statements: usize,
    /// Phase-2 amount auto-corrections applied (statement charges whose booked
    /// estimate was rewritten in place to the statement's billed figure). Tracked
    /// separately because a correction is an in-place mutation, not a booking — it
    /// would otherwise be invisible in the run summary.
    pub corrected: usize,
    /// No-progress signal: messages left in the INBOX this run because they
    /// deferred (provider outage). The co-primary alert keys on this staying > 0
    /// across runs (a defer-forever backlog). `> 0` also means JMAP state was held.
    pub deferred: usize,
}

/// Run the full one-shot pipeline. Returns the run summary on success; an
/// `Err` here is a real (non-zero-exit) failure.
///
/// `http` is the shared client (built in `main` so the optional OTLP trace
/// exporter can reuse the same reqwest/rustls stack); the money path uses it for
/// JMAP, Firefly, FX, and the LLM.
///
/// This is instrumented as the **root span** of the run (`stage = "run"`): every
/// child stage span and every log line nests under it, so when trace export is
/// on (see [`telemetry`]) one trace_id covers the whole run and links Loki↔Tempo.
#[tracing::instrument(name = "run", skip(http), fields(trace_id = tracing::field::Empty))]
pub async fn run(http: Client) -> Result<Summary> {
    // Fill the root span's trace_id from the OTel context (no-op when traces are
    // off), so every event below carries the run's trace_id.
    crate::telemetry::record_trace_id();
    let cfg = Config::from_env().context("loading configuration")?;
    if cfg.dry_run {
        info!(
            "DRY RUN enabled (RECEIPT_DRY_RUN): no Firefly writes, no mailbox moves, no state advance"
        );
    }

    // --- 1. JMAP read -----------------------------------------------------
    // `fetch` stage span: connect + fetch new mail. Attributes are an allowlist
    // (stage + a count); never any message content.
    let mailbox = Mailbox::connect(&cfg)
        .instrument(tracing::info_span!("fetch", stage = "fetch"))
        .await
        .context("JMAP connect")?;
    let prior_state = jmap::load_state(&cfg.state_path);
    let (messages, new_state) = mailbox
        .fetch_new(prior_state)
        .instrument(tracing::info_span!("fetch", stage = "fetch"))
        .await
        .context("fetching new mail")?;

    if messages.is_empty() {
        info!("no new messages");
        if !cfg.dry_run {
            jmap::save_state(&cfg.state_path, &new_state).context("saving JMAP state")?;
        }
        return Ok(Summary::default());
    }
    info!(count = messages.len(), "new messages to process");

    // Model selection is shared across the batch — pick once per run. A failure
    // here aborts the run (non-zero exit, caught by kube-state-metrics) *before*
    // any Summary exists, so it can't be a summary field — emit a dedicated
    // structured event (`stage = "model_selection"`) so a log-derived alert can
    // key on it directly.
    let model = match model_selection::select_model(&http, &cfg.ollama_url, &cfg.model_allowlist)
        .instrument(tracing::info_span!(
            "model_selection",
            stage = "model_selection"
        ))
        .await
    {
        Ok(m) => m,
        Err(e) => {
            error!(stage = "model_selection", error = ?e, "model selection failed");
            return Err(e).context("selecting extraction model");
        }
    };
    info!(%model, "using extraction model");
    let llm = LlmClient::new(&http, &cfg.ollama_url, model, cfg.llm_timeout);
    // FX over the shared client; converts foreign charges into the target
    // account's currency before booking. An FX error propagates from `submit`
    // and routes the message to Review (never books at a wrong amount).
    // Persistent FX cache on the `/state` volume: a date's rate is fetched once
    // and reused across the hourly one-shot runs instead of re-hitting Frankfurter
    // / the rate-limited Banco Popular consultaTasa every run a message lingers in
    // the INBOX. Past-date rates never expire; the current-day rate has a 15-min TTL.
    let mut fx = FxClient::new(&http, &cfg.fx_url).with_cache_file(&cfg.fx_cache_path);
    // Frankfurter has no Dominican Peso; attach Banco Popular's consultaTasa as a
    // DOP override when its credentials are configured.
    if let Some(d) = &cfg.dop_rate {
        fx = fx.with_dop(crate::fx::DopRate::new(
            &http,
            &d.rates_url,
            &d.token_url,
            &d.client_id,
            &d.client_secret,
            &d.scope,
            d.retry_budget,
        ));
    }
    let firefly = FireflyClient::new(
        &http,
        &cfg.firefly_url,
        &cfg.firefly_token,
        &fx,
        cfg.paypal_balance_account.clone(),
        cfg.paypal_credit_account.clone(),
        cfg.banco_popular_usd_account.clone(),
        cfg.banco_popular_dop_account.clone(),
        cfg.bp_paying_usd_account.clone(),
        cfg.bp_paying_dop_account.clone(),
        cfg.paying_account_by_last4.clone(),
        cfg.swift_debtor_by_last4.clone(),
        cfg.swift_dest_by_bic.clone(),
    );

    // Alias map for the consumo double-book probe (Phase 2), fetched once so the
    // probe canonicalizes merchants the same way the statement reconciler does
    // (otherwise an alias-only rename would slip past it and double-book). Empty
    // unless the probe is enabled and a rule group is configured.
    let consumo_aliases = if cfg.bp_double_book_probe
        && let Some(group) = &cfg.bp_alias_rule_group
    {
        firefly.fetch_alias_map(group).await.unwrap_or_else(|e| {
            warn!(error = ?e, "consumo-probe alias map fetch failed; proceeding without aliases");
            Vec::new()
        })
    } else {
        Vec::new()
    };

    // --- per-message pipeline --------------------------------------------
    let mut summary = Summary::default();
    // Set when any message defers on a transient rate outage: the batch's JMAP
    // state is then *not* advanced, so deferred messages (left in INBOX) are
    // refetched next run. Already-booked rows dedup; moved messages are filtered
    // out by the INBOX check in `fetch_new`.
    let mut hold_state = false;
    // No-progress signal: messages left in the INBOX this run because they
    // deferred (provider outage / per-row rate defer). The co-primary alert keys
    // on this staying > 0 across runs. Counted explicitly because `route()` is
    // fire-and-forget and deferred messages are never routed.
    let mut deferred_messages = 0usize;
    for msg in &messages {
        summary.processed += 1;

        // A statement PDF takes the reconcile path; everything else is a
        // per-transaction notification (the adapter path).
        if classify_message(msg, cfg.bp_statement_sender.as_deref()) == Ingest::Statement {
            summary.statements += 1;
            match process_statement(msg, &mailbox, &firefly, &fx, &cfg).await {
                Ok(report) => {
                    summary.booked += report.booked_new + report.payments_booked;
                    summary.duplicates += report.reconciled;
                    summary.corrected += report.corrected;
                    summary.review +=
                        report.review + report.amount_mismatch + report.unmatched_booked;
                    if report.deferred > 0 {
                        // Some rows need a rate the provider couldn't give right
                        // now. Leave the whole message in INBOX; the rows that did
                        // book dedup on retry (book USD now, retry DOP later).
                        hold_state = true;
                        deferred_messages += 1;
                        warn!(id = %msg.id, ?report, deferred = report.deferred,
                            "statement has deferred rows (rate provider down); kept in INBOX for retry");
                    } else {
                        let clean = report.is_clean();
                        // Per-message outcome event for statements too (source is
                        // statically `banco_popular`), so the source×disposition
                        // LogQL metric isn't blind to the statement path. `?report`
                        // is numeric-only (no PII), safe to log unguarded.
                        info!(
                            id = %msg.id,
                            source = "banco_popular",
                            disposition = if clean { "booked" } else { "review" },
                            "message outcome"
                        );
                        if clean {
                            info!(id = %msg.id, ?report, "statement clean → processed");
                        } else {
                            warn!(id = %msg.id, ?report, "statement has flags → review");
                        }
                        route(&mailbox, &msg.id, clean, cfg.dry_run).await;
                    }
                }
                Err(e) if is_transient_outage(&e) => {
                    // A provider (FX/LLM) outage mid-statement — defer the whole
                    // message (kept in INBOX, state not advanced); booked rows
                    // dedup on retry. Never burn a statement to Review for a
                    // transient network failure.
                    hold_state = true;
                    deferred_messages += 1;
                    warn!(id = %msg.id, error = %e, "statement deferred (provider outage); kept in INBOX for retry");
                }
                Err(e) => {
                    summary.review += 1;
                    warn!(id = %msg.id, error = ?e, "statement processing error; routing to review");
                    route(&mailbox, &msg.id, false, cfg.dry_run).await;
                }
            }
            continue;
        }

        match process_message(
            msg,
            &llm,
            &firefly,
            &fx,
            &cfg.validation,
            cfg.dry_run,
            cfg.bp_double_book_probe,
            &consumo_aliases,
        )
        .await
        {
            Ok((source, disposition)) => {
                // Per-message structured OUTCOME event — the LogQL substrate for
                // the source×disposition metric. Carries only low-cardinality
                // labels: `source`, the bare disposition discriminant, and (for a
                // review) a bounded reason CATEGORY — never the free-form reason
                // string, which can carry PII (last-4/currency/account).
                let review_category = match &disposition {
                    Disposition::Review(reason) => Some(review_reason_category(reason)),
                    _ => None,
                };
                info!(
                    id = %msg.id,
                    source,
                    disposition = disposition_label(&disposition),
                    review_reason_category = review_category,
                    "message outcome"
                );
                match disposition {
                    Disposition::Booked => {
                        summary.booked += 1;
                        route(&mailbox, &msg.id, true, cfg.dry_run).await;
                    }
                    Disposition::Duplicate => {
                        summary.duplicates += 1;
                        route(&mailbox, &msg.id, true, cfg.dry_run).await;
                    }
                    Disposition::Skipped(reason) => {
                        // Not a transaction at all — a clean skip, NOT a review.
                        // The reason can name the source/merchant, so gate it on
                        // RECEIPT_LOG_PII (the bounded category went out above).
                        summary.skipped += 1;
                        if cfg.log_pii {
                            info!(id = %msg.id, %reason, "not a transaction; skipping to processed");
                        } else {
                            info!(id = %msg.id, "not a transaction; skipping to processed");
                        }
                        route(&mailbox, &msg.id, true, cfg.dry_run).await;
                    }
                    Disposition::Review(reason) => {
                        // The free-form reason can carry PII (merchant / amount /
                        // last-4 / sender email), so gate it on RECEIPT_LOG_PII; the
                        // bounded `review_reason_category` already went out on the
                        // outcome event above and is always safe to log.
                        summary.review += 1;
                        if cfg.log_pii {
                            warn!(id = %msg.id, %reason, "routing to review");
                        } else {
                            warn!(id = %msg.id, review_reason_category = review_category, "routing to review");
                        }
                        route(&mailbox, &msg.id, false, cfg.dry_run).await;
                    }
                }
            }
            // CENTRAL DEFER CHOKEPOINT: any transient FX/LLM provider outage,
            // wherever it originated in processing this message, arrives here as a
            // typed `Err` and defers (kept in INBOX, JMAP state not advanced) so
            // the hourly cron retries — it is NEVER routed to Review. Making this
            // the single classification site means a new provider call site cannot
            // forget to defer. Booked rows dedup on retry.
            Err(e) if is_transient_outage(&e) => {
                hold_state = true;
                deferred_messages += 1;
                warn!(id = %msg.id, error = %e, "deferred (provider outage); kept in INBOX for retry");
            }
            Err(e) => {
                // A genuine (non-transient) per-message processing error is not
                // fatal to the job → Review.
                summary.review += 1;
                warn!(id = %msg.id, error = ?e, "processing error; routing to review");
                route(&mailbox, &msg.id, false, cfg.dry_run).await;
            }
        }
    }

    // Persist the FX cache regardless of dry-run: it is rate data, not
    // transaction/JMAP state, so writing it never books anything or advances the
    // cursor — and persisting during dry-run observation is exactly when it most
    // avoids re-fetching the same rates hourly. A write failure is non-fatal.
    if let Err(e) = fx.persist() {
        warn!(error = %e, "failed to persist FX cache (non-fatal)");
    }

    // --- 7. Persist state cursor -----------------------------------------
    // Only after the batch is fully handled; a crash mid-batch re-processes
    // (dedup makes that safe) rather than skipping unprocessed mail. Skipped in
    // dry-run so the same messages can be re-observed.
    if cfg.dry_run {
        info!("DRY RUN: not advancing JMAP state");
    } else if hold_state {
        warn!(
            "deferred messages kept in INBOX (provider outage); not advancing JMAP state — will retry next run"
        );
    } else {
        jmap::save_state(&cfg.state_path, &new_state).context("saving JMAP state")?;
    }

    summary.deferred = deferred_messages;
    Ok(summary)
}

/// What happened to a single message.
///
/// Note there is no `Defer` variant: a transient provider outage is expressed as
/// a typed `Err` (`fx::RateError::Transient` / `llm::LlmError::Transient`) that
/// propagates to [`run`]'s central classifier ([`is_transient_outage`]), which
/// holds the message in the INBOX for retry. Keeping defer out of `Disposition`
/// is what makes "transient never reaches Review" impossible to bypass per-site.
#[derive(Debug)]
enum Disposition {
    Booked,
    Duplicate,
    /// Not a transaction notification at all — a clean skip (→ Processed).
    Skipped(String),
    Review(String),
}

/// Whether an error is a transient FX or LLM provider outage — the single
/// predicate [`run`] uses to defer (keep in INBOX, retry next run) rather than
/// route to Review. Walks the `anyhow` chain via each module's classifier, so it
/// survives `.context(...)` wrapping and catches the outage wherever it arose.
fn is_transient_outage(e: &anyhow::Error) -> bool {
    crate::fx::is_transient(e) || crate::llm::is_transient(e)
}

/// The low-cardinality disposition label for the per-message outcome event — the
/// bare enum discriminant only. The `Skipped`/`Review` payload Strings are NEVER
/// included (they can carry last-4/currency/account → PII).
fn disposition_label(d: &Disposition) -> &'static str {
    match d {
        Disposition::Booked => "booked",
        Disposition::Duplicate => "duplicate",
        Disposition::Skipped(_) => "skipped",
        Disposition::Review(_) => "review",
    }
}

/// Map a review `reason` to a **bounded** category for the outcome event, so a
/// reviewer can see *why* reviews accumulate without the free-form (PII-bearing)
/// reason string reaching Loki. Classifies our own reason strings; unknown → `other`.
fn review_reason_category(reason: &str) -> &'static str {
    let r = reason.to_ascii_lowercase();
    if r.contains("not a recognisable forward") {
        "not_a_forward"
    } else if r.contains("no adapter") {
        "no_adapter"
    } else if r.contains("already booked by a statement") {
        "double_book"
    } else if r.contains("routed to review") {
        // the USD-ceiling reason ("amount ≈ $X (>Y USD) — routed to review")
        "over_ceiling"
    } else if r.contains("currency") {
        "currency_mismatch"
    } else if r.contains("account") {
        "no_account"
    } else if r.contains("extracted no") || r.contains("no record") {
        "extraction"
    } else {
        "other"
    }
}

/// The deterministic-core + I/O pipeline for one message.
///
/// Instrumented as a `process` child span of the run. **No-PII span allowlist:**
/// the only attributes are `stage` and (recorded later) the resolved `source`
/// label and bare `outcome` discriminant — never the raw body, merchant, amount,
/// last-4, or ref#. `msg` and the clients are `skip`ped so their contents never
/// land on the span.
#[tracing::instrument(
    name = "process",
    skip_all,
    fields(stage = "process", source = tracing::field::Empty, outcome = tracing::field::Empty)
)]
#[allow(clippy::too_many_arguments)]
async fn process_message(
    msg: &FetchedMessage,
    llm: &LlmClient<'_>,
    firefly: &FireflyClient<'_>,
    fx: &FxClient<'_>,
    policy: &ValidationPolicy,
    dry_run: bool,
    double_book_probe: bool,
    aliases: &[(String, String)],
) -> Result<(&'static str, Disposition)> {
    // 2. Unwrap the Gmail forward (manual marker or auto-forward) + detect the
    //    original sender. No adapter is known yet, so the observability `source`
    //    is `unknown` for the pre-routing failures.
    let unwrapped = match unwrap::unwrap_message(msg.from.as_deref(), &msg.text) {
        Some(u) => u,
        None => {
            return Ok((
                "unknown",
                Disposition::Review("not a recognisable forward".to_string()),
            ));
        }
    };

    // 3. Route to the per-sender adapter.
    let adapter = match adapters::select(&unwrapped.original_sender) {
        Some(a) => a,
        None => {
            return Ok((
                "unknown",
                Disposition::Review(format!(
                    "no adapter for sender {}",
                    unwrapped.original_sender
                )),
            ));
        }
    };
    // The canonical observability `source` label — `adapter.name()`
    // (paypal / paypal_payment / banco_popular). Stable + low-cardinality, so it
    // is safe to record on the `process` span (allowlist: stage/source/outcome).
    let source = adapter.name();
    tracing::Span::current().record("source", source);

    // 3b. Deterministic pre-filter: if the body clearly is not a transaction
    //     notification (a PayPal shipping update, plan reminder, survey), skip
    //     it cleanly without an LLM call. Never a Review — it never was a
    //     transaction.
    if !adapter.is_transaction(&unwrapped.body) {
        return Ok((
            source,
            Disposition::Skipped(format!(
                "{source} mail did not look like a transaction notification"
            )),
        ));
    }

    // 4. Extraction. A fixed-format source (PayPal Credit payment receipt) parses
    //    its body deterministically and bypasses the LLM entirely; everything
    //    else builds a prompt and asks the model. Both paths land on one
    //    `Outcome`, so the routing below is shared.
    let outcome = match adapter.deterministic_extract(&unwrapped.body) {
        Some(result) => result.context("adapter deterministic_extract")?,
        None => {
            let prompt = adapter.prompt(&unwrapped.body);
            // A transient LLM/FX outage is NOT caught here: it propagates as an
            // `Err` carrying a typed `LlmError::Transient` / `RateError::Transient`,
            // and `run()` classifies every such error to Defer (kept in INBOX,
            // retried next run) at one chokepoint — so no call site can forget to
            // defer and accidentally burn a real receipt to Review.
            //
            // `extract` span: NO-PII allowlist. The prompt is built from the raw
            // email body (`adapter.prompt(&unwrapped.body)`) and the model's
            // completion is likewise sensitive — NEITHER is attached to the span.
            // The only attribute is the `stage` label; see the span-shaping test.
            let json = llm
                .extract_json(&prompt)
                .instrument(tracing::info_span!("extract", stage = "extract"))
                .await
                .context("LLM extraction")?;
            // `postprocess_with_body` applies any deterministic body-derived
            // override (PayPal P1: a cross-currency receipt's authoritative USD
            // total) on top of the model's extraction.
            adapter
                .postprocess_with_body(&json, &unwrapped.body)
                .context("adapter postprocess")?
        }
    };

    let records = match outcome {
        Outcome::Transaction(records) => records,
        // A payment receipt → book as a transfer (funding account → credit), a
        // distinct path from the withdrawal loop below.
        Outcome::Transfer(tr) => {
            return Ok((
                source,
                book_transfer(tr, firefly, fx, policy, dry_run).await?,
            ));
        }
        // The mail was classified as a non-transaction → clean skip.
        Outcome::NotATransaction { reason } => {
            return Ok((source, Disposition::Skipped(reason)));
        }
    };

    if records.is_empty() {
        return Ok((
            source,
            Disposition::Review("adapter extracted no records".to_string()),
        ));
    }

    // For a single-transaction source (PayPal v1) we expect one record. Process
    // each; the message disposition is the "best" outcome among them.
    let mut booked_any = false;
    let mut dup_any = false;
    let mut review_reason: Option<String> = None;

    for record in records {
        // 5. Validation gates (deterministic, sync). Only `validate` can mint a
        //    `Validated`, which `firefly.submit` requires — the gate is
        //    impossible to bypass.
        match validate(record) {
            Verdict::Booked(validated) => {
                // 5b. USD-equivalent ceiling (`RECEIPT_MAX_AMOUNT`). FX-dependent,
                //     so it lives in the async pipeline, not the pure `validate`
                //     gate. An FX failure propagates as `Err` (→ Review), never
                //     books an unscreened large amount.
                let ext = validated.as_extracted();
                // A transient rate outage propagates as `Err` and is deferred by
                // `run()`'s central classifier; a permanent FX error also
                // propagates (→ Review). An over-ceiling verdict is `Ok(Some)`.
                if let Some(reason) = usd_ceiling_review(
                    fx,
                    policy,
                    ext.currency().as_str(),
                    ext.amount().value(),
                    ext.date,
                )
                .await?
                {
                    review_reason.get_or_insert(reason);
                    continue;
                }

                // 5c. Symmetric double-book guard (Phase 2, opt-in). A Banco
                //     Popular consumo for a charge a statement already booked
                //     (`bpstmt:<ref>`) would double-book — the consumo path keys
                //     by composite-hash, the statement path by ref#, so Firefly
                //     can't dedup across them. Probe in-window journals for a
                //     plausible statement booking → Review instead of booking. A
                //     read failure propagates as `Err` (deferred/Review centrally).
                if double_book_probe
                    && ext.source == crate::schema::Source::BancoPopular
                    && let Some(j) = firefly
                        .statement_duplicate_probe(
                            ext,
                            &crate::statement::reconcile::ReconcileParams::default(),
                            aliases,
                        )
                        .await?
                {
                    review_reason.get_or_insert(format!(
                        "charge appears already booked by a statement (journal {} '{}') — not double-booking",
                        j.id, j.merchant
                    ));
                    continue;
                }

                // 6. Dedup key. 7. Submit to Firefly (skipped in dry-run).
                let external_id = dedup::external_id(validated.as_extracted());
                if dry_run {
                    info!(%external_id, "DRY RUN: would book withdrawal");
                    booked_any = true;
                } else {
                    match firefly.submit(&validated, &external_id).await? {
                        SubmitOutcome::Created => booked_any = true,
                        SubmitOutcome::Duplicate => dup_any = true,
                    }
                }
            }
            Verdict::Review { reason } => {
                review_reason.get_or_insert(reason);
            }
        }
    }

    let disposition = if booked_any {
        Disposition::Booked
    } else if dup_any {
        Disposition::Duplicate
    } else {
        Disposition::Review(review_reason.unwrap_or_else(|| "no record booked".to_string()))
    };
    // Record the bare outcome discriminant on the span (allowlisted, no payload).
    tracing::Span::current().record("outcome", disposition_label(&disposition));
    Ok((source, disposition))
}

/// Book a payment as a Firefly transfer: funding account (source, resolved from
/// the record's funding/debtor last-4) → a destination account chosen by the
/// record's [`DestHint`] — the configured PayPal Credit account (PayPal payment
/// receipt) or the user's own foreign account resolved from the creditor BIC
/// (outbound SWIFT wire). Mirrors the withdrawal path's gates — the same
/// USD-equivalent ceiling, then the transfer validation gate — but routes both
/// account legs from config rather than from the record. A missing destination
/// or an unresolved source is a clear Review (never guess — and never auto-book
/// a wire to an unmapped/third-party account); a transient rate outage defers
/// (kept in INBOX for retry).
async fn book_transfer(
    tr: TransferRecord,
    firefly: &FireflyClient<'_>,
    fx: &FxClient<'_>,
    policy: &ValidationPolicy,
    dry_run: bool,
) -> Result<Disposition> {
    // Destination depends on the transfer source. Exhaustive over `DestHint` so a
    // new transfer kind forces a destination-routing decision here.
    let dest = match &tr.dest {
        // PayPal payment → the configured PayPal Credit account. Absent → Review.
        DestHint::PayPalCredit => match firefly.paypal_credit_account() {
            Some(dest) => dest,
            None => {
                return Ok(Disposition::Review(
                    "no Firefly account configured for PayPal Credit (RECEIPT_PAYPAL_CREDIT_ACCOUNT)"
                        .to_string(),
                ));
            }
        },
        // SWIFT wire → the own foreign account mapped from the creditor BIC.
        // Unmapped → Review; a wire to an unmapped/third-party BIC must never
        // auto-book.
        DestHint::CreditorBic(bic) => match firefly.swift_dest_for_bic(bic) {
            Some(dest) => dest,
            None => {
                return Ok(Disposition::Review(format!(
                    "no Firefly account configured for SWIFT creditor BIC {bic} (RECEIPT_SWIFT_DEST_BY_BIC)"
                )));
            }
        },
    };
    // Source = the funding/debtor account resolved from the record's last-4
    // against the map its `SourceHint` names. Exhaustive over `SourceHint` so a
    // new transfer source forces a source-map decision here. A PayPal funding
    // card resolves against RECEIPT_PAYING_ACCOUNT_BY_LAST4; a SWIFT debtor IBAN
    // resolves against the DEDICATED RECEIPT_SWIFT_DEBTOR_BY_LAST4 — so a last-4
    // shared between the two cannot mis-route. Absent (or empty map) → Review; we
    // never guess the source account.
    //
    // Cross-currency wires (e.g. a USD wire into a EUR ABN AMRO account) ARE
    // booked: `submit_transfer_between` books the exact settled amount on the
    // source leg and an FX-estimated `foreign_amount` on the destination leg.
    // The one case it still routes to Review is when the settlement currency
    // disagrees with the SOURCE account's own currency (the source-leg debit
    // would be unknown) — never a silent mis-book of the figure that left BPD.
    let source = match &tr.source {
        SourceHint::PayPalFundingLast4(last4) => match firefly.paying_account_for_last4(last4) {
            Some(source) => source,
            None => {
                return Ok(Disposition::Review(format!(
                    "no paying account configured for PayPal funding last-4 {last4} (RECEIPT_PAYING_ACCOUNT_BY_LAST4)"
                )));
            }
        },
        SourceHint::SwiftDebtorLast4(last4) => match firefly.swift_debtor_for_last4(last4) {
            Some(source) => source,
            None => {
                return Ok(Disposition::Review(format!(
                    "no debtor account configured for SWIFT debtor last-4 {last4} (RECEIPT_SWIFT_DEBTOR_BY_LAST4)"
                )));
            }
        },
    };

    // USD-equivalent ceiling — same gate as withdrawals (a crafted huge payment
    // must not silently move money). A transient rate outage propagates as `Err`
    // and is deferred by `run()`'s central classifier (kept in INBOX); a
    // permanent FX error also propagates (→ Review).
    if let Some(reason) = usd_ceiling_review(
        fx,
        policy,
        tr.money.currency.as_str(),
        tr.money.amount.value(),
        tr.date,
    )
    .await?
    {
        return Ok(Disposition::Review(reason));
    }

    // The transfer gate mints a `ValidatedTransfer`; `submit_transfer_between`
    // cannot be called without one, so the gate is impossible to skip.
    let transfer = match validate_transfer(tr.money, tr.date, tr.description, tr.external_id) {
        TransferVerdict::Booked(t) => t,
        TransferVerdict::Review { reason } => return Ok(Disposition::Review(reason)),
    };

    if dry_run {
        info!(
            external_id = transfer.external_id(),
            "DRY RUN: would book payment transfer"
        );
        return Ok(Disposition::Booked);
    }
    // `submit_transfer_between` books cross-currency (USD→EUR) by FX-converting
    // the destination leg, but still requires the transfer currency to match the
    // SOURCE account currency; if it doesn't, the source-leg debit is unknown and
    // it returns a typed CurrencyMismatch we route to Review (never a guessed
    // debit). A transient FX outage propagates as `Err` → deferred by `run()`.
    match firefly
        .submit_transfer_between(&transfer, source, dest)
        .await?
    {
        TransferSubmit::Submitted(SubmitOutcome::Created) => Ok(Disposition::Booked),
        TransferSubmit::Submitted(SubmitOutcome::Duplicate) => Ok(Disposition::Duplicate),
        TransferSubmit::CurrencyMismatch { reason } => Ok(Disposition::Review(reason)),
    }
}

/// Apply the USD-equivalent ceiling (`RECEIPT_MAX_AMOUNT`) to one record's
/// money. Returns `Ok(None)` when within the ceiling (or no ceiling set), or
/// `Ok(Some(reason))` when it exceeds and should route to Review. FX-dependent,
/// hence async and out of the pure `validate` gate. Shared by the notification
/// and statement paths so there is one ceiling implementation.
pub(crate) async fn usd_ceiling_review(
    fx: &FxClient<'_>,
    policy: &ValidationPolicy,
    currency: &str,
    amount: rust_decimal::Decimal,
    date: chrono::NaiveDate,
) -> Result<Option<String>> {
    let Some(ceiling) = policy.max_amount else {
        return Ok(None);
    };
    let rate = fx
        .rate(currency, "USD", date)
        .await
        .context("resolving FX rate for USD ceiling")?;
    match usd_ceiling::check(amount, rate, Some(ceiling)) {
        CeilingVerdict::Within { .. } => Ok(None),
        CeilingVerdict::Over {
            usd_equivalent,
            ceiling,
        } => Ok(Some(format!(
            "amount ≈ ${} (>{} USD) — routed to review",
            usd_equivalent.round_dp(2).normalize(),
            ceiling.normalize()
        ))),
    }
}

/// Move a message, logging (but not failing the run) on error — the
/// transaction is already booked/idempotent, so a failed move is non-fatal.
/// In dry-run the move is skipped (the message stays put to be re-observed).
async fn route(mailbox: &Mailbox, id: &str, processed: bool, dry_run: bool) {
    if dry_run {
        info!(%id, target = if processed { "Processed" } else { "Review" }, "DRY RUN: would move message");
        return;
    }
    let result = if processed {
        mailbox.move_to_processed(id).await
    } else {
        mailbox.move_to_review(id).await
    };
    if let Err(e) = result {
        warn!(%id, error = %e, "failed to move message");
    }
}

/// Build the shared HTTP client used by JMAP and Firefly. Its 120s default is a
/// sane cap for those request/response APIs; the LLM chat-completions path
/// overrides it per-request (see [`LlmClient`]) because a cold reasoning model
/// can run for minutes — far longer than mail or ledger calls should ever take.
pub fn build_http_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("receipt-ledger/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("building HTTP client")
}

#[cfg(test)]
mod book_transfer_tests {
    //! Routing tests for the SWIFT wire path through [`book_transfer`]. With no
    //! USD ceiling configured, `usd_ceiling_review` short-circuits before any FX
    //! call, and the unresolved-account paths return Review before any Firefly
    //! call — so these tests reach no network.

    use std::collections::HashMap;

    use chrono::NaiveDate;
    use reqwest::Client;

    use crate::adapters::{DestHint, SourceHint, TransferRecord};
    use crate::config::{AccountId, ValidationPolicy};
    use crate::firefly::FireflyClient;
    use crate::fx::FxClient;
    use crate::schema::{Amount, Currency, Money};

    use super::{Disposition, book_transfer};

    fn acct(id: &str) -> AccountId {
        AccountId::parse(id).unwrap()
    }

    /// A no-ceiling policy, so the FX-dependent USD ceiling is never consulted.
    fn no_ceiling() -> ValidationPolicy {
        ValidationPolicy { max_amount: None }
    }

    /// A SWIFT transfer record for the given creditor BIC and debtor last-4.
    fn swift_record(bic: &str, debtor_last4: &str) -> TransferRecord {
        TransferRecord {
            money: Money::new(
                Amount::parse("2100.00").unwrap(),
                Currency::parse("USD").unwrap(),
            ),
            date: NaiveDate::from_ymd_opt(2026, 5, 29).unwrap(),
            description: "SWIFT wire to RODOLFO HANSEN".to_string(),
            external_id: "swift:5dd60267-659f-446e-92c4-c1540b8f8253".to_string(),
            source: SourceHint::SwiftDebtorLast4(debtor_last4.to_string()),
            dest: DestHint::CreditorBic(bic.to_string()),
        }
    }

    /// A client wired with the SWIFT debtor last-4 and creditor BIC maps. The
    /// account-target cache is empty, so any path that reaches a submit would hit
    /// the network — the tests below all return Review before that point.
    fn client<'a>(http: &'a Client, fx: &'a FxClient<'a>) -> FireflyClient<'a> {
        FireflyClient::new(
            http,
            "http://firefly.invalid",
            "tok",
            fx,
            acct("103"),
            None,
            None,
            None,
            None,
            None,
            HashMap::new(), // PayPal funding map — empty for these SWIFT tests
            HashMap::from([("4189".to_string(), acct("127"))]), // SWIFT debtor map
            HashMap::from([("CHASUS33".to_string(), acct("1"))]),
        )
    }

    fn run(record: TransferRecord) -> Disposition {
        let http = Client::new();
        let fx = FxClient::new(&http, "http://fx.invalid");
        let c = client(&http, &fx);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(book_transfer(record, &c, &fx, &no_ceiling(), false))
            .expect("book_transfer should not error on these review paths")
    }

    #[test]
    fn unmapped_creditor_bic_routes_to_review() {
        // A wire to a BIC absent from RECEIPT_SWIFT_DEST_BY_BIC must NOT auto-book.
        match run(swift_record("DEUTDEFF", "4189")) {
            Disposition::Review(reason) => {
                assert!(
                    reason.contains("DEUTDEFF"),
                    "names the unmapped BIC: {reason}"
                );
                assert!(reason.contains("RECEIPT_SWIFT_DEST_BY_BIC"), "{reason}");
            }
            other => panic!("expected Review for unmapped BIC, got {other:?}"),
        }
    }

    #[test]
    fn unmapped_debtor_last4_routes_to_review() {
        // BIC maps, but the debtor last-4 does not → Review (no source guess).
        match run(swift_record("CHASUS33", "0000")) {
            Disposition::Review(reason) => {
                assert!(
                    reason.contains("0000"),
                    "names the unmapped last-4: {reason}"
                );
                assert!(reason.contains("RECEIPT_SWIFT_DEBTOR_BY_LAST4"), "{reason}");
            }
            other => panic!("expected Review for unmapped last-4, got {other:?}"),
        }
    }

    #[test]
    fn dry_run_books_when_both_legs_resolve() {
        // Both legs resolve (4189 → 127, CHASUS33 → 1). In dry-run the transfer
        // gate passes and book_transfer reports Booked without any Firefly call.
        let http = Client::new();
        let fx = FxClient::new(&http, "http://fx.invalid");
        let c = client(&http, &fx);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let d = rt
            .block_on(book_transfer(
                swift_record("CHASUS33", "4189"),
                &c,
                &fx,
                &no_ceiling(),
                true,
            ))
            .expect("dry-run books without network");
        assert!(
            matches!(d, Disposition::Booked),
            "both legs resolve → Booked (dry-run)"
        );
    }
}

#[cfg(test)]
mod outcome_label_tests {
    use super::{Disposition, disposition_label, review_reason_category};

    #[test]
    fn disposition_label_is_bare_discriminant() {
        assert_eq!(disposition_label(&Disposition::Booked), "booked");
        assert_eq!(disposition_label(&Disposition::Duplicate), "duplicate");
        // The payload String (which may carry PII) is never reflected in the label.
        assert_eq!(
            disposition_label(&Disposition::Skipped("merchant Foo x-1234".into())),
            "skipped"
        );
        assert_eq!(
            disposition_label(&Disposition::Review("amount ≈ $9000".into())),
            "review"
        );
    }

    #[test]
    fn review_reason_category_maps_known_reasons() {
        assert_eq!(
            review_reason_category("not a recognisable forward"),
            "not_a_forward"
        );
        assert_eq!(
            review_reason_category("no adapter for sender x@y.com"),
            "no_adapter"
        );
        assert_eq!(
            review_reason_category("charge appears already booked by a statement (journal 5 'X')"),
            "double_book"
        );
        assert_eq!(
            review_reason_category("amount ≈ $9000 (>5000 USD) — routed to review"),
            "over_ceiling"
        );
        assert_eq!(
            review_reason_category(
                "transfer currency USD does not match source account currency DOP"
            ),
            "currency_mismatch"
        );
        assert_eq!(
            review_reason_category("no paying account configured for PayPal funding last-4 9999"),
            "no_account"
        );
        assert_eq!(
            review_reason_category("adapter extracted no records"),
            "extraction"
        );
        assert_eq!(
            review_reason_category("something we don't classify"),
            "other"
        );
    }
}
