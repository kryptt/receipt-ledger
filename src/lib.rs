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
pub mod unwrap;
pub mod usd_ceiling;
pub mod validate;

use anyhow::{Context, Result};
use reqwest::Client;
use tracing::{info, warn};

use crate::adapters::Outcome;
use crate::config::{Config, ValidationPolicy};
use crate::firefly::{FireflyClient, SubmitOutcome};
use crate::fx::FxClient;
use crate::jmap::{FetchedMessage, Mailbox};
use crate::llm::LlmClient;
use crate::statement::pipeline::{Ingest, classify_message, process_statement};
use crate::usd_ceiling::CeilingVerdict;
use crate::validate::{Verdict, validate};

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
}

/// Run the full one-shot pipeline. Returns the run summary on success; an
/// `Err` here is a real (non-zero-exit) failure.
pub async fn run() -> Result<Summary> {
    let cfg = Config::from_env().context("loading configuration")?;
    if cfg.dry_run {
        info!("DRY RUN enabled (RECEIPT_DRY_RUN): no Firefly writes, no mailbox moves, no state advance");
    }
    let http = build_http_client()?;

    // --- 1. JMAP read -----------------------------------------------------
    let mailbox = Mailbox::connect(&cfg).await.context("JMAP connect")?;
    let prior_state = jmap::load_state(&cfg.state_path);
    let (messages, new_state) = mailbox
        .fetch_new(prior_state)
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

    // Model selection is shared across the batch — pick once per run.
    let model = model_selection::select_model(&http, &cfg.ollama_url, &cfg.model_allowlist)
        .await
        .context("selecting extraction model")?;
    info!(%model, "using extraction model");
    let llm = LlmClient::new(&http, &cfg.ollama_url, model, cfg.llm_timeout);
    // FX over the shared client; converts foreign charges into the target
    // account's currency before booking. An FX error propagates from `submit`
    // and routes the message to Review (never books at a wrong amount).
    let fx = FxClient::new(&http, &cfg.fx_url);
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
    );

    // --- per-message pipeline --------------------------------------------
    let mut summary = Summary::default();
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
                    summary.review += report.review + report.amount_mismatch + report.unmatched_booked;
                    let clean = report.is_clean();
                    if clean {
                        info!(id = %msg.id, ?report, "statement clean → processed");
                    } else {
                        warn!(id = %msg.id, ?report, "statement has flags → review");
                    }
                    route(&mailbox, &msg.id, clean, cfg.dry_run).await;
                }
                Err(e) => {
                    summary.review += 1;
                    warn!(id = %msg.id, error = ?e, "statement processing error; routing to review");
                    route(&mailbox, &msg.id, false, cfg.dry_run).await;
                }
            }
            continue;
        }

        match process_message(msg, &llm, &firefly, &fx, &cfg.validation, cfg.dry_run).await {
            Ok(Disposition::Booked) => {
                summary.booked += 1;
                route(&mailbox, &msg.id, true, cfg.dry_run).await;
            }
            Ok(Disposition::Duplicate) => {
                summary.duplicates += 1;
                route(&mailbox, &msg.id, true, cfg.dry_run).await;
            }
            Ok(Disposition::Skipped(reason)) => {
                // Not a transaction at all — a clean skip, NOT a review. Move
                // to Processed: it never needed human eyes.
                summary.skipped += 1;
                info!(id = %msg.id, %reason, "not a transaction; skipping to processed");
                route(&mailbox, &msg.id, true, cfg.dry_run).await;
            }
            Ok(Disposition::Review(reason)) => {
                summary.review += 1;
                warn!(id = %msg.id, %reason, "routing to review");
                route(&mailbox, &msg.id, false, cfg.dry_run).await;
            }
            Err(e) => {
                // A per-message processing error is not fatal to the job.
                summary.review += 1;
                warn!(id = %msg.id, error = ?e, "processing error; routing to review");
                route(&mailbox, &msg.id, false, cfg.dry_run).await;
            }
        }
    }

    // --- 7. Persist state cursor -----------------------------------------
    // Only after the batch is fully handled; a crash mid-batch re-processes
    // (dedup makes that safe) rather than skipping unprocessed mail. Skipped in
    // dry-run so the same messages can be re-observed.
    if cfg.dry_run {
        info!("DRY RUN: not advancing JMAP state");
    } else {
        jmap::save_state(&cfg.state_path, &new_state).context("saving JMAP state")?;
    }

    Ok(summary)
}

/// What happened to a single message.
enum Disposition {
    Booked,
    Duplicate,
    /// Not a transaction notification at all — a clean skip (→ Processed).
    Skipped(String),
    Review(String),
}

/// The deterministic-core + I/O pipeline for one message.
async fn process_message(
    msg: &FetchedMessage,
    llm: &LlmClient<'_>,
    firefly: &FireflyClient<'_>,
    fx: &FxClient<'_>,
    policy: &ValidationPolicy,
    dry_run: bool,
) -> Result<Disposition> {
    // 2. Unwrap the Gmail forward (manual marker or auto-forward) + detect the
    //    original sender.
    let unwrapped = match unwrap::unwrap_message(msg.from.as_deref(), &msg.text) {
        Some(u) => u,
        None => {
            return Ok(Disposition::Review(
                "not a recognisable forward".to_string(),
            ));
        }
    };

    // 3. Route to the per-sender adapter.
    let adapter = match adapters::select(&unwrapped.original_sender) {
        Some(a) => a,
        None => {
            return Ok(Disposition::Review(format!(
                "no adapter for sender {}",
                unwrapped.original_sender
            )));
        }
    };

    // 3b. Deterministic pre-filter: if the body clearly is not a transaction
    //     notification (a PayPal shipping update, plan reminder, survey), skip
    //     it cleanly without an LLM call. Never a Review — it never was a
    //     transaction.
    if !adapter.is_transaction(&unwrapped.body) {
        return Ok(Disposition::Skipped(format!(
            "{} mail did not look like a transaction notification",
            adapter.name()
        )));
    }

    // 4. LLM extraction via ollama-router.
    let prompt = adapter.prompt(&unwrapped.body);
    let json = llm.extract_json(&prompt).await.context("LLM extraction")?;
    // `postprocess_with_body` applies any deterministic body-derived override
    // (PayPal P1: a cross-currency receipt's authoritative USD total) on top of
    // the model's extraction.
    let records = match adapter
        .postprocess_with_body(&json, &unwrapped.body)
        .context("adapter postprocess")?
    {
        Outcome::Transaction(records) => records,
        // The model classified the mail as a non-transaction → clean skip.
        Outcome::NotATransaction { reason } => return Ok(Disposition::Skipped(reason)),
    };

    if records.is_empty() {
        return Ok(Disposition::Review(
            "adapter extracted no records".to_string(),
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
                if let Some(reason) =
                    usd_ceiling_review(fx, policy, ext.currency().as_str(), ext.amount().value(), ext.date)
                        .await?
                {
                    review_reason.get_or_insert(reason);
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

    Ok(if booked_any {
        Disposition::Booked
    } else if dup_any {
        Disposition::Duplicate
    } else {
        Disposition::Review(review_reason.unwrap_or_else(|| "no record booked".to_string()))
    })
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
        CeilingVerdict::Over { usd_equivalent, ceiling } => Ok(Some(format!(
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
fn build_http_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("receipt-ledger/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("building HTTP client")
}
