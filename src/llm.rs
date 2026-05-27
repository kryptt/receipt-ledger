//! ollama-router chat-completions client.
//!
//! Speaks the OpenAI-compatible `/chat/completions` endpoint that ollama-router
//! fronts. We request JSON-object output and return the model's raw answer
//! parsed as a [`serde_json::Value`], leaving field extraction to the adapter's
//! `postprocess`. The client itself does no validation of the *contents* —
//! that is the validation gate's job.

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
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
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
    pub fn new(http: &'a Client, base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            model: model.into(),
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
            response_format: ResponseFormat { kind: "json_object" },
        };

        debug!(%url, model = %self.model, "requesting extraction");
        let resp = self
            .http
            .post(&url)
            .json(&request)
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

/// Parse the model's textual answer into JSON, tolerating markdown fences and
/// leading/trailing prose that small models sometimes add despite JSON mode.
fn parse_json_content(content: &str) -> Result<Value> {
    // Fast path: the whole thing is valid JSON.
    if let Ok(v) = serde_json::from_str::<Value>(content.trim()) {
        return Ok(v);
    }
    // Strip a ```json ... ``` fence if present.
    let stripped = strip_fence(content);
    if let Ok(v) = serde_json::from_str::<Value>(stripped.trim()) {
        return Ok(v);
    }
    // Last resort: grab the outermost {...} or [...] span.
    if let Some(span) = outermost_json_span(stripped) {
        return serde_json::from_str::<Value>(span)
            .with_context(|| format!("extracted JSON span did not parse: {span}"));
    }
    Err(anyhow!("model answer was not JSON: {content}"))
}

fn strip_fence(s: &str) -> &str {
    let t = s.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    t.strip_suffix("```").unwrap_or(t)
}

/// Return the smallest slice spanning the first opening bracket to its matching
/// last closing bracket of the same family. Good enough to rescue an object or
/// array embedded in prose.
fn outermost_json_span(s: &str) -> Option<&str> {
    let obj = (s.find('{'), s.rfind('}'));
    let arr = (s.find('['), s.rfind(']'));
    let pick = |open: Option<usize>, close: Option<usize>| match (open, close) {
        (Some(o), Some(c)) if c > o => Some((o, c)),
        _ => None,
    };
    match (pick(obj.0, obj.1), pick(arr.0, arr.1)) {
        (Some((o, c)), None) => Some(&s[o..=c]),
        (None, Some((o, c))) => Some(&s[o..=c]),
        (Some((oo, oc)), Some((ao, ac))) => {
            // Prefer whichever opens first.
            if oo <= ao {
                Some(&s[oo..=oc])
            } else {
                Some(&s[ao..=ac])
            }
        }
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_json() {
        let v = parse_json_content(r#"{"a":1}"#).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn parses_fenced_json() {
        let v = parse_json_content("```json\n{\"a\":1}\n```").unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn rescues_json_from_prose() {
        let v = parse_json_content("Sure! Here you go: {\"a\":1} hope that helps").unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn errors_on_non_json() {
        assert!(parse_json_content("no json here").is_err());
    }
}
