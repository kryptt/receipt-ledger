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
                statements = summary.statements,
                corrected = summary.corrected,
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

/// Log output format, selected by `RECEIPT_LOG_FORMAT`. JSON (the default) is for
/// Loki ingestion + LogQL field queries; text is for readable local dev.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogFormat {
    Json,
    Text,
}

/// Parse `RECEIPT_LOG_FORMAT`. Unset/blank/unknown → JSON (the prod default, so a
/// misconfigured value never silently degrades observability); `text`/`plain`/
/// `compact` → text. Pure (the raw value is passed in) so it is unit-testable.
fn parse_log_format(raw: Option<&str>) -> LogFormat {
    match raw.map(str::trim) {
        Some("text") | Some("plain") | Some("compact") => LogFormat::Text,
        _ => LogFormat::Json,
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,receipt_ledger=info"));
    // `.json()` and `.compact()` return different builder types, so each arm must
    // terminate in its own `.init()` (no shared binding, no boxing). The format is
    // read directly from env here — `init_tracing` runs before `Config::from_env`,
    // so it must not depend on it.
    let format = parse_log_format(std::env::var("RECEIPT_LOG_FORMAT").ok().as_deref());
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Text => builder.with_target(false).init(),
    }
}

/// Install the ring crypto provider for rustls. We use reqwest's
/// `rustls-no-provider` feature to keep the default off, so we must install one
/// process-wide before any TLS happens.
fn install_crypto_provider() {
    // An `Err` means a provider is already installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use super::{LogFormat, parse_log_format};

    #[test]
    fn log_format_defaults_to_json_and_honours_text() {
        assert_eq!(parse_log_format(None), LogFormat::Json);
        assert_eq!(parse_log_format(Some("")), LogFormat::Json);
        assert_eq!(parse_log_format(Some("json")), LogFormat::Json);
        assert_eq!(parse_log_format(Some(" JSON ")), LogFormat::Json); // trimmed; unknown-case → default
        assert_eq!(parse_log_format(Some("text")), LogFormat::Text);
        assert_eq!(parse_log_format(Some("plain")), LogFormat::Text);
        assert_eq!(parse_log_format(Some("compact")), LogFormat::Text);
        // Unknown value falls back to the JSON default (never panics).
        assert_eq!(parse_log_format(Some("yaml")), LogFormat::Json);
    }
}
