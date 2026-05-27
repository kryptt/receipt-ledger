//! Liveness-aware model selection against ollama-router.
//!
//! Goal: pick the highest-priority allowlisted model that is *already loaded*
//! on the router, so this background job never triggers a model swap that would
//! evict a model an interactive agent (Hermes/Roci) is using. Only if none of
//! the allowlist is live do we fall back to the first allowlist entry and let
//! the router cold-load it.
//!
//! Confirmed (2026-05-27): ollama-router exposes `GET /api/ps` at its root
//! (not under `/v1`), aggregating loaded models across backends in ollama
//! shape `{"models":[{"name":..,"model":..,"owned_by":..}]}`. We use that as
//! the liveness signal and fall back to `/v1/models` (available, not
//! necessarily loaded) only if `/api/ps` is unreachable.

use std::collections::HashSet;

use reqwest::Client;
use serde_json::Value;
use tracing::{debug, warn};

/// Choose a model to use for extraction.
///
/// Returns the chosen model id. Never errors on liveness-probe failure — it
/// falls back to the first allowlist entry, since an occasional cold load is
/// acceptable at hourly cadence. Errors only if the allowlist is empty (a
/// configuration bug).
pub async fn select_model(
    http: &Client,
    base_url: &str,
    allowlist: &[String],
) -> anyhow::Result<String> {
    let fallback = allowlist
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("model allowlist is empty"))?;

    let live = match query_live_models(http, base_url).await {
        Ok(set) => set,
        Err(e) => {
            warn!(error = %e, "liveness probe failed; using first allowlist model");
            return Ok(fallback);
        }
    };

    // Highest-priority allowlist model that is currently live wins.
    for model in allowlist {
        if live.contains(model) {
            debug!(%model, "selected live allowlist model");
            return Ok(model.clone());
        }
    }

    debug!(model = %fallback, "no allowlist model live; falling back to first (may cold-load)");
    Ok(fallback)
}

/// Query the set of currently-loaded model ids from ollama-router.
///
// Confirmed against the live router. We try, in order:
//   1. `GET {root}/api/ps`  — ollama-native "running models"
//      (`{"models":[{"name":"...","model":"..."}]}`); the real liveness signal.
//   2. `GET {base}/models`  — OpenAI-style `{"data":[{"id":"..."}]}`, *available*
//      (not necessarily loaded) models; weak fallback only if (1) is unreachable.
async fn query_live_models(http: &Client, base_url: &str) -> anyhow::Result<HashSet<String>> {
    let base = base_url.trim_end_matches('/');

    // Candidate 1: ollama-native /api/ps. Note ollama mounts this at the host
    // root, not under the OpenAI `/v1` prefix, so strip a trailing `/v1`.
    let root = base.strip_suffix("/v1").unwrap_or(base);
    if let Ok(set) = probe(http, &format!("{root}/api/ps"), parse_ollama_ps).await
        && !set.is_empty()
    {
        return Ok(set);
    }

    // Candidate 2: OpenAI-style /models.
    let set = probe(http, &format!("{base}/models"), parse_openai_models).await?;
    Ok(set)
}

async fn probe(
    http: &Client,
    url: &str,
    parse: fn(&Value) -> HashSet<String>,
) -> anyhow::Result<HashSet<String>> {
    debug!(%url, "probing for live models");
    let resp = http.get(url).send().await?.error_for_status()?;
    let body: Value = resp.json().await?;
    Ok(parse(&body))
}

/// Parse `{"models":[{"name":"gemma4:e2b", ...}]}` (ollama `/api/ps`).
fn parse_ollama_ps(body: &Value) -> HashSet<String> {
    body.get("models")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    m.get("name")
                        .or_else(|| m.get("model"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parse `{"data":[{"id":"gemma4:e2b"}]}` (OpenAI `/models`).
fn parse_openai_models(body: &Value) -> HashSet<String> {
    body.get("data")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_ollama_ps_shape() {
        let body = json!({"models": [{"name": "gemma4:e2b"}, {"name": "qwen3:8b"}]});
        let set = parse_ollama_ps(&body);
        assert!(set.contains("gemma4:e2b"));
        assert!(set.contains("qwen3:8b"));
    }

    #[test]
    fn parses_openai_models_shape() {
        let body = json!({"data": [{"id": "gemma4:e2b"}]});
        assert!(parse_openai_models(&body).contains("gemma4:e2b"));
    }

    #[test]
    fn empty_when_unexpected_shape() {
        assert!(parse_ollama_ps(&json!({"unexpected": true})).is_empty());
        assert!(parse_openai_models(&json!([])).is_empty());
    }
}
