//! Configuration, parsed once from the environment at startup.
//!
//! Every field is read at the boundary in [`Config::from_env`]; the rest of the
//! program receives a fully-populated, validated `Config` and never touches
//! `std::env` again. Required secrets that are missing produce a hard error so
//! the CronJob fails loudly (and the `CronJobFailing` alert can fire) rather
//! than silently doing nothing.

use std::env;

use anyhow::{Context, Result};

/// Default JMAP base URL — the in-cluster Stalwart ClusterIP service. The
/// macvlan `mail.hr-home.xyz` is not reachable from ordinary pods.
const DEFAULT_JMAP_URL: &str = "http://stalwart.system.svc.cluster.local:8080";
const DEFAULT_JMAP_USER: &str = "ledger@example.test";
const DEFAULT_STATE_PATH: &str = "/state/jmap.state";
const DEFAULT_OLLAMA_URL: &str = "http://ollama-router.ai:11434/v1";
const DEFAULT_MODEL_ALLOWLIST: &str = "gemma4:e2b";
const DEFAULT_FIREFLY_URL: &str = "http://firefly:8080";
const DEFAULT_PROCESSED_MAILBOX: &str = "Processed";
const DEFAULT_REVIEW_MAILBOX: &str = "Review";

/// Fully-resolved runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub jmap_url: String,
    pub jmap_user: String,
    pub jmap_password: String,
    pub state_path: String,

    pub ollama_url: String,
    /// Allowlisted extraction models, highest priority first.
    pub model_allowlist: Vec<String>,

    pub firefly_url: String,
    pub firefly_token: String,
    /// PayPal asset account in Firefly — name or numeric id.
    pub paypal_account: String,

    pub processed_mailbox: String,
    pub review_mailbox: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let model_allowlist = env_or(
            "RECEIPT_MODEL_ALLOWLIST",
            DEFAULT_MODEL_ALLOWLIST,
        )
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();

        Ok(Config {
            jmap_url: env_or("RECEIPT_JMAP_URL", DEFAULT_JMAP_URL),
            jmap_user: env_or("RECEIPT_JMAP_USER", DEFAULT_JMAP_USER),
            jmap_password: required("RECEIPT_JMAP_PASSWORD")?,
            state_path: env_or("RECEIPT_STATE_PATH", DEFAULT_STATE_PATH),

            ollama_url: env_or("RECEIPT_OLLAMA_URL", DEFAULT_OLLAMA_URL),
            model_allowlist,

            firefly_url: env_or("RECEIPT_FIREFLY_URL", DEFAULT_FIREFLY_URL),
            firefly_token: required("FIREFLY_III_ACCESS_TOKEN")?,
            // No sensible default — must point at a real Firefly asset account.
            paypal_account: required("RECEIPT_PAYPAL_ACCOUNT")?,

            processed_mailbox: env_or("RECEIPT_PROCESSED_MAILBOX", DEFAULT_PROCESSED_MAILBOX),
            review_mailbox: env_or("RECEIPT_REVIEW_MAILBOX", DEFAULT_REVIEW_MAILBOX),
        })
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn required(key: &str) -> Result<String> {
    let v = env::var(key)
        .with_context(|| format!("required env var {key} is not set"))?;
    if v.trim().is_empty() {
        anyhow::bail!("required env var {key} is empty");
    }
    Ok(v)
}
