//! What the loop talks to — the browser runtime, the decision runtime and an
//! LLM — as traits, so the loop can be tested without any of them running.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Method;
use serde::Serialize;
use serde_json::{json, Value};

use crate::decision::types::AskRequest;
use crate::runtime::manager::{RuntimeClientError, RuntimeManager};

/// An error from the browser runtime, in its own `{error, code}` shape.
#[derive(Debug, Clone, Serialize)]
pub struct PortError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl PortError {
    pub fn new(status: u16, code: &str, message: impl Into<String>) -> PortError {
        PortError { status, code: code.to_string(), message: message.into() }
    }
}

impl std::fmt::Display for PortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

/// The `sen-browser` HTTP API (`/v1/...`).
#[async_trait]
pub trait BrowserPort: Send + Sync {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, PortError>;
}

/// The decision runtime: one request, the `answers` object back.
#[async_trait]
pub trait Decider: Send + Sync {
    async fn ask(&self, request: &AskRequest) -> Result<Value, String>;
}

/// One-shot completions (no tools). `model` is an LLM config id; `None` is the active one.
#[async_trait]
pub trait Llm: Send + Sync {
    async fn complete(&self, model: Option<&str>, system: &str, user: &str, max_tokens: u32) -> Result<String, String>;
}

#[derive(Clone)]
pub struct Ports {
    pub browser: Arc<dyn BrowserPort>,
    pub decider: Arc<dyn Decider>,
    pub llm: Arc<dyn Llm>,
}

// ----- production implementations -----

pub struct RuntimeBrowser {
    pub manager: Arc<RuntimeManager>,
}

fn client_error(e: RuntimeClientError) -> PortError {
    let text = e.to_string();
    let code = if text.contains("not installed") {
        "runtime_not_installed"
    } else if text.contains("not selected") {
        "runtime_not_selected"
    } else {
        "runtime_unavailable"
    };
    PortError::new(503, code, format!("the browser runtime is unavailable: {text}"))
}

#[async_trait]
impl BrowserPort for RuntimeBrowser {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, PortError> {
        let method = Method::from_bytes(method.as_bytes()).map_err(|_| PortError::new(500, "internal", "bad method"))?;
        let body = body.map(|b| b.to_string().into_bytes());
        let (status, bytes) = crate::runtime::clients::browser_call(&self.manager, method, path, body)
            .await
            .map_err(client_error)?;
        let value: Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({ "error": String::from_utf8_lossy(&bytes) }));
        if status.is_success() {
            Ok(value)
        } else {
            Err(PortError::new(
                status.as_u16(),
                value.get("code").and_then(Value::as_str).unwrap_or("runtime_error"),
                value.get("error").and_then(Value::as_str).unwrap_or("the browser runtime failed"),
            ))
        }
    }
}

pub struct RuntimeDecider {
    pub manager: Arc<RuntimeManager>,
}

#[async_trait]
impl Decider for RuntimeDecider {
    async fn ask(&self, request: &AskRequest) -> Result<Value, String> {
        let response = crate::decision::client::ask(&self.manager, request).await.map_err(|e| e.to_string())?;
        serde_json::to_value(&response.answers).map_err(|e| e.to_string())
    }
}

pub struct ConfigLlm {
    pub config_path: PathBuf,
}

/// How long one step waits for the LLM. Not a provider timeout (a local
/// provider has none, on purpose): a step whose answer is a short JSON object
/// is better parked than stalled while a local model runs on for minutes.
const LLM_STEP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

#[async_trait]
impl Llm for ConfigLlm {
    async fn complete(&self, model: Option<&str>, system: &str, user: &str, max_tokens: u32) -> Result<String, String> {
        let call = crate::gateway::ui_server::llm_config::chat_completion(&self.config_path, model, system, user, max_tokens, None);
        match tokio::time::timeout(LLM_STEP_TIMEOUT, call).await {
            Ok(answer) => answer.map(|r| r.text),
            Err(_) => Err(format!("the LLM did not answer within {} s", LLM_STEP_TIMEOUT.as_secs())),
        }
    }
}

// ----- typed helpers over the browser port -----

pub async fn open_session(b: &dyn BrowserPort, driver: &str, profile: &str, headless: bool) -> Result<String, PortError> {
    let s = b.call("POST", "/v1/sessions", Some(json!({ "driver": driver, "profile": profile, "headless": headless }))).await?;
    s.get("id").and_then(Value::as_str).map(str::to_string).ok_or_else(|| PortError::new(500, "internal", "no session id"))
}

/// The owner's tab (created or reused) and its first observation.
pub async fn open_tab(
    b: &dyn BrowserPort,
    session: &str,
    url: Option<&str>,
    owner: &str,
    ext_tab: Option<i64>,
) -> Result<(String, Value), PortError> {
    let mut body = json!({ "owner": owner });
    if let Some(u) = url {
        body["url"] = json!(u);
    }
    if let Some(t) = ext_tab {
        body["ext_tab"] = json!(t);
    }
    let r = b.call("POST", &format!("/v1/sessions/{session}/tabs"), Some(body)).await?;
    let tab = r.pointer("/tab/id").and_then(Value::as_str).ok_or_else(|| PortError::new(500, "internal", "no tab id"))?;
    Ok((tab.to_string(), r.get("observation").cloned().unwrap_or(Value::Null)))
}

pub async fn observe(b: &dyn BrowserPort, tab: &str) -> Result<Value, PortError> {
    b.call("POST", &format!("/v1/tabs/{tab}/observe"), Some(json!({}))).await
}

pub async fn act(b: &dyn BrowserPort, tab: &str, observation_id: u64, action_id: &str, text: Option<&str>) -> Result<Value, PortError> {
    let mut body = json!({ "observation_id": observation_id, "action_id": action_id });
    if let Some(t) = text {
        body["text"] = json!(t);
    }
    b.call("POST", &format!("/v1/tabs/{tab}/act"), Some(body)).await
}

pub async fn navigate(b: &dyn BrowserPort, tab: &str, url: &str) -> Result<Value, PortError> {
    b.call("POST", &format!("/v1/tabs/{tab}/navigate"), Some(json!({ "url": url }))).await
}

pub async fn read(b: &dyn BrowserPort, tab: &str, max_chars: u64) -> Result<Value, PortError> {
    b.call("POST", &format!("/v1/tabs/{tab}/read"), Some(json!({ "max_chars": max_chars }))).await
}
