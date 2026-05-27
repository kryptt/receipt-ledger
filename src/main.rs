//! receipt-ledger — one-shot transaction-email → Firefly III ingestor.
//!
//! Runs once per invocation (hourly Kubernetes CronJob) and exits with a
//! process status code:
//!
//! - **0** — success, including "nothing new to do".
//! - **non-zero** — a real failure (config/auth/connection error) so the
//!   `CronJobFailing` alert fires.
//!
//! Per-message parse/validation failures do *not* fail the job; they route the
//! offending message to the `Review` mailbox and the run continues. All of that
//! logic lives in the library crate ([`receipt_ledger::run`]); this binary is a
//! thin shell that wires logging, the crypto provider, and the exit code.

use std::process::ExitCode;

use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    install_crypto_provider();

    match receipt_ledger::run().await {
        Ok(summary) => {
            info!(
                processed = summary.processed,
                booked = summary.booked,
                duplicates = summary.duplicates,
                review = summary.review,
                skipped = summary.skipped,
                "run complete"
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = ?e, "fatal error");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,receipt_ledger=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

/// Install the ring crypto provider for rustls. We use reqwest's
/// `rustls-no-provider` feature to keep the default off, so we must install one
/// process-wide before any TLS happens.
fn install_crypto_provider() {
    // An `Err` means a provider is already installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
