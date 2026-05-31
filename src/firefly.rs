//! Firefly III submission.
//!
//! Posts a single-split withdrawal transaction group to
//! `POST {base}/api/v1/transactions` with `error_if_duplicate_hash: true`.
//! Firefly answers a duplicate import with HTTP 422 — we parse the body and
//! treat *only* the duplicate-hash shape as success (the transaction is already
//! in the ledger), which makes re-runs idempotent. Any other 422 is a real
//! validation failure and routes to Review.
//!
//! Payload shape confirmed against the Firefly III v1 API docs: a transaction
//! group with a `transactions` array of splits; the group-level
//! `error_if_duplicate_hash` flag guards double-imports.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use chrono::NaiveDate;
use reqwest::{Client, StatusCode};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::AccountId;
use crate::fx::FxClient;
use crate::schema::{Amount, Currency, Direction, Extracted, Money, Source};
use crate::statement::reconcile::ExistingJournal;
use crate::validate::{Validated, ValidatedTransfer};

/// Tag attached to every transaction this service books, for easy filtering in
/// Firefly.
const IMPORT_TAG: &str = "receipt-ledger";

/// Outcome of a submission attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Newly created in Firefly.
    Created,
    /// Firefly reported it as a duplicate (422) — already imported.
    Duplicate,
}

/// Outcome of a *checked* transfer submission ([`FireflyClient::submit_transfer_between`]).
///
/// A transfer books both legs in the receipt's currency with no FX leg, which is
/// only correct when that currency matches BOTH account currencies. Rather than
/// risk a cross-currency mis-book (or a 422), the currency-agreement check runs
/// before submission and surfaces a mismatch as a typed [`Self::CurrencyMismatch`]
/// the caller routes to Review — distinct from a network/submit `Err`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a CurrencyMismatch must route the payment to Review, not be dropped"]
pub enum TransferSubmit {
    /// The transfer was submitted; carries the underlying create/duplicate result.
    Submitted(SubmitOutcome),
    /// The transfer currency did not match a leg's account currency — not
    /// submitted. The `reason` is ready to hand to the review mailbox.
    CurrencyMismatch { reason: String },
}

/// The authoritative booking target for an account, as Firefly reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    /// ISO-4217 currency code the account books in.
    currency: String,
    /// The currency's real minor-unit precision (e.g. 2 for USD/EUR, 0 for
    /// JPY/KRW). Conversions round to exactly this many places.
    decimals: u32,
}

pub struct FireflyClient<'a> {
    http: &'a Client,
    base_url: String,
    token: String,
    /// FX-rate resolver, used to convert a charge into the target account's
    /// currency before booking. An FX failure propagates as `Err` so the
    /// message routes to Review rather than booking at the wrong amount.
    fx: &'a FxClient<'a>,
    /// PayPal Balance account id. The safe default for any PayPal record whose
    /// funding is not a credit product, so always present.
    paypal_balance_account: AccountId,
    /// PayPal Credit account id. `None` when unconfigured; a credit-funded
    /// PayPal record then errors out (→ Review).
    paypal_credit_account: Option<AccountId>,
    /// Banco Popular VISA USD account id. `None` when unconfigured; a non-DOP
    /// Banco Popular record then errors out (→ Review).
    banco_popular_usd_account: Option<AccountId>,
    /// Banco Popular VISA DOP account id. `None` when unconfigured; a DOP Banco
    /// Popular record then errors out (→ Review).
    banco_popular_dop_account: Option<AccountId>,
    /// Banco Popular USD savings account — the source of a USD-card statement
    /// payment booked as a transfer. `None` → such payments error out (→ Review).
    bp_paying_usd_account: Option<AccountId>,
    /// Banco Popular DOP checking account — the source of a DOP-card statement
    /// payment booked as a transfer. `None` → such payments error out (→ Review).
    bp_paying_dop_account: Option<AccountId>,
    /// Funding-account lookup for PayPal Credit *payment* receipts, keyed by the
    /// funding card's last-4. The payment transfer's source leg is resolved here;
    /// a last-4 absent from this map (or an empty map) routes the payment to
    /// Review rather than guessing a source. SWIFT wires use the dedicated
    /// [`swift_debtor_by_last4`](Self::swift_debtor_by_last4) map instead, so a
    /// colliding last-4 cannot mis-route across the two sources.
    paying_account_by_last4: HashMap<String, AccountId>,
    /// Source-account lookup for outbound SWIFT wires, keyed by the debtor IBAN's
    /// last-4. Dedicated to SWIFT (kept separate from
    /// [`paying_account_by_last4`](Self::paying_account_by_last4)); a last-4
    /// absent from this map (or an empty map) routes the wire to Review.
    swift_debtor_by_last4: HashMap<String, AccountId>,
    /// Destination-account lookup for outbound SWIFT wires, keyed by the
    /// creditor institution's normalized 8-char BIC. The wire transfer's
    /// destination leg (the user's own foreign account) is resolved here; a BIC
    /// absent from this map (or an empty map) routes the wire to Review rather
    /// than auto-booking a wire to an unmapped/third-party account.
    swift_dest_by_bic: HashMap<String, AccountId>,
    /// Per-account-id target cache: numeric account id → its Firefly
    /// `currency_code` + `decimal_places`. Authoritative source of the
    /// conversion target so we book in the account's real currency at its real
    /// precision. Populated lazily on first use.
    account_target: Mutex<HashMap<String, Target>>,
}

#[derive(Serialize)]
struct TransactionGroup<'a> {
    error_if_duplicate_hash: bool,
    apply_rules: bool,
    transactions: Vec<Split<'a>>,
}

impl<'a> TransactionGroup<'a> {
    /// A one-split group with the standard guards — the single place
    /// `error_if_duplicate_hash` (idempotency) and `apply_rules` are set, so the
    /// withdrawal and transfer builders can't drift on them.
    fn single(split: Split<'a>) -> Self {
        TransactionGroup {
            error_if_duplicate_hash: true,
            // Let Firefly rules fire (operator-authored, part of the trusted base).
            apply_rules: true,
            transactions: vec![split],
        }
    }
}

/// A split's destination: either a name (Firefly creates/looks-up an expense
/// account — for withdrawals) or an existing account id (for transfers). A sum
/// type so a split can never set both or neither; it serializes to exactly one
/// of `destination_name` / `destination_id` (flattened into the split).
enum Destination<'a> {
    Name(&'a str),
    Id(&'a str),
}

impl Serialize for Destination<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(1))?;
        match self {
            Destination::Name(n) => m.serialize_entry("destination_name", n)?,
            Destination::Id(id) => m.serialize_entry("destination_id", id)?,
        }
        m.end()
    }
}

#[derive(Serialize)]
struct Split<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    date: String,
    amount: String,
    currency_code: &'a str,
    description: &'a str,
    external_id: &'a str,
    tags: Vec<&'a str>,
    /// Source account by numeric id.
    source_id: &'a str,
    /// Destination — name (withdrawal) or account id (transfer). Flattened to a
    /// single `destination_name`/`destination_id` key.
    #[serde(flatten)]
    destination: Destination<'a>,
    /// Original (pre-conversion) amount as a string, set only when the charge
    /// currency differs from the account currency. Firefly stores this as the
    /// transaction's "foreign amount" alongside the converted `amount`.
    #[serde(skip_serializing_if = "Option::is_none")]
    foreign_amount: Option<String>,
    /// ISO-4217 code of the original charge currency, paired with
    /// `foreign_amount`. Set only on a converted (cross-currency) split.
    #[serde(skip_serializing_if = "Option::is_none")]
    foreign_currency_code: Option<&'a str>,
}

impl<'a> FireflyClient<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        http: &'a Client,
        base_url: impl Into<String>,
        token: impl Into<String>,
        fx: &'a FxClient<'a>,
        paypal_balance_account: AccountId,
        paypal_credit_account: Option<AccountId>,
        banco_popular_usd_account: Option<AccountId>,
        banco_popular_dop_account: Option<AccountId>,
        bp_paying_usd_account: Option<AccountId>,
        bp_paying_dop_account: Option<AccountId>,
        paying_account_by_last4: HashMap<String, AccountId>,
        swift_debtor_by_last4: HashMap<String, AccountId>,
        swift_dest_by_bic: HashMap<String, AccountId>,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            token: token.into(),
            fx,
            paypal_balance_account,
            paypal_credit_account,
            banco_popular_usd_account,
            banco_popular_dop_account,
            bp_paying_usd_account,
            bp_paying_dop_account,
            paying_account_by_last4,
            swift_debtor_by_last4,
            swift_dest_by_bic,
            account_target: Mutex::new(HashMap::new()),
        }
    }

    /// Submit one validated, deduped record as a withdrawal.
    ///
    /// Requires a [`Validated`] — an unvalidated [`Extracted`] cannot reach this
    /// call, so the validation gate is impossible to skip. `external_id` is the
    /// dedup key computed by [`crate::dedup`].
    ///
    /// Resolves the target account, its authoritative currency + precision, and
    /// the FX rate from the charge currency to that target — then builds and
    /// posts the split. Any of those resolutions failing is an `Err`, which the
    /// pipeline turns into a per-message Review (never a mis-booked amount).
    pub async fn submit(&self, record: &Validated, external_id: &str) -> Result<SubmitOutcome> {
        let record = record.as_extracted();
        let account = self.route_account(record)?;
        let target = self.account_target(account.as_str()).await?;
        let rate = self
            .fx
            .rate(record.currency().as_str(), &target.currency, record.date)
            .await
            .context("resolving FX rate for conversion")?;

        let group = build_group(record, external_id, account.as_str(), &target, rate);
        self.post_group(&group, external_id, "transaction").await
    }

    /// POST a transaction group and classify the response — the one place that
    /// decides Created vs Duplicate vs hard failure, shared by [`submit`] and
    /// [`submit_transfer`]. A 422 whose body is Firefly's duplicate-hash shape is
    /// success-as-duplicate (idempotent re-run); any other 422 (or non-2xx) is a
    /// real failure that bails (→ the pipeline routes the message to Review),
    /// never silently treated as Processed.
    async fn post_group(
        &self,
        group: &TransactionGroup<'_>,
        external_id: &str,
        kind: &str,
    ) -> Result<SubmitOutcome> {
        let url = format!("{}/api/v1/transactions", self.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "application/json")
            .json(group)
            .send()
            .await
            .with_context(|| format!("sending Firefly {kind} request"))?;

        let status = resp.status();
        if status.is_success() {
            info!(%external_id, %kind, "booked in Firefly");
            return Ok(SubmitOutcome::Created);
        }
        if status == StatusCode::UNPROCESSABLE_ENTITY {
            let body = resp.text().await.unwrap_or_default();
            if is_duplicate_error(&body) {
                info!(%external_id, %kind, "already imported (duplicate hash)");
                return Ok(SubmitOutcome::Duplicate);
            }
            warn!(%external_id, %kind, %body, "Firefly 422 was not a duplicate");
            anyhow::bail!("Firefly rejected {kind} (422): {body}");
        }
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Firefly returned {status} on {kind}: {body}")
    }

    /// Route to the Firefly account id for this record. Exhaustive over
    /// `Source` so a new variant forces a routing decision here rather than
    /// silently booking against the wrong account. Within each source the
    /// funding/currency rules pick balance-vs-credit (PayPal) or USD-vs-DOP
    /// (Banco Popular). A needed-but-unconfigured `Option` account is an `Err`,
    /// which the pipeline turns into a per-message Review. Pure: depends only on
    /// the record and the configured account ids.
    fn route_account(&self, record: &Extracted) -> Result<&AccountId> {
        let account = match record.source {
            Source::Paypal => {
                if is_paypal_credit_funding(record) {
                    self.paypal_credit_account
                        .as_ref()
                        .context("no Firefly account configured for PayPal Credit")?
                } else {
                    &self.paypal_balance_account
                }
            }
            Source::BancoPopular => {
                if record.currency().as_str() == "DOP" {
                    self.banco_popular_dop_account
                        .as_ref()
                        .context("no Firefly account configured for Banco Popular DOP")?
                } else {
                    self.banco_popular_usd_account
                        .as_ref()
                        .context("no Firefly account configured for Banco Popular USD")?
                }
            }
        };
        Ok(account)
    }

    /// The authoritative booking [`Target`] (currency + precision) of an
    /// account id, as Firefly reports it.
    ///
    /// Does `GET {base}/api/v1/accounts/{id}` and reads
    /// `data.attributes.currency_code` + `currency_decimal_places`, caching the
    /// result per id so a batch hits the network at most once per account.
    async fn account_target(&self, account: &str) -> Result<Target> {
        // Recover a poisoned cache rather than panicking the whole batch.
        if let Some(t) = self
            .account_target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(account)
        {
            return Ok(t.clone());
        }

        let url = format!(
            "{}/api/v1/accounts/{}",
            self.base_url.trim_end_matches('/'),
            account
        );
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .with_context(|| format!("fetching Firefly account {account}"))?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("Firefly returned {status} fetching account {account}: {body}");
        }

        let target = parse_account_target(&body)
            .with_context(|| format!("reading currency for account {account}"))?;

        self.account_target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(account.to_string(), target.clone());
        Ok(target)
    }

    /// Submit a statement payment as a Firefly `transfer` (paying bank account →
    /// card liability). Requires a [`ValidatedTransfer`], so the gate cannot be
    /// skipped. Routes both legs by the transfer currency (DOP → checking→107,
    /// else → savings→106); a needed-but-unconfigured account is an `Err`
    /// (→ Review). No FX: both legs are the same currency.
    pub async fn submit_transfer(&self, transfer: &ValidatedTransfer) -> Result<SubmitOutcome> {
        let (source, dest) = self.route_transfer_accounts(transfer)?;
        let group = build_transfer_group(transfer, source.as_str(), dest.as_str());
        self.post_group(&group, transfer.external_id(), "transfer").await
    }

    /// Submit a transfer between two **explicitly supplied** accounts: `source`
    /// (the funding account) → `dest` (the credit/card liability). Used by the
    /// PayPal-payment path, where the pipeline has already resolved both account
    /// ids from config (dest = PayPal Credit; source = the funding last-4 map) —
    /// so no currency-based routing is needed here. Shares the same
    /// `build_transfer_group` + `post_group` plumbing as [`submit_transfer`], so
    /// both transfer entry points stay in lockstep on idempotency + duplicate
    /// handling.
    ///
    /// The transfer books both legs in `transfer.money().currency` with no FX
    /// leg, so it is only correct when that currency matches BOTH account
    /// currencies. This method reads both account [`Target`]s and verifies the
    /// agreement BEFORE submitting; a mismatch returns
    /// [`TransferSubmit::CurrencyMismatch`] (→ the caller routes to Review)
    /// rather than booking a silent cross-currency transfer. The actual
    /// `build_transfer_group`/`post_group` is reached only once agreement holds,
    /// so no caller can submit a mismatched transfer.
    pub async fn submit_transfer_between(
        &self,
        transfer: &ValidatedTransfer,
        source: &AccountId,
        dest: &AccountId,
    ) -> Result<TransferSubmit> {
        let source_target = self.account_target(source.as_str()).await?;
        let dest_target = self.account_target(dest.as_str()).await?;
        if let Some(reason) = transfer_currency_mismatch(
            transfer.money().currency.as_str(),
            &source_target.currency,
            &dest_target.currency,
        ) {
            return Ok(TransferSubmit::CurrencyMismatch { reason });
        }

        let group = build_transfer_group(transfer, source.as_str(), dest.as_str());
        let outcome = self.post_group(&group, transfer.external_id(), "transfer").await?;
        Ok(TransferSubmit::Submitted(outcome))
    }

    /// The configured PayPal Credit account, when present — the destination leg
    /// of a PayPal-payment transfer. `None` routes the payment to Review.
    #[must_use]
    pub fn paypal_credit_account(&self) -> Option<&AccountId> {
        self.paypal_credit_account.as_ref()
    }

    /// Resolve the funding (source) account for a PayPal-payment transfer from
    /// the receipt's `funding_last4` against the configured map. `None` when the
    /// last-4 is absent (or the map is empty) — the pipeline then routes the
    /// payment to Review rather than guessing a source account.
    #[must_use]
    pub fn paying_account_for_last4(&self, last4: &str) -> Option<&AccountId> {
        self.paying_account_by_last4.get(last4)
    }

    /// Resolve the source (debtor) account for an outbound SWIFT wire from the
    /// debtor IBAN's last-4 against the DEDICATED SWIFT debtor map. `None` when
    /// the last-4 is absent (or the map is empty) — the pipeline then routes the
    /// wire to Review rather than guessing a source account. Kept separate from
    /// [`paying_account_for_last4`](Self::paying_account_for_last4) so a PayPal
    /// funding last-4 and a BPD IBAN last-4 cannot collide.
    #[must_use]
    pub fn swift_debtor_for_last4(&self, last4: &str) -> Option<&AccountId> {
        self.swift_debtor_by_last4.get(last4)
    }

    /// Resolve the destination (foreign own-account) for an outbound SWIFT wire
    /// from the creditor institution's normalized 8-char BIC against the
    /// configured map. The lookup uppercases the BIC so it matches the
    /// uppercased map keys regardless of case. `None` when the BIC is absent (or
    /// the map is empty) — the pipeline then routes the wire to Review rather
    /// than auto-booking a wire to an unmapped/third-party account.
    #[must_use]
    pub fn swift_dest_for_bic(&self, bic: &str) -> Option<&AccountId> {
        self.swift_dest_by_bic.get(&bic.to_ascii_uppercase())
    }

    /// Resolve `(source paying account, destination card account)` for a transfer
    /// by its currency. DOP → checking + DOP card (107); anything else → USD
    /// savings + USD card (106). A needed-but-unconfigured account is an `Err`.
    fn route_transfer_accounts(
        &self,
        transfer: &ValidatedTransfer,
    ) -> Result<(&AccountId, &AccountId)> {
        let dop = transfer.money().currency.as_str() == "DOP";
        let (source, dest) = if dop {
            (
                self.bp_paying_dop_account.as_ref(),
                self.banco_popular_dop_account.as_ref(),
            )
        } else {
            (
                self.bp_paying_usd_account.as_ref(),
                self.banco_popular_usd_account.as_ref(),
            )
        };
        let kind = if dop { "DOP" } else { "USD" };
        let source = source.with_context(|| format!("no Banco Popular {kind} paying account configured"))?;
        let dest = dest.with_context(|| format!("no Banco Popular {kind} card account configured"))?;
        Ok((source, dest))
    }

    /// List this service's previously-booked **withdrawals** on `account` in the
    /// `[start, end]` window, as [`ExistingJournal`]s — the input the statement
    /// reconciler matches its charges against.
    ///
    /// Calls `GET {base}/api/v1/accounts/{id}/transactions?type=withdrawal&start&end`
    /// and walks `meta.pagination` to the last page. Only `receipt-ledger`-tagged
    /// splits are returned, so manually-entered transactions are not mistaken for
    /// our bookings.
    pub async fn list_transactions(
        &self,
        account: &AccountId,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<Vec<ExistingJournal>> {
        // A safety cap on the page walk: we trust the server's `total_pages`, but
        // a buggy/misbehaving endpoint reporting a huge value must not spin this
        // CronJob forever. A statement cycle is dozens of rows; 1000 pages is
        // astronomically more than real.
        const MAX_PAGES: u32 = 1000;

        let mut out = Vec::new();
        let mut skipped_total = 0usize;
        let mut page = 1u32;
        loop {
            let url = format!(
                "{}/api/v1/accounts/{}/transactions?type=withdrawal&start={start}&end={end}&page={page}",
                self.base_url.trim_end_matches('/'),
                account.as_str(),
            );
            let resp = self
                .http
                .get(&url)
                .bearer_auth(&self.token)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .with_context(|| format!("listing transactions for account {}", account.as_str()))?;

            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                anyhow::bail!(
                    "Firefly returned {status} listing account {}: {body}",
                    account.as_str()
                );
            }

            let (mut journals, pagination, skipped) = parse_transactions_page(&body)
                .with_context(|| format!("parsing transactions page {page} for {}", account.as_str()))?;
            out.append(&mut journals);
            skipped_total += skipped;

            if pagination.total_pages == 0 || page >= pagination.total_pages {
                break;
            }
            if page >= MAX_PAGES {
                warn!(account = account.as_str(), total_pages = pagination.total_pages, "pagination cap hit; stopping");
                break;
            }
            page += 1;
        }
        if skipped_total > 0 {
            // Loud, not silent: a non-zero skip means tagged splits failed to
            // parse — the wiring should treat this cycle's reconcile as suspect.
            warn!(
                account = account.as_str(),
                skipped = skipped_total,
                "skipped unparseable receipt-ledger transactions while listing"
            );
        }
        Ok(out)
    }

    /// Read the merchant-alias map from a Firefly rule-group identified by
    /// `group_title`: each rule's `description_*` trigger value paired with its
    /// `set_destination_account` action value (the canonical payee). The
    /// reconciler applies these to both sides before fuzzy matching. Returns an
    /// empty map (not an error) when the group doesn't exist.
    pub async fn fetch_alias_map(&self, group_title: &str) -> Result<Vec<(String, String)>> {
        let base = self.base_url.trim_end_matches('/');
        // 1. resolve the group id by title.
        let groups_body = self
            .get_json(&format!("{base}/api/v1/rule-groups?limit=200"))
            .await
            .context("listing Firefly rule-groups")?;
        let Some(group_id) = find_rule_group_id(&groups_body, group_title) else {
            warn!(group = group_title, "alias rule-group not found; no merchant aliases applied");
            return Ok(Vec::new());
        };
        // 2. read its rules and extract (trigger → canonical) pairs.
        let rules_body = self
            .get_json(&format!("{base}/api/v1/rule-groups/{group_id}/rules?limit=500"))
            .await
            .with_context(|| format!("listing rules in group {group_id}"))?;
        let map = parse_alias_rules(&rules_body)?;
        info!(group = group_title, aliases = map.len(), "loaded merchant alias map");
        Ok(map)
    }

    /// GET a URL with auth + Accept, returning the body on 2xx (else an error).
    async fn get_json(&self, url: &str) -> Result<String> {
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .context("sending Firefly GET")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("Firefly returned {status} for {url}: {body}");
        }
        Ok(body)
    }
}

/// Firefly's transaction-list envelope: `{"data":[...], "meta":{"pagination":{...}}}`.
#[derive(Deserialize)]
struct TxListEnvelope {
    #[serde(default)]
    data: Vec<TxGroup>,
    #[serde(default)]
    meta: TxMeta,
}

/// A transaction *group* (the `{id}` a later `PUT /transactions/{id}` targets),
/// holding one or more splits under `attributes.transactions`.
#[derive(Deserialize)]
struct TxGroup {
    id: String,
    attributes: TxAttributes,
}

#[derive(Deserialize)]
struct TxAttributes {
    #[serde(default)]
    transactions: Vec<TxSplit>,
}

#[derive(Deserialize)]
struct TxSplit {
    #[serde(default)]
    date: String,
    #[serde(default)]
    amount: String,
    #[serde(default)]
    currency_code: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    external_id: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Deserialize, Default)]
struct TxMeta {
    #[serde(default)]
    pagination: Pagination,
}

/// Firefly pagination block. `total_pages` 0 (absent) → single page.
#[derive(Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq)]
struct Pagination {
    #[serde(default)]
    total_pages: u32,
}

// --- rule-group / rules (merchant alias map) -------------------------------

#[derive(Deserialize)]
struct RuleGroupList {
    #[serde(default)]
    data: Vec<RuleGroupItem>,
}
#[derive(Deserialize)]
struct RuleGroupItem {
    id: String,
    attributes: RuleGroupAttrs,
}
#[derive(Deserialize)]
struct RuleGroupAttrs {
    #[serde(default)]
    title: String,
}
#[derive(Deserialize)]
struct RuleList {
    #[serde(default)]
    data: Vec<RuleItem>,
}
#[derive(Deserialize)]
struct RuleItem {
    attributes: RuleAttrs,
}
#[derive(Deserialize)]
struct RuleAttrs {
    #[serde(default)]
    triggers: Vec<RuleClause>,
    #[serde(default)]
    actions: Vec<RuleClause>,
}
#[derive(Deserialize)]
struct RuleClause {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    value: String,
}

/// Find a rule-group's id by (case-insensitive) title. Pure.
fn find_rule_group_id(body: &str, title: &str) -> Option<String> {
    let list: RuleGroupList = serde_json::from_str(body).ok()?;
    list.data
        .into_iter()
        .find(|g| g.attributes.title.eq_ignore_ascii_case(title))
        .map(|g| g.id)
}

/// Extract `(lowercased description trigger → canonical payee)` pairs from a
/// rules-list body: each rule's first `description_*` trigger and its
/// `set_destination_account` action. Pure — unit-testable.
fn parse_alias_rules(body: &str) -> Result<Vec<(String, String)>> {
    let list: RuleList = serde_json::from_str(body).context("decoding Firefly rules JSON")?;
    let mut map = Vec::new();
    for rule in list.data {
        let trigger = rule
            .attributes
            .triggers
            .iter()
            .find(|t| t.kind.starts_with("description_"))
            .map(|t| t.value.trim().to_lowercase());
        let canonical = rule
            .attributes
            .actions
            .iter()
            .find(|a| a.kind == "set_destination_account")
            .map(|a| a.value.trim().to_string());
        if let (Some(t), Some(c)) = (trigger, canonical)
            && !t.is_empty()
            && !c.is_empty()
        {
            map.push((t, c));
        }
    }
    Ok(map)
}

/// Parse one page of the transaction-list response into [`ExistingJournal`]s
/// (one per `receipt-ledger`-tagged split), the pagination block, and a count of
/// tagged splits that were **skipped** because their date/amount/currency would
/// not parse. Pure — no I/O — so the JSON contract is unit-testable.
///
/// The skip count is surfaced (not swallowed) so the caller can react if a
/// Firefly serialization change ever silently drops every split — which would
/// otherwise make the reconciler treat every charge as missing and mass-book
/// duplicates. See [`crate::statement`] notes / `feedback_no_silent_catchall`.
fn parse_transactions_page(body: &str) -> Result<(Vec<ExistingJournal>, Pagination, usize)> {
    let env: TxListEnvelope =
        serde_json::from_str(body).context("decoding Firefly transaction list JSON")?;
    let mut journals = Vec::new();
    let mut skipped = 0usize;
    for group in env.data {
        for split in group.attributes.transactions {
            if !split.tags.iter().any(|t| t == IMPORT_TAG) {
                continue; // not ours — not a skip
            }
            match existing_journal(&group.id, &split) {
                Some(j) => journals.push(j),
                None => skipped += 1,
            }
        }
    }
    Ok((journals, env.meta.pagination, skipped))
}

/// Build an [`ExistingJournal`] from a tagged split, or `None` if any field is
/// unusable (unparseable date/amount/currency, or an amount whose scale exceeds
/// [`Amount::MAX_SCALE`] — not a real currency figure). The booked amount is
/// taken as a positive magnitude (`abs`, trailing zeros trimmed) so it compares
/// cleanly against a statement charge's non-negative [`Amount`], regardless of
/// the sign convention Firefly returns. `None` here is counted as a skip by the
/// caller (never silently dropped).
fn existing_journal(group_id: &str, split: &TxSplit) -> Option<ExistingJournal> {
    let date = parse_firefly_date(&split.date)?;
    let magnitude = Decimal::from_str_exact(split.amount.trim()).ok()?.abs().normalize();
    let amount = Amount::parse(&magnitude.to_string()).ok()?;
    let currency = Currency::parse(split.currency_code.trim()).ok()?;
    Some(ExistingJournal {
        id: group_id.to_string(),
        date,
        amount: Money::new(amount, currency),
        merchant: split.description.trim().to_string(),
        external_id: split
            .external_id
            .clone()
            .filter(|s| !s.trim().is_empty()),
    })
}

/// Firefly dates are RFC3339 (`2026-04-21T00:00:00-04:00`); fall back to the
/// leading `YYYY-MM-DD`. Returns the calendar date.
fn parse_firefly_date(s: &str) -> Option<NaiveDate> {
    let t = s.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(t) {
        return Some(dt.date_naive());
    }
    t.get(..10)
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
}

/// The Firefly single-account envelope: `{"data":{"attributes":{...}}}`.
#[derive(Deserialize)]
struct AccountEnvelope {
    data: AccountData,
}
#[derive(Deserialize)]
struct AccountData {
    attributes: AccountAttributes,
}
#[derive(Deserialize)]
struct AccountAttributes {
    currency_code: Option<String>,
    /// Firefly reports the currency's minor-unit precision here. Absent on some
    /// account kinds; we default to 2 (the overwhelming common case) only when
    /// the currency_code is present but this field is not.
    currency_decimal_places: Option<u32>,
}

/// Extract the booking [`Target`] (currency + precision) from a Firefly account
/// response. Pure — no I/O — so it is unit-testable. Errors when the currency
/// is absent or blank so the caller never converts against an unknown target.
fn parse_account_target(body: &str) -> Result<Target> {
    let env: AccountEnvelope =
        serde_json::from_str(body).context("decoding Firefly account JSON")?;
    let currency = match env.data.attributes.currency_code {
        Some(code) if !code.trim().is_empty() => code.trim().to_string(),
        _ => return Err(anyhow!("account response missing currency_code")),
    };
    let decimals = env.data.attributes.currency_decimal_places.unwrap_or(2);
    Ok(Target { currency, decimals })
}

/// Build the transaction group for a record, given the resolved `account` id,
/// its `target` (currency + precision), and the conversion `rate` (multiply the
/// record amount by it to get the target-currency amount). Pure — no I/O — so
/// the conversion and foreign-amount shaping are unit-testable without a live
/// Firefly or FX API.
///
/// When the record currency already equals the target currency the split books
/// unchanged: `amount` is the record amount, no foreign fields. Otherwise the
/// split books the converted `amount` (rounded to the TARGET currency's real
/// `decimals`) in the target currency and carries the original as Firefly's
/// `foreign_amount` + `foreign_currency_code`.
fn build_group<'b>(
    record: &'b Extracted,
    external_id: &'b str,
    account: &'b str,
    target: &'b Target,
    rate: Decimal,
) -> TransactionGroup<'b> {
    let kind = match record.direction {
        Direction::Out => "withdrawal",
        Direction::In => "deposit",
    };

    let amount = record.amount().value();
    let same_currency = record
        .currency()
        .as_str()
        .eq_ignore_ascii_case(&target.currency);
    let (amount, foreign_amount, foreign_currency_code) = if same_currency {
        // No conversion: book the record amount in the target currency.
        (amount.normalize().to_string(), None, None)
    } else {
        // Convert to the account currency, rounded to the target currency's
        // REAL minor-unit precision (2 for USD/EUR, 0 for JPY/KRW). We use
        // banker's rounding (MidpointNearestEven): over a large batch it does
        // not bias the ledger high or low, which matters for a feed that books
        // many small conversions.
        let converted = (amount * rate)
            .round_dp_with_strategy(target.decimals, RoundingStrategy::MidpointNearestEven);
        (
            converted.normalize().to_string(),
            Some(amount.normalize().to_string()),
            Some(record.currency().as_str()),
        )
    };

    TransactionGroup::single(Split {
        kind,
        date: record.date.to_string(),
        amount,
        currency_code: &target.currency,
        description: &record.merchant,
        external_id,
        tags: vec![IMPORT_TAG],
        source_id: account,
        // The merchant becomes the expense (destination) account.
        destination: Destination::Name(&record.merchant),
        foreign_amount,
        foreign_currency_code,
    })
}

/// Whether a transfer's currency disagrees with either leg's account currency.
///
/// A transfer books both legs in `transfer_currency` with no FX leg, so booking
/// is only correct when that currency matches BOTH the source and destination
/// account currencies. Returns `Some(reason)` describing the first mismatch
/// (→ route to Review, never submit a silent cross-currency transfer) or `None`
/// when all three agree. Case-insensitive on the ISO codes. Pure —
/// unit-testable without a live Firefly.
fn transfer_currency_mismatch(
    transfer_currency: &str,
    source_currency: &str,
    dest_currency: &str,
) -> Option<String> {
    if !transfer_currency.eq_ignore_ascii_case(source_currency) {
        return Some(format!(
            "transfer currency {transfer_currency} does not match source account currency {source_currency}"
        ));
    }
    if !transfer_currency.eq_ignore_ascii_case(dest_currency) {
        return Some(format!(
            "transfer currency {transfer_currency} does not match destination account currency {dest_currency}"
        ));
    }
    None
}

/// Build a `transfer` group for a statement payment: paying bank account
/// (`source`) → card liability (`dest`), same currency on both legs (no FX).
/// Pure — unit-testable without a live Firefly.
fn build_transfer_group<'b>(
    transfer: &'b ValidatedTransfer,
    source: &'b str,
    dest: &'b str,
) -> TransactionGroup<'b> {
    TransactionGroup::single(Split {
        kind: "transfer",
        date: transfer.date().to_string(),
        amount: transfer.money().amount.value().normalize().to_string(),
        currency_code: transfer.money().currency.as_str(),
        description: transfer.description(),
        external_id: transfer.external_id(),
        tags: vec![IMPORT_TAG],
        source_id: source,
        destination: Destination::Id(dest),
        foreign_amount: None,
        foreign_currency_code: None,
    })
}

/// Funding hints that mark a PayPal record as funded by a credit product (→ the
/// PayPal Credit liability account) rather than the PayPal balance. Matched
/// case-insensitively as substrings of the trimmed `account_hint`. "credit" is
/// deliberately broad: in a PayPal funding hint it can only mean the credit
/// product. An empty/absent hint is not a credit signal — it defaults to the
/// balance account.
const PAYPAL_CREDIT_HINTS: &[&str] = &[
    "paypal credit",
    "pay later",
    "pay in 4",
    "pay in4",
    "pay monthly",
    "credit",
];

/// Public, pure view of the PayPal funding classification, for the eval harness
/// (and anything that needs to predict routing without the live client). `true`
/// → PayPal Credit liability account; `false` → PayPal Balance asset account.
/// Identical logic to the private [`is_paypal_credit_funding`] the submit path
/// uses, so the eval scores the *real* routing decision.
#[must_use]
pub fn paypal_is_credit_funded(record: &Extracted) -> bool {
    is_paypal_credit_funding(record)
}

/// Classify a PayPal record's funding method from its `account_hint`.
///
/// Returns `true` when the hint names a PayPal credit product, so the record
/// books against the PayPal Credit liability account; `false` (the default)
/// routes it to the PayPal Balance account. Pure — depends only on the record.
fn is_paypal_credit_funding(record: &Extracted) -> bool {
    match record.account_hint.as_deref() {
        Some(hint) => {
            let hint = hint.trim().to_ascii_lowercase();
            PAYPAL_CREDIT_HINTS
                .iter()
                .any(|needle| hint.contains(needle))
        }
        None => false,
    }
}

/// The Firefly 422 validation-error envelope. Firefly returns
/// `{"message": "...", "errors": {"field": ["msg", ...], ...}}`. A duplicate
/// import surfaces as an error message about a duplicate transaction hash.
#[derive(Deserialize)]
struct ValidationError {
    #[serde(default)]
    message: String,
    #[serde(default)]
    errors: HashMap<String, Vec<String>>,
}

/// Whether a 422 body is Firefly's duplicate-hash rejection specifically.
///
/// We parse the JSON and look for the duplicate phrasing in either the
/// top-level `message` or any field error (Firefly phrases it as "Duplicate of
/// transaction #N." and attaches it under a `transactions.N.description`-style
/// key). Any other 422 shape is NOT a duplicate — it is a real validation
/// failure the caller must surface. A body that does not parse as the expected
/// envelope is conservatively treated as not-a-duplicate.
fn is_duplicate_error(body: &str) -> bool {
    let Ok(parsed) = serde_json::from_str::<ValidationError>(body) else {
        return false;
    };
    // Firefly's exact phrasing is "Duplicate of transaction #N." Match that
    // contiguous phrase rather than the looser "duplicate" + "transaction"
    // anywhere, so an unrelated rule message mentioning both words is not
    // misread as a duplicate (which would silently drop a real charge).
    let mentions_duplicate = |s: &str| s.to_ascii_lowercase().contains("duplicate of transaction");
    if mentions_duplicate(&parsed.message) {
        return true;
    }
    parsed
        .errors
        .values()
        .flatten()
        .any(|msg| mentions_duplicate(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fx::FxClient;
    use crate::schema::{Amount, Currency, Direction, Money, Source};
    use chrono::NaiveDate;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn acct(id: &str) -> AccountId {
        AccountId::parse(id).unwrap()
    }

    fn usd_target() -> Target {
        Target {
            currency: "USD".to_string(),
            decimals: 2,
        }
    }

    /// A PayPal record with a configurable funding hint and currency. Defaults
    /// to a balance-funded EUR purchase.
    fn paypal_record(account_hint: Option<&str>, currency: &str) -> Extracted {
        Extracted {
            source: Source::Paypal,
            external_id: Some("8XY12345AB678901C".to_string()),
            money: Money::new(
                Amount::parse("149.99").unwrap(),
                Currency::parse(currency).unwrap(),
            ),
            direction: Direction::Out,
            date: NaiveDate::from_ymd_opt(2026, 5, 11).unwrap(),
            merchant: "Example Merchant B.V.".to_string(),
            account_hint: account_hint.map(str::to_string),
            status: "approved".to_string(),
            raw_ref: "TESTORDER0123456".to_string(),
        }
    }

    /// A Banco Popular record with a configurable currency.
    fn banco_record(currency: &str) -> Extracted {
        Extracted {
            source: Source::BancoPopular,
            external_id: None,
            money: Money::new(
                Amount::parse("1.50").unwrap(),
                Currency::parse(currency).unwrap(),
            ),
            direction: Direction::Out,
            date: NaiveDate::from_ymd_opt(2026, 5, 27).unwrap(),
            merchant: "Example Cafe Amsterdam".to_string(),
            account_hint: Some("1234".to_string()),
            status: "Aprobada".to_string(),
            raw_ref: String::new(),
        }
    }

    /// A throwaway FX client; the build/routing tests below never call its
    /// `rate` (they exercise the pure `route_account`/`build_group` directly),
    /// so the URL is never contacted.
    fn fx(http: &Client) -> FxClient<'_> {
        FxClient::new(http, "http://fx.invalid")
    }

    /// A client wired with the production account ids.
    fn client<'a>(http: &'a Client, fx: &'a FxClient<'a>) -> FireflyClient<'a> {
        FireflyClient::new(
            http,
            "http://firefly:8080",
            "tok",
            fx,
            acct("103"),       // PayPal Balance
            Some(acct("105")), // PayPal Credit
            Some(acct("106")), // Banco Popular USD
            Some(acct("107")), // Banco Popular DOP
            Some(acct("201")), // BP USD savings (paying)
            Some(acct("202")), // BP DOP checking (paying)
            HashMap::from([
                ("0130".to_string(), acct("1")), // PayPal-payment funding by last-4
            ]),
            HashMap::from([
                ("4189".to_string(), acct("127")), // SWIFT debtor IBAN last-4 (BPD)
            ]),
            HashMap::from([
                ("CHASUS33".to_string(), acct("1")), // SWIFT creditor BIC → own foreign account
                ("ABNANL2A".to_string(), acct("8")),
            ]),
        )
    }

    #[test]
    fn builds_withdrawal_same_currency_without_foreign_fields() {
        // A same-currency target books unchanged: no foreign fields.
        let rec = paypal_record(Some("Balance"), "EUR");
        let target = Target {
            currency: "EUR".to_string(),
            decimals: 2,
        };
        let group = build_group(&rec, "8XY12345AB678901C", "103", &target, Decimal::ONE);
        let json = serde_json::to_value(&group).unwrap();

        assert_eq!(json["error_if_duplicate_hash"], true);
        let split = &json["transactions"][0];
        assert_eq!(split["type"], "withdrawal");
        assert_eq!(split["amount"], "149.99");
        assert_eq!(split["currency_code"], "EUR");
        assert_eq!(split["external_id"], "8XY12345AB678901C");
        assert_eq!(split["source_id"], "103");
        assert_eq!(split["destination_name"], "Example Merchant B.V.");
        assert_eq!(split["tags"][0], "receipt-ledger");
        assert_eq!(split["date"], "2026-05-11");
        assert!(split.get("foreign_amount").is_none());
        assert!(split.get("foreign_currency_code").is_none());
    }

    /// Route a record to its account id via the pure `route_account`.
    fn source_id_of(c: &FireflyClient, rec: &Extracted) -> String {
        c.route_account(rec)
            .expect("routing should succeed")
            .as_str()
            .to_string()
    }

    // --- routing -----------------------------------------------------------

    #[test]
    fn paypal_credit_funded_routes_to_credit_account() {
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        for hint in ["Pay in 4", "PayPal Credit"] {
            let rec = paypal_record(Some(hint), "USD");
            assert_eq!(
                source_id_of(&c, &rec),
                "105",
                "hint {hint:?} should be credit"
            );
        }
    }

    #[test]
    fn paypal_balance_funded_routes_to_balance_account() {
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        assert_eq!(
            source_id_of(&c, &paypal_record(Some("Balance"), "USD")),
            "103"
        );
        assert_eq!(source_id_of(&c, &paypal_record(None, "USD")), "103");
    }

    #[test]
    fn banco_dop_routes_to_dop_account() {
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        assert_eq!(source_id_of(&c, &banco_record("DOP")), "107");
    }

    #[test]
    fn banco_non_dop_routes_to_usd_account() {
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        for cur in ["USD", "EUR", "JPY", "KRW"] {
            assert_eq!(
                source_id_of(&c, &banco_record(cur)),
                "106",
                "currency {cur} → USD acct"
            );
        }
    }

    #[test]
    fn needed_but_unconfigured_account_errors() {
        let http = Client::new();
        let fx = fx(&http);
        // Only the required balance account is configured; everything else None.
        let c = FireflyClient::new(
            &http,
            "http://firefly:8080",
            "tok",
            &fx,
            acct("103"),
            None,
            None,
            None,
            None,
            None,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        );
        assert!(
            c.route_account(&paypal_record(Some("Pay in 4"), "USD"))
                .is_err()
        );
        assert!(c.route_account(&banco_record("DOP")).is_err());
        assert!(c.route_account(&banco_record("USD")).is_err());
        assert!(
            c.route_account(&paypal_record(Some("Balance"), "USD"))
                .is_ok()
        );
    }

    // --- transfer booking (statement payments) -----------------------------

    fn transfer(currency: &str, amount: &str) -> crate::validate::ValidatedTransfer {
        match crate::validate::validate_transfer(
            Money::new(Amount::parse(amount).unwrap(), Currency::parse(currency).unwrap()),
            NaiveDate::from_ymd_opt(2026, 4, 28).unwrap(),
            "Pago Via App".to_string(),
            format!("bpstmt:{currency}"),
        ) {
            crate::validate::TransferVerdict::Booked(t) => t,
            crate::validate::TransferVerdict::Review { reason } => panic!("transfer should validate: {reason}"),
        }
    }

    #[test]
    fn dop_transfer_routes_checking_to_dop_card_and_builds() {
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        let t = transfer("DOP", "60999.81");
        let (src, dst) = c.route_transfer_accounts(&t).unwrap();
        assert_eq!(src.as_str(), "202", "DOP checking is the source");
        assert_eq!(dst.as_str(), "107", "DOP card is the destination");

        let json = serde_json::to_value(build_transfer_group(&t, src.as_str(), dst.as_str())).unwrap();
        let split = &json["transactions"][0];
        assert_eq!(split["type"], "transfer");
        assert_eq!(split["amount"], "60999.81");
        assert_eq!(split["currency_code"], "DOP");
        assert_eq!(split["source_id"], "202");
        assert_eq!(split["destination_id"], "107");
        assert!(split.get("destination_name").is_none(), "transfer uses dest id, not name");
        assert!(split.get("foreign_amount").is_none(), "same-currency transfer, no FX");
    }

    #[test]
    fn usd_transfer_routes_savings_to_usd_card() {
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        let (src, dst) = c.route_transfer_accounts(&transfer("USD", "2491.46")).unwrap();
        assert_eq!(src.as_str(), "201", "USD savings is the source");
        assert_eq!(dst.as_str(), "106", "USD card is the destination");
    }

    #[test]
    fn submit_transfer_between_builds_explicit_account_group() {
        // The PayPal-payment path supplies source + dest directly (no currency
        // routing); the built group books a same-currency transfer between them.
        let t = transfer("USD", "1300.00");
        let json =
            serde_json::to_value(build_transfer_group(&t, "1", "105")).unwrap();
        let split = &json["transactions"][0];
        assert_eq!(split["type"], "transfer");
        assert_eq!(split["amount"], "1300");
        assert_eq!(split["currency_code"], "USD");
        assert_eq!(split["source_id"], "1", "funding account is the source");
        assert_eq!(split["destination_id"], "105", "PayPal Credit is the destination");
        assert!(split.get("foreign_amount").is_none(), "same-currency, no FX");
    }

    // --- Fix 4: transfer currency-agreement guard --------------------------

    #[test]
    fn transfer_currency_agreement_pure_check() {
        // All three agree (case-insensitively) → no mismatch.
        assert!(transfer_currency_mismatch("USD", "USD", "USD").is_none());
        assert!(transfer_currency_mismatch("usd", "USD", "Usd").is_none());
        // Source disagrees → mismatch naming the source.
        let r = transfer_currency_mismatch("USD", "DOP", "USD").unwrap();
        assert!(r.contains("source"), "{r}");
        // Destination disagrees → mismatch naming the destination.
        let r = transfer_currency_mismatch("USD", "USD", "DOP").unwrap();
        assert!(r.contains("destination"), "{r}");
    }

    /// Seed the per-account target cache so `submit_transfer_between` can read
    /// account currencies without any network I/O.
    fn seed_target(c: &FireflyClient, account: &str, currency: &str) {
        c.account_target
            .lock()
            .unwrap()
            .insert(account.to_string(), Target { currency: currency.to_string(), decimals: 2 });
    }

    #[test]
    fn submit_transfer_between_routes_currency_mismatch_to_review() {
        // A USD transfer against a source account that books in DOP must NOT be
        // submitted: the checked path returns CurrencyMismatch (→ Review),
        // reaching no network call (both targets are seeded from the cache).
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        seed_target(&c, "1", "DOP"); // source funding account books in DOP
        seed_target(&c, "105", "USD"); // dest (PayPal Credit) books in USD

        let t = transfer("USD", "1300.00");
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let outcome = rt
            .block_on(c.submit_transfer_between(&t, &acct("1"), &acct("105")))
            .expect("a currency mismatch is Ok(CurrencyMismatch), not Err");
        match outcome {
            TransferSubmit::CurrencyMismatch { reason } => {
                assert!(reason.contains("source"), "names the disagreeing leg: {reason}");
            }
            TransferSubmit::Submitted(o) => panic!("expected mismatch, submitted {o:?}"),
        }
    }

    #[test]
    fn resolves_paypal_credit_and_paying_accounts() {
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        assert_eq!(c.paypal_credit_account().map(AccountId::as_str), Some("105"));
        assert_eq!(c.paying_account_for_last4("0130").map(AccountId::as_str), Some("1"));
        // An unmapped last-4 → None (the pipeline routes such a payment to Review).
        assert!(c.paying_account_for_last4("9999").is_none());
    }

    #[test]
    fn swift_debtor_map_is_independent_of_paying_map() {
        // Fix 5: the SWIFT debtor and PayPal funding maps are SEPARATE. The
        // SWIFT debtor last-4 4189 resolves only via `swift_debtor_for_last4`
        // (→ 127), and the PayPal funding last-4 0130 only via
        // `paying_account_for_last4` (→ 1); neither leaks into the other.
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        assert_eq!(c.swift_debtor_for_last4("4189").map(AccountId::as_str), Some("127"));
        // The SWIFT debtor last-4 is NOT in the PayPal funding map.
        assert!(c.paying_account_for_last4("4189").is_none());
        // The PayPal funding last-4 is NOT in the SWIFT debtor map.
        assert!(c.swift_debtor_for_last4("0130").is_none());
        // An unmapped SWIFT debtor last-4 → None (→ Review).
        assert!(c.swift_debtor_for_last4("9999").is_none());
    }

    #[test]
    fn swift_wire_routes_debtor_last4_to_creditor_bic_accounts() {
        // The SWIFT path resolves source from the debtor IBAN last-4 (4189 → 127)
        // via the DEDICATED SWIFT debtor map and dest from the normalized creditor
        // BIC (CHASUS33 → 1), then builds a same-currency transfer between exactly
        // those account ids.
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        let source = c.swift_debtor_for_last4("4189").expect("debtor last-4 mapped");
        let dest = c.swift_dest_for_bic("CHASUS33").expect("creditor BIC mapped");
        assert_eq!(source.as_str(), "127");
        assert_eq!(dest.as_str(), "1");

        let t = transfer("USD", "2100.00");
        let json =
            serde_json::to_value(build_transfer_group(&t, source.as_str(), dest.as_str())).unwrap();
        let split = &json["transactions"][0];
        assert_eq!(split["type"], "transfer");
        assert_eq!(split["amount"], "2100");
        assert_eq!(split["currency_code"], "USD");
        assert_eq!(split["source_id"], "127", "BPD debtor is the source");
        assert_eq!(split["destination_id"], "1", "own foreign account is the destination");
        assert!(split.get("foreign_amount").is_none(), "same-currency, no FX");
    }

    #[test]
    fn resolves_swift_dest_by_bic() {
        let http = Client::new();
        let fx = fx(&http);
        let c = client(&http, &fx);
        assert_eq!(c.swift_dest_for_bic("CHASUS33").map(AccountId::as_str), Some("1"));
        assert_eq!(c.swift_dest_for_bic("ABNANL2A").map(AccountId::as_str), Some("8"));
        // Lookup is case-insensitive (keys are uppercased at parse + lookup).
        assert_eq!(c.swift_dest_for_bic("chasus33").map(AccountId::as_str), Some("1"));
        // An unmapped BIC → None (the pipeline routes such a wire to Review).
        assert!(c.swift_dest_for_bic("DEUTDEFF").is_none());
    }

    #[test]
    fn transfer_with_unconfigured_paying_account_errors() {
        let http = Client::new();
        let fx = fx(&http);
        // Card accounts present, paying accounts absent.
        let c = FireflyClient::new(
            &http,
            "http://firefly:8080",
            "tok",
            &fx,
            acct("103"),
            None,
            Some(acct("106")),
            Some(acct("107")),
            None,
            None,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
        );
        assert!(c.route_transfer_accounts(&transfer("DOP", "1.00")).is_err());
        assert!(c.route_transfer_accounts(&transfer("USD", "1.00")).is_err());
    }

    // --- conversion / foreign amount ---------------------------------------

    #[test]
    fn same_currency_books_unchanged_without_foreign_fields() {
        let rec = banco_record("USD");
        let target = usd_target();
        let group = build_group(&rec, "h", "106", &target, Decimal::ONE);
        let json = serde_json::to_value(&group).unwrap();
        let split = &json["transactions"][0];
        assert_eq!(split["amount"], "1.5");
        assert_eq!(split["currency_code"], "USD");
        assert_eq!(split["source_id"], "106");
        assert!(split.get("foreign_amount").is_none());
        assert!(split.get("foreign_currency_code").is_none());
    }

    #[test]
    fn foreign_currency_books_converted_amount_with_foreign_fields() {
        // JPY 5130 → USD at 0.0064 = 32.832 → 32.83 (2 dp, MidpointNearestEven).
        let mut rec = banco_record("JPY");
        rec.money = Money::new(
            Amount::parse("5130").unwrap(),
            Currency::parse("JPY").unwrap(),
        );
        let rate = Decimal::from_str("0.0064").unwrap();
        let target = usd_target();
        let group = build_group(&rec, "h", "106", &target, rate);
        let json = serde_json::to_value(&group).unwrap();
        let split = &json["transactions"][0];

        assert_eq!(split["amount"], "32.83", "converted USD amount, 2 dp");
        assert_eq!(
            split["currency_code"], "USD",
            "books in the account currency"
        );
        assert_eq!(split["foreign_amount"], "5130", "original charge amount");
        assert_eq!(
            split["foreign_currency_code"], "JPY",
            "original charge currency"
        );
        assert_eq!(split["source_id"], "106");
    }

    /// H1/H2: a 0-decimal target currency (JPY) rounds to whole units.
    #[test]
    fn zero_decimal_target_rounds_to_whole_units() {
        // USD 65.33 → JPY at rate 156.0 = 10191.48 → 10191 (0 dp).
        let mut rec = banco_record("USD");
        rec.money = Money::new(
            Amount::parse("65.33").unwrap(),
            Currency::parse("USD").unwrap(),
        );
        let jpy = Target {
            currency: "JPY".to_string(),
            decimals: 0,
        };
        let rate = Decimal::from_str("156.0").unwrap();
        let group = build_group(&rec, "h", "999", &jpy, rate);
        let json = serde_json::to_value(&group).unwrap();
        let split = &json["transactions"][0];
        assert_eq!(split["amount"], "10191", "0-dp target → whole units");
        assert_eq!(split["currency_code"], "JPY");
        assert_eq!(split["foreign_currency_code"], "USD");
    }

    /// H1/H2: a `.xx5` midpoint rounds to the nearest EVEN last digit under
    /// MidpointNearestEven (banker's rounding), 2-dp target.
    #[test]
    fn midpoint_rounds_to_nearest_even() {
        // amount 1.005, rate 1 → 1.005 → 1.00 (round half to even: 0 is even).
        let mut rec = banco_record("EUR");
        rec.money = Money::new(
            Amount::parse("1.005").unwrap(),
            Currency::parse("EUR").unwrap(),
        );
        let target = usd_target();
        let group = build_group(&rec, "h", "106", &target, Decimal::ONE);
        let json = serde_json::to_value(&group).unwrap();
        assert_eq!(json["transactions"][0]["amount"], "1");

        // amount 1.015, rate 1 → 1.015 → 1.02 (round half to even: 2 is even).
        let mut rec2 = banco_record("EUR");
        rec2.money = Money::new(
            Amount::parse("1.015").unwrap(),
            Currency::parse("EUR").unwrap(),
        );
        let group2 = build_group(&rec2, "h", "106", &target, Decimal::ONE);
        let json2 = serde_json::to_value(&group2).unwrap();
        assert_eq!(json2["transactions"][0]["amount"], "1.02");
    }

    // --- account-target parsing --------------------------------------------

    #[test]
    fn parses_account_currency_and_decimals() {
        let body = r#"{"data":{"id":"106","attributes":{"name":"Banco USD","currency_code":"USD","currency_decimal_places":2}}}"#;
        let t = parse_account_target(body).unwrap();
        assert_eq!(t.currency, "USD");
        assert_eq!(t.decimals, 2);
    }

    #[test]
    fn parses_zero_decimal_currency() {
        let body = r#"{"data":{"attributes":{"currency_code":"JPY","currency_decimal_places":0}}}"#;
        let t = parse_account_target(body).unwrap();
        assert_eq!(t.currency, "JPY");
        assert_eq!(t.decimals, 0);
    }

    #[test]
    fn defaults_decimals_to_two_when_absent() {
        let body = r#"{"data":{"attributes":{"currency_code":"EUR"}}}"#;
        assert_eq!(parse_account_target(body).unwrap().decimals, 2);
    }

    #[test]
    fn parse_account_target_errors_when_currency_absent() {
        assert!(parse_account_target(r#"{"data":{"attributes":{}}}"#).is_err());
        assert!(parse_account_target(r#"{"data":{"attributes":{"currency_code":""}}}"#).is_err());
        assert!(parse_account_target("not json").is_err());
    }

    /// A foreign Banco record's rate threads through the public client via a
    /// pre-seeded FX cache (no network for the rate).
    #[test]
    fn seeded_fx_rate_threads_through_client() {
        let http = Client::new();
        let date = NaiveDate::from_ymd_opt(2026, 5, 27).unwrap();
        let fx = FxClient::with_seeded_rate(
            &http,
            "JPY",
            "USD",
            date,
            Decimal::from_str("0.0064").unwrap(),
        );
        let c = client(&http, &fx);
        let mut rec = banco_record("JPY");
        rec.date = date;
        rec.money = Money::new(
            Amount::parse("5130").unwrap(),
            Currency::parse("JPY").unwrap(),
        );
        let account = c.route_account(&rec).unwrap();
        assert_eq!(account.as_str(), "106");
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let rate = rt
            .block_on(c.fx.rate(rec.currency().as_str(), "USD", rec.date))
            .expect("seeded rate, no network");
        let target = usd_target();
        let group = build_group(&rec, "h", account.as_str(), &target, rate);
        let json = serde_json::to_value(&group).unwrap();
        assert_eq!(json["transactions"][0]["amount"], "32.83");
        assert_eq!(json["transactions"][0]["foreign_currency_code"], "JPY");
    }

    // --- classifier --------------------------------------------------------

    #[test]
    fn classifier_flags_credit_hints() {
        for hint in [
            "PayPal Credit",
            "Pay Later",
            "Pay in 4",
            "Pay in4",
            "Pay Monthly",
            "credit",
            "  funded by PAYPAL CREDIT  ",
        ] {
            assert!(
                is_paypal_credit_funding(&paypal_record(Some(hint), "USD")),
                "hint {hint:?} should classify as credit"
            );
        }
    }

    #[test]
    fn classifier_rejects_non_credit_hints() {
        for hint in [
            None,
            Some(""),
            Some("   "),
            Some("Balance"),
            Some("Visa ending 1234"),
            Some("bank"),
        ] {
            assert!(
                !is_paypal_credit_funding(&paypal_record(hint, "USD")),
                "hint {hint:?} should NOT classify as credit"
            );
        }
    }

    // --- M3: 422 duplicate detection ---------------------------------------

    #[test]
    fn detects_duplicate_message_in_top_level_message() {
        assert!(is_duplicate_error(
            r#"{"message":"Duplicate of transaction #123.","errors":{}}"#
        ));
    }

    #[test]
    fn detects_duplicate_message_in_field_errors() {
        let body = r#"{"message":"The given data was invalid.","errors":{"transactions.0.description":["Duplicate of transaction #99."]}}"#;
        assert!(is_duplicate_error(body));
    }

    #[test]
    fn non_duplicate_422_is_not_a_duplicate() {
        // A real validation failure must NOT be swallowed as a duplicate.
        assert!(!is_duplicate_error(
            r#"{"message":"The given data was invalid.","errors":{"transactions.0.amount":["The amount is required."]}}"#
        ));
        // A 422 whose body isn't the expected envelope is conservatively a
        // non-duplicate (→ real failure → Review).
        assert!(!is_duplicate_error("not json"));
        assert!(!is_duplicate_error(r#"{"unexpected":true}"#));
    }

    // --- property tests --------------------------------------------------

    use proptest::prelude::*;

    /// Ten-to-the-power for the minor-unit width of a currency, as a `Decimal`.
    fn minor_unit(decimals: u32) -> Decimal {
        // 10^-decimals: e.g. decimals=2 → 0.01, decimals=0 → 1.
        Decimal::new(1, decimals)
    }

    proptest! {
        /// FX rounding reconciliation: the booked (rounded) target amount is
        /// always within one minor unit of the exact (unrounded) conversion.
        #[test]
        fn prop_fx_rounding_within_one_minor_unit(
            // record amount in EUR (a non-USD source so conversion fires),
            amount_cents in 1u64..100_000_000,
            // rate scaled by 1e4, kept positive and bounded,
            rate_e4 in 1u64..2_000_000,
            decimals in 0u32..=2,
        ) {
            let amount = Decimal::new(amount_cents as i64, 2); // e.g. 1234 → 12.34
            let rate = Decimal::new(rate_e4 as i64, 4);        // e.g. 6400 → 0.6400

            let mut rec = banco_record("EUR");
            rec.money = Money::new(
                Amount::parse(&amount.to_string()).unwrap(),
                Currency::parse("EUR").unwrap(),
            );
            let target = Target { currency: "USD".to_string(), decimals };
            let group = build_group(&rec, "h", "106", &target, rate);
            let booked = Decimal::from_str(&group.transactions[0].amount).unwrap();

            let exact = amount * rate;
            let diff = (booked - exact).abs();
            // Strictly within one minor unit (round-half-even is at most half a
            // unit off, comfortably inside one unit).
            prop_assert!(
                diff < minor_unit(decimals),
                "booked {} vs exact {} differ by {} >= one minor unit",
                booked, exact, diff,
            );
            // And the booked amount carries no more than `decimals` places.
            prop_assert!(booked.scale() <= decimals);
        }
    }

    // --- transaction-list parsing (reconcile read path) --------------------

    #[test]
    fn parses_tagged_withdrawals_and_pagination() {
        let body = r#"{
          "data": [
            {"id":"100","attributes":{"transactions":[
               {"date":"2026-04-21T00:00:00-04:00","amount":"50.93","currency_code":"USD","description":"JR EAST","external_id":"bpstmt:1","tags":["receipt-ledger"]}
            ]}},
            {"id":"101","attributes":{"transactions":[
               {"date":"2026-04-22T00:00:00-04:00","amount":"9.99","currency_code":"USD","description":"MANUAL ENTRY","tags":["other"]}
            ]}}
          ],
          "meta":{"pagination":{"total_pages":3}}
        }"#;
        let (js, pag, skipped) = parse_transactions_page(body).unwrap();
        assert_eq!(js.len(), 1, "only the receipt-ledger-tagged split is returned");
        assert_eq!(skipped, 0);
        assert_eq!(js[0].id, "100");
        assert_eq!(js[0].date, NaiveDate::from_ymd_opt(2026, 4, 21).unwrap());
        assert_eq!(js[0].amount.amount.value(), Decimal::from_str_exact("50.93").unwrap());
        assert_eq!(js[0].amount.currency.as_str(), "USD");
        assert_eq!(js[0].merchant, "JR EAST");
        assert_eq!(js[0].external_id.as_deref(), Some("bpstmt:1"));
        assert_eq!(pag.total_pages, 3);
    }

    #[test]
    fn negative_booked_amount_is_stored_as_magnitude() {
        // H3: whatever sign Firefly uses, the journal magnitude is positive so it
        // compares cleanly against a non-negative statement Amount.
        let body = r#"{"data":[{"id":"1","attributes":{"transactions":[
            {"date":"2026-04-21","amount":"-50.9300","currency_code":"USD","description":"x","tags":["receipt-ledger"]}
        ]}}],"meta":{"pagination":{"total_pages":1}}}"#;
        let (js, _, _) = parse_transactions_page(body).unwrap();
        assert_eq!(js.len(), 1);
        assert_eq!(js[0].amount.amount.value(), Decimal::from_str_exact("50.93").unwrap());
    }

    #[test]
    fn empty_list_is_single_page() {
        let (js, pag, skipped) = parse_transactions_page(r#"{"data":[],"meta":{}}"#).unwrap();
        assert!(js.is_empty());
        assert_eq!(skipped, 0);
        assert_eq!(pag.total_pages, 0);
    }

    #[test]
    fn skips_and_counts_unparseable_splits() {
        let body = r#"{"data":[{"id":"1","attributes":{"transactions":[
            {"date":"nope","amount":"1.00","currency_code":"USD","description":"x","tags":["receipt-ledger"]},
            {"date":"2026-04-21","amount":"notnum","currency_code":"USD","description":"y","tags":["receipt-ledger"]},
            {"date":"2026-04-21","amount":"1.00","currency_code":"","description":"z","tags":["receipt-ledger"]}
        ]}}],"meta":{"pagination":{"total_pages":1}}}"#;
        let (js, _, skipped) = parse_transactions_page(body).unwrap();
        assert!(js.is_empty(), "malformed splits are skipped, not fatal");
        assert_eq!(skipped, 3, "skips are counted, not silently dropped");
    }

    #[test]
    fn parses_alias_rules_and_finds_group() {
        let groups = r#"{"data":[{"id":"7","attributes":{"title":"receipt-ledger-aliases"}},{"id":"3","attributes":{"title":"Other"}}]}"#;
        assert_eq!(find_rule_group_id(groups, "receipt-ledger-aliases").as_deref(), Some("7"));
        assert_eq!(find_rule_group_id(groups, "RECEIPT-LEDGER-ALIASES").as_deref(), Some("7"));
        assert_eq!(find_rule_group_id(groups, "nope"), None);

        let rules = r#"{"data":[
          {"attributes":{"triggers":[{"type":"description_contains","value":"JOMPEAME"}],"actions":[{"type":"set_destination_account","value":"Jompeame"}]}},
          {"attributes":{"triggers":[{"type":"description_contains","value":"NAGANO DENTETSU"}],"actions":[{"type":"set_category","value":"x"},{"type":"set_destination_account","value":"Nagano Dentetsu"}]}},
          {"attributes":{"triggers":[{"type":"amount_more","value":"5"}],"actions":[{"type":"set_destination_account","value":"Skip"}]}}
        ]}"#;
        let map = parse_alias_rules(rules).unwrap();
        assert_eq!(map.len(), 2, "only rules with a description trigger + set-destination action");
        assert_eq!(map[0], ("jompeame".to_string(), "Jompeame".to_string()));
        assert_eq!(map[1], ("nagano dentetsu".to_string(), "Nagano Dentetsu".to_string()));
    }

    #[test]
    fn firefly_date_rfc3339_or_plain_or_none() {
        assert_eq!(
            parse_firefly_date("2026-04-21T00:00:00-04:00"),
            NaiveDate::from_ymd_opt(2026, 4, 21)
        );
        assert_eq!(parse_firefly_date("2026-04-21"), NaiveDate::from_ymd_opt(2026, 4, 21));
        assert_eq!(parse_firefly_date("garbage"), None);
    }
}
