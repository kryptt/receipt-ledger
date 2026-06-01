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
    install_crypto_provider();

    // Init logging + (optionally) traces. Returns `Some(Telemetry)` ONLY when
    // OTEL_EXPORTER_OTLP_ENDPOINT is set; unset → fmt-only, unchanged behavior.
    let telemetry = init_tracing();

    // The shared HTTP client for the money path (JMAP / Firefly / FX / LLM).
    let result = match receipt_ledger::build_http_client() {
        Ok(client) => receipt_ledger::run(client).await,
        // A client build failure is a real (config-level) failure: surface it as
        // a non-zero exit, same as any other fatal startup error.
        Err(e) => Err(e),
    };

    let exit = match result {
        Ok(summary) => {
            info!(
                processed = summary.processed,
                booked = summary.booked,
                duplicates = summary.duplicates,
                review = summary.review,
                skipped = summary.skipped,
                statements = summary.statements,
                corrected = summary.corrected,
                deferred = summary.deferred,
                "run complete"
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = ?e, "fatal error");
            ExitCode::FAILURE
        }
    };

    // Flush + shut down the tracer provider on a BOUNDED budget before the tokio
    // runtime drops. A slow/unreachable collector is abandoned within the timeout
    // and never changes `exit` — telemetry is strictly non-blocking and additive.
    if let Some(telemetry) = telemetry {
        telemetry.shutdown().await;
    }
    exit
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

/// Build the subscriber (fmt layer always; OpenTelemetry layer only when an OTLP
/// endpoint is configured) and install it process-wide. Returns `Some(Telemetry)`
/// when trace export is on, so `main` knows it owes a flush.
///
/// The format is read directly from env here — this runs before
/// `Config::from_env`, so it must not depend on it.
fn init_tracing() -> Option<receipt_ledger::telemetry::Telemetry> {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,receipt_ledger=info"));
    let json =
        parse_log_format(std::env::var("RECEIPT_LOG_FORMAT").ok().as_deref()) == LogFormat::Json;
    receipt_ledger::telemetry::init(json, filter)
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
