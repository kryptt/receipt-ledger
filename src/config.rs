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
/// LLM chat-completions request timeout, in seconds. Generous because a cold
/// reasoning model on slow hardware (e.g. ternary-bonsai-8b on Strix Halo) can
/// take minutes to produce a full receipt extraction. Applies *only* to the
/// LLM request path — JMAP and Firefly keep the shared client's shorter timeout.
const DEFAULT_LLM_TIMEOUT_SECS: u64 = 600;
const DEFAULT_FIREFLY_URL: &str = "http://firefly:8080";
/// Default FX-rate provider — Frankfurter (ECB rates, key-free). Mirrors
/// [`crate::fx::DEFAULT_FX_URL`]; kept as a literal here so config has no
/// compile-time dependency on the fx module.
const DEFAULT_FX_URL: &str = "https://api.frankfurter.app";
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
    /// Per-request timeout for the LLM chat-completions call.
    pub llm_timeout: std::time::Duration,

    pub firefly_url: String,
    pub firefly_token: String,
    /// FX-rate provider base URL (Frankfurter-compatible). Used to convert a
    /// foreign-currency charge into the target account's currency before
    /// booking. An FX failure routes the message to Review rather than booking
    /// the foreign number as the account currency.
    pub fx_url: String,
    /// PayPal Balance account in Firefly (asset, USD) — name or numeric id.
    /// Required: a PayPal record whose funding is *not* a credit product books
    /// here, so this is the safe default and must always be present.
    pub paypal_balance_account: String,
    /// PayPal Credit account in Firefly (liability, USD) — name or numeric id.
    /// `None` when unconfigured; a credit-funded PayPal record then routes to
    /// Review rather than booking against the balance account.
    pub paypal_credit_account: Option<String>,
    /// Banco Popular VISA USD account in Firefly (liability, USD) — name or
    /// numeric id. `None` when unconfigured; a non-DOP Banco Popular record then
    /// routes to Review.
    pub banco_popular_usd_account: Option<String>,
    /// Banco Popular VISA DOP account in Firefly (liability, DOP) — name or
    /// numeric id. `None` when unconfigured; a DOP Banco Popular record then
    /// routes to Review.
    pub banco_popular_dop_account: Option<String>,

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
            llm_timeout: std::time::Duration::from_secs(env_u64(
                "RECEIPT_LLM_TIMEOUT_SECS",
                DEFAULT_LLM_TIMEOUT_SECS,
            )?),

            firefly_url: env_or("RECEIPT_FIREFLY_URL", DEFAULT_FIREFLY_URL),
            firefly_token: required("FIREFLY_III_ACCESS_TOKEN")?,
            fx_url: env_or("RECEIPT_FX_URL", DEFAULT_FX_URL),
            // No sensible default — the safe-default PayPal account must always
            // point at a real Firefly account.
            paypal_balance_account: required("RECEIPT_PAYPAL_BALANCE_ACCOUNT")?,
            // Optional — absent means credit-funded PayPal mail routes to Review.
            paypal_credit_account: optional("RECEIPT_PAYPAL_CREDIT_ACCOUNT"),
            // Optional — absent means non-DOP Banco Popular mail routes to Review.
            banco_popular_usd_account: optional("RECEIPT_BANCO_POPULAR_USD_ACCOUNT"),
            // Optional — absent means DOP Banco Popular mail routes to Review.
            banco_popular_dop_account: optional("RECEIPT_BANCO_POPULAR_DOP_ACCOUNT"),

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

/// Read an optional env var, returning `None` when unset or blank. Used for
/// settings that are legitimately absent (e.g. a per-source account that has
/// not been provisioned yet).
fn optional(key: &str) -> Option<String> {
    env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn required(key: &str) -> Result<String> {
    let v = env::var(key)
        .with_context(|| format!("required env var {key} is not set"))?;
    if v.trim().is_empty() {
        anyhow::bail!("required env var {key} is empty");
    }
    Ok(v)
}

/// Parse an optional `u64` env var, falling back to `default` when unset/blank.
/// A present-but-unparseable value is a hard error — a typo'd timeout should
/// fail the CronJob loudly, not silently revert to the default.
fn env_u64(key: &str, default: u64) -> Result<u64> {
    match env::var(key).ok().filter(|v| !v.trim().is_empty()) {
        None => Ok(default),
        Some(v) => v
            .trim()
            .parse::<u64>()
            .with_context(|| format!("env var {key}={v:?} is not a non-negative integer")),
    }
}
