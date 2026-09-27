//! Talking to the SenClaw daemon.
//!
//! Three things every Space App needs and none of them belong in the app:
//!
//! - **LLM access.** The app holds no provider key. `llm()` goes through the
//!   daemon's bridge, which uses whichever provider the user configured.
//! - **Settings.** The config KV is shared with the app's own settings UI.
//! - **Identity.** `SENCLAW_TOKEN_ACCESS_APP` is this app's access token,
//!   injected into its process by the daemon. It is sent on every call: under
//!   the default strict mode a tokenless call to another app's data routes is
//!   refused, and a token presented against another app's id is refused in
//!   every mode.

use anyhow::{bail, Result};
use serde_json::{json, Value};

/// Header names — the daemon's contract, not ours to rename.
const HEADER_APP_TOKEN: &str = "x-senclaw-app-token";
const HEADER_API_VERSION: &str = "x-senclaw-api-version";

pub struct Space {
    app_id: String,
    base_url: String,
    token: String,
    api_version: String,
    http: reqwest::Client,
}

impl Space {
    /// Read the environment the daemon sets. The explicit `app_id` fallback
    /// keeps a bare `cargo run` working during development.
    pub fn from_env(app_id: &str) -> Space {
        Space {
            app_id: std::env::var("SENCLAW_SPACE_APP_ID").unwrap_or_else(|_| app_id.to_string()),
            base_url: std::env::var("SENCLAW_BASE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:18788".to_string())
                .trim_end_matches('/')
                .to_string(),
            token: std::env::var("SENCLAW_TOKEN_ACCESS_APP").unwrap_or_default(),
            api_version: std::env::var("SENCLAW_API_VERSION").unwrap_or_else(|_| "{{api_version}}".to_string()),
            http: reqwest::Client::new(),
        }
    }

    fn url(&self, suffix: &str) -> String {
        format!("{}/api/space/apps/{}{}", self.base_url, self.app_id, suffix)
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        // An empty token is omitted rather than sent blank: the daemon would
        // try to resolve "" and refuse a call its default mode would have
        // served.
        let rb = rb.header(HEADER_API_VERSION, &self.api_version);
        if self.token.is_empty() {
            rb
        } else {
            rb.header(HEADER_APP_TOKEN, &self.token)
        }
    }

    /// Ask the daemon's model. Returns the reply text.
    pub async fn llm(&self, prompt: &str, max_tokens: u32) -> Result<String> {
        let body = json!({
            // The wire field is `action`, not `capability`. The daemon's
            // request struct requires it, and a body without it is rejected by
            // the JSON extractor with a 422 before any handler runs.
            "action": "llm.request",
            // Only these fields are honoured — temperature and friends are not
            // part of the bridge contract and are silently dropped.
            "payload": { "prompt": prompt, "maxTokens": max_tokens }
        });
        let resp = self
            .auth(self.http.post(self.url("/bridge")))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let v: Value = resp.json().await.unwrap_or(json!({}));
        if !status.is_success() {
            bail!(
                "bridge HTTP {status}: {}",
                v.get("error").and_then(|e| e.as_str()).unwrap_or("")
            );
        }
        // A failed completion comes back as HTTP **200** with status "error".
        // Checking only the HTTP status turns a provider outage into a
        // successful empty summary, which the agent has no way to notice.
        if v.get("status").and_then(|s| s.as_str()) == Some("error") {
            bail!(
                "{}",
                v.get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("model trả về lỗi không rõ")
            );
        }
        if v.get("finish").and_then(|f| f.as_str()) == Some("length") {
            bail!("câu trả lời bị cắt ở maxTokens — chia nhỏ công việc ra");
        }
        Ok(v.get("text")
            .or_else(|| v.get("content"))
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string())
    }

    /// One stored setting, or `None` when it has never been set.
    pub async fn get_config(&self, key: &str) -> Result<Option<Value>> {
        let resp = self.auth(self.http.get(self.url(&format!("/config/{key}")))).send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let v: Value = resp.json().await.unwrap_or(json!({}));
        Ok(v.get("value").cloned())
    }

    pub async fn set_config(&self, key: &str, value: Value) -> Result<()> {
        self.auth(self.http.put(self.url(&format!("/config/{key}"))))
            .json(&json!({ "value": value }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}
