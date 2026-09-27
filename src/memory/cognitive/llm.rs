//! Thin LLM abstraction used by the cognify pipeline.
//!
//! Kept narrow on purpose — `cognitive::cognify` only needs to send a prompt
//! and get JSON back. The concrete implementation (a `local:<key>` runtime
//! model, OpenAI, Ollama, …) is wired by the caller. This way the cognitive
//! layer stays independent of runtime-client details and can be unit-tested
//! with a canned `StubLlm`.

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

#[async_trait]
pub trait LlmClient: Send + Sync {
    /// Single-turn completion. Returns the raw assistant text.
    /// Implementations MUST be deterministic enough for downstream JSON
    /// parsing — the cognify prompt instructs the model to return JSON only.
    async fn complete(&self, system: &str, user: &str) -> Result<String>;
}

/// Feed one cognify LLM call into the global usage recorder (source
/// `cognitive`). Implementations call this with the provider's raw response
/// JSON right after a successful HTTP round-trip; a response without a usage
/// object, or a daemon without the recorder running (unit tests, CLI
/// one-shots), is a silent no-op. Kept here so every `LlmClient` impl shares
/// one code path instead of re-parsing usage five ways.
pub(crate) fn record_cognitive_usage(
    provider: &str,
    model: &str,
    response_json: &serde_json::Value,
    latency_ms: u64,
) {
    let Some(rec) = crate::usage::global() else {
        return;
    };
    let Some(u) = crate::zen_core::RawUsage::from_json(&response_json["usage"]) else {
        return;
    };
    rec.record(
        crate::usage::UsageEvent {
            agent_id: "cognify".to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            latency_ms,
            ..crate::usage::UsageEvent::new(crate::usage::UsageSource::Cognitive)
        }
        .with_tokens(&u),
    );
}

// =====================================================================
// Raw response schema — what the LLM is asked to emit.
// =====================================================================

/// One extracted triplet. Matches the JSON the LLM is prompted to return.
///
/// `subject_type` / `object_type` are the cognee-style entity categories
/// (e.g. `"person"`, `"city"`). They are optional: smaller models often omit
/// them, and `#[serde(default)]` keeps older `{subject,predicate,object}`
/// payloads parsing unchanged. An empty / missing type means "untyped".
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RawTriplet {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    #[serde(default)]
    pub subject_type: String,
    #[serde(default)]
    pub object_type: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RawTripletEnvelope {
    pub triplets: Vec<RawTriplet>,
}

/// Best-effort JSON extraction — handles models that wrap the JSON in
/// markdown fences or prose. Returns `Err` if no plausible JSON envelope is
/// found.
pub(crate) fn parse_triplets(raw: &str) -> Result<Vec<RawTriplet>> {
    // Strip ```json fences if present.
    let trimmed = raw.trim();
    let body = if let Some(rest) = trimmed.strip_prefix("```json") {
        rest.trim_start_matches('\n')
            .trim_end_matches("```")
            .trim_end_matches('`')
    } else if let Some(rest) = trimmed.strip_prefix("```") {
        rest.trim_start_matches('\n')
            .trim_end_matches("```")
            .trim_end_matches('`')
    } else {
        trimmed
    };

    // Find the first `{` ... last `}` to allow models that prepend prose.
    let start = body.find('{');
    let end = body.rfind('}');
    let json_slice = match (start, end) {
        (Some(s), Some(e)) if e >= s => &body[s..=e],
        _ => body,
    };

    let env: RawTripletEnvelope = serde_json::from_str(json_slice)
        .map_err(|e| anyhow::anyhow!("triplet JSON parse failed: {e} — body was: {body}"))?;
    Ok(env.triplets)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use tokio::sync::Mutex;

    /// Test double — returns the next canned reply on each `complete` call.
    pub struct StubLlm {
        replies: Mutex<Vec<String>>,
    }

    impl StubLlm {
        pub fn new(replies: Vec<String>) -> Self {
            Self {
                replies: Mutex::new(replies),
            }
        }
    }

    #[async_trait]
    impl LlmClient for StubLlm {
        async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
            let mut q = self.replies.lock().await;
            if q.is_empty() {
                anyhow::bail!("StubLlm exhausted");
            }
            Ok(q.remove(0))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plain_json() {
        let raw = r#"{"triplets":[{"subject":"Ada","predicate":"invented","object":"compiler"}]}"#;
        let t = parse_triplets(raw).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].subject, "Ada");
    }

    #[test]
    fn parse_fenced_json() {
        let raw = "```json\n{\"triplets\":[{\"subject\":\"a\",\"predicate\":\"p\",\"object\":\"b\"}]}\n```";
        assert_eq!(parse_triplets(raw).unwrap().len(), 1);
    }

    #[test]
    fn parse_with_preamble() {
        let raw = "Sure, here are the triplets:\n{\"triplets\":[{\"subject\":\"x\",\"predicate\":\"y\",\"object\":\"z\"}]}";
        let t = parse_triplets(raw).unwrap();
        assert_eq!(t[0].object, "z");
    }
}
