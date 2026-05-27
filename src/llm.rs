//! ollama-router chat-completions client.
//!
//! Speaks the OpenAI-compatible `/chat/completions` endpoint that ollama-router
//! fronts. We request JSON-object output and return the model's raw answer
//! parsed as a [`serde_json::Value`], leaving field extraction to the adapter's
//! `postprocess`. The client itself does no validation of the *contents* —
//! that is the validation gate's job.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;

/// A thin wrapper over the chat-completions endpoint.
pub struct LlmClient<'a> {
    http: &'a Client,
    /// OpenAI-compatible base, e.g. `http://ollama-router.ai:11434/v1`.
    base_url: String,
    model: String,
    /// Per-request timeout, applied only to the chat-completions call so a slow
    /// cold-loading model does not abort under the shared client's tighter cap.
    timeout: Duration,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    /// Force deterministic-ish extraction.
    temperature: f32,
    /// OpenAI JSON-mode hint; ollama honours `format: json` natively and most
    /// routers accept `response_format` too. Harmless if ignored.
    response_format: ResponseFormat,
    /// Jinja chat-template arguments forwarded by ollama-router to the model.
    /// Setting `enable_thinking: false` disables Qwen3-family reasoning (the
    /// model emits an empty `<think></think>` then clean JSON — much faster).
    /// A no-op for non-Qwen models, which ignore unknown chat-template kwargs.
    chat_template_kwargs: ChatTemplateKwargs,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Serialize)]
struct ChatTemplateKwargs {
    enable_thinking: bool,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: String,
}

impl<'a> LlmClient<'a> {
    pub fn new(
        http: &'a Client,
        base_url: impl Into<String>,
        model: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            model: model.into(),
            timeout,
        }
    }

    /// Send `prompt` and return the model's answer parsed as JSON.
    pub async fn extract_json(&self, prompt: &str) -> Result<Value> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let request = ChatRequest {
            model: &self.model,
            messages: vec![ChatMessage {
                role: "user",
                content: prompt,
            }],
            temperature: 0.0,
            response_format: ResponseFormat {
                kind: "json_object",
            },
            chat_template_kwargs: ChatTemplateKwargs {
                enable_thinking: false,
            },
        };

        debug!(%url, model = %self.model, timeout_secs = self.timeout.as_secs(), "requesting extraction");
        let resp = self
            .http
            .post(&url)
            .json(&request)
            // Per-request override of the shared client's timeout: extraction on
            // a cold reasoning model can run for minutes.
            .timeout(self.timeout)
            .send()
            .await
            .context("sending chat-completions request")?
            .error_for_status()
            .context("chat-completions returned an error status")?;

        let parsed: ChatResponse = resp
            .json()
            .await
            .context("decoding chat-completions response")?;

        let content = parsed
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| anyhow!("chat-completions returned no choices"))?;

        parse_json_content(&content)
    }
}

/// Parse the model's textual answer into JSON, tolerating reasoning blocks,
/// markdown fences, and trailing prose that models add despite JSON mode.
fn parse_json_content(content: &str) -> Result<Value> {
    let json = extract_json(content)
        .with_context(|| format!("could not locate a JSON object in model answer: {content}"))?;
    serde_json::from_str::<Value>(&json)
        .with_context(|| format!("extracted JSON span did not parse: {json}"))
}

/// Pull a single JSON object out of a model's free-form answer.
///
/// Fast path: if the trimmed content already parses as a JSON value, return it
/// verbatim — no surgery, so a perfectly-formed answer (the common case under
/// JSON mode) is never disturbed.
///
/// Slow path (only when the trimmed content is not itself valid JSON):
///   1. drop any `<think> … </think>` reasoning block(s) (reasoning models),
///   2. drop Markdown code fences (```json … ``` or ``` … ```),
///   3. take the slice from the first `{` to its *matching* `}` via a
///      string-aware balanced-brace scan (so trailing prose after the object is
///      ignored, and a `}` inside a JSON string value does not truncate).
///
/// Returns the object slice as an owned `String` (the `<think>`/fence steps may
/// reallocate, so a borrow cannot span all cases).
fn extract_json(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if serde_json::from_str::<Value>(trimmed).is_ok() {
        return Some(trimmed.to_string());
    }
    let without_think = strip_think_blocks(content);
    let unfenced = strip_fences(&without_think);
    balanced_object_span(&unfenced).map(str::to_string)
}

/// Remove every `<think> … </think>` block, case-insensitively and across
/// newlines. Unterminated `<think>` (no closing tag) drops the remainder, which
/// is the safe choice — a half-streamed reasoning block carries no JSON.
fn strip_think_blocks(s: &str) -> String {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    while let Some(rel_open) = lower[cursor..].find(OPEN) {
        let open = cursor + rel_open;
        out.push_str(&s[cursor..open]);
        match lower[open + OPEN.len()..].find(CLOSE) {
            Some(rel_close) => {
                // Skip past the closing tag and continue scanning.
                cursor = open + OPEN.len() + rel_close + CLOSE.len();
            }
            None => {
                // No closing tag: discard everything from the open tag onward.
                return out;
            }
        }
    }
    out.push_str(&s[cursor..]);
    out
}

/// Strip a single surrounding Markdown code fence, if the (trimmed) content is
/// wrapped in one. Handles ```json … ``` and plain ``` … ```.
fn strip_fences(s: &str) -> String {
    let t = s.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t.to_string();
    };
    // After the opening fence an optional language tag runs to end-of-line.
    let body = match rest.split_once('\n') {
        Some((_lang, after)) => after,
        None => rest,
    };
    body.strip_suffix("```")
        .or_else(|| body.trim_end().strip_suffix("```"))
        .unwrap_or(body)
        .to_string()
}

/// Return the slice from the first `{` to its matching `}` via a *string-aware*
/// balanced-brace scan. `None` if there is no `{`, or if braces never balance.
///
/// Braces *inside a JSON string value* are ignored, so a merchant name like
/// `"Tasty } Burgers"` does not truncate the object early. The scanner tracks
/// whether it is inside a `"…"` string and honours `\`-escapes so an escaped
/// quote (`\"`) does not falsely toggle string state.
fn balanced_object_span(s: &str) -> Option<&str> {
    let start = s.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, ch) in s[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + offset + ch.len_utf8();
                    return Some(&s[start..end]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Extracted;

    /// A complete extraction object the PayPal prompt asks for, used to assert
    /// the extracted span actually deserializes into the typed schema.
    const RECEIPT_OBJECT: &str = r#"{
        "source": "paypal",
        "external_id": "8XY12345AB678901C",
        "amount": "1.00",
        "currency": "EUR",
        "direction": "out",
        "date": "2026-05-11",
        "merchant": "Example Merchant B.V.",
        "account_hint": "Pay in 4",
        "status": "approved",
        "raw_ref": "TESTORDER0123456"
    }"#;

    fn assert_extracts_receipt(content: &str) {
        let span = extract_json(content).expect("a JSON object should be located");
        let extracted: Extracted =
            serde_json::from_str(&span).expect("extracted span should deserialize to Extracted");
        assert_eq!(extracted.external_id.as_deref(), Some("8XY12345AB678901C"));
        assert_eq!(extracted.currency().as_str(), "EUR");
        assert_eq!(extracted.merchant, "Example Merchant B.V.");
    }

    // (a) bare JSON.
    #[test]
    fn extracts_bare_json() {
        assert_extracts_receipt(RECEIPT_OBJECT);
    }

    // (b) JSON preceded by a <think> reasoning block.
    #[test]
    fn extracts_after_think_block() {
        let content = format!(
            "<think>\nThe transaction id is 8XY..., the total is 1.00 EUR.\nLet me format it.\n</think>\n{RECEIPT_OBJECT}"
        );
        assert_extracts_receipt(&content);
    }

    // (c) JSON inside a ```json fence.
    #[test]
    fn extracts_from_json_fence() {
        let content = format!("```json\n{RECEIPT_OBJECT}\n```");
        assert_extracts_receipt(&content);
    }

    // (d) JSON followed by trailing prose (with a stray brace, to prove the
    // balanced scan stops at the matching `}` rather than the last one).
    #[test]
    fn extracts_with_trailing_prose() {
        let content =
            format!("{RECEIPT_OBJECT}\n\nHope that helps! Let me know if anything looks off }}.");
        assert_extracts_receipt(&content);
    }

    // The combination the qwen3.x tiers actually emit: think block, fence, and
    // trailing prose all at once.
    #[test]
    fn extracts_through_think_fence_and_prose() {
        let content =
            format!("<THINK>multi\nline\nreasoning</THINK>\n```json\n{RECEIPT_OBJECT}\n```\nDone.");
        assert_extracts_receipt(&content);
    }

    #[test]
    fn balanced_scan_ignores_nested_then_trailing() {
        // Nested object must not close the outer span early.
        let s = r#"prefix {"a":{"b":1},"c":2} trailing }"#;
        assert_eq!(balanced_object_span(s), Some(r#"{"a":{"b":1},"c":2}"#));
    }

    // M2: a `}` inside a JSON string value must not truncate the object.
    #[test]
    fn balanced_scan_ignores_brace_inside_string() {
        let s = r#"{"merchant":"Tasty } Burgers","amount":"1.00"}"#;
        assert_eq!(balanced_object_span(s), Some(s));
    }

    #[test]
    fn balanced_scan_honours_escaped_quote_in_string() {
        // An escaped quote must not end the string early, so the `}` after it
        // (still inside the string) does not truncate.
        let s = r#"{"merchant":"He said \"hi} there\"","amount":"1.00"}"#;
        assert_eq!(balanced_object_span(s), Some(s));
    }

    // M2: the string-brace case end-to-end through the extractor (with prose).
    #[test]
    fn extracts_object_with_brace_in_merchant_name() {
        let content = r#"```json
{"source":"paypal","external_id":"X","amount":"1.00","currency":"EUR","direction":"out","date":"2026-05-11","merchant":"Tasty } Burgers","account_hint":"","status":"approved","raw_ref":"X"}
```
Done."#;
        let span = extract_json(content).expect("a JSON object should be located");
        let v: serde_json::Value = serde_json::from_str(&span).unwrap();
        assert_eq!(v["merchant"], "Tasty } Burgers");
    }

    #[test]
    fn strips_multiple_think_blocks() {
        let s = "<think>one</think>keep<Think>two</Think>more";
        assert_eq!(strip_think_blocks(s), "keepmore");
    }

    #[test]
    fn unterminated_think_drops_remainder() {
        assert_eq!(strip_think_blocks("good<think>oops no close"), "good");
    }

    #[test]
    fn parse_json_content_errors_on_non_json() {
        assert!(parse_json_content("no json here").is_err());
    }

    // --- property tests --------------------------------------------------

    use proptest::prelude::*;

    proptest! {
        /// Round-trip: a flat JSON object, wrapped in arbitrary prose before and
        /// after (including stray braces), is recovered byte-for-byte by the
        /// string-aware span scan and re-parses to the same value — even when a
        /// string value itself contains `{`/`}`.
        #[test]
        fn prop_balanced_span_roundtrip(
            merchant in "[A-Za-z{} ]{0,24}",
            amount in "[0-9]{1,6}\\.[0-9]{2}",
            pre in "[a-z .}]{0,12}",
            post in "[a-z .}]{0,12}",
        ) {
            // Build a real object so the embedded braces are inside a JSON
            // string, and serde gives us the canonical escaped form.
            let obj = serde_json::json!({"merchant": merchant, "amount": amount});
            let canonical = serde_json::to_string(&obj).unwrap();
            let wrapped = format!("{pre}{canonical}{post}");

            let span = balanced_object_span(&wrapped)
                .expect("span scan must find the object");
            // The recovered span is exactly the canonical object text ...
            prop_assert_eq!(span, canonical.as_str());
            // ... and re-parses to the same value, merchant intact.
            let v: serde_json::Value = serde_json::from_str(span).unwrap();
            prop_assert_eq!(v["merchant"].as_str().unwrap(), merchant.as_str());
        }
    }
}
