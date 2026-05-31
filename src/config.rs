//! Configuration, parsed once from the environment at startup.
//!
//! Every field is read at the boundary in [`Config::from_env`]; the rest of the
//! program receives a fully-populated, validated `Config` and never touches
//! `std::env` again. Required secrets that are missing produce a hard error so
//! the CronJob fails loudly (and the `CronJobFailing` alert can fire) rather
//! than silently doing nothing.
//!
//! Account references are parsed into [`AccountId`] (numeric Firefly id) at this
//! boundary, so a misconfigured non-numeric account name fails the CronJob here
//! rather than producing an ambiguous routing path downstream.

use std::collections::HashMap;
use std::env;

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use std::str::FromStr;

/// Default JMAP base URL — the in-cluster Stalwart ClusterIP service. The
/// macvlan `mail.hr-home.xyz` is not reachable from ordinary pods.
const DEFAULT_JMAP_URL: &str = "http://stalwart.system.svc.cluster.local:8080";
const DEFAULT_JMAP_USER: &str = "ledger@example.test";
const DEFAULT_STATE_PATH: &str = "/state/jmap.state";
/// Persistent FX-rate cache, on the same `/state` volume as the JMAP cursor so
/// it survives between the hourly one-shot runs. Lets a statement that sits in
/// the INBOX for many cycles reuse already-fetched rates instead of re-hitting
/// Frankfurter / the rate-limited Banco Popular `consultaTasa` every run.
const DEFAULT_FX_CACHE_PATH: &str = "/state/fx-cache.json";
const DEFAULT_OLLAMA_URL: &str = "http://ollama-router.ai:11434/v1";
const DEFAULT_MODEL_ALLOWLIST: &str = "gemma4:e2b";
/// LLM chat-completions request timeout, in seconds. Generous because a cold
/// reasoning model on slow hardware (e.g. ternary-bonsai-8b on Strix Halo) can
/// take minutes to produce a full receipt extraction. Applies *only* to the
/// LLM request path — JMAP and Firefly keep the shared client's shorter timeout.
const DEFAULT_LLM_TIMEOUT_SECS: u64 = 600;
const DEFAULT_FIREFLY_URL: &str = "http://firefly:8080";
/// Default FX-rate provider — Frankfurter (ECB rates, key-free). The legacy
/// `.app` host 301-redirects to the `.dev` host; we pin the working base
/// directly so a deployment that does not override `RECEIPT_FX_URL` still
/// resolves rates without depending on redirect-following. Mirrors
/// [`crate::fx::DEFAULT_FX_URL`]; kept as a literal here so config has no
/// compile-time dependency on the fx module.
const DEFAULT_FX_URL: &str = "https://api.frankfurter.dev/v1";
/// Default Banco Popular `BPDConsultaTasa` rates endpoint (IBM API Connect
/// sandbox) — the full `consultaTasa` URL. Production and development share this
/// host per the OpenAPI `servers` block. Override with `RECEIPT_DOP_RATES_URL`.
const DEFAULT_DOP_RATES_URL: &str = "https://api.us-east-a.apiconnect.ibmappdomain.cloud/apiportalpopular/bpdsandbox/consultatasa/consultaTasa";
/// Default OAuth2 token endpoint (client-credentials grant) for the DOP rates
/// API. Override with `RECEIPT_DOP_TOKEN_URL`.
const DEFAULT_DOP_TOKEN_URL: &str = "https://api.us-east-a.apiconnect.ibmappdomain.cloud/apiportalpopular/bpdsandbox/bpd/Authentication/oauth2/token";
/// Default OAuth2 scope for the DOP rates API. Override with `RECEIPT_DOP_SCOPE`.
const DEFAULT_DOP_SCOPE: &str = "scope_1";
/// Default in-run retry budget (seconds) for transient DOP-rate failures: the
/// token+rates fetch is retried with exponential backoff up to this long before
/// the run defers (keeps the message in INBOX for the hourly cron to retry).
/// Kept well inside the job's `activeDeadlineSeconds`. Override with
/// `RECEIPT_DOP_RETRY_BUDGET_SECS`.
const DEFAULT_DOP_RETRY_BUDGET_SECS: u64 = 120;
const DEFAULT_PROCESSED_MAILBOX: &str = "Processed";
const DEFAULT_REVIEW_MAILBOX: &str = "Review";

/// A Firefly account reference: a numeric account id.
///
/// Our deployment routes against numeric ids (103/105/106/107). Parsing at the
/// config boundary means a non-numeric value (a stale account *name*) is a hard
/// startup error, not a silent mis-route. The name-based account path is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountId(String);

impl AccountId {
    /// Parse a numeric account id: trimmed, non-empty, all ASCII digits.
    pub fn parse(raw: &str) -> Result<Self> {
        let t = raw.trim();
        if !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()) {
            Ok(AccountId(t.to_string()))
        } else {
            anyhow::bail!("account id {raw:?} is not numeric (expected a Firefly account id)")
        }
    }

    /// The id as it appears in Firefly API paths and `source_id`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Deterministic validation policy, derived from configuration. Carries only
/// the knobs that gate booking.
///
/// Note: the only field is the *USD-equivalent* ceiling, which is FX-dependent
/// and therefore applied in the async pipeline ([`crate::process_message`]),
/// NOT in the pure [`crate::validate::validate`] gate. The struct lives in
/// config so the threshold is parsed once at the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidationPolicy {
    /// Plausibility ceiling for a single transaction amount, **interpreted as
    /// US dollars** (`RECEIPT_MAX_AMOUNT`). `None` means no upper bound. When
    /// `Some(max)`, a charge whose USD-equivalent (`fx_rate(currency→USD) ×
    /// amount`) strictly exceeds `max` routes to Review rather than booking.
    /// The conversion to USD happens in the pipeline because it needs a live FX
    /// rate; a non-USD charge like ₩100,000 (≈ $72) must NOT trip a $100,000
    /// ceiling, and the raw 100000 figure alone cannot tell us that.
    pub max_amount: Option<Decimal>,
}

/// Fully-resolved runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub jmap_url: String,
    pub jmap_user: String,
    pub jmap_password: String,
    pub state_path: String,
    /// Path to the persistent FX-rate cache file (see [`DEFAULT_FX_CACHE_PATH`]).
    pub fx_cache_path: String,

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
    /// Banco Popular DOP-rate provider (`BPDConsultaTasa`). `None` when its
    /// credentials are unset — DOP conversions then fall through to Frankfurter
    /// (which has no DOP) and route to Review. See [`DopRateConfig`].
    pub dop_rate: Option<DopRateConfig>,
    /// PayPal Balance account in Firefly (asset, USD), by numeric id. Required:
    /// a PayPal record whose funding is *not* a credit product books here, so
    /// this is the safe default and must always be present.
    pub paypal_balance_account: AccountId,
    /// PayPal Credit account in Firefly (liability, USD), by numeric id. `None`
    /// when unconfigured; a credit-funded PayPal record then routes to Review.
    pub paypal_credit_account: Option<AccountId>,
    /// Funding-account lookup for PayPal Credit *payment* receipts, keyed by the
    /// funding instrument's last-4 (e.g. `"0130" → account 1`). The payment
    /// transfer's source leg is resolved here; a last-4 absent from this map (or
    /// an empty map) routes the payment to Review rather than guessing a source.
    /// Parsed from `RECEIPT_PAYING_ACCOUNT_BY_LAST4` (`last4:accountid` pairs).
    pub paying_account_by_last4: HashMap<String, AccountId>,
    /// Source-account lookup for outbound SWIFT wires, keyed by the debtor IBAN's
    /// last-4 (e.g. `"4189" → account 127`). DEDICATED to SWIFT — kept separate
    /// from [`paying_account_by_last4`](Config::paying_account_by_last4) so a
    /// PayPal funding card and a BPD IBAN that share a last-4 cannot collide. The
    /// wire transfer's source leg (the BPD debtor account) is resolved here; a
    /// last-4 absent from this map (or an empty map) routes the wire to Review.
    /// Parsed from `RECEIPT_SWIFT_DEBTOR_BY_LAST4` (`last4:accountid` pairs).
    pub swift_debtor_by_last4: HashMap<String, AccountId>,
    /// Destination-account lookup for outbound SWIFT wires, keyed by the
    /// creditor institution's normalized 8-char BIC (e.g. `"CHASUS33" → account
    /// 1`, `"ABNANL2A" → account 8`). The wire transfer's destination leg (the
    /// user's own foreign account) is resolved here; a BIC absent from this map
    /// (or an empty map) routes the wire to Review rather than guessing a — or
    /// auto-booking a wire to a third-party — account. Parsed from
    /// `RECEIPT_SWIFT_DEST_BY_BIC` (`BIC:accountid` pairs); keys are uppercased.
    pub swift_dest_by_bic: HashMap<String, AccountId>,
    /// Banco Popular VISA USD account (liability, USD), by numeric id. `None`
    /// when unconfigured; a non-DOP Banco Popular record then routes to Review.
    pub banco_popular_usd_account: Option<AccountId>,
    /// Banco Popular VISA DOP account (liability, DOP), by numeric id. `None`
    /// when unconfigured; a DOP Banco Popular record then routes to Review.
    pub banco_popular_dop_account: Option<AccountId>,
    /// Banco Popular USD savings account (asset, USD), by numeric id — the
    /// source of a USD-card statement payment booked as a transfer. `None` when
    /// unconfigured; a USD payment row then routes to Review.
    pub bp_paying_usd_account: Option<AccountId>,
    /// Banco Popular DOP checking account (asset, DOP), by numeric id — the
    /// source of a DOP-card statement payment booked as a transfer. `None` when
    /// unconfigured; a DOP payment row then routes to Review.
    pub bp_paying_dop_account: Option<AccountId>,
    /// Static password for the Banco Popular monthly-statement PDF (a SealedSecret
    /// in deployment). `None` disables statement ingestion — a statement-looking
    /// message then routes to Review rather than failing.
    pub bp_statement_password: Option<String>,
    /// Substring identifying who forwards the statement (e.g. the forwarding
    /// address). Combined with a PDF attachment to classify a message as a
    /// statement. `None` falls back to subject-based detection only.
    pub bp_statement_sender: Option<String>,
    /// Dry-run (`RECEIPT_DRY_RUN`): compute + log the full plan but perform **no**
    /// Firefly writes, **no** mailbox moves, and **no** JMAP state advance — so a
    /// run can be repeated and observed (via logs) before booking for real.
    pub dry_run: bool,
    /// Title of a Firefly rule-group whose `description_contains → set destination
    /// account` rules define merchant aliases. The reconciler reads it and
    /// applies the same canonicalization to both the statement merchant and the
    /// booked journal before fuzzy matching. `None` disables alias lookup.
    pub bp_alias_rule_group: Option<String>,
    /// Phase 2: auto-correct a matched foreign charge's booked amount to the
    /// statement's billed figure (`RECEIPT_BP_AUTOCORRECT_AMOUNTS`, default off).
    /// Off → an amount mismatch reports + routes to Review (Phase-1 behavior). On
    /// → the reconciler PUTs the billed amount, guarded by a TOCTOU check and
    /// [`bp_max_correction_pct`](Self::bp_max_correction_pct), tagging the old
    /// estimate for audit.
    pub bp_autocorrect_amounts: bool,
    /// Phase 2: the maximum `|billed − estimate| / estimate` (as a percent) an
    /// auto-correction may apply; beyond it the charge routes to Review instead
    /// (`RECEIPT_BP_MAX_CORRECTION_PCT`, default 20). A guard against a crafted or
    /// mis-parsed statement amount silently overwriting a journal with a wild value.
    pub bp_max_correction_pct: Decimal,
    /// Phase 2: symmetric double-book guard on the *consumo* path
    /// (`RECEIPT_BP_DOUBLE_BOOK_PROBE`, default off). When on, before booking a
    /// Banco Popular charge the pipeline probes the account's in-window journals
    /// for a statement booking (`bpstmt:`) that plausibly already represents it
    /// (same fuzzy merchant + date-window signals as the reconciler) and routes
    /// to Review instead of double-booking — closing the late-notification race
    /// where a statement booked a charge before its consumo arrived. Off → the
    /// consumo books as before (dedup only catches an identical re-send).
    pub bp_double_book_probe: bool,
    /// Phase 2: MCC→Firefly-category map for statement charge bookings, parsed
    /// from `RECEIPT_BP_MCC_CATEGORY_MAP` (`mcc:category` pairs, e.g.
    /// `"5411:Groceries,5814:Eating Out"`). A booked statement charge whose MCC is
    /// in the map gets that Firefly `category_name`; an unmapped (or absent) MCC
    /// books uncategorized. Empty by default.
    pub bp_mcc_category: HashMap<String, String>,

    /// Deterministic validation policy applied to every extracted record.
    pub validation: ValidationPolicy,

    pub processed_mailbox: String,
    pub review_mailbox: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let model_allowlist = env_or("RECEIPT_MODEL_ALLOWLIST", DEFAULT_MODEL_ALLOWLIST)
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>();

        Ok(Config {
            jmap_url: env_or("RECEIPT_JMAP_URL", DEFAULT_JMAP_URL),
            jmap_user: env_or("RECEIPT_JMAP_USER", DEFAULT_JMAP_USER),
            jmap_password: required("RECEIPT_JMAP_PASSWORD")?,
            state_path: env_or("RECEIPT_STATE_PATH", DEFAULT_STATE_PATH),
            fx_cache_path: env_or("RECEIPT_FX_CACHE_PATH", DEFAULT_FX_CACHE_PATH),

            ollama_url: env_or("RECEIPT_OLLAMA_URL", DEFAULT_OLLAMA_URL),
            model_allowlist,
            llm_timeout: std::time::Duration::from_secs(env_u64(
                "RECEIPT_LLM_TIMEOUT_SECS",
                DEFAULT_LLM_TIMEOUT_SECS,
            )?),

            firefly_url: env_or("RECEIPT_FIREFLY_URL", DEFAULT_FIREFLY_URL),
            firefly_token: required("FIREFLY_III_ACCESS_TOKEN")?,
            fx_url: env_or("RECEIPT_FX_URL", DEFAULT_FX_URL),
            dop_rate: dop_rate_from_env()?,
            // No sensible default — the safe-default PayPal account must always
            // point at a real numeric Firefly account id.
            paypal_balance_account: account_required("RECEIPT_PAYPAL_BALANCE_ACCOUNT")?,
            // Optional — absent means credit-funded PayPal mail routes to Review.
            paypal_credit_account: account_optional("RECEIPT_PAYPAL_CREDIT_ACCOUNT")?,
            // Optional — absent/empty means PayPal-payment receipts route to
            // Review (no source account can be resolved from the funding last-4).
            paying_account_by_last4: account_map_by_last4("RECEIPT_PAYING_ACCOUNT_BY_LAST4")?,
            // Optional — absent/empty means SWIFT wires route to Review (no
            // source account can be resolved from the debtor IBAN last-4).
            // Dedicated to SWIFT so a PayPal funding last-4 cannot collide.
            swift_debtor_by_last4: account_map_by_last4("RECEIPT_SWIFT_DEBTOR_BY_LAST4")?,
            // Optional — absent/empty means SWIFT wires route to Review (no
            // destination account can be resolved from the creditor BIC).
            swift_dest_by_bic: account_map_by_bic("RECEIPT_SWIFT_DEST_BY_BIC")?,
            // Optional — absent means non-DOP Banco Popular mail routes to Review.
            banco_popular_usd_account: account_optional("RECEIPT_BANCO_POPULAR_USD_ACCOUNT")?,
            // Optional — absent means DOP Banco Popular mail routes to Review.
            banco_popular_dop_account: account_optional("RECEIPT_BANCO_POPULAR_DOP_ACCOUNT")?,
            // Optional paying accounts for statement payments booked as transfers
            // — absent means the matching payment rows route to Review.
            bp_paying_usd_account: account_optional("RECEIPT_BP_PAYING_USD_ACCOUNT")?,
            bp_paying_dop_account: account_optional("RECEIPT_BP_PAYING_DOP_ACCOUNT")?,
            bp_statement_password: optional("RECEIPT_BP_STATEMENT_PASSWORD"),
            bp_statement_sender: optional("RECEIPT_BP_STATEMENT_SENDER"),
            dry_run: env_bool("RECEIPT_DRY_RUN"),
            bp_alias_rule_group: optional("RECEIPT_BP_ALIAS_RULE_GROUP"),
            bp_autocorrect_amounts: env_bool("RECEIPT_BP_AUTOCORRECT_AMOUNTS"),
            bp_max_correction_pct: decimal_optional("RECEIPT_BP_MAX_CORRECTION_PCT")?
                .unwrap_or_else(|| Decimal::from(20)),
            bp_double_book_probe: env_bool("RECEIPT_BP_DOUBLE_BOOK_PROBE"),
            bp_mcc_category: mcc_category_map("RECEIPT_BP_MCC_CATEGORY_MAP")?,

            validation: ValidationPolicy {
                max_amount: decimal_optional("RECEIPT_MAX_AMOUNT")?,
            },

            processed_mailbox: env_or("RECEIPT_PROCESSED_MAILBOX", DEFAULT_PROCESSED_MAILBOX),
            review_mailbox: env_or("RECEIPT_REVIEW_MAILBOX", DEFAULT_REVIEW_MAILBOX),
        })
    }
}

/// Credentials + endpoints for the Banco Popular `BPDConsultaTasa` DOP-rate API.
/// Present only when both `RECEIPT_DOP_CLIENT_ID` and `RECEIPT_DOP_CLIENT_SECRET`
/// are set (URLs and scope default to the sandbox values). The client id doubles
/// as the OAuth2 client id *and* the `X-IBM-Client-Id` header.
#[derive(Debug, Clone)]
pub struct DopRateConfig {
    pub rates_url: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub scope: String,
    /// In-run retry budget for transient (5xx/network) failures before deferring.
    pub retry_budget: std::time::Duration,
}

/// Build [`DopRateConfig`] from the environment. Absent credentials → `None`
/// (DOP support disabled). Exactly one of id/secret set is a hard error — a
/// half-configured provider must fail loudly, not silently disable DOP.
fn dop_rate_from_env() -> Result<Option<DopRateConfig>> {
    match (
        optional("RECEIPT_DOP_CLIENT_ID"),
        optional("RECEIPT_DOP_CLIENT_SECRET"),
    ) {
        (None, None) => Ok(None),
        (Some(client_id), Some(client_secret)) => Ok(Some(DopRateConfig {
            rates_url: env_or("RECEIPT_DOP_RATES_URL", DEFAULT_DOP_RATES_URL),
            token_url: env_or("RECEIPT_DOP_TOKEN_URL", DEFAULT_DOP_TOKEN_URL),
            client_id,
            client_secret,
            scope: env_or("RECEIPT_DOP_SCOPE", DEFAULT_DOP_SCOPE),
            retry_budget: std::time::Duration::from_secs(env_u64(
                "RECEIPT_DOP_RETRY_BUDGET_SECS",
                DEFAULT_DOP_RETRY_BUDGET_SECS,
            )?),
        })),
        _ => anyhow::bail!(
            "RECEIPT_DOP_CLIENT_ID and RECEIPT_DOP_CLIENT_SECRET must be set together (or neither)"
        ),
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Read a boolean env var: true for `1`/`true`/`yes` (case-insensitive), else
/// false (incl. unset).
fn env_bool(key: &str) -> bool {
    optional(key).is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

/// Read an optional env var, returning `None` when unset or blank.
fn optional(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn required(key: &str) -> Result<String> {
    let v = env::var(key).with_context(|| format!("required env var {key} is not set"))?;
    if v.trim().is_empty() {
        anyhow::bail!("required env var {key} is empty");
    }
    Ok(v)
}

/// Parse a required numeric [`AccountId`] env var.
fn account_required(key: &str) -> Result<AccountId> {
    AccountId::parse(&required(key)?).with_context(|| format!("env var {key}"))
}

/// Parse an optional numeric [`AccountId`] env var. Absent → `None`; present but
/// non-numeric → hard error (a stale account name must not silently disable a
/// route).
fn account_optional(key: &str) -> Result<Option<AccountId>> {
    match optional(key) {
        None => Ok(None),
        Some(v) => AccountId::parse(&v)
            .map(Some)
            .with_context(|| format!("env var {key}")),
    }
}

/// Read and parse a `last4:accountid`-pair map env var (e.g. `"0130:1,5678:2"`).
/// Absent/blank → an empty map (the PayPal-payment path then routes to Review).
/// A malformed entry — missing the `:`, a blank last-4, or a non-numeric
/// [`AccountId`] — is a hard startup error, like the other config parse
/// failures, so a typo fails the CronJob loudly rather than silently dropping a
/// funding-source route.
fn account_map_by_last4(key: &str) -> Result<HashMap<String, AccountId>> {
    match optional(key) {
        None => Ok(HashMap::new()),
        Some(raw) => parse_account_map_by_last4(&raw).with_context(|| format!("env var {key}")),
    }
}

/// Read and parse a `BIC:accountid`-pair map env var (e.g.
/// `"CHASUS33:1,ABNANL2A:8"`). Absent/blank → an empty map (the SWIFT path then
/// routes to Review). Parses exactly like [`account_map_by_last4`] but uppercases
/// each BIC key so lookups against the normalized creditor BIC are
/// case-insensitive. A malformed entry is a hard startup error.
fn account_map_by_bic(key: &str) -> Result<HashMap<String, AccountId>> {
    match optional(key) {
        None => Ok(HashMap::new()),
        Some(raw) => parse_account_map_by_bic(&raw).with_context(|| format!("env var {key}")),
    }
}

/// Parse a `last4:accountid`-pair map from a comma-separated string. Pure — the
/// env read lives in [`account_map_by_last4`] — so the format is unit-testable
/// without mutating the process environment.
fn parse_account_map_by_last4(raw: &str) -> Result<HashMap<String, AccountId>> {
    parse_account_map(raw, "last-4", |k| k.to_string())
}

/// Parse a `BIC:accountid`-pair map (keys uppercased). Pure — see
/// [`account_map_by_bic`].
fn parse_account_map_by_bic(raw: &str) -> Result<HashMap<String, AccountId>> {
    parse_account_map(raw, "BIC", |k| k.to_ascii_uppercase())
}

/// Parse a comma-separated `key:accountid`-pair map, applying `normalize_key` to
/// each key. Shared by the last-4 and BIC maps so the `key:id` grammar,
/// whitespace tolerance, and hard-error-on-malformed behaviour stay identical.
/// `key_label` only sharpens the error message. Pure.
fn parse_account_map(
    raw: &str,
    key_label: &str,
    normalize_key: impl Fn(&str) -> String,
) -> Result<HashMap<String, AccountId>> {
    parse_kv_map(raw, &format!("{key_label}:accountid"), normalize_key, |v| {
        AccountId::parse(v)
    })
}

/// Parse a comma-separated `key:value`-pair map. `normalize_key` is applied to
/// each key; `parse_value` parses (and validates) each value. Only the FIRST `:`
/// of an entry splits key from value, so a value may itself contain `:`-free
/// spaces (e.g. a category name). A missing `:`, an empty key, or a `parse_value`
/// failure is a hard error. Shared by the account-id maps (last-4 / BIC) and the
/// MCC→category map so the entry grammar and error behaviour stay identical.
fn parse_kv_map<V>(
    raw: &str,
    label: &str,
    normalize_key: impl Fn(&str) -> String,
    parse_value: impl Fn(&str) -> Result<V>,
) -> Result<HashMap<String, V>> {
    let mut map = HashMap::new();
    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (key, value) = entry
            .split_once(':')
            .with_context(|| format!("entry {entry:?} is not `{label}`"))?;
        let key = key.trim();
        if key.is_empty() {
            anyhow::bail!("entry {entry:?} has an empty key");
        }
        let value = parse_value(value).with_context(|| format!("entry {entry:?}"))?;
        map.insert(normalize_key(key), value);
    }
    Ok(map)
}

/// Read and parse the MCC→category map env var (e.g.
/// `"5411:Groceries,5814:Eating Out"`): comma-separated `mcc:category` pairs.
/// The key is a 4-digit MCC; the value is a free-text Firefly category name (may
/// contain spaces — only `,` separates entries and the FIRST `:` splits key from
/// value). Absent/blank → empty map (charges book uncategorized). A malformed
/// entry (no `:`, empty key, or empty category) is a hard startup error.
fn mcc_category_map(key: &str) -> Result<HashMap<String, String>> {
    match optional(key) {
        None => Ok(HashMap::new()),
        Some(raw) => parse_mcc_category_map(&raw).with_context(|| format!("env var {key}")),
    }
}

/// Parse comma-separated `mcc:category` pairs into a map. The value is free text
/// (only the FIRST `:` splits key from value, so a category may itself contain no
/// `:` but may contain spaces). Empty mcc or category is a hard error. Pure.
fn parse_mcc_category_map(raw: &str) -> Result<HashMap<String, String>> {
    parse_kv_map(
        raw,
        "mcc:category",
        |k| k.to_string(),
        |v| {
            let v = v.trim();
            if v.is_empty() {
                anyhow::bail!("empty category");
            }
            Ok(v.to_string())
        },
    )
}

/// Parse an optional [`Decimal`] env var. Absent → `None`; present but
/// unparseable → hard error.
fn decimal_optional(key: &str) -> Result<Option<Decimal>> {
    match optional(key) {
        None => Ok(None),
        Some(v) => Decimal::from_str(&v)
            .map(Some)
            .with_context(|| format!("env var {key}={v:?} is not a decimal")),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcc_category_map_parses_pairs_incl_spaces() {
        let m = parse_mcc_category_map("5411:Groceries, 5814:Eating Out").unwrap();
        assert_eq!(m.get("5411").map(String::as_str), Some("Groceries"));
        // A category with a space is preserved (only `,` and the first `:` delimit).
        assert_eq!(m.get("5814").map(String::as_str), Some("Eating Out"));
        // Empty input → empty map (charges book uncategorized).
        assert!(parse_mcc_category_map("").unwrap().is_empty());
        // Malformed entries are hard errors.
        assert!(parse_mcc_category_map("5411").is_err(), "missing colon");
        assert!(parse_mcc_category_map("5411:").is_err(), "empty category");
        assert!(parse_mcc_category_map(":Groceries").is_err(), "empty mcc");
    }

    #[test]
    fn account_id_parses_numeric() {
        assert_eq!(AccountId::parse(" 103 ").unwrap().as_str(), "103");
    }

    #[test]
    fn account_id_rejects_non_numeric() {
        assert!(AccountId::parse("PayPal Balance").is_err());
        assert!(AccountId::parse("").is_err());
        assert!(AccountId::parse("10a").is_err());
    }

    #[test]
    fn paying_account_map_parses_pairs() {
        let map = parse_account_map_by_last4("0130:1,5678:2").unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("0130").unwrap().as_str(), "1");
        assert_eq!(map.get("5678").unwrap().as_str(), "2");
        // Whitespace around entries and inside a pair is tolerated.
        let spaced = parse_account_map_by_last4(" 0130 : 1 , 5678:2 ").unwrap();
        assert_eq!(spaced.get("0130").unwrap().as_str(), "1");
        // Blank → an empty map (routes payments to Review, not an error).
        assert!(parse_account_map_by_last4("").unwrap().is_empty());
    }

    #[test]
    fn paying_account_map_rejects_malformed() {
        // No colon separator.
        assert!(parse_account_map_by_last4("0130").is_err());
        // Non-numeric account id.
        assert!(parse_account_map_by_last4("0130:abc").is_err());
        // Empty last-4 key.
        assert!(parse_account_map_by_last4(":1").is_err());
    }

    #[test]
    fn swift_dest_by_bic_parses_pairs() {
        let map = parse_account_map_by_bic("CHASUS33:1,ABNANL2A:8").unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("CHASUS33").unwrap().as_str(), "1");
        assert_eq!(map.get("ABNANL2A").unwrap().as_str(), "8");
        // Keys are uppercased so a lower-case env value still matches the
        // normalized (uppercase) creditor BIC.
        let lower = parse_account_map_by_bic("chasus33:1").unwrap();
        assert_eq!(lower.get("CHASUS33").unwrap().as_str(), "1");
        // Blank → an empty map (routes wires to Review, not an error).
        assert!(parse_account_map_by_bic("").unwrap().is_empty());
    }

    #[test]
    fn swift_dest_by_bic_rejects_malformed() {
        // No colon separator.
        assert!(parse_account_map_by_bic("CHASUS33").is_err());
        // Non-numeric account id.
        assert!(parse_account_map_by_bic("CHASUS33:abc").is_err());
        // Empty BIC key.
        assert!(parse_account_map_by_bic(":1").is_err());
    }

    #[test]
    fn swift_debtor_by_last4_parses_independently_of_paying_map() {
        // Fix 5: the dedicated SWIFT debtor map parses with the same `last4:id`
        // grammar as the PayPal funding map, but is a SEPARATE map — so a last-4
        // shared between a PayPal card and a BPD IBAN resolves to different
        // accounts without colliding.
        let swift = parse_account_map_by_last4("4189:127").unwrap();
        assert_eq!(swift.get("4189").unwrap().as_str(), "127");
        let paypal = parse_account_map_by_last4("4189:1").unwrap();
        assert_eq!(paypal.get("4189").unwrap().as_str(), "1");
        // Same key, different maps → no collision.
        assert_ne!(
            swift.get("4189").unwrap().as_str(),
            paypal.get("4189").unwrap().as_str()
        );
        // Blank → an empty map (routes wires to Review, not an error).
        assert!(parse_account_map_by_last4("").unwrap().is_empty());
        // Malformed entries are a hard error, like the other maps.
        assert!(parse_account_map_by_last4("4189").is_err());
        assert!(parse_account_map_by_last4("4189:abc").is_err());
    }
}
