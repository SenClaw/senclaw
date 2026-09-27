//! Anthropic adapter for the cognify LLM client.
//!
//! The OpenAI-compat adapter ([`super::llm_openai::OpenAiCompatLlm`]) sends
//! `POST /v1/chat/completions` with a `messages` array. Anthropic uses
//! `POST /v1/messages` with a top-level `system` string + a separate
//! `messages` array. Trying to talk to one with the other's shape returns
//! 4xx, which the cognify pipeline soft-fails as `llm_skipped = true`.
//!
//! This file is small on purpose — we only need single-turn completion
//! (no streaming, no tool-use), so the Anthropic JSON we exchange stays
//! flat:
//!
//! ```text
//!   request  = { model, max_tokens, system, messages:[{role:"user", content}] }
//!   response = { content:[{type:"text", text:"..."}, ...] }
//! ```

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::llm::LlmClient;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_TOKENS: u32 = 4096;
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicLlm {
    client: Client,
    base_url: String,
    api_key: String,
    model: String,
    max_tokens: u32,
}

impl AnthropicLlm {
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
            max_tokens: DEFAULT_MAX_TOKENS,
        })
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    fn endpoint(&self) -> String {
        if self.base_url.contains("/v1") {
            format!("{}/messages", self.base_url)
        } else {
            format!("{}/v1/messages", self.base_url)
        }
    }
}

#[derive(Debug, Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<UserTurn<'a>>,
}

#[derive(Debug, Serialize)]
struct UserTurn<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

/// Concatenate every `text` block from the response. Anthropic can return
/// multiple blocks when it interleaves "thinking" + final text; for
/// cognify we want everything readable.
fn collect_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter(|b| b.kind == "text")
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("")
}

#[async_trait]
impl LlmClient for AnthropicLlm {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let started = std::time::Instant::now();
        let url = self.endpoint();
        let body = MessagesRequest {
            model: &self.model,
            max_tokens: self.max_tokens,
            system,
            messages: vec![UserTurn {
                role: "user",
                content: user,
            }],
        };

        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("send anthropic messages request")?;

        let status = resp.status();
        let text = resp.text().await.context("read anthropic body")?;
        if !status.is_success() {
            anyhow::bail!("Anthropic API {status}: {text}");
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            super::llm::record_cognitive_usage(
                "anthropic",
                &self.model,
                &v,
                started.elapsed().as_millis() as u64,
            );
        }
        let parsed: MessagesResponse =
            serde_json::from_str(&text).with_context(|| format!("parse anthropic JSON: {text}"))?;
        Ok(collect_text(&parsed.content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_handles_bare_host() {
        let llm = AnthropicLlm::new("https://api.anthropic.com", "k", "claude-3-5-sonnet").unwrap();
        assert_eq!(llm.endpoint(), "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn endpoint_preserves_existing_v1() {
        let llm = AnthropicLlm::new("https://proxy.example.com/v1", "k", "m").unwrap();
        assert_eq!(llm.endpoint(), "https://proxy.example.com/v1/messages");
    }

    #[test]
    fn collect_text_joins_all_text_blocks() {
        let blocks = vec![
            ContentBlock {
                kind: "text".into(),
                text: "part one ".into(),
            },
            ContentBlock {
                kind: "thinking".into(),
                text: "ignored".into(),
            },
            ContentBlock {
                kind: "text".into(),
                text: "part two".into(),
            },
        ];
        assert_eq!(collect_text(&blocks), "part one part two");
    }
}
