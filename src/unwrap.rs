//! Gmail-forward unwrapping + original-sender detection.
//!
//! Notification mails reach the `ledger@` mailbox as Gmail forwards. The
//! envelope `From:` is the human who forwards them; the *original* sender
//! (e.g. `service@paypal.com`) lives inside the forwarded block:
//!
//! ```text
//! ---------- Forwarded message ---------
//! From: PayPal <service@paypal.com>
//! Date: Mon, 11 May 2026, 19:10
//! Subject: ...
//! To: buyer@example.com
//!
//! <original body>
//! ```
//!
//! This module is deterministic and operates on the already-decoded text body
//! (JMAP hands us the decoded `text/plain` value), so it is fully unit-testable
//! against a fixture with no network or MIME machinery.

use mail_parser::MessageParser;

/// The marker Gmail inserts ahead of a forwarded message. We match on the
/// distinctive prefix rather than the exact dash count, which varies.
const FORWARD_MARKER: &str = "Forwarded message";

/// Decode a raw RFC822 message into its plain-text body.
///
/// Used as a fallback when JMAP does not hand us a decoded `text/plain` value
/// (e.g. an HTML-only forward): we parse the raw blob with `mail-parser` and
/// take its text rendition (HTML is down-converted to text). Returns `None`
/// when the message has no recoverable text body.
#[must_use]
pub fn text_from_raw(raw: &[u8]) -> Option<String> {
    let message = MessageParser::default().parse(raw)?;
    message.body_text(0).map(|c| c.into_owned())
}

/// An email body split into the recovered original sender address and the
/// original message text (everything after the forwarded headers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unwrapped {
    /// Lower-cased original sender email address (e.g. `service@paypal.com`).
    pub original_sender: String,
    /// The original `Subject:` line value, if recovered.
    pub original_subject: Option<String>,
    /// The original message body (text after the forwarded header block).
    pub body: String,
}

/// Recover the original sender + body from a Gmail-forwarded text body.
///
/// Returns `None` when no forwarded block is present or no sender address can
/// be located — the caller routes such messages to `Review` rather than
/// guessing.
#[must_use]
pub fn unwrap_forward(text: &str) -> Option<Unwrapped> {
    let marker_pos = text.find(FORWARD_MARKER)?;
    // Region after the marker line holds the forwarded headers + body.
    let after_marker = &text[marker_pos..];
    let forwarded = after_marker
        .split_once('\n')
        .map(|(_, rest)| rest)
        .unwrap_or(after_marker);

    let original_sender = find_header(forwarded, "From").and_then(|v| extract_email(&v))?;
    let original_subject = find_header(forwarded, "Subject");

    // The body begins after the first blank line that follows the forwarded
    // header block. If we cannot find one, treat the whole region as body.
    let body = split_body(forwarded).to_string();

    Some(Unwrapped {
        original_sender: original_sender.to_ascii_lowercase(),
        original_subject,
        body,
    })
}

/// Recover the original sender + body from *any* forwarded receipt, whether a
/// manual Gmail "Fwd:" (with a "Forwarded message" marker) or a Gmail
/// *auto*-forward (no marker — the envelope `From:` is preserved as the original
/// sender and the body IS the original email).
///
/// Strategy:
/// 1. If a "Forwarded message" marker is present, this is a *manual* forward:
///    the real original sender lives in the forwarded header block, NOT the
///    envelope `From:` (which is the human who forwarded it). We extract the
///    inner sender via [`unwrap_forward`]. If the marker is present but the
///    inner sender cannot be parsed, we return `None` — we must NOT fall back to
///    the human forwarder's envelope `From:`, which would mis-attribute the mail
///    to a person rather than its bank/PayPal origin. Such a message routes to
///    Review for human eyes.
/// 2. Only when there is NO marker (an auto-forward) do we fall back to the
///    envelope `from` header. The whole text body is the original message.
///
/// Returns `None` when no sender can be recovered — the caller routes such
/// messages to `Review` rather than guessing.
#[must_use]
pub fn unwrap_message(from: Option<&str>, text: &str) -> Option<Unwrapped> {
    // Marker present → manual forward → the inner sender is authoritative.
    // Marker-present-but-unparseable yields None (do NOT use the envelope From).
    if text.contains(FORWARD_MARKER) {
        return unwrap_forward(text);
    }

    // No marker → auto-forward → the envelope From is the original sender.
    let original_sender = extract_email(from?)?;
    Some(Unwrapped {
        original_sender: original_sender.to_ascii_lowercase(),
        original_subject: None,
        body: text.to_string(),
    })
}

/// Find the value of a `Header:` line (case-insensitive name match) within the
/// forwarded header block. Only scans until the first blank line so a header
/// name appearing in the body cannot be mistaken for a real header.
fn find_header(forwarded: &str, name: &str) -> Option<String> {
    for line in forwarded.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            break; // end of header block
        }
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case(name)
        {
            return Some(v.trim().to_string());
        }
    }
    None
}

/// Everything after the first blank line that terminates the forwarded header
/// block. Falls back to the whole slice when no blank line exists.
fn split_body(forwarded: &str) -> &str {
    let mut idx = 0usize;
    for line in forwarded.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.trim().is_empty() {
            return &forwarded[idx + line.len()..];
        }
        idx += line.len();
    }
    forwarded
}

/// Pull a bare email address out of a `From:` value such as
/// `PayPal <service@paypal.com>` or `service@paypal.com`.
fn extract_email(value: &str) -> Option<String> {
    if let (Some(lt), Some(gt)) = (value.find('<'), value.find('>'))
        && lt < gt
    {
        let inner = value[lt + 1..gt].trim();
        if inner.contains('@') {
            return Some(inner.to_string());
        }
    }
    // No angle brackets — take the first whitespace-delimited token with an '@'.
    value
        .split_whitespace()
        .find(|t| t.contains('@'))
        .map(|t| t.trim_matches(|c: char| !c.is_ascii_graphic()).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/paypal_accell.txt");

    #[test]
    fn recovers_paypal_sender_from_fixture() {
        let u = unwrap_forward(FIXTURE).expect("fixture is a forward");
        assert_eq!(u.original_sender, "service@paypal.com");
        assert!(
            u.original_subject
                .as_deref()
                .unwrap_or("")
                .contains("Example Merchant"),
            "subject recovered: {:?}",
            u.original_subject
        );
        assert!(
            u.body.contains("Transaction ID: 8XY12345AB678901C"),
            "body should contain the original receipt"
        );
        // The forwarded headers must NOT bleed into the recovered body.
        assert!(!u.body.contains("Forwarded message"));
    }

    #[test]
    fn extracts_bracketed_address() {
        assert_eq!(
            extract_email("PayPal <service@paypal.com>").as_deref(),
            Some("service@paypal.com")
        );
    }

    #[test]
    fn extracts_bare_address() {
        assert_eq!(
            extract_email("service@paypal.com").as_deref(),
            Some("service@paypal.com")
        );
    }

    #[test]
    fn none_when_not_a_forward() {
        assert!(unwrap_forward("just a regular email, no marker").is_none());
    }

    #[test]
    fn auto_forward_uses_envelope_from() {
        // No "Forwarded message" marker: a Gmail auto-forward where the envelope
        // From is preserved as the original sender and the body is the original.
        let body = "Notificación de Consumo\nMonto EUR$1.50\nEstatus Aprobada\n";
        let u = unwrap_message(Some("<notificaciones@popularenlinea.com>"), body)
            .expect("auto-forward falls back to envelope from");
        assert_eq!(u.original_sender, "notificaciones@popularenlinea.com");
        assert_eq!(u.original_subject, None);
        // The full text is the original message — nothing stripped.
        assert_eq!(u.body, body);
    }

    #[test]
    fn manual_forward_prefers_inner_sender_over_envelope() {
        // The envelope From is the human forwarder; the marker block carries the
        // real original sender. unwrap_message must prefer the inner one.
        let text = "\
---------- Forwarded message ---------
From: PayPal <service@paypal.com>
Subject: receipt

You paid.
";
        let u = unwrap_message(Some("Buyer <buyer@example.com>"), text)
            .expect("manual forward is still unwrapped");
        assert_eq!(u.original_sender, "service@paypal.com");
    }

    #[test]
    fn none_when_neither_marker_nor_from() {
        assert!(unwrap_message(None, "a plain message").is_none());
    }

    #[test]
    fn marker_present_but_unparseable_sender_does_not_fall_back_to_envelope() {
        // A manual forward whose forwarded header block has no usable From:.
        // We must NOT attribute it to the human forwarder's envelope From —
        // that would route a real receipt to the wrong adapter. → None → Review.
        let text = "\
---------- Forwarded message ---------
Date: Mon, 11 May 2026, 19:10
Subject: receipt

You paid.
";
        assert!(
            unwrap_message(Some("Buyer <buyer@example.com>"), text).is_none(),
            "marker present but sender unparseable must not use the envelope From"
        );
    }
}
