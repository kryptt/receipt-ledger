//! JMAP mail access against Stalwart.
//!
//! Responsibilities:
//! - connect with Basic auth (session discovery at `/.well-known/jmap`),
//! - list *new* INBOX messages incrementally via the `Email/changes` state
//!   cursor (persisted between runs),
//! - expose each message's decoded text body so the deterministic
//!   [`crate::unwrap`] layer can recover the original sender,
//! - move a processed message to the `Processed` or `Review` mailbox.
//!
//! The exact set of JMAP method shapes here is implemented against the
//! `jmap-client` 0.4 helpers, which mirror the JMAP spec. Connectivity details
//! (auth, redirect-following to `/jmap/session`) are handled by the crate.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use jmap_client::client::{Client, Credentials};
use jmap_client::core::response::EmailGetResponse;
use jmap_client::email::{self, Property};
use jmap_client::mailbox::{self, Role};
use tracing::{debug, info, warn};

use crate::config::Config;

/// One attachment part of a message, reduced to what classification + the
/// statement path need. The `blob_id` is downloaded on demand via
/// [`Mailbox::download`].
#[derive(Debug, Clone)]
pub struct Attachment {
    pub blob_id: String,
    pub content_type: Option<String>,
    pub name: Option<String>,
    pub size: usize,
}

impl Attachment {
    /// Whether this part is a PDF (by content-type or `.pdf` name).
    #[must_use]
    pub fn is_pdf(&self) -> bool {
        self.content_type
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("application/pdf"))
            || self
                .name
                .as_deref()
                .is_some_and(|n| n.to_ascii_lowercase().ends_with(".pdf"))
    }
}

/// A message fetched from the INBOX, reduced to what the pipeline needs.
#[derive(Debug, Clone)]
pub struct FetchedMessage {
    pub id: String,
    /// JMAP envelope subject (the Gmail "Fwd: ..." line).
    pub subject: Option<String>,
    /// Rendered `From:` header value, e.g. `PayPal <service@paypal.com>` or
    /// `<notificaciones@popularenlinea.com>`. Used by the unwrap layer to
    /// recover the original sender of an *auto*-forwarded mail, where Gmail
    /// preserves the original `From:` and inserts no "Forwarded message" marker.
    pub from: Option<String>,
    /// Decoded `text/plain` body — input to the forward-unwrap step.
    pub text: String,
    /// Attachment metadata (e.g. a statement PDF). Bodies are fetched lazily.
    pub attachments: Vec<Attachment>,
}

/// Connected JMAP session with the three mailbox ids we route between.
pub struct Mailbox {
    client: Client,
    inbox_id: String,
    processed_id: String,
    review_id: String,
}

impl Mailbox {
    /// Connect and resolve the INBOX / Processed / Review mailbox ids.
    pub async fn connect(cfg: &Config) -> Result<Self> {
        let client = Client::new()
            .credentials(Credentials::basic(&cfg.jmap_user, &cfg.jmap_password))
            // Stalwart's session endpoint may redirect to `/jmap/session`; the
            // host is the same, so trust it for redirect-following.
            .follow_redirects(host_of(&cfg.jmap_url))
            .connect(cfg.jmap_url.trim_end_matches('/'))
            .await
            .context("connecting to JMAP session")?;

        let inbox_id = mailbox_id_by_role(&client, Role::Inbox)
            .await?
            .ok_or_else(|| anyhow!("no INBOX mailbox found"))?;
        let processed_id = mailbox_id_by_name(&client, &cfg.processed_mailbox)
            .await?
            .ok_or_else(|| anyhow!("mailbox {:?} not found", cfg.processed_mailbox))?;
        let review_id = mailbox_id_by_name(&client, &cfg.review_mailbox)
            .await?
            .ok_or_else(|| anyhow!("mailbox {:?} not found", cfg.review_mailbox))?;

        info!(%inbox_id, %processed_id, %review_id, "resolved mailboxes");
        Ok(Self {
            client,
            inbox_id,
            processed_id,
            review_id,
        })
    }

    /// Fetch new messages, using the persisted `Email/changes` state when
    /// present. Returns the messages plus the *new* state string the caller
    /// must persist after a successful run.
    pub async fn fetch_new(
        &self,
        prior_state: Option<String>,
    ) -> Result<(Vec<FetchedMessage>, String)> {
        let (ids, new_state) = match prior_state {
            Some(state) => self.changed_ids(state).await?,
            None => self.bootstrap_ids().await?,
        };
        debug!(count = ids.len(), "candidate message ids");

        // Keep only ids still in the INBOX (changes can surface moved/deleted
        // mail), then fetch their bodies.
        let mut messages = Vec::with_capacity(ids.len());
        for id in ids {
            match self.fetch_one(&id).await {
                Ok(Some(msg)) => messages.push(msg),
                Ok(None) => debug!(%id, "message no longer in inbox; skipping"),
                Err(e) => warn!(%id, error = %e, "failed to fetch message; skipping"),
            }
        }
        Ok((messages, new_state))
    }

    /// `Email/changes` since `state`: returns created+updated ids and the new
    /// state cursor.
    async fn changed_ids(&self, state: String) -> Result<(Vec<String>, String)> {
        let mut changes = self
            .client
            .email_changes(state, None)
            .await
            .context("Email/changes")?;
        let new_state = changes.take_new_state();
        let mut ids = changes.take_created();
        ids.extend(changes.take_updated());
        Ok((ids, new_state))
    }

    /// First run (no prior state): query everything currently in the INBOX and
    /// capture the current Email state so subsequent runs are incremental.
    async fn bootstrap_ids(&self) -> Result<(Vec<String>, String)> {
        let mut query = self
            .client
            .email_query(
                email::query::Filter::in_mailbox(&self.inbox_id).into(),
                None::<Vec<_>>,
            )
            .await
            .context("Email/query INBOX")?;
        let ids = query.take_ids();
        let state = self.current_email_state().await?;
        Ok((ids, state))
    }

    /// Read the current `Email` state via a minimal `Email/get`.
    async fn current_email_state(&self) -> Result<String> {
        let mut request = self.client.build();
        request.get_email().ids(Vec::<String>::new());
        let response: EmailGetResponse = request
            .send_single()
            .await
            .context("Email/get for state cursor")?;
        Ok(response.state().to_string())
    }

    /// Fetch one message's subject + decoded text body, if still in the INBOX.
    async fn fetch_one(&self, id: &str) -> Result<Option<FetchedMessage>> {
        let mut request = self.client.build();
        let get = request.get_email();
        get.ids([id]).properties([
            Property::Id,
            Property::BlobId,
            Property::MailboxIds,
            Property::Subject,
            Property::From,
            Property::TextBody,
            Property::BodyValues,
            Property::Attachments,
        ]);
        // Body-value fetching is a `GetArguments` concern, reached via
        // `arguments()`. Forwarded receipts are small; cap to bound responses.
        get.arguments()
            .fetch_all_body_values(true)
            .max_body_value_bytes(256 * 1024);

        let mut response: EmailGetResponse =
            request.send_single().await.context("Email/get body")?;
        let email = match response.take_list().into_iter().next() {
            Some(e) => e,
            None => return Ok(None),
        };

        if !email.mailbox_ids().iter().any(|m| *m == self.inbox_id) {
            return Ok(None); // moved out from under us
        }

        // Prefer the JMAP-decoded text body. If it is empty (e.g. an HTML-only
        // forward), fall back to downloading the raw blob and decoding it with
        // mail-parser (which down-converts HTML to text).
        let mut text = decoded_text(&email);
        if text.trim().is_empty()
            && let Some(blob_id) = email.blob_id()
        {
            match self.client.download(blob_id).await {
                Ok(raw) => {
                    if let Some(t) = crate::unwrap::text_from_raw(&raw) {
                        text = t;
                    }
                }
                Err(e) => warn!(%id, error = %e, "raw blob download failed"),
            }
        }

        let attachments = email
            .attachments()
            .unwrap_or(&[])
            .iter()
            .filter_map(|a| {
                a.blob_id().map(|blob_id| Attachment {
                    blob_id: blob_id.to_string(),
                    content_type: a.content_type().map(str::to_string),
                    name: a.name().map(str::to_string),
                    size: a.size(),
                })
            })
            .collect();

        Ok(Some(FetchedMessage {
            id: id.to_string(),
            subject: email.subject().map(str::to_string),
            from: render_from(&email),
            text,
            attachments,
        }))
    }

    /// Download a blob (e.g. a statement PDF attachment) by its `blob_id`.
    pub async fn download(&self, blob_id: &str) -> Result<Vec<u8>> {
        self.client
            .download(blob_id)
            .await
            .with_context(|| format!("downloading blob {blob_id}"))
    }

    /// Move a message to the Processed mailbox.
    pub async fn move_to_processed(&self, id: &str) -> Result<()> {
        self.move_to(id, &self.processed_id).await
    }

    /// Move a message to the Review mailbox.
    pub async fn move_to_review(&self, id: &str) -> Result<()> {
        self.move_to(id, &self.review_id).await
    }

    async fn move_to(&self, id: &str, mailbox_id: &str) -> Result<()> {
        self.client
            .email_set_mailboxes(id, [mailbox_id])
            .await
            .with_context(|| format!("moving {id} to {mailbox_id}"))?;
        Ok(())
    }
}

/// Render the `From:` header of an email as a single string the unwrap layer
/// can feed to `extract_email`. JMAP exposes `From` as structured addresses, so
/// we reconstruct the familiar `Name <addr>` / `<addr>` shapes. Returns `None`
/// when the message carries no usable from-address.
fn render_from(email: &jmap_client::email::Email) -> Option<String> {
    let addr = email.from()?.iter().find(|a| !a.email().is_empty())?;
    Some(match addr.name() {
        Some(name) if !name.trim().is_empty() => {
            format!("{} <{}>", name.trim(), addr.email())
        }
        _ => format!("<{}>", addr.email()),
    })
}

/// Extract the decoded text body of an email, concatenating any text parts.
fn decoded_text(email: &jmap_client::email::Email) -> String {
    let mut out = String::new();
    if let Some(parts) = email.text_body() {
        for part in parts {
            if let Some(pid) = part.part_id()
                && let Some(bv) = email.body_value(pid)
            {
                out.push_str(bv.value());
                out.push('\n');
            }
        }
    }
    out
}

async fn mailbox_id_by_role(client: &Client, role: Role) -> Result<Option<String>> {
    let mut q = client
        .mailbox_query(mailbox::query::Filter::role(role).into(), None::<Vec<_>>)
        .await
        .context("Mailbox/query by role")?;
    Ok(q.take_ids().into_iter().next())
}

async fn mailbox_id_by_name(client: &Client, name: &str) -> Result<Option<String>> {
    let mut q = client
        .mailbox_query(mailbox::query::Filter::name(name).into(), None::<Vec<_>>)
        .await
        .context("Mailbox/query by name")?;
    Ok(q.take_ids().into_iter().next())
}

/// Host portion of a URL, for the JMAP client's redirect allowlist.
fn host_of(url: &str) -> Vec<String> {
    let no_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let host = no_scheme
        .split(['/', ':'])
        .next()
        .unwrap_or(no_scheme)
        .to_string();
    if host.is_empty() {
        Vec::new()
    } else {
        vec![host]
    }
}

/// Load the persisted JMAP state cursor, if the file exists and is non-empty.
pub fn load_state(path: &str) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    }
}

/// Persist the JMAP state cursor, creating the parent directory if needed.
pub fn save_state(path: &str, state: &str) -> Result<()> {
    if let Some(parent) = Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating state dir {}", parent.display()))?;
    }
    fs::write(path, state).with_context(|| format!("writing state file {path}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_extraction() {
        assert_eq!(
            host_of("http://jmap.internal.example:8080"),
            vec!["jmap.internal.example".to_string()]
        );
        assert_eq!(
            host_of("https://mail.example.com/jmap"),
            vec!["mail.example.com".to_string()]
        );
    }

    #[test]
    fn state_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rl-state-{}", std::process::id()));
        let path = dir.join("jmap.state");
        let p = path.to_str().unwrap();
        assert!(load_state(p).is_none());
        save_state(p, "state-123").unwrap();
        assert_eq!(load_state(p).as_deref(), Some("state-123"));
        let _ = fs::remove_dir_all(&dir);
    }
}
