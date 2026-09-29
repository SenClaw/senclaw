//! `/api/browser-agent/*` — what the agent's browser tools call (over
//! loopback, from the MCP process) and what the UI reads.
//!
//! Tasks run here, in the daemon, where the decision client, the LLM configs
//! and the extension hub live; the MCP tools are thin clients of this API.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::extension;
use super::ports::{self, ConfigLlm, Ports, RuntimeBrowser, RuntimeDecider};
use super::run::{self, TaskSpec};
use super::settings::{self, BrowserSettings, Driver};
use super::{llm as llm_role, policy};
use crate::runtime::manager::RuntimeManager;

pub struct AgentState {
    pub config_path: PathBuf,
    pub manager: Option<Arc<RuntimeManager>>,
    /// Tests inject stand-ins; production builds ports from the manager.
    pub ports_override: Option<Ports>,
}

type AppState = Arc<AgentState>;

/// The observation each tab last showed the LLM (look / open / do), so a
/// step names an element of *that* page. The runtime then refuses the action
/// if the page changed since — an index is never re-read against a newer page.
static SHOWN: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Value>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

const MAX_SHOWN: usize = 256;

fn remember_shown(tab: &str, obs: &Value) {
    if let Ok(mut m) = SHOWN.lock() {
        if m.len() >= MAX_SHOWN && !m.contains_key(tab) {
            if let Some(k) = m.keys().next().cloned() {
                m.remove(&k);
            }
        }
        m.insert(tab.to_string(), obs.clone());
    }
}

fn forget_shown(tab: &str) {
    if let Ok(mut m) = SHOWN.lock() {
        m.remove(tab);
    }
}

/// SenClaw's own API is never a page the browser tools may open or read.
fn refuse_own_api(url: &str) -> Result<(), (StatusCode, Json<Value>)> {
    if policy::is_own_api(url) {
        return Err(err(StatusCode::FORBIDDEN, "blocked", "SenClaw's own API is off limits to the browser"));
    }
    Ok(())
}

/// Where a step landed: a redirect or a link can reach SenClaw's own API even
/// though opening it is refused, and then its page is not handed out either.
fn refuse_landing(tab: &str, obs: &Value) -> Result<(), (StatusCode, Json<Value>)> {
    let landed = refuse_own_api(obs.get("url").and_then(Value::as_str).unwrap_or_default());
    if landed.is_err() {
        forget_shown(tab);
    }
    landed
}
type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn err(status: StatusCode, code: &str, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": message.into(), "code": code })))
}

impl AgentState {
    fn settings(&self) -> BrowserSettings {
        settings::load(&self.config_path)
    }

    fn ports(&self) -> Result<Ports, (StatusCode, Json<Value>)> {
        if let Some(p) = &self.ports_override {
            return Ok(p.clone());
        }
        let manager = self
            .manager
            .clone()
            .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "runtime_manager_unset", "the runtime manager is not wired"))?;
        Ok(Ports {
            browser: Arc::new(RuntimeBrowser { manager: manager.clone() }),
            decider: Arc::new(RuntimeDecider { manager }),
            llm: Arc::new(ConfigLlm { config_path: self.config_path.clone() }),
        })
    }

    /// The browser for this request, with the extension pipe opened when needed.
    async fn driver(&self, requested: Option<Driver>, url: Option<&str>) -> Result<Driver, (StatusCode, Json<Value>)> {
        let settings = self.settings();
        let connected = self.ports_override.is_some() || extension::hub().is_connected();
        let driver = policy::select_driver(requested, url, &settings, connected).map_err(|_| {
            err(
                StatusCode::CONFLICT,
                "extension_not_connected",
                "The SenClaw extension is not connected. Open its side panel in Chrome, connect and pair it, or run the task in SenClaw's own browser (browser: \"managed\").",
            )
        })?;
        if driver == Driver::Extension && self.ports_override.is_none() {
            let manager = self.manager.clone().ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "runtime_manager_unset", "no runtime manager"))?;
            extension::hub()
                .ensure_pipe(manager)
                .await
                .map_err(|e| err(StatusCode::SERVICE_UNAVAILABLE, "extension_pipe", e))?;
        }
        Ok(driver)
    }
}

/// Generic over the outer router's state so it merges into any of them.
pub fn router<S: Clone + Send + Sync + 'static>(state: AppState) -> Router<S> {
    Router::new()
        .route("/api/browser-agent/status", get(status))
        .route("/api/browser-agent/settings", get(settings_get).put(settings_put))
        .route("/api/browser-agent/tasks", post(task))
        .route("/api/browser-agent/tasks/:id/resume", post(task_resume))
        .route("/api/browser-agent/approvals", get(approvals_list))
        .route("/api/browser-agent/approvals/:id", post(approval))
        .route("/api/browser-agent/look", post(look))
        .route("/api/browser-agent/do", post(do_step))
        .route("/api/browser-agent/open", post(open))
        .route("/api/browser-agent/read", post(read))
        .route("/api/browser-agent/screenshot", post(screenshot))
        .route("/api/browser-agent/handover", post(handover))
        .route("/api/browser-agent/tabs", get(tabs))
        .route("/api/browser-agent/extension", get(extension_status))
        .route("/api/browser-agent/extension/pairings/:code/approve", post(extension_approve))
        .route("/api/browser-agent/extension/paired/:ext_id", delete(extension_revoke))
        .with_state(state)
}

/// What the settings screens show: the stored settings with defaults filled
/// in, plus the engine `auto` resolves to right now.
fn settings_view(s: &AgentState, settings: &BrowserSettings) -> Value {
    let home = s.config_path.parent().unwrap_or(std::path::Path::new("."));
    json!({
        "settings": settings,
        "engine": match settings::resolved_engine(&s.config_path, home) {
            settings::Engine::V2 => "v2",
            _ => "legacy",
        },
        "runtimeInstalled": settings::runtime_installed(home),
    })
}

async fn settings_get(State(s): State<AppState>) -> ApiResult {
    Ok(Json(settings_view(&s, &s.settings())))
}

/// A partial update: the fields sent replace the stored ones, the rest stay.
/// The engine choice reaches the agent's tools in chats started afterwards.
async fn settings_put(State(s): State<AppState>, Json(patch): Json<Value>) -> ApiResult {
    let Value::Object(patch) = patch else {
        return Err(err(StatusCode::BAD_REQUEST, "bad_request", "send a JSON object of browserAgent fields"));
    };
    let mut merged = serde_json::to_value(s.settings()).unwrap_or_else(|_| json!({}));
    for (k, v) in patch {
        merged[k] = v;
    }
    let next: BrowserSettings = serde_json::from_value(merged)
        .map_err(|e| err(StatusCode::UNPROCESSABLE_ENTITY, "invalid_settings", e.to_string()))?;
    let next = next.validated().map_err(|e| err(StatusCode::UNPROCESSABLE_ENTITY, "invalid_settings", e))?;
    crate::gateway::group_manager::save_browser_agent_settings(&s.config_path, &next)
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()))?;
    Ok(Json(settings_view(&s, &next)))
}

async fn status(State(s): State<AppState>) -> ApiResult {
    let runtime = match s.ports() {
        Ok(p) => p.browser.call("GET", "/v1/status", None).await.unwrap_or_else(|e| json!({ "error": e.to_string(), "code": e.code })),
        Err((_, Json(v))) => v,
    };
    Ok(Json(json!({ "settings": s.settings(), "runtime": runtime, "extension": extension::hub().status() })))
}

#[derive(Deserialize)]
struct TaskReq {
    goal: String,
    url: Option<String>,
    question: Option<String>,
    #[serde(default)]
    done_criteria: Vec<String>,
    max_steps: Option<u32>,
    browser: Option<String>,
    ext_tab: Option<i64>,
    chat_jid: Option<String>,
}

fn parse_driver(raw: Option<&str>) -> Result<Option<Driver>, (StatusCode, Json<Value>)> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("auto") => Ok(None),
        Some("managed") => Ok(Some(Driver::Managed)),
        Some("extension") => Ok(Some(Driver::Extension)),
        Some(other) => Err(err(StatusCode::BAD_REQUEST, "bad_browser", format!("browser must be auto, managed or extension, not {other}"))),
    }
}

fn owner(chat_jid: Option<&str>) -> String {
    chat_jid.map(str::trim).filter(|j| !j.is_empty()).unwrap_or("default").to_string()
}

async fn task(State(s): State<AppState>, Json(req): Json<TaskReq>) -> ApiResult {
    if req.goal.trim().is_empty() || req.goal.len() > 4000 {
        return Err(err(StatusCode::BAD_REQUEST, "bad_goal", "goal must be 1-4000 characters"));
    }
    if let Some(url) = req.url.as_deref() {
        refuse_own_api(url)?;
    }
    let requested = parse_driver(req.browser.as_deref())?;
    let driver = s.driver(requested, req.url.as_deref()).await?;
    let ports = s.ports()?;
    let spec = TaskSpec {
        goal: req.goal,
        url: req.url,
        question: req.question,
        done_criteria: req.done_criteria,
        max_steps: req.max_steps,
        driver,
        owner: owner(req.chat_jid.as_deref()),
        ext_tab: req.ext_tab,
    };
    let out = run::start(&ports, s.settings(), spec).await;
    Ok(Json(serde_json::to_value(out).unwrap_or_default()))
}

#[derive(Deserialize, Default)]
struct ResumeReq {
    chat_jid: Option<String>,
}

async fn task_resume(State(s): State<AppState>, Path(id): Path<String>, body: Option<Json<ResumeReq>>) -> ApiResult {
    // An agent resumes only its own chat's tasks.
    if let Some(chat) = body.as_ref().and_then(|b| b.chat_jid.as_deref()) {
        if run::owner_of_task(&id).as_deref() != Some(owner(Some(chat)).as_str()) {
            return Err(err(StatusCode::NOT_FOUND, "no_task", format!("no paused task {id}")));
        }
    }
    // A task paused in the person's Chrome needs the extension pipe again.
    if run::driver_of_task(&id) == Some(Driver::Extension) {
        s.driver(Some(Driver::Extension), None).await?;
    }
    let ports = s.ports()?;
    match run::resume(&ports, &id).await {
        Some(out) => Ok(Json(serde_json::to_value(out).unwrap_or_default())),
        None => Err(err(StatusCode::NOT_FOUND, "no_task", format!("no paused task {id}"))),
    }
}

#[derive(Deserialize)]
struct ApprovalReq {
    approve: bool,
    /// Sent by the agent's tool: a chat answers only its own tasks. The
    /// settings screens send none and answer for the person.
    chat_jid: Option<String>,
}

/// Actions waiting for the person — for the settings screens, which approve
/// through the route below without going through an agent.
async fn approvals_list() -> Json<Value> {
    Json(json!({ "approvals": run::pending_approvals() }))
}

async fn approval(State(s): State<AppState>, Path(id): Path<String>, Json(req): Json<ApprovalReq>) -> ApiResult {
    if let Some(chat) = req.chat_jid.as_deref() {
        if run::owner_of_approval(&id).as_deref() != Some(owner(Some(chat)).as_str()) {
            return Err(err(StatusCode::NOT_FOUND, "no_approval", format!("no pending approval {id}")));
        }
    }
    if run::driver_of_approval(&id) == Some(Driver::Extension) {
        // Acting needs the person's Chrome; a decline goes through without it.
        let pipe = s.driver(Some(Driver::Extension), None).await;
        if req.approve {
            pipe?;
        }
    }
    let ports = s.ports()?;
    match run::approve(&ports, &id, req.approve).await {
        Some(out) => Ok(Json(serde_json::to_value(out).unwrap_or_default())),
        None => Err(err(StatusCode::NOT_FOUND, "no_approval", format!("no pending approval {id}"))),
    }
}

#[derive(Deserialize, Default)]
struct TabReq {
    chat_jid: Option<String>,
    browser: Option<String>,
    tab_id: Option<String>,
}

/// The chat's current tab for this driver — only ever a tab this chat opened.
/// A remembered id that no longer names one (closed, or the runtime restarted
/// and its ids started over) is forgotten, never followed into another chat's tab.
async fn current_tab(s: &AgentState, req: &TabReq) -> Result<(String, Driver), (StatusCode, Json<Value>)> {
    let driver = s.driver(parse_driver(req.browser.as_deref())?, None).await?;
    let who = owner(req.chat_jid.as_deref());
    let no_tab = || err(StatusCode::NOT_FOUND, "no_tab", "this chat has no open browser tab; open a page first (browser_open or browser_task)");
    let tab = match req.tab_id.clone().filter(|t| !t.is_empty()) {
        Some(t) => t,
        None => run::chat_tab(&who, driver).ok_or_else(no_tab)?,
    };
    let ports = s.ports()?;
    let sessions = ports.browser.call("GET", "/v1/sessions", None).await.map_err(port_err)?;
    let owned = sessions["sessions"].as_array().into_iter().flatten().flat_map(|s| s["tabs"].as_array().into_iter().flatten()).find(|t| {
        t["id"].as_str() == Some(tab.as_str()) && t["owner"].as_str() == Some(who.as_str()) && t["closed"] != Value::Bool(true)
    });
    let Some(owned) = owned else {
        if run::chat_tab(&who, driver).as_deref() == Some(tab.as_str()) {
            run::forget_chat_tab(&who, driver);
        }
        forget_shown(&tab);
        return Err(no_tab());
    };
    // A link can lead there even though opening it is refused.
    refuse_own_api(owned["url"].as_str().unwrap_or_default())?;
    Ok((tab, driver))
}

fn port_err(e: ports::PortError) -> (StatusCode, Json<Value>) {
    (StatusCode::from_u16(e.status).unwrap_or(StatusCode::BAD_GATEWAY), Json(json!({ "error": e.message, "code": e.code })))
}

/// The table the decision model sees, for the LLM to act on by hand.
fn table(obs: &Value) -> Value {
    let encoded = super::encoder::encode(obs, "", &[], super::encoder::Profile::JevFull, None, None);
    json!({
        "observation_id": obs.get("observation_id"),
        "url": obs.get("url"), "title": obs.get("title"),
        "text": obs.get("text").and_then(Value::as_str).map(|t| t.chars().take(3000).collect::<String>()),
        "elements": serde_json::to_value(&encoded.space.elements).unwrap_or(Value::Null),
        "operations": encoded.operations.iter().filter(|o| *o != "DONE" && *o != "BLOCKED").collect::<Vec<_>>(),
        "dialog": obs.get("dialog"),
    })
}

async fn look(State(s): State<AppState>, Json(req): Json<TabReq>) -> ApiResult {
    let (tab, _) = current_tab(&s, &req).await?;
    let ports = s.ports()?;
    let obs = ports::observe(ports.browser.as_ref(), &tab).await.map_err(port_err)?;
    refuse_own_api(obs["url"].as_str().unwrap_or_default())?;
    remember_shown(&tab, &obs);
    Ok(Json(table(&obs)))
}

#[derive(Deserialize)]
struct DoReq {
    #[serde(flatten)]
    tab: TabReq,
    observation_id: u64,
    operation: String,
    target: Option<String>,
    text: Option<String>,
}

/// One step chosen by the LLM itself (after `look`), under the same policy.
async fn do_step(State(s): State<AppState>, Json(req): Json<DoReq>) -> ApiResult {
    let (tab, driver) = current_tab(&s, &req.tab).await?;
    let ports = s.ports()?;
    let shown = SHOWN.lock().ok().and_then(|m| m.get(&tab).cloned()).unwrap_or(Value::Null);
    if shown.get("observation_id").and_then(Value::as_u64) != Some(req.observation_id) {
        return Err(err(StatusCode::CONFLICT, "stale_page", "that observation is not the latest one shown for this tab; call browser_look again"));
    }
    let encoded = super::encoder::encode(&shown, "", &[], super::encoder::Profile::JevFull, None, None);
    let operation = req.operation.trim().to_uppercase();
    let action = match req.target.as_deref() {
        Some(t) => encoded.space.target(&operation, t.trim_start_matches('[').trim_end_matches(']')).map(|x| x.action.clone()),
        None => encoded.space.control(&operation).cloned(),
    }
    .ok_or_else(|| err(StatusCode::BAD_REQUEST, "not_offered", format!("{operation} {:?} is not offered on that page", req.target)))?;
    let dialog_type = shown.pointer("/dialog/type").and_then(Value::as_str);
    let (mut tier, mut reason) = policy::risk_tier(&operation, &action, dialog_type);
    if tier < policy::Tier::Approve {
        if let Some(raised) = run::manual_step_raises_risk(&ports, &s.settings(), driver, &shown, &operation, &action).await {
            (tier, reason) = (policy::Tier::Approve, raised);
        }
    }
    if tier >= policy::Tier::Approve {
        return Err(err(
            StatusCode::FORBIDDEN,
            if tier == policy::Tier::Human { "needs_user" } else { "needs_approval" },
            format!("{reason}. Use browser_task so the person can approve it, or hand the tab over."),
        ));
    }
    if operation == "TYPE_TEXT" && req.text.as_deref().map(str::is_empty).unwrap_or(true) {
        return Err(err(StatusCode::BAD_REQUEST, "text_required", "TYPE_TEXT needs text"));
    }
    let id = action.get("id").and_then(Value::as_str).unwrap_or_default();
    let out = ports::act(ports.browser.as_ref(), &tab, req.observation_id, id, req.text.as_deref()).await.map_err(port_err)?;
    let next = out.get("observation").cloned().unwrap_or(Value::Null);
    if !next.is_null() {
        refuse_landing(&tab, &next)?;
        remember_shown(&tab, &next);
    }
    Ok(Json(json!({ "executed": operation, "label": action.get("label"), "page": if next.is_null() { Value::Null } else { table(&next) } })))
}

#[derive(Deserialize)]
struct OpenReq {
    url: String,
    #[serde(flatten)]
    tab: TabReq,
}

async fn open(State(s): State<AppState>, Json(req): Json<OpenReq>) -> ApiResult {
    refuse_own_api(&req.url)?;
    let driver = s.driver(parse_driver(req.tab.browser.as_deref())?, Some(&req.url)).await?;
    let ports = s.ports()?;
    let settings = s.settings();
    let session = ports::open_session(ports.browser.as_ref(), driver.as_str(), &settings.profile, settings.headless).await.map_err(port_err)?;
    let who = owner(req.tab.chat_jid.as_deref());
    let (tab, obs) = ports::open_tab(ports.browser.as_ref(), &session, Some(&req.url), &who, None).await.map_err(port_err)?;
    run::remember_chat_tab(&who, driver, &tab);
    refuse_landing(&tab, &obs)?;
    remember_shown(&tab, &obs);
    Ok(Json(json!({ "tab_id": tab, "driver": driver, "page": table(&obs) })))
}

#[derive(Deserialize)]
struct ReadReq {
    #[serde(flatten)]
    tab: TabReq,
    question: Option<String>,
    max_chars: Option<u64>,
}

async fn read(State(s): State<AppState>, Json(req): Json<ReadReq>) -> ApiResult {
    let (tab, _) = current_tab(&s, &req.tab).await?;
    let ports = s.ports()?;
    let page = ports::read(ports.browser.as_ref(), &tab, req.max_chars.unwrap_or(20_000)).await.map_err(port_err)?;
    refuse_own_api(page["url"].as_str().unwrap_or_default())?;
    match req.question.filter(|q| !q.trim().is_empty()) {
        Some(q) => {
            let settings = s.settings();
            let answer = llm_role::answer(ports.llm.as_ref(), settings.fallback_model.as_deref(), &q, &page)
                .await
                .map_err(|e| err(StatusCode::BAD_GATEWAY, "llm_error", e))?;
            Ok(Json(json!({ "url": page.get("url"), "title": page.get("title"), "answer": answer })))
        }
        None => Ok(Json(page)),
    }
}

async fn screenshot(State(s): State<AppState>, Json(req): Json<TabReq>) -> ApiResult {
    let (tab, _) = current_tab(&s, &req).await?;
    let ports = s.ports()?;
    let shot = ports.browser.call("POST", &format!("/v1/tabs/{tab}/screenshot"), Some(json!({}))).await.map_err(port_err)?;
    Ok(Json(shot))
}

#[derive(Deserialize)]
struct HandoverReq {
    #[serde(flatten)]
    tab: TabReq,
    /// `start` (default) or `done`.
    action: Option<String>,
}

async fn handover(State(s): State<AppState>, Json(req): Json<HandoverReq>) -> ApiResult {
    let (tab, driver) = current_tab(&s, &req.tab).await?;
    let ports = s.ports()?;
    let done = req.action.as_deref() == Some("done");
    let method = if done { "DELETE" } else { "POST" };
    let out = ports.browser.call(method, &format!("/v1/tabs/{tab}/handover"), None).await.map_err(port_err)?;
    let message = match (done, driver) {
        (true, _) => "The agent has the tab back.",
        (false, Driver::Managed) => "SenClaw's browser window is open for the person; the agent cannot read or act until they finish.",
        (false, Driver::Extension) => "The tab is in front of the person in their Chrome; SenClaw detached from it until they finish.",
    };
    Ok(Json(json!({ "tab": out, "message": message })))
}

#[derive(Deserialize)]
struct TabsQuery {
    chat_jid: Option<String>,
}

/// Open tabs. A chat (the agent's `browser_tabs`) sees only its own; the
/// settings screens, asking for no chat, see all. Pairing codes are never here.
async fn tabs(State(s): State<AppState>, Query(q): Query<TabsQuery>) -> ApiResult {
    let ports = s.ports()?;
    let sessions = ports.browser.call("GET", "/v1/sessions", None).await.map_err(port_err)?;
    let mut sessions = sessions.get("sessions").cloned().unwrap_or(json!([]));
    let chat = q.chat_jid.as_deref().map(str::trim).filter(|j| !j.is_empty());
    if let (Some(who), Some(list)) = (chat, sessions.as_array_mut()) {
        for session in list.iter_mut() {
            if let Some(tabs) = session.get_mut("tabs").and_then(Value::as_array_mut) {
                tabs.retain(|t| t["owner"].as_str() == Some(who));
            }
        }
        list.retain(|s| s["tabs"].as_array().is_some_and(|t| !t.is_empty()));
    }
    let who = owner(chat);
    Ok(Json(json!({
        "sessions": sessions,
        "current": { "managed": run::chat_tab(&who, Driver::Managed), "extension": run::chat_tab(&who, Driver::Extension) },
        "extension": { "connected": extension::hub().is_connected(), "shared_tabs": extension::hub().shared_tabs() },
    })))
}

async fn extension_status() -> Json<Value> {
    Json(extension::hub().status())
}

async fn extension_approve(Path(code): Path<String>) -> ApiResult {
    extension::hub()
        .approve_code(&code)
        .map(|ext| Json(json!({ "paired": ext })))
        .map_err(|e| err(StatusCode::NOT_FOUND, "bad_code", e))
}

async fn extension_revoke(Path(ext_id): Path<String>) -> ApiResult {
    extension::hub()
        .revoke(&ext_id)
        .map(|removed| Json(json!({ "revoked": removed })))
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, "store", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser_agent::ports::{BrowserPort, PortError};
    use crate::browser_agent::run::tests::{ports as fake_ports, FakeBrowser, FakeDecider, NoLlm};
    use tower::ServiceExt;

    /// Every page it opens redirects to SenClaw's own API.
    struct RedirectsHome;

    #[async_trait::async_trait]
    impl BrowserPort for RedirectsHome {
        async fn call(&self, _method: &str, path: &str, _body: Option<Value>) -> Result<Value, PortError> {
            if path == "/v1/sessions" {
                return Ok(json!({ "id": "s1" }));
            }
            if path.ends_with("/tabs") {
                let page = json!({ "observation_id": 1, "tab_id": "t9", "url": "http://127.0.0.1:18788/api/llm-config",
                                   "title": "", "text": "{\"apiKey\":\"sk-not-for-the-agent\"}", "actions": [] });
                return Ok(json!({ "tab": { "id": "t9" }, "observation": page }));
            }
            Ok(json!({}))
        }
    }

    async fn post_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
        let r = app
            .clone()
            .oneshot(
                axum::http::Request::post(path)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// Opening is refused for SenClaw's own address, and a page that only
    /// gets there by redirect is not handed out either.
    #[tokio::test]
    async fn a_redirect_to_senclaw_itself_is_not_handed_out() {
        crate::browser_agent::policy::set_own_ports(&[18788, 18789]);
        let dir = tempfile::tempdir().unwrap();
        let ports = crate::browser_agent::ports::Ports {
            browser: Arc::new(RedirectsHome),
            decider: Arc::new(FakeDecider),
            llm: Arc::new(NoLlm),
        };
        let state = Arc::new(AgentState { config_path: dir.path().join("config.json"), manager: None, ports_override: Some(ports) });
        let app: axum::Router = router(state);
        let (status, body) = post_json(&app, "/api/browser-agent/open", json!({ "url": "https://redirect.test/", "chat_jid": "c1" })).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(!body.to_string().contains("sk-not-for-the-agent"));
    }

    /// An agent answers only its own chat's approvals; the settings screens
    /// (no chat) answer for the person.
    #[tokio::test]
    async fn a_chat_cannot_answer_another_chats_approval() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AgentState {
            config_path: dir.path().join("config.json"),
            manager: None,
            ports_override: Some(fake_ports(FakeBrowser::new(true))),
        });
        let app: axum::Router = router(state);
        let ports = crate::browser_agent::ports::Ports {
            browser: FakeBrowser::new(true),
            decider: Arc::new(crate::browser_agent::run::tests::SeesRisk),
            llm: Arc::new(NoLlm),
        };
        let mut spec = crate::browser_agent::run::tests::spec("Search for books");
        spec.owner = "owner-chat".into();
        let out = run::start(&ports, settings::BrowserSettings::default(), spec).await;
        let id = out.pending.as_ref().and_then(|p| p["approval_id"].as_str()).unwrap().to_string();
        let path = format!("/api/browser-agent/approvals/{id}");
        let (status, _) = post_json(&app, &path, json!({ "approve": true, "chat_jid": "other-chat" })).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(run::pending_approvals().iter().any(|a| a["approval_id"] == id.as_str()), "still waiting for the person");
        let (status, body) = post_json(&app, &path, json!({ "approve": false })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!run::pending_approvals().iter().any(|a| a["approval_id"] == id.as_str()));
    }

    #[tokio::test]
    async fn settings_endpoint_merges_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AgentState { config_path: dir.path().join("config.json"), manager: None, ports_override: None });
        // Settings: a partial update keeps the other fields, and nonsense is refused.
        let put = |body: Value| {
            let app: axum::Router = router(state.clone());
            async move {
                app.oneshot(
                    axum::http::Request::builder()
                        .method("PUT")
                        .uri("/api/browser-agent/settings")
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap()
            }
        };
        let r = put(json!({ "defaultDriver": "extension", "hostedDomains": ["example.com"] })).await;
        assert_eq!(r.status(), 200);
        let saved = settings::load(&state.config_path);
        assert_eq!(saved.default_driver, Driver::Extension);
        assert_eq!(saved.local_model, "laya-browser", "untouched fields keep their value");
        assert_eq!(put(json!({ "maxSteps": 0 })).await.status(), 422);
        assert_eq!(put(json!({ "defaultDriver": "carrier-pigeon" })).await.status(), 422);
        assert_eq!(settings::load(&state.config_path).default_driver, Driver::Extension, "a refused update changes nothing");
    }

    #[tokio::test]
    async fn task_endpoint_runs_the_loop() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AgentState {
            config_path: dir.path().join("config.json"),
            manager: None,
            ports_override: Some(fake_ports(FakeBrowser::new(true))),
        });
        let app: axum::Router = router(state.clone());

        let body = json!({ "goal": "Search for books", "url": "https://shop.test/", "done_criteria": ["Search results are shown"], "chat_jid": "rest-chat" });
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::post("/api/browser-agent/tasks")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["status"], "done", "{out}");
        assert_eq!(out["driver"], "managed");
        assert_eq!(out["stats"]["steps"], 1);

        // The chat now has a tab the step-level tools can address.
        let look = app
            .clone()
            .oneshot(
                axum::http::Request::post("/api/browser-agent/look")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(json!({ "chat_jid": "rest-chat" }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(look.status(), StatusCode::OK);

        // Another chat can neither steer that tab nor see it.
        let other_look = app
            .clone()
            .oneshot(
                axum::http::Request::post("/api/browser-agent/look")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(json!({ "chat_jid": "other-chat", "tab_id": "t1" }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(other_look.status(), StatusCode::NOT_FOUND);
        let tabs_of = |chat: &'static str| {
            let app = app.clone();
            async move {
                let r = app
                    .oneshot(axum::http::Request::get(format!("/api/browser-agent/tabs?chat_jid={chat}")).body(axum::body::Body::empty()).unwrap())
                    .await
                    .unwrap();
                let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
                serde_json::from_slice::<Value>(&bytes).unwrap()
            }
        };
        assert_eq!(tabs_of("rest-chat").await["sessions"][0]["tabs"][0]["id"], "t1");
        let other = tabs_of("other-chat").await;
        assert_eq!(other["sessions"], json!([]), "{other}");
        assert!(other["extension"].get("pending").is_none(), "pairing codes stay on the settings screens");

        // SenClaw's own API is not a page the browser tools may open.
        crate::browser_agent::policy::set_own_ports(&[18788, 18789]);
        let own = app
            .clone()
            .oneshot(
                axum::http::Request::post("/api/browser-agent/open")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(json!({ "chat_jid": "rest-chat", "url": "http://127.0.0.1:18788/api/llm-config" }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(own.status(), StatusCode::FORBIDDEN);

        // A bad browser value is a clear 400.
        let bad = app
            .oneshot(
                axum::http::Request::post("/api/browser-agent/tasks")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(json!({ "goal": "x", "browser": "firefox" }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    }
}
