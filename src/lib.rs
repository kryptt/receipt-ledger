//! receipt-ledger library crate.
//!
//! The binary (`main.rs`) is a thin shell over [`run`]; everything testable
//! lives here so integration tests in `tests/` can exercise the deterministic
//! core (schema, unwrap, adapters, validate, dedup) directly.

pub mod adapters;
pub mod config;
pub mod dedup;
pub mod firefly;
pub mod jmap;
pub mod llm;
pub mod model_selection;
pub mod schema;
pub mod unwrap;
pub mod validate;

use anyhow::{Context, Result};
use reqwest::Client;
use tracing::{info, warn};

use crate::config::Config;
use crate::firefly::{FireflyClient, SubmitOutcome};
use crate::jmap::{FetchedMessage, Mailbox};
use crate::llm::LlmClient;
use crate::validate::{Verdict, validate};

/// Tallies for the end-of-run summary.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Summary {
    pub processed: usize,
    pub booked: usize,
    pub duplicates: usize,
    pub review: usize,
}

/// Run the full one-shot pipeline. Returns the run summary on success; an
/// `Err` here is a real (non-zero-exit) failure.
pub async fn run() -> Result<Summary> {
    let cfg = Config::from_env().context("loading configuration")?;
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
        jmap::save_state(&cfg.state_path, &new_state).context("saving JMAP state")?;
        return Ok(Summary::default());
    }
    info!(count = messages.len(), "new messages to process");

    // Model selection is shared across the batch — pick once per run.
    let model = model_selection::select_model(&http, &cfg.ollama_url, &cfg.model_allowlist)
        .await
        .context("selecting extraction model")?;
    info!(%model, "using extraction model");
    let llm = LlmClient::new(&http, &cfg.ollama_url, model, cfg.llm_timeout);
    let firefly =
        FireflyClient::new(&http, &cfg.firefly_url, &cfg.firefly_token, &cfg.paypal_account);

    // --- per-message pipeline --------------------------------------------
    let mut summary = Summary::default();
    for msg in &messages {
        summary.processed += 1;
        match process_message(msg, &llm, &firefly).await {
            Ok(Disposition::Booked) => {
                summary.booked += 1;
                route(&mailbox, &msg.id, true).await;
            }
            Ok(Disposition::Duplicate) => {
                summary.duplicates += 1;
                route(&mailbox, &msg.id, true).await;
            }
            Ok(Disposition::Review(reason)) => {
                summary.review += 1;
                warn!(id = %msg.id, %reason, "routing to review");
                route(&mailbox, &msg.id, false).await;
            }
            Err(e) => {
                // A per-message processing error is not fatal to the job.
                summary.review += 1;
                warn!(id = %msg.id, error = ?e, "processing error; routing to review");
                route(&mailbox, &msg.id, false).await;
            }
        }
    }

    // --- 7. Persist state cursor -----------------------------------------
    // Only after the batch is fully handled; a crash mid-batch re-processes
    // (dedup makes that safe) rather than skipping unprocessed mail.
    jmap::save_state(&cfg.state_path, &new_state).context("saving JMAP state")?;

    Ok(summary)
}

/// What happened to a single message.
enum Disposition {
    Booked,
    Duplicate,
    Review(String),
}

/// The deterministic-core + I/O pipeline for one message.
async fn process_message(
    msg: &FetchedMessage,
    llm: &LlmClient<'_>,
    firefly: &FireflyClient<'_>,
) -> Result<Disposition> {
    // 2. Unwrap the Gmail forward + detect the original sender.
    let unwrapped = match unwrap::unwrap_forward(&msg.text) {
        Some(u) => u,
        None => return Ok(Disposition::Review("not a recognisable forward".to_string())),
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

    // 4. LLM extraction via ollama-router.
    let prompt = adapter.prompt(&unwrapped.body);
    let json = llm.extract_json(&prompt).await.context("LLM extraction")?;
    let records = adapter.postprocess(&json).context("adapter postprocess")?;

    if records.is_empty() {
        return Ok(Disposition::Review("adapter extracted no records".to_string()));
    }

    // For a single-transaction source (PayPal v1) we expect one record. Process
    // each; the message disposition is the "best" outcome among them.
    let mut booked_any = false;
    let mut dup_any = false;
    let mut review_reason: Option<String> = None;

    for record in records {
        // 5. Validation gates (deterministic).
        match validate(record) {
            Verdict::Booked(record) => {
                // 6. Dedup key. 7. Submit to Firefly.
                let external_id = dedup::external_id(&record);
                match firefly.submit(&record, &external_id).await? {
                    SubmitOutcome::Created => booked_any = true,
                    SubmitOutcome::Duplicate => dup_any = true,
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

/// Move a message, logging (but not failing the run) on error — the
/// transaction is already booked/idempotent, so a failed move is non-fatal.
async fn route(mailbox: &Mailbox, id: &str, processed: bool) {
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
