//! OpenAI-compatible `LlmClient` — concrete backend for cognify triplet
//! extraction.
//!
//! Works with any provider that speaks the OpenAI `/v1/chat/completions`
//! shape (OpenAI, OpenRouter, Ollama-OpenAI, vLLM, LM Studio, llama.cpp
//! `--server`, etc.). The cognify prompt asks for JSON, so we set
//! `response_format = {"type":"json_object"}` when the model supports it.
//!
//! ## Configuration
//!
//! Reuses [`MemoryConfig`] fields to avoid adding a second auth surface:
//!   * `openai_api_key`   → Authorization header
//!   * `openai_base_url`  → endpoint root (e.g. `https://api.openai.com`)
//! And one new env var for the chat model (since `openai_model` is taken by
//! the embedding model):
//!   * `SENCLAW_COG_CHAT_MODEL`  default `gpt-4o-mini`
//!
//! Disabled-by-default: callers go through [`create_cognitive_llm`], which
//! returns a [`DisabledLlm`] when no API key is configured rather than
//! constructing a half-broken HTTP client.

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::llm::LlmClient;

const DEFAULT_MODEL: &str = "gpt-4o-mini";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

// =====================================================================
// Public client
// =====================================================================

pub struct OpenAiCompatLlm {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    /// Whether the endpoint honours `response_format = json_object`. Most
    /// real providers do; off-brand local servers sometimes 400. We default
    /// to true and fall back to plain text on 4xx — see [`complete`].
    request_json_object: bool,
}

impl OpenAiCompatLlm {
    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("build reqwest client")?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_key: api_key.into(),
            model: model.into(),
            request_json_object: true,
        })
    }

    /// Override default `response_format` handling — exposed so tests and
    /// stricter local servers can disable it without env var twiddling.
    pub fn with_json_object(mut self, on: bool) -> Self {
        self.request_json_object = on;
        self
    }

    fn endpoint(&self) -> String {
        // Allow both bare host (`https://api.openai.com`) and explicit
        // `/v1` suffix. Detect by presence of `/v1` to stay forgiving.
        if self.base_url.contains("/v1") {
            format!("{}/chat/completions", self.base_url)
        } else {
            format!("{}/v1/chat/completions", self.base_url)
        }
    }
}

// =====================================================================
// Wire schemas — internal only, kept private so the public surface stays
// small. Serializing structs (rather than json!{}) lets the unit tests
// snapshot the exact body shape.
// =====================================================================

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    temperature: f32,
    /// Always serialized as `false`. Some gateways (antigravity, various
    /// local proxies) default to SSE streaming when the field is absent —
    /// the streamed `data: {...chunk...}` body then fails the JSON parse
    /// and every cognify call soft-fails to `SkippedNoLlm` (observed as
    /// "N chunks, 0 edges" in the Knowledge UI).
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct ChatChoiceMessage {
    #[serde(default)]
    content: String,
}

// =====================================================================
// Internal helpers — extracted so they're directly testable.
// =====================================================================

pub(crate) fn build_body(
    model: &str,
    system: &str,
    user: &str,
    request_json_object: bool,
) -> serde_json::Value {
    let req = ChatRequest {
        model,
        messages: vec![
            ChatMessage {
                role: "system",
                content: system,
            },
            ChatMessage {
                role: "user",
                content: user,
            },
        ],
        temperature: 0.1, // low — we want deterministic JSON
        stream: false,
        response_format: if request_json_object {
            Some(ResponseFormat {
                kind: "json_object",
            })
        } else {
            None
        },
    };
    serde_json::to_value(&req).unwrap_or(serde_json::Value::Null)
}

pub(crate) fn parse_response(raw: &str) -> Result<String> {
    // Defence-in-depth: a gateway that streams despite `stream: false`
    // hands us an SSE body. Reassemble it instead of failing the parse.
    if raw.trim_start().starts_with("data:") {
        return parse_sse_response(raw);
    }
    let parsed: ChatResponse = serde_json::from_str(raw)
        .with_context(|| format!("chat-completion JSON parse failed: {raw}"))?;
    let choice = parsed
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("chat completion returned no choices"))?;
    Ok(choice.message.content)
}

/// Assemble assistant content from an OpenAI-style SSE stream body
/// (`data: {"choices":[{"delta":{"content":"…"}}]}` lines, terminated by
/// `data: [DONE]`). Non-JSON lines and empty deltas are skipped.
pub(crate) fn parse_sse_response(raw: &str) -> Result<String> {
    let mut out = String::new();
    for line in raw.lines() {
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        if let Some(delta) = v
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("content"))
            .and_then(|s| s.as_str())
        {
            out.push_str(delta);
        }
    }
    if out.is_empty() {
        anyhow::bail!("SSE chat stream contained no assistant content");
    }
    Ok(out)
}

#[async_trait]
impl LlmClient for OpenAiCompatLlm {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let started = std::time::Instant::now();
        let record = |text: &str| {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
                super::llm::record_cognitive_usage(
                    "openai",
                    &self.model,
                    &v,
                    started.elapsed().as_millis() as u64,
                );
            }
        };
        let url = self.endpoint();
        let body = build_body(&self.model, system, user, self.request_json_object);

        let mut req = self.client.post(&url).json(&body);
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        // An app-provided config carries an empty `api_key` on purpose — the
        // app proxy needs no credential of its own. The *daemon* route it goes
        // through does, once auth mode `always` is in force.
        if let Some((name, value)) = crate::util::internal_auth::header_for(&url) {
            req = req.header(name, value);
        }

        let resp = req.send().await.context("send chat request")?;
        let status = resp.status();
        let text = resp.text().await.context("read chat response body")?;

        if !status.is_success() {
            // Retry once without `response_format` if a 400 looks like the
            // server doesn't support json_object. Cheap to do; saves the
            // user a config tweak for local servers.
            if status.as_u16() == 400 && self.request_json_object {
                let body = build_body(&self.model, system, user, false);
                let mut retry = self.client.post(&url).json(&body);
                if !self.api_key.is_empty() {
                    retry = retry.bearer_auth(&self.api_key);
                }
                if let Some((name, value)) = crate::util::internal_auth::header_for(&url) {
                    retry = retry.header(name, value);
                }
                let resp = retry.send().await.context("retry chat request")?;
                let status = resp.status();
                let text = resp.text().await.context("read retry response body")?;
                if !status.is_success() {
                    anyhow::bail!("chat completion HTTP {status}: {text}");
                }
                record(&text);
                return parse_response(&text);
            }
            anyhow::bail!("chat completion HTTP {status}: {text}");
        }
        record(&text);
        parse_response(&text)
    }
}

// =====================================================================
// Factory
// =====================================================================

/// Build a cognitive LLM client from the current `Config`, or return None
/// if no LLM is configured anywhere. Resolution order:
///
/// 1. **Settings → LLM Models → Cognitive Model** (explicit user pick).
/// 2. **Settings → LLM Models → Main Model** — most installs only set
///    the Main model. Borrowing it here means the cognify pipeline works
///    out of the box without the user having to configure a second LLM.
/// 3. **Settings → LLM Models → Quick Model** — last UI fallback.
/// 4. **Env / MemoryConfig** — legacy `SENCLAW_OPENAI_*` env vars.
/// 5. **None** → cognify will soft-skip triplet extraction (chunks still
///    embed); CogAdd warns the agent in its return message.
///
/// Returns an `Arc<dyn LlmClient>` so we can pick the right adapter at
/// resolution time. Earlier this returned a concrete `OpenAiCompatLlm`,
/// which silently misbehaved when the user picked an Anthropic-provider
/// LLM as the Cognitive Model — `/v1/messages` and `/v1/chat/completions`
/// take incompatible payloads, so requests 4xx'd and cognify soft-failed
/// with `llm_skipped = true` even though the LLM *was* configured. We
/// now dispatch by [`LlmConfig::adapt`] (`"openai"` vs `"anthropic"`).
pub fn create_cognitive_llm(
    config: &crate::config::Config,
) -> Option<std::sync::Arc<dyn super::llm::LlmClient>> {
    let stored = crate::gateway::group_manager::load_llm_configs(&config.paths.global_config_path);

    // Try each LLM-config id in priority order. First one with both an
    // API key AND a base URL wins.
    let try_ids: [Option<&str>; 3] = [
        stored.active_cognitive_id.as_deref(),
        stored.active_id.as_deref(),
        stored.active_quick_id.as_deref(),
    ];
    for id in try_ids.iter().flatten() {
        if let Some(cfg) = stored.configs.iter().find(|c| c.id == *id) {
            let adapt_lc = cfg.adapt.trim().to_lowercase();
            tracing::debug!(
                llm_id = %cfg.id,
                model = %cfg.model_name,
                adapt = %adapt_lc,
                "[cognitive] LLM resolved from Settings"
            );

            // A local model is no longer an in-process runtime: a Space App
            // provider or a `local:<key>` GGUF/MLX model served by a runtime
            // is reached over loopback HTTP, registered as an ordinary
            // provider with `adapt: "openai"`. So it needs no special case
            // here — it falls through to the HTTP adapter below, where its
            // empty `api_key` is fine because the endpoint is the daemon's own
            // app/runtime proxy.
            // ───── HTTP adapters ─────
            let key = cfg.api_key.trim();
            let base = cfg.base_url.trim();
            if base.is_empty() {
                continue;
            }
            // An empty key disqualifies a *remote* config — it cannot
            // authenticate, and trying costs a round trip and a 401. A local
            // one is different: a model served by a Space App or a runtime is
            // reached through the daemon's own proxy on loopback, which needs
            // no credential and is handed none. Skipping those on an empty key
            // would make the cognitive layer silently unable to use any local
            // model.
            let is_local = crate::apps::llm_provider::is_app_config(&cfg.id) || crate::local_models::is_local_config(&cfg.id);
            if key.is_empty() && !is_local {
                continue;
            }
            let client: Option<std::sync::Arc<dyn super::llm::LlmClient>> =
                if adapt_lc == "anthropic" || adapt_lc == "claude" {
                    super::llm_anthropic::AnthropicLlm::new(base, key, cfg.model_name.clone())
                        .ok()
                        .map(|c| std::sync::Arc::new(c) as _)
                } else {
                    OpenAiCompatLlm::new(base, key, cfg.model_name.clone())
                        .ok()
                        .map(|c| std::sync::Arc::new(c) as _)
                };
            if let Some(c) = client {
                return Some(c);
            }
        }
    }

    // Env fallback (assumes OpenAI shape — no Anthropic env vars are
    // wired today).
    let key = config.memory.openai_api_key.trim();
    if key.is_empty() {
        return None;
    }
    let base_url = if config.memory.openai_base_url.trim().is_empty() {
        "https://api.openai.com".to_owned()
    } else {
        config.memory.openai_base_url.clone()
    };
    let model = std::env::var("SENCLAW_COG_CHAT_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_owned());

    OpenAiCompatLlm::new(base_url, key.to_owned(), model)
        .ok()
        .map(|c| std::sync::Arc::new(c) as std::sync::Arc<dyn super::llm::LlmClient>)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_handles_bare_host() {
        let llm = OpenAiCompatLlm::new("https://api.openai.com", "k", "m").unwrap();
        assert_eq!(llm.endpoint(), "https://api.openai.com/v1/chat/completions");
    }

    #[test]
    fn endpoint_preserves_existing_v1() {
        let llm = OpenAiCompatLlm::new("https://example.com/v1", "k", "m").unwrap();
        assert_eq!(llm.endpoint(), "https://example.com/v1/chat/completions");
    }

    #[test]
    fn endpoint_strips_trailing_slash() {
        let llm = OpenAiCompatLlm::new("https://example.com/", "k", "m").unwrap();
        assert_eq!(llm.endpoint(), "https://example.com/v1/chat/completions");
    }

    #[test]
    fn build_body_emits_system_user_temperature_json_object() {
        let body = build_body("gpt-x", "sys-prompt", "usr-prompt", true);
        assert_eq!(body["model"], "gpt-x");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "sys-prompt");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "usr-prompt");
        let temp = body["temperature"].as_f64().expect("temperature is number");
        assert!((temp - 0.1).abs() < 1e-4, "temperature ≈ 0.1, got {temp}");
        assert_eq!(body["response_format"]["type"], "json_object");
    }

    #[test]
    fn build_body_skips_response_format_when_disabled() {
        let body = build_body("m", "s", "u", false);
        assert!(body.get("response_format").is_none());
    }

    /// Regression: gateways like antigravity default to SSE streaming when
    /// `stream` is absent — the body must always pin it to false.
    #[test]
    fn build_body_pins_stream_false() {
        let body = build_body("m", "s", "u", true);
        assert_eq!(body["stream"], false);
    }

    /// Regression: a gateway that streams anyway must still parse — every
    /// cognify call used to soft-fail here ("57 chunks, 0 edges").
    #[test]
    fn parse_response_reassembles_sse_stream() {
        let raw = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"triplets\\\":\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"[]}\"}}]}\n\n",
            "data: [DONE]\n",
        );
        assert_eq!(parse_response(raw).unwrap(), "{\"triplets\":[]}");
    }

    #[test]
    fn parse_sse_response_errors_when_stream_has_no_content() {
        let raw = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\ndata: [DONE]\n";
        assert!(parse_sse_response(raw).is_err());
    }

    #[test]
    fn parse_response_extracts_content() {
        let raw = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#;
        assert_eq!(parse_response(raw).unwrap(), "hello");
    }

    #[test]
    fn parse_response_handles_missing_content() {
        // Some providers return null/missing content on tool-only responses.
        let raw = r#"{"choices":[{"message":{"role":"assistant"}}]}"#;
        assert_eq!(parse_response(raw).unwrap(), "");
    }

    #[test]
    fn parse_response_errors_on_no_choices() {
        let raw = r#"{"choices":[]}"#;
        assert!(parse_response(raw).is_err());
    }

    /// Build a Config pointing at a fresh empty `global_config.json` so the
    /// new "Settings UI selection" resolution path can't accidentally
    /// satisfy the test from a developer's real saved config.
    fn cfg_with_isolated_config() -> crate::config::Config {
        let mut cfg = crate::config::Config::from_env();
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("global_config.json");
        // Leak the TempDir so the file persists for the lifetime of the test.
        std::mem::forget(tmp);
        cfg.paths.global_config_path = path;
        cfg
    }

    #[test]
    fn create_returns_none_without_api_key() {
        let mut cfg = cfg_with_isolated_config();
        cfg.memory.openai_api_key = String::new();
        assert!(create_cognitive_llm(&cfg).is_none());
    }

    #[test]
    fn create_returns_some_with_api_key() {
        let mut cfg = cfg_with_isolated_config();
        cfg.memory.openai_api_key = "sk-test-123".into();
        cfg.memory.openai_base_url = "https://example.com".into();
        assert!(create_cognitive_llm(&cfg).is_some());
    }

    /// A local model now arrives as an app-provided config: a real loopback
    /// `base_url` (the daemon's app proxy) and a deliberately **empty**
    /// `api_key`, because that hop carries no credential. Resolution must not
    /// treat the empty key as "unconfigured" — doing so would leave the
    /// cognitive layer unable to use any local model at all.
    #[test]
    fn create_accepts_an_app_provided_config_with_no_api_key() {
        use crate::gateway::group_manager::{save_llm_config, set_active_cognitive_llm_config};
        let cfg = cfg_with_isolated_config();
        let id = crate::apps::llm_provider::config_id("mlx-lm", "gemma-4-e2b");
        let llm_cfg = crate::gateway::group_manager::LlmConfig {
            id: id.clone(),
            label: "MLX · gemma-4-e2b".into(),
            provider: "app:mlx-lm".into(),
            base_url: "http://127.0.0.1:18788/api/space/apps/mlx-lm/proxy/v1".into(),
            api_key: String::new(),
            model_name: "gemma-4-e2b".into(),
            adapt: "openai".into(),
            max_tokens: 4096,
            context_length: 32_000,
            vision: Some(true),
            auth: None,
            oauth_account_id: None,
            edit_format: None,
        };
        // App configs are refused by `save_llm_config` on purpose, so write it
        // the way the daemon does at runtime: through the provider registry.
        assert!(save_llm_config(&cfg.paths.global_config_path, &llm_cfg).is_err());

        let db = crate::db::Db::open_in_memory(&cfg).unwrap();
        crate::apps::llm_provider::register(
            &db,
            &crate::apps::llm_provider::AppProvider {
                app_id: "mlx-lm".into(),
                label: "MLX".into(),
                adapt: "openai".into(),
                base_url: "http://127.0.0.1:18788/api/space/apps/mlx-lm/proxy/v1".into(),
                models: vec![app_space_sdk::llm::ModelCard::new(
                    "gemma-4-e2b",
                    32_000,
                    4096,
                    true,
                )],
                edit_format: None,
            },
        )
        .unwrap();
        set_active_cognitive_llm_config(&cfg.paths.global_config_path, Some(&id)).unwrap();

        assert!(
            create_cognitive_llm(&cfg).is_some(),
            "an app-provided local model must resolve despite an empty api_key"
        );
    }


    #[test]
    fn create_picks_anthropic_adapter_when_stored_adapt_is_anthropic() {
        // Reproduces the original bug report: user picked an Anthropic
        // LLM as Cognitive Model and saw the "not configured" warning
        // because OpenAiCompatLlm couldn't talk to /v1/messages.
        // We can't make a real HTTP request, so we just verify
        // `create_cognitive_llm` doesn't return None and that the
        // resolved client is wired (the integration is exercised by
        // `endpoint_handles_bare_host` for AnthropicLlm).
        use crate::gateway::group_manager::{save_llm_config, set_active_cognitive_llm_config};
        let cfg = cfg_with_isolated_config();
        let llm_cfg = crate::gateway::group_manager::LlmConfig {
            id: "test-anthropic".into(),
            label: "Anthropic test".into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            api_key: "sk-ant-test-key".into(),
            model_name: "claude-3-5-sonnet-20241022".into(),
            adapt: "anthropic".into(),
            max_tokens: 4096,
            context_length: 200_000,
            vision: None,
            ..Default::default()
        };
        save_llm_config(&cfg.paths.global_config_path, &llm_cfg).unwrap();
        set_active_cognitive_llm_config(&cfg.paths.global_config_path, Some("test-anthropic"))
            .unwrap();

        let client = create_cognitive_llm(&cfg);
        assert!(
            client.is_some(),
            "Anthropic config in Cognitive Model slot must produce a client"
        );
    }
}
