//! The browser task loop.
//!
//! Each step: rule (dialogs) → one decision request for operation + targets →
//! the LLM tier when the decision model is unsure → policy (approve / hand
//! over) → the text helper for TYPE_TEXT → one guarded action → log. A DONE is
//! accepted only when an independent check of the page agrees. Paused tasks
//! (approval, the person's turn) are kept so they can continue.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use serde::Serialize;
use serde_json::{json, Value};

use super::budget::{self, DEFAULT_OPTION_CHARS};
use super::decide::{resolve, Band, Step};
use super::encoder::{encode, to_json, HistoryItem, Profile};
use super::llm::{self as llm_role, TextValue};
use super::policy::{self, DecisionRoute, Tier};
use super::ports::{self, PortError, Ports};
use super::prompts::VERIFY;
use super::settings::{BrowserSettings, Driver};
use crate::decision::json::Json;
use crate::decision::types::{AskRequest, Backend};

#[derive(Debug, Clone)]
pub struct TaskSpec {
    pub goal: String,
    pub url: Option<String>,
    pub question: Option<String>,
    pub done_criteria: Vec<String>,
    pub max_steps: Option<u32>,
    pub driver: Driver,
    /// The chat that owns the tab.
    pub owner: String,
    /// Adopt a tab the person shared (extension).
    pub ext_tab: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepLog {
    pub step: u32,
    pub operation: String,
    pub target: Option<String>,
    pub label: String,
    /// `jev`, `llm` or `rule`.
    pub by: &'static str,
    pub confidence: Option<f64>,
    pub band: Option<Band>,
    pub decision_ms: u64,
    pub text: Option<String>,
    pub outcome: String,
    pub page_changed: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub steps: u32,
    pub decisions: u32,
    pub llm_calls: u32,
    pub fallbacks: u32,
    pub stale: u32,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskOutcome {
    pub task_id: String,
    /// `done`, `unverified`, `blocked`, `needs_approval`, `needs_user`,
    /// `needs_input`, `budget`, `cancelled`, `error`.
    pub status: String,
    pub message: String,
    pub driver: Driver,
    pub tab_id: Option<String>,
    pub url: Option<String>,
    pub title: Option<String>,
    pub answer: Option<String>,
    pub evidence: Vec<Value>,
    pub pending: Option<Value>,
    pub steps: Vec<StepLog>,
    pub stats: Stats,
}

#[derive(Clone)]
struct Pending {
    approval_id: String,
    observation_id: u64,
    action_id: String,
    operation: String,
    label: String,
    kind: String,
    text: Option<String>,
    /// For a dialog: what "reject" means (dismiss instead of cancel).
    reject_action: Option<String>,
}

#[derive(Clone)]
struct TaskState {
    id: String,
    spec: TaskSpec,
    settings: BrowserSettings,
    tab: String,
    history: Vec<HistoryItem>,
    steps: Vec<StepLog>,
    stats: Stats,
    observation: Value,
    text_cache: Option<(Value, String)>,
    criteria: Option<Vec<String>>,
    done_rejections: u32,
    llm_next: bool,
    option_chars: usize,
    pending: Option<Pending>,
}

/// Paused tasks by task id, and approvals → task id.
static TASKS: LazyLock<Mutex<HashMap<String, TaskState>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static APPROVALS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// The tab each chat works in, per driver: `owner|driver` → tab id.
static CHAT_TABS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn chat_tab(owner: &str, driver: Driver) -> Option<String> {
    CHAT_TABS.lock().ok()?.get(&format!("{owner}|{}", driver.as_str())).cloned()
}

pub fn remember_chat_tab(owner: &str, driver: Driver, tab: &str) {
    if let Ok(mut m) = CHAT_TABS.lock() {
        m.insert(format!("{owner}|{}", driver.as_str()), tab.to_string());
    }
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

/// Semantic identity of a page: what changed matters, not animation.
fn fingerprint(obs: &Value) -> String {
    let actions: Vec<Value> = obs
        .get("actions")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(|x| json!([x.get("id"), x.get("label"), x.get("value"), x.get("checked")])).collect())
        .unwrap_or_default();
    json!([obs.get("url"), obs.get("text"), actions, obs.pointer("/viewport/scroll_y"), obs.get("dialog")]).to_string()
}

fn outcome(state: &TaskState, status: &str, message: impl Into<String>) -> TaskOutcome {
    TaskOutcome {
        task_id: state.id.clone(),
        status: status.to_string(),
        message: message.into(),
        driver: state.spec.driver,
        tab_id: Some(state.tab.clone()),
        url: state.observation.get("url").and_then(Value::as_str).map(str::to_string),
        title: state.observation.get("title").and_then(Value::as_str).map(str::to_string),
        answer: None,
        evidence: Vec::new(),
        pending: None,
        steps: state.steps.clone(),
        stats: state.stats.clone(),
    }
}

fn port_failure(spec: &TaskSpec, e: &PortError) -> TaskOutcome {
    TaskOutcome {
        task_id: new_id("brw"),
        status: "error".into(),
        message: e.to_string(),
        driver: spec.driver,
        tab_id: None,
        url: None,
        title: None,
        answer: None,
        evidence: Vec::new(),
        pending: None,
        steps: Vec::new(),
        stats: Stats::default(),
    }
}

/// Start a task: open (or reuse) the owner's tab and run the loop.
pub async fn start(ports: &Ports, settings: BrowserSettings, spec: TaskSpec) -> TaskOutcome {
    let browser = ports.browser.as_ref();
    let session = match ports::open_session(browser, spec.driver.as_str(), &settings.profile, settings.headless).await {
        Ok(s) => s,
        Err(e) => return port_failure(&spec, &e),
    };
    let (tab, mut observation) =
        match ports::open_tab(browser, &session, spec.url.as_deref(), &spec.owner, spec.ext_tab).await {
            Ok(t) => t,
            Err(e) => return port_failure(&spec, &e),
        };
    let blank = observation.get("url").and_then(Value::as_str).map(|u| u == "about:blank" || u.is_empty()).unwrap_or(true);
    if spec.url.is_none() && blank {
        observation = match ports::navigate(browser, &tab, &settings.start_url).await {
            Ok(o) => o,
            Err(e) => return port_failure(&spec, &e),
        };
    }
    remember_chat_tab(&spec.owner, spec.driver, &tab);
    let state = TaskState {
        id: new_id("brw"),
        spec,
        option_chars: DEFAULT_OPTION_CHARS,
        settings,
        tab,
        history: Vec::new(),
        steps: Vec::new(),
        stats: Stats::default(),
        observation,
        text_cache: None,
        criteria: None,
        done_rejections: 0,
        llm_next: false,
        pending: None,
    };
    drive(ports, state).await
}

/// Continue a paused task (after the person's turn): observe again and go on.
pub async fn resume(ports: &Ports, task_id: &str) -> Option<TaskOutcome> {
    let mut state = TASKS.lock().ok()?.remove(task_id)?;
    if let Some(p) = state.pending.take() {
        APPROVALS.lock().ok()?.remove(&p.approval_id);
    }
    state.observation = Value::Null;
    Some(drive(ports, state).await)
}

/// What a pending approval would do — shown to the person in the permission
/// prompt for `browser_approve`, so they confirm the action itself rather
/// than an opaque id the agent chose.
pub fn describe_approval(approval_id: &str) -> Option<Value> {
    let task_id = APPROVALS.lock().ok()?.get(approval_id)?.clone();
    let tasks = TASKS.lock().ok()?;
    let state = tasks.get(&task_id)?;
    let p = state.pending.as_ref()?;
    Some(json!({
        "approval_id": p.approval_id,
        "action": p.label,
        "operation": p.operation,
        "text": p.text,
        "url": state.observation.get("url"),
        "goal": state.spec.goal,
    }))
}

/// The person's answer to a pending approval.
pub async fn approve(ports: &Ports, approval_id: &str, approved: bool) -> Option<TaskOutcome> {
    let task_id = APPROVALS.lock().ok()?.remove(approval_id)?;
    let mut state = TASKS.lock().ok()?.remove(&task_id)?;
    let pending = state.pending.take()?;
    if !approved {
        match &pending.reject_action {
            Some(dismiss) => {
                let label = format!("{} (declined)", pending.label);
                if let Err(done) = execute(ports, &mut state, &pending.operation, dismiss, &label, "dialog", None, pending.observation_id, "person", None, None, 0).await {
                    return Some(done);
                }
            }
            None => return Some(outcome(&state, "cancelled", format!("The person declined: {}", pending.label))),
        }
    } else if let Err(done) = execute(
        ports,
        &mut state,
        &pending.operation,
        &pending.action_id,
        &pending.label,
        &pending.kind,
        pending.text.as_deref(),
        pending.observation_id,
        "person",
        None,
        None,
        0,
    )
    .await
    {
        return Some(done);
    }
    Some(drive(ports, state).await)
}

fn park(mut state: TaskState, status: &str, message: String, pending: Option<Pending>) -> TaskOutcome {
    let mut out = outcome(&state, status, message);
    if let Some(p) = &pending {
        out.pending = Some(json!({
            "approval_id": p.approval_id,
            "operation": p.operation,
            "action": p.label,
            "url": state.observation.get("url"),
        }));
        if let Ok(mut a) = APPROVALS.lock() {
            a.insert(p.approval_id.clone(), state.id.clone());
        }
    }
    state.pending = pending;
    if let Ok(mut t) = TASKS.lock() {
        t.insert(state.id.clone(), state);
    }
    out
}

/// Execute one action and fold the result into the state. `Err` ends the task.
#[allow(clippy::too_many_arguments)]
async fn execute(
    ports: &Ports,
    state: &mut TaskState,
    operation: &str,
    action_id: &str,
    label: &str,
    kind: &str,
    text: Option<&str>,
    observation_id: u64,
    by: &'static str,
    confidence: Option<f64>,
    band: Option<Band>,
    decision_ms: u64,
) -> Result<(), TaskOutcome> {
    let before = fingerprint(&state.observation);
    let target = None;
    match ports::act(ports.browser.as_ref(), &state.tab, observation_id, action_id, text).await {
        Ok(out) => {
            let mut next = out.get("observation").cloned().unwrap_or(Value::Null);
            if next.is_null() {
                next = ports::observe(ports.browser.as_ref(), &state.tab).await.unwrap_or(Value::Null);
            }
            let changed = fingerprint(&next) != before;
            state.stats.steps += 1;
            state.history.push(HistoryItem {
                action: label.to_string(),
                kind: kind.to_string(),
                text: text.map(str::to_string),
                page_changed: Some(changed),
            });
            state.steps.push(StepLog {
                step: state.stats.steps,
                operation: operation.to_string(),
                target,
                label: label.to_string(),
                by,
                confidence,
                band,
                decision_ms,
                text: text.map(str::to_string),
                outcome: "executed".into(),
                page_changed: Some(changed),
            });
            state.observation = next;
            state.text_cache = None;
            Ok(())
        }
        Err(e) => {
            let recoverable = matches!(e.code.as_str(), "stale_page" | "target_covered" | "invalid_target" | "dialog_open");
            state.steps.push(StepLog {
                step: state.stats.steps,
                operation: operation.to_string(),
                target,
                label: label.to_string(),
                by,
                confidence,
                band,
                decision_ms,
                text: None,
                outcome: e.code.clone(),
                page_changed: None,
            });
            if recoverable {
                state.stats.stale += 1;
                state.observation = ports::observe(ports.browser.as_ref(), &state.tab).await.unwrap_or(Value::Null);
                return Ok(());
            }
            let status = match e.code.as_str() {
                "handover_active" | "user_active" | "debugger_detached" | "blocked" => "needs_user",
                _ => "error",
            };
            Err(park(state.clone(), status, e.to_string(), None))
        }
    }
}

fn text_of(obs: &Value, limit: usize) -> String {
    obs.get("text").and_then(Value::as_str).unwrap_or_default().chars().take(limit).collect()
}

/// Ask the decision model whether the page proves each criterion.
async fn verify_done(ports: &Ports, state: &mut TaskState, route: &DecisionRoute) -> (bool, Vec<Value>) {
    if state.criteria.is_none() {
        state.criteria = Some(if !state.spec.done_criteria.is_empty() {
            state.spec.done_criteria.clone()
        } else {
            state.stats.llm_calls += 1;
            llm_role::criteria(ports.llm.as_ref(), state.settings.fallback_model.as_deref(), &state.spec.goal).await
        });
    }
    let criteria = state.criteria.clone().unwrap_or_default();
    let redact = matches!(route, DecisionRoute::Model { redact: true, .. });
    let text = text_of(&state.observation, 6000);
    let text = if redact { policy::redact_pii(&text) } else { text };
    let page = json!({ "url": state.observation.get("url"), "title": state.observation.get("title"), "text": text });

    if let DecisionRoute::Model { backend, .. } = route {
        let questions = Json::Object(
            criteria
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    (
                        format!("c{}", i + 1),
                        Json::Object(vec![
                            ("type".into(), Json::String("noul".into())),
                            ("instructions".into(), Json::String(format!("{VERIFY}\nCriterion: {c}"))),
                        ]),
                    )
                })
                .collect(),
        );
        let request = AskRequest { backend: Some(*backend), model: None, state: to_json(&page), questions };
        state.stats.decisions += 1;
        if let Ok(answers) = ports.decider.ask(&request).await {
            let mut all = true;
            let mut evidence = Vec::new();
            for (i, c) in criteria.iter().enumerate() {
                let p = answers.pointer(&format!("/c{}/noul", i + 1)).and_then(Value::as_f64);
                let ok = p.map(|p| p >= 0.5).unwrap_or(false);
                all &= ok;
                evidence.push(json!({ "criterion": c, "verdict": ok, "p": p, "by": "jev" }));
            }
            if evidence.iter().all(|e| e["p"].is_number()) {
                return (all, evidence);
            }
        }
    }
    // No decision model (or it could not answer): the LLM checks, and can only
    // confirm what the page shows.
    let mut all = true;
    let mut evidence = Vec::new();
    for c in &criteria {
        state.stats.llm_calls += 1;
        let verdict = llm_role::verify(ports.llm.as_ref(), state.settings.fallback_model.as_deref(), c, &page).await;
        let ok = verdict.as_ref().map(|v| *v).unwrap_or(false);
        all &= ok;
        evidence.push(json!({ "criterion": c, "verdict": ok, "by": "llm", "error": verdict.err() }));
    }
    (all, evidence)
}

fn record_trace(owner: &str, step: &Step, latency_ms: u64) {
    crate::control_plane::trace::record_decision(
        owner,
        crate::control_plane::ladder::Decision {
            spec: "browser.step".into(),
            version: 1,
            by: crate::control_plane::ladder::DecidedBy::Jev,
            answer: match &step.target {
                Some(t) => format!("{} [{t}]", step.operation),
                None => step.operation.clone(),
            },
            confidence: step.confidence,
            band: match step.band {
                Band::Act => "act",
                Band::Fallback => "fallback",
                Band::Review => "review",
            },
            latency_ms: latency_ms as f64,
            shadow: false,
        },
    );
}

/// The loop proper.
async fn drive(ports: &Ports, mut state: TaskState) -> TaskOutcome {
    let started = Instant::now();
    let max_steps = state.spec.max_steps.unwrap_or(state.settings.max_steps).clamp(1, 120);
    let base_elapsed = state.stats.elapsed_ms;
    loop {
        state.stats.elapsed_ms = base_elapsed + started.elapsed().as_millis() as u64;
        if state.stats.steps >= max_steps || state.stats.decisions >= max_steps * 2 {
            return park(state, "budget", format!("Stopped at the {max_steps}-step budget"), None);
        }
        if state.observation.is_null() {
            match ports::observe(ports.browser.as_ref(), &state.tab).await {
                Ok(o) => state.observation = o,
                Err(e) => {
                    let status = if matches!(e.code.as_str(), "handover_active" | "user_active" | "debugger_detached") { "needs_user" } else { "error" };
                    return park(state, status, e.to_string(), None);
                }
            }
        }
        let obs = state.observation.clone();
        let observation_id = obs.get("observation_id").and_then(Value::as_u64).unwrap_or(0);
        let url = obs.get("url").and_then(Value::as_str).unwrap_or_default().to_string();

        // Rule tier: a dialog blocks the page.
        if let Some(dialog) = obs.get("dialog").filter(|d| !d.is_null()) {
            let kind = dialog.get("type").and_then(Value::as_str).unwrap_or("alert").to_string();
            let message = dialog.get("message").and_then(Value::as_str).unwrap_or_default().to_string();
            if kind == "alert" {
                if let Err(done) = execute(ports, &mut state, "DIALOG_ACCEPT", "dialog_accept", &format!("Accept alert: {message}"), "dialog", None, observation_id, "rule", None, None, 0).await {
                    return done;
                }
                continue;
            }
            let pending = Pending {
                approval_id: new_id("apv"),
                observation_id,
                action_id: "dialog_accept".into(),
                operation: "DIALOG_ACCEPT".into(),
                label: format!("Accept the {kind} dialog: {message}"),
                kind: "dialog".into(),
                text: None,
                reject_action: Some("dialog_dismiss".into()),
            };
            let msg = format!("The page asks to confirm: {message}");
            return park(state, "needs_approval", msg, Some(pending));
        }

        // Decision tier.
        let route = policy::select_backend(&state.settings, &url, state.spec.driver);
        let decision_started = Instant::now();
        let mut model_step: Option<Step> = None;
        let mut guesses = Value::Null;
        let encoded_for_llm;
        match &route {
            DecisionRoute::LlmOnly => {
                encoded_for_llm = encode(&obs, &state.spec.goal, &state.history, Profile::JevFull, None, None);
            }
            DecisionRoute::Model { profile, backend, model, redact } => {
                let (mut view, _) = if *profile == Profile::LayaV3 { budget::prune(&obs, &state.spec.goal, state.option_chars) } else { (obs.clone(), 0) };
                if *redact {
                    let t = view.get("text").and_then(Value::as_str).map(policy::redact_pii);
                    if let Some(t) = t {
                        view["text"] = Value::String(t);
                    }
                }
                let bands = if *backend == Backend::Online { state.settings.bands_hosted } else { state.settings.bands_local };
                let encoded = encode(&view, &state.spec.goal, &state.history, *profile, model.clone(), Some(*backend));
                state.stats.decisions += 1;
                match ports.decider.ask(&encoded.request).await {
                    Ok(answers) => match resolve(&answers, &encoded, bands) {
                        Ok(step) => {
                            guesses = json!({ "operations": step.top_operations, "targets": step.top_targets });
                            record_trace(&state.spec.owner, &step, decision_started.elapsed().as_millis() as u64);
                            model_step = Some(step);
                        }
                        Err(e) => tracing::warn!("[browser] decision answer rejected: {e}"),
                    },
                    Err(e) if e.contains("do not fit") && state.option_chars > 300 => {
                        state.option_chars /= 2;
                        continue;
                    }
                    Err(e) => tracing::warn!("[browser] decision runtime unavailable: {e}"),
                }
                encoded_for_llm = encode(&obs, &state.spec.goal, &state.history, Profile::JevFull, None, None);
            }
        }

        // LLM tier when the model is unsure, said BLOCKED, or progress stalled.
        let needs_llm = match &model_step {
            None => true,
            Some(s) => s.band != Band::Act || s.operation == "BLOCKED" || state.llm_next,
        };
        let (operation, target, action, by, confidence, band) = if needs_llm {
            state.llm_next = false;
            state.stats.llm_calls += 1;
            state.stats.fallbacks += 1;
            match llm_role::fallback(ports.llm.as_ref(), state.settings.fallback_model.as_deref(), &encoded_for_llm, &obs, &state.spec.goal, &state.history, &guesses).await {
                Ok((op, target, _reason)) => {
                    let action = match &target {
                        Some(t) => encoded_for_llm.space.target(&op, t).map(|x| x.action.clone()).unwrap_or(Value::Null),
                        None => encoded_for_llm.space.control(&op).cloned().unwrap_or(Value::Null),
                    };
                    (op, target, action, "llm", None, model_step.as_ref().map(|s| s.band))
                }
                Err(e) => match model_step {
                    // Without an LLM, a merely unsure answer is still the best
                    // guess; a very unsure one waits for the person.
                    Some(s) if s.band == Band::Fallback && s.operation != "BLOCKED" => {
                        (s.operation.clone(), s.target.clone(), s.action.clone(), "jev", Some(s.confidence), Some(s.band))
                    }
                    _ => return park(state, "needs_user", format!("Neither the decision model nor the LLM could choose a step: {e}"), None),
                },
            }
        } else {
            let s = model_step.expect("checked");
            (s.operation, s.target, s.action, "jev", Some(s.confidence), Some(s.band))
        };
        let decision_ms = decision_started.elapsed().as_millis() as u64;

        if operation == "BLOCKED" {
            return park(state, "blocked", "No offered operation can make progress on this page".into(), None);
        }
        if operation == "DONE" {
            let (verified, evidence) = verify_done(ports, &mut state, &route).await;
            if verified {
                let mut out = outcome(&state, "done", "Verified on the page");
                out.evidence = evidence;
                if let Some(q) = state.spec.question.clone() {
                    if let Ok(read) = ports::read(ports.browser.as_ref(), &state.tab, 20_000).await {
                        state.stats.llm_calls += 1;
                        out.answer = llm_role::answer(ports.llm.as_ref(), state.settings.fallback_model.as_deref(), &q, &read).await.ok();
                    }
                    out.stats = state.stats.clone();
                }
                return out;
            }
            state.done_rejections += 1;
            let missing: Vec<String> = evidence.iter().filter(|e| e["verdict"] == false).filter_map(|e| e["criterion"].as_str().map(str::to_string)).collect();
            state.history.push(HistoryItem {
                action: format!("DONE rejected: the page does not show {}", missing.join("; ")),
                kind: "rejected".into(),
                text: None,
                page_changed: Some(false),
            });
            if state.done_rejections >= 2 {
                let mut out = park(state, "unverified", "The page does not prove the goal was reached".into(), None);
                out.evidence = evidence;
                return out;
            }
            continue;
        }

        let action_id = action.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let label = action.get("label").and_then(Value::as_str).unwrap_or(&operation).to_string();
        let kind = action.get("kind").and_then(Value::as_str).unwrap_or_default().to_string();
        if action_id.is_empty() {
            return park(state, "error", format!("{operation} has no executable action"), None);
        }

        // Policy: code decides, no model can lower a tier.
        let dialog_type = None;
        let (tier, reason) = policy::risk_tier(&operation, &action, dialog_type);
        match tier {
            Tier::Human => {
                return park(state, "needs_user", format!("{reason}: hand the tab to the person (browser_handover)"), None);
            }
            Tier::Approve => {
                let text = None;
                let pending = Pending {
                    approval_id: new_id("apv"),
                    observation_id,
                    action_id,
                    operation,
                    label: label.clone(),
                    kind,
                    text,
                    reject_action: None,
                };
                return park(state, "needs_approval", format!("Needs the person's approval: {reason}"), Some(pending));
            }
            Tier::Auto | Tier::Logged => {}
        }

        // Text for TYPE_TEXT: generated once per identical input.
        let mut text = None;
        if operation == "TYPE_TEXT" {
            let context = llm_role::text_context(&state.spec.goal, &action, &obs, &state.history);
            let cached = state.text_cache.as_ref().filter(|(c, _)| *c == context).map(|(_, t)| t.clone());
            let value = match cached {
                Some(t) => t,
                None => {
                    state.stats.llm_calls += 1;
                    match llm_role::text_value(ports.llm.as_ref(), state.settings.text_model.as_deref(), &context).await {
                        Ok(TextValue::Text(t)) => {
                            state.text_cache = Some((context, t.clone()));
                            t
                        }
                        Ok(TextValue::Missing) => {
                            return park(state, "needs_input", format!("The goal does not say what to enter in \"{label}\""), None);
                        }
                        Err(e) => return park(state, "error", e, None),
                    }
                }
            };
            text = Some(value);
        }

        let steps_before = state.stats.steps;
        if let Err(done) = execute(ports, &mut state, &operation, &action_id, &label, &kind, text.as_deref(), observation_id, by, confidence, band, decision_ms).await {
            return done;
        }
        if let Some(last) = state.steps.last_mut() {
            last.target = target;
        }

        // No progress: three executed non-wait steps that changed nothing.
        if state.stats.steps > steps_before {
            let recent: Vec<&HistoryItem> = state.history.iter().rev().take(3).collect();
            let stuck = recent.len() == 3 && recent.iter().all(|h| h.page_changed == Some(false) && h.kind != "wait");
            if stuck {
                let llm_already = state.steps.iter().rev().take(3).any(|s| s.by == "llm");
                if llm_already {
                    return park(state, "blocked", "Three actions in a row changed nothing on the page".into(), None);
                }
                state.llm_next = true;
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::browser_agent::ports::{BrowserPort, Decider, Llm};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// A page with a Search button that, once clicked, shows results.
    pub struct FakeBrowser {
        pub clicked: std::sync::Mutex<bool>,
        pub seq: AtomicU64,
        /// When false, clicking changes nothing (a dead button).
        pub responsive: bool,
    }

    impl FakeBrowser {
        pub fn new(responsive: bool) -> Arc<FakeBrowser> {
            Arc::new(FakeBrowser { clicked: std::sync::Mutex::new(false), seq: AtomicU64::new(0), responsive })
        }

        fn page(&self) -> Value {
            let id = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
            let done = *self.clicked.lock().unwrap() && self.responsive;
            json!({
                "observation_id": id, "tab_id": "t1", "url": if done { "https://shop.test/results" } else { "https://shop.test/" },
                "title": "Shop", "text": if done { "Results for books: 3 found" } else { "Find books" },
                "actions": [
                    {"id": "e1", "node": 1, "kind": "click", "role": "button", "label": "Search", "value": ""},
                    {"id": "wait", "kind": "wait", "label": "Wait for the page to update"}
                ],
                "dialog": null, "viewport": {"scroll_y": 0}
            })
        }
    }

    #[async_trait]
    impl BrowserPort for FakeBrowser {
        async fn call(&self, _method: &str, path: &str, _body: Option<Value>) -> Result<Value, PortError> {
            if path == "/v1/sessions" {
                return Ok(json!({ "id": "s1" }));
            }
            if path.ends_with("/tabs") {
                return Ok(json!({ "tab": { "id": "t1" }, "observation": self.page() }));
            }
            if path.ends_with("/act") {
                *self.clicked.lock().unwrap() = true;
                return Ok(json!({ "executed": "e1", "observation": self.page() }));
            }
            if path.ends_with("/read") {
                return Ok(json!({ "text": "Results for books: 3 found" }));
            }
            Ok(self.page())
        }
    }

    /// CLICK Search until the page shows results, then DONE; verification by
    /// noul reads the page text.
    pub struct FakeDecider;

    fn choice(ids: &[&str], selected: &str, p: f64) -> Value {
        let rest = if ids.len() > 1 { (1.0 - p) / (ids.len() - 1) as f64 } else { 0.0 };
        let probs: serde_json::Map<String, Value> = ids.iter().map(|i| (i.to_string(), json!(if *i == selected { p } else { rest }))).collect();
        json!({ "choice": selected, "confidence": p, "probabilities": probs })
    }

    #[async_trait]
    impl Decider for FakeDecider {
        async fn ask(&self, request: &AskRequest) -> Result<Value, String> {
            let text = serde_json::to_string(&request.state).unwrap();
            if request.questions.get("c1").is_some() {
                let ok = text.contains("Results");
                return Ok(json!({ "c1": { "type": "noul", "noul": if ok { 0.95 } else { 0.05 }, "confidence": 0.9 } }));
            }
            let ops: Vec<String> = request
                .questions
                .get("operation")
                .and_then(|q| q.get("criteria"))
                .and_then(|c| c.as_object())
                .map(|o| o.iter().map(|(k, _)| k.clone()).collect())
                .unwrap_or_default();
            let ops: Vec<&str> = ops.iter().map(String::as_str).collect();
            if text.contains("Results") {
                Ok(json!({ "operation": choice(&ops, "DONE", 0.95) }))
            } else {
                Ok(json!({ "operation": choice(&ops, "CLICK", 0.95), "click_target": choice(&["1"], "1", 1.0) }))
            }
        }
    }

    pub struct NoLlm;

    #[async_trait]
    impl Llm for NoLlm {
        async fn complete(&self, _m: Option<&str>, _s: &str, _u: &str, _t: u32) -> Result<String, String> {
            Err("no LLM in this test".into())
        }
    }

    pub fn ports(browser: Arc<FakeBrowser>) -> Ports {
        Ports { browser, decider: Arc::new(FakeDecider), llm: Arc::new(NoLlm) }
    }

    pub fn spec(goal: &str) -> TaskSpec {
        TaskSpec {
            goal: goal.into(),
            url: Some("https://shop.test/".into()),
            question: None,
            done_criteria: vec!["Search results are shown".into()],
            max_steps: Some(10),
            driver: Driver::Managed,
            owner: "test-chat".into(),
            ext_tab: None,
        }
    }

    #[tokio::test]
    async fn loop_completes_with_verified_done() {
        let out = start(&ports(FakeBrowser::new(true)), BrowserSettings::default(), spec("Search for books")).await;
        assert_eq!(out.status, "done", "{:?}", out.message);
        assert_eq!(out.stats.steps, 1);
        assert_eq!(out.steps[0].by, "jev");
        assert_eq!(out.steps[0].target.as_deref(), Some("1"));
        assert_eq!(out.evidence[0]["verdict"], true);
        assert_eq!(chat_tab("test-chat", Driver::Managed).as_deref(), Some("t1"));
    }

    #[tokio::test]
    async fn loop_stops_without_progress() {
        let out = start(&ports(FakeBrowser::new(false)), BrowserSettings::default(), spec("Search for books")).await;
        // The dead button: three clicks change nothing; the LLM tier is tried
        // (unavailable here) and the task stops instead of clicking forever.
        assert!(matches!(out.status.as_str(), "blocked" | "needs_user"), "{} {}", out.status, out.message);
        assert!(out.stats.steps <= 4, "stopped early ({} steps)", out.stats.steps);
    }
}
