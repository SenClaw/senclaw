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
use super::prompts::{RISK, RISK_EFFECTS, VERIFY};
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
    /// The round trip to the browser: freshness check, input, settle, observe.
    pub act_ms: u64,
    pub text: Option<String>,
    pub outcome: String,
    pub page_changed: Option<bool>,
}

/// Where a task's wall-clock time went, in milliseconds. `decide` is the
/// decision model choosing steps, `llm` the fallback tier choosing instead,
/// `act` the round trips to the browser and `page` the part of those the
/// browser itself reported (input, settle, snapshot).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Timing {
    pub open: u64,
    pub decide: u64,
    pub llm: u64,
    pub risk: u64,
    pub text: u64,
    pub act: u64,
    pub page: u64,
    pub verify: u64,
    pub answer: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub steps: u32,
    pub decisions: u32,
    pub llm_calls: u32,
    pub fallbacks: u32,
    pub stale: u32,
    pub elapsed_ms: u64,
    pub timing: Timing,
}

fn ms_since(started: Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

/// What the browser reported spending on one action: input, settle and the
/// snapshot that followed.
fn page_ms(act: &Value) -> u64 {
    let part = |pointer: &str| act.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
    part("/executed_ms") + part("/settle_ms") + part("/observation/timing_ms/snapshot")
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
    /// What the person should know about how the task ran (a missing
    /// decision model made it slow, …).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
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
    /// The criteria an LLM is writing while the task runs (none were given).
    criteria_job: Option<CriteriaJob>,
    /// The last check of the criteria, with the observation it was made on.
    checked: Option<(u64, bool, Vec<Value>)>,
    notes: Vec<String>,
    done_rejections: u32,
    llm_next: bool,
    option_chars: usize,
    pending: Option<Pending>,
    /// Every action taken, keyed with the page state it was taken from.
    tried: Vec<u64>,
    /// A text field the last step clicked into without changing the page.
    type_into: Option<i64>,
    /// Steps and decisions already spent when this run began: a resumed task
    /// gets a fresh budget instead of stopping at once.
    budget_base: (u32, u32),
    /// Recoverable failures in a row (the page changed under the target).
    stale_streak: u32,
    /// Answers of the raise-only risk check, by page and action.
    risk_checked: HashMap<u64, bool>,
    parked_at: Option<Instant>,
    /// When this run of the loop began, and the time earlier runs had used.
    clock: (Instant, u64),
}

/// Load the local decision checkpoints while the page opens: the one that
/// picks the steps, then the default one that checks risk and completion.
/// One after the other — the decision runtime can fail a checkpoint when two
/// are loading at the same moment.
fn warm_decision_models(ports: &Ports, settings: &BrowserSettings, spec: &TaskSpec) {
    let start = spec.url.as_deref().unwrap_or(&settings.start_url);
    let DecisionRoute::Model { backend: Backend::Local, model, .. } = policy::select_backend(settings, start, spec.driver) else { return };
    let decider = ports.decider.clone();
    tokio::spawn(async move {
        decider.warm(model.as_deref()).await;
        decider.warm(None).await;
    });
}

/// Completion criteria on their way from an LLM.
type CriteriaJob = futures::future::Shared<futures::future::BoxFuture<'static, Vec<String>>>;

/// Have an LLM write the completion criteria now, while the task runs: by the
/// time the page looks done they are usually there, instead of being one more
/// model call between the last step and the answer.
fn write_criteria(ports: &Ports, settings: &BrowserSettings, goal: &str) -> CriteriaJob {
    use futures::FutureExt;
    let (llm, model, goal) = (ports.llm.clone(), settings.fallback_model.clone(), goal.to_string());
    let writing = tokio::spawn(async move { llm_role::criteria(llm.as_ref(), model.as_deref(), &goal).await });
    async move { writing.await.unwrap_or_default() }.boxed().shared()
}

fn elapsed_ms(state: &TaskState) -> u64 {
    state.clock.1 + ms_since(state.clock.0)
}

/// Paused tasks by task id, and approvals → task id.
static TASKS: LazyLock<Mutex<HashMap<String, TaskState>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static APPROVALS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// The tab each chat works in, per driver: `owner|driver` → tab id.
static CHAT_TABS: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// Tasks driving the person's Chrome right now (the extension pipe stays open while any run).
static RUNNING_EXTENSION: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Paused tasks kept for resume or approval; the oldest goes first.
const MAX_PARKED: usize = 100;
const MAX_CHAT_TABS: usize = 512;
/// One run of the loop, so a tool call always gets an answer: the task parks
/// as `budget` (resumable) instead of being cut off by the caller's timeout.
const RUN_WALL_CLOCK: std::time::Duration = std::time::Duration::from_secs(14 * 60);
/// Recoverable failures in a row before the task waits for the person.
const MAX_STALE_STREAK: u32 = 5;

pub fn chat_tab(owner: &str, driver: Driver) -> Option<String> {
    CHAT_TABS.lock().ok()?.get(&format!("{owner}|{}", driver.as_str())).cloned()
}

pub fn remember_chat_tab(owner: &str, driver: Driver, tab: &str) {
    if let Ok(mut m) = CHAT_TABS.lock() {
        if m.len() >= MAX_CHAT_TABS {
            if let Some(k) = m.keys().next().cloned() {
                m.remove(&k);
            }
        }
        m.insert(format!("{owner}|{}", driver.as_str()), tab.to_string());
    }
}

/// The remembered tab is gone (closed, or the runtime restarted).
pub fn forget_chat_tab(owner: &str, driver: Driver) {
    if let Ok(mut m) = CHAT_TABS.lock() {
        m.remove(&format!("{owner}|{}", driver.as_str()));
    }
}

/// The driver of the paused task an approval belongs to.
pub fn driver_of_approval(approval_id: &str) -> Option<Driver> {
    let task = APPROVALS.lock().ok()?.get(approval_id)?.clone();
    driver_of_task(&task)
}

pub fn driver_of_task(task_id: &str) -> Option<Driver> {
    TASKS.lock().ok()?.get(task_id).map(|t| t.spec.driver)
}

/// The chat a paused task belongs to.
pub fn owner_of_task(task_id: &str) -> Option<String> {
    TASKS.lock().ok()?.get(task_id).map(|t| t.spec.owner.clone())
}

pub fn owner_of_approval(approval_id: &str) -> Option<String> {
    let task = APPROVALS.lock().ok()?.get(approval_id)?.clone();
    owner_of_task(&task)
}

/// How long a task paused in the person's Chrome keeps the extension pipe
/// (and so its tabs) open while it waits for them.
const PARKED_LEASE: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Whether a task needs the extension pipe: one running in the person's
/// Chrome, or one paused there recently enough to still be answered.
pub fn extension_busy() -> bool {
    RUNNING_EXTENSION.load(std::sync::atomic::Ordering::SeqCst) > 0
        || TASKS
            .lock()
            .map(|t| {
                t.values().any(|s| s.spec.driver == Driver::Extension && s.parked_at.is_none_or(|at| at.elapsed() < PARKED_LEASE))
            })
            .unwrap_or(true)
}

/// Every action waiting for the person, for the settings screens.
pub fn pending_approvals() -> Vec<Value> {
    let Ok(tasks) = TASKS.lock() else { return Vec::new() };
    let mut list: Vec<(Instant, Value)> = tasks
        .values()
        .filter_map(|s| {
            let p = s.pending.as_ref()?;
            Some((
                s.parked_at.unwrap_or_else(Instant::now),
                json!({
                    "approval_id": p.approval_id, "task_id": s.id, "chat": s.spec.owner, "goal": s.spec.goal,
                    "action": p.label, "operation": p.operation, "text": p.text, "driver": s.spec.driver,
                    "url": s.observation.get("url"), "waiting_secs": s.parked_at.map(|t| t.elapsed().as_secs()),
                }),
            ))
        })
        .collect();
    list.sort_by_key(|(t, _)| *t);
    list.into_iter().map(|(_, v)| v).collect()
}

/// Leaves the running-in-the-person's-Chrome count however the run ends.
struct ExtensionRun(bool);

impl ExtensionRun {
    fn start(driver: Driver) -> ExtensionRun {
        let on = driver == Driver::Extension;
        if on {
            RUNNING_EXTENSION.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        ExtensionRun(on)
    }
}

impl Drop for ExtensionRun {
    fn drop(&mut self) {
        if self.0 {
            RUNNING_EXTENSION.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

/// Semantic identity of a page: what changed matters, not animation.
fn fingerprint(obs: &Value) -> String {
    fingerprint_where(obs, |_| true)
}

/// The page without what focus alone changes (the "Press Enter in …" key
/// control appears once a field has focus).
fn fingerprint_unfocused(obs: &Value) -> String {
    fingerprint_where(obs, |a| a.get("kind").and_then(Value::as_str) != Some("key"))
}

fn fingerprint_where(obs: &Value, keep: impl Fn(&Value) -> bool) -> String {
    let actions: Vec<Value> = obs
        .get("actions")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(|x| keep(x)).map(|x| json!([x.get("id"), x.get("label"), x.get("value"), x.get("checked")])).collect())
        .unwrap_or_default();
    json!([obs.get("url"), obs.get("text"), actions, obs.pointer("/viewport/scroll_y"), obs.get("dialog")]).to_string()
}

/// The TYPE_TEXT action of the editable field `node`, when the page offers one.
fn fill_action(obs: &Value, node: i64) -> Option<Value> {
    obs.get("actions")?
        .as_array()?
        .iter()
        .find(|a| a.get("kind").and_then(Value::as_str) == Some("fill") && a.get("node").and_then(Value::as_i64) == Some(node))
        .cloned()
}

/// One action from one page state. Pages are deterministic enough that taking
/// the same action from the same state again only repeats a detour.
fn cycle_key(obs: &Value, operation: &str, action: &Value, text: Option<&str>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let element = (action.get("id").and_then(Value::as_str), action.get("node").and_then(Value::as_i64), action.get("label").and_then(Value::as_str));
    (fingerprint_unfocused(obs), operation, element, text).hash(&mut h);
    h.finish()
}

/// Hosted decisions see no personal data: page text, element labels and
/// values, and what was typed, masked the same way.
fn redact_view(view: &mut Value) {
    if let Some(t) = view.get("text").and_then(Value::as_str).map(policy::redact_pii) {
        view["text"] = Value::String(t);
    }
    if let Some(actions) = view.get_mut("actions").and_then(Value::as_array_mut) {
        for a in actions {
            for key in ["label", "value", "current_value"] {
                if let Some(v) = a.get(key).and_then(Value::as_str).map(policy::redact_pii) {
                    a[key] = Value::String(v);
                }
            }
        }
    }
}

fn redacted_history(history: &[HistoryItem]) -> Vec<HistoryItem> {
    history
        .iter()
        .map(|h| HistoryItem {
            action: policy::redact_pii(&h.action),
            kind: h.kind.clone(),
            text: h.text.as_deref().map(policy::redact_pii),
            page_changed: h.page_changed,
        })
        .collect()
}

/// The design's backstop behind the word lists: the decision model may raise
/// a click or an Enter to Approve, never lower anything. Asked once per page
/// state and action; no decision model (or no answer) raises nothing.
async fn model_raises_risk(ports: &Ports, state: &mut TaskState, route: &DecisionRoute, obs: &Value, operation: &str, action: &Value) -> Option<String> {
    use std::hash::{Hash, Hasher};
    if !matches!(operation, "CLICK" | "KEY_ENTER") {
        return None;
    }
    let DecisionRoute::Model { backend, redact, .. } = route else { return None };
    let label = action.get("label").and_then(Value::as_str).unwrap_or_default();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (fingerprint_unfocused(obs), operation, label).hash(&mut h);
    let key = h.finish();
    let raised = match state.risk_checked.get(&key) {
        Some(r) => *r,
        None => {
            state.stats.decisions += 1;
            let asked = Instant::now();
            let raised = risk_answer(ports, *backend, *redact, obs, label).await;
            state.stats.timing.risk += ms_since(asked);
            state.risk_checked.insert(key, raised);
            raised
        }
    };
    raised.then(|| format!("\"{label}\" may be irreversible (decision model)"))
}

/// The same raise-only check for one step chosen by hand (`browser_do`).
pub async fn manual_step_raises_risk(ports: &Ports, settings: &BrowserSettings, driver: Driver, obs: &Value, operation: &str, action: &Value) -> Option<String> {
    if !matches!(operation, "CLICK" | "KEY_ENTER") {
        return None;
    }
    let url = obs.get("url").and_then(Value::as_str).unwrap_or_default();
    let DecisionRoute::Model { backend, redact, .. } = policy::select_backend(settings, url, driver) else { return None };
    let label = action.get("label").and_then(Value::as_str).unwrap_or_default();
    risk_answer(ports, backend, redact, obs, label)
        .await
        .then(|| format!("\"{label}\" may be irreversible (decision model)"))
}

async fn risk_answer(ports: &Ports, backend: Backend, redact: bool, obs: &Value, label: &str) -> bool {
    let text = text_of(obs, 1200);
    let page = json!({
        "page": { "url": obs.get("url"), "title": obs.get("title"), "text": if redact { policy::redact_pii(&text) } else { text } },
        "action": if redact { policy::redact_pii(label) } else { label.to_string() },
    });
    let effects = RISK_EFFECTS.iter().map(|(id, what)| (id.to_string(), Json::String(what.to_string()))).collect();
    let questions = Json::Object(vec![(
        "effect".into(),
        Json::Object(vec![
            ("type".into(), Json::String("choice".into())),
            ("instructions".into(), Json::String(RISK.into())),
            ("criteria".into(), Json::Object(effects)),
        ]),
    )]);
    let request = AskRequest { backend: Some(backend), model: None, state: to_json(&page), questions };
    match ports.decider.ask(&request).await {
        Ok(a) => a.pointer("/effect/probabilities/commit").and_then(Value::as_f64).is_some_and(|p| p >= 0.7),
        Err(_) => false,
    }
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
        notes: state.notes.clone(),
        steps: state.steps.clone(),
        stats: Stats { elapsed_ms: elapsed_ms(state), ..state.stats.clone() },
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
        notes: Vec::new(),
        steps: Vec::new(),
        stats: Stats::default(),
    }
}

/// Start a task: open (or reuse) the owner's tab and run the loop.
pub async fn start(ports: &Ports, settings: BrowserSettings, spec: TaskSpec) -> TaskOutcome {
    let browser = ports.browser.as_ref();
    let opening = Instant::now();
    let criteria_job = spec.done_criteria.is_empty().then(|| write_criteria(ports, &settings, &spec.goal));
    warm_decision_models(ports, &settings, &spec);
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
        stats: Stats {
            llm_calls: criteria_job.is_some() as u32,
            timing: Timing { open: ms_since(opening), ..Timing::default() },
            ..Stats::default()
        },
        observation,
        text_cache: None,
        criteria: None,
        criteria_job,
        checked: None,
        notes: Vec::new(),
        done_rejections: 0,
        llm_next: false,
        pending: None,
        tried: Vec::new(),
        type_into: None,
        budget_base: (0, 0),
        stale_streak: 0,
        risk_checked: HashMap::new(),
        parked_at: None,
        clock: (Instant::now(), 0),
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
    state.budget_base = (state.stats.steps, state.stats.decisions);
    state.stale_streak = 0;
    state.clock = (Instant::now(), state.stats.elapsed_ms);
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
    state.clock = (Instant::now(), state.stats.elapsed_ms);
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
    state.stats.elapsed_ms = elapsed_ms(&state);
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
    state.parked_at = Some(Instant::now());
    if let Ok(mut t) = TASKS.lock() {
        while t.len() >= MAX_PARKED {
            // A task waiting for the person goes last: evicting it drops their approval.
            let oldest = t
                .iter()
                .filter(|(_, s)| s.pending.is_none())
                .min_by_key(|(_, s)| s.parked_at)
                .or_else(|| t.iter().min_by_key(|(_, s)| s.parked_at))
                .map(|(k, _)| k.clone());
            let Some(oldest) = oldest else { break };
            if let Some(gone) = t.remove(&oldest) {
                if let (Some(p), Ok(mut a)) = (gone.pending, APPROVALS.lock()) {
                    a.remove(&p.approval_id);
                }
            }
        }
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
    let acting = Instant::now();
    let acted = ports::act(ports.browser.as_ref(), &state.tab, observation_id, action_id, text).await;
    let act_ms = ms_since(acting);
    state.stats.timing.act += act_ms;
    match acted {
        Ok(out) => {
            state.stats.timing.page += page_ms(&out);
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
                act_ms,
                text: text.map(str::to_string),
                outcome: "executed".into(),
                page_changed: Some(changed),
            });
            state.observation = next;
            state.text_cache = None;
            state.stale_streak = 0;
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
                act_ms,
                text: None,
                outcome: e.code.clone(),
                page_changed: None,
            });
            if recoverable {
                state.stats.stale += 1;
                state.stale_streak += 1;
                if state.stale_streak >= MAX_STALE_STREAK {
                    return Err(park(state.clone(), "needs_user", format!("The page keeps changing under \"{label}\"; it needs a look"), None));
                }
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

/// Text a criterion quotes (`The page shows "Business class"`) that the page
/// does not contain. A quote is a literal claim, so no model is asked; a
/// negated criterion (`does not show "Error"`) is left to the models.
fn missing_quote(criterion: &str, page_text: &str) -> Option<String> {
    let lower = criterion.to_lowercase();
    if [" not ", " no ", "without", "never", "n't "].iter().any(|n| lower.contains(n)) {
        return None;
    }
    let page = page_text.to_lowercase();
    let mut rest = criterion;
    while let Some(open) = rest.find(['"', '“']) {
        let after = &rest[open + rest[open..].chars().next().map(char::len_utf8).unwrap_or(1)..];
        let Some(close) = after.find(['"', '”']) else { break };
        let quoted = after[..close].trim();
        if !quoted.is_empty() && !page.contains(&quoted.to_lowercase()) {
            return Some(quoted.to_string());
        }
        rest = &after[close + after[close..].chars().next().map(char::len_utf8).unwrap_or(1)..];
    }
    None
}

/// Ask the decision model whether the page proves each criterion.
async fn verify_done(ports: &Ports, state: &mut TaskState, route: &DecisionRoute) -> (bool, Vec<Value>) {
    // The same page gives the same answer: asked twice (the decision model
    // unsure, then the LLM agreeing), it is checked once.
    let observation = state.observation.get("observation_id").and_then(Value::as_u64).unwrap_or(0);
    if let Some((on, verified, evidence)) = &state.checked {
        if *on == observation {
            return (*verified, evidence.clone());
        }
    }
    let checking = Instant::now();
    let (verified, evidence) = check_done(ports, state, route).await;
    state.stats.timing.verify += ms_since(checking);
    state.checked = Some((observation, verified, evidence.clone()));
    (verified, evidence)
}

async fn check_done(ports: &Ports, state: &mut TaskState, route: &DecisionRoute) -> (bool, Vec<Value>) {
    if state.criteria.is_none() {
        state.criteria = Some(if !state.spec.done_criteria.is_empty() {
            state.spec.done_criteria.clone()
        } else {
            let written = match state.criteria_job.take() {
                Some(job) => job.await,
                None => {
                    state.stats.llm_calls += 1;
                    llm_role::criteria(ports.llm.as_ref(), state.settings.fallback_model.as_deref(), &state.spec.goal).await
                }
            };
            if written.is_empty() { vec![state.spec.goal.clone()] } else { written }
        });
    }
    let criteria = state.criteria.clone().unwrap_or_default();
    let redact = matches!(route, DecisionRoute::Model { redact: true, .. });
    let text = text_of(&state.observation, 6000);

    // Rule first: a quoted text the page lacks fails its criterion outright.
    let unmet: Vec<Value> = criteria
        .iter()
        .filter_map(|c| missing_quote(c, &text).map(|q| json!({ "criterion": c, "verdict": false, "by": "rule", "missing": q })))
        .collect();
    if !unmet.is_empty() {
        return (false, unmet);
    }

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
    state.stats.llm_calls += 1;
    match llm_role::verify(ports.llm.as_ref(), state.settings.fallback_model.as_deref(), &criteria, &page).await {
        Ok(verdicts) => {
            let evidence = criteria.iter().zip(&verdicts).map(|(c, ok)| json!({ "criterion": c, "verdict": ok, "by": "llm" })).collect();
            (verdicts.iter().all(|ok| *ok), evidence)
        }
        Err(e) => (false, criteria.iter().map(|c| json!({ "criterion": c, "verdict": false, "by": "llm", "error": e })).collect()),
    }
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

/// Why a task was slow, in words for the person: without the decision model
/// every step is an LLM call — seconds where the model takes a fraction of one.
fn unavailable_note(model: Option<&str>, error: &str) -> String {
    let what = match model {
        Some(m) if error.contains("not installed") => format!("The decision model `{m}` is not installed (Settings → Decision)"),
        _ => format!("The decision model could not answer ({})", error.chars().take(160).collect::<String>()),
    };
    format!("{what}: an LLM chose every step instead, which takes seconds per step rather than a fraction of one.")
}

/// The loop proper.
async fn drive(ports: &Ports, mut state: TaskState) -> TaskOutcome {
    let max_steps = state.spec.max_steps.unwrap_or(state.settings.max_steps).clamp(1, 120);
    let _extension_run = ExtensionRun::start(state.spec.driver);
    loop {
        let (steps_before_run, decisions_before_run) = state.budget_base;
        if state.stats.steps - steps_before_run >= max_steps || state.stats.decisions - decisions_before_run >= max_steps * 2 {
            return park(state, "budget", format!("Stopped at the {max_steps}-step budget; resume to go on"), None);
        }
        if state.clock.0.elapsed() >= RUN_WALL_CLOCK {
            let minutes = RUN_WALL_CLOCK.as_secs() / 60;
            return park(state, "budget", format!("Stopped after {minutes} minutes; resume to go on"), None);
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
        if policy::is_own_api(&url) {
            return park(state, "blocked", "The browser reached SenClaw's own API, which is off limits".into(), None);
        }

        // Rule tier: a dialog blocks the page.
        if let Some(dialog) = obs.get("dialog").filter(|d| !d.is_null()) {
            // Only a known alert is accepted without the person.
            let kind = dialog.get("type").and_then(Value::as_str).unwrap_or("unknown").to_string();
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

        // Rule tier, continued: clicking a plain text field only focuses it.
        // When that changed nothing, the click was the decision model opening
        // the field to type into (the "open, then type" of comboboxes); asked
        // again, it would open it again.
        let focused = state.type_into.take().and_then(|node| fill_action(&obs, node));

        // Decision tier. Every pass counts against the budget, whichever tier answers.
        let route = policy::select_backend(&state.settings, &url, state.spec.driver);
        let decision_started = Instant::now();
        if focused.is_none() {
            state.stats.decisions += 1;
        }
        let mut model_step: Option<Step> = None;
        let mut guesses = Value::Null;
        let mut encoded_for_llm = None;
        match &route {
            _ if focused.is_some() => {}
            DecisionRoute::LlmOnly => {
                encoded_for_llm = Some(encode(&obs, &state.spec.goal, &state.history, Profile::JevFull, None, None));
            }
            DecisionRoute::Model { profile, backend, model, redact } => {
                let (mut view, _) = if *profile == Profile::LayaV3 { budget::prune(&obs, &state.spec.goal, state.option_chars) } else { (obs.clone(), 0) };
                if *redact {
                    redact_view(&mut view);
                }
                let bands = if *backend == Backend::Online { state.settings.bands_hosted } else { state.settings.bands_local };
                let history = if *redact { redacted_history(&state.history) } else { state.history.clone() };
                let encoded = encode(&view, &state.spec.goal, &history, *profile, model.clone(), Some(*backend));
                let asked = Instant::now();
                let answered = ports.decider.ask(&encoded.request).await;
                state.stats.timing.decide += ms_since(asked);
                match answered {
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
                    Err(e) => {
                        tracing::warn!("[browser] decision runtime unavailable: {e}");
                        if state.notes.is_empty() {
                            state.notes.push(unavailable_note(model.as_deref(), &e));
                        }
                    }
                }
                encoded_for_llm = Some(encode(&obs, &state.spec.goal, &state.history, Profile::JevFull, None, None));
            }
        }

        // An unsure DONE is checked against the page before an LLM is asked:
        // the check is what accepts a DONE whoever proposes it, and it takes a
        // fraction of the time.
        let unsure_done = model_step.as_ref().is_some_and(|s| s.operation == "DONE" && s.band != Band::Act) && !state.llm_next;
        let proven = unsure_done && verify_done(ports, &mut state, &route).await.0;

        // LLM tier when the model is unsure, said BLOCKED, or progress stalled.
        let needs_llm = match &model_step {
            None => true,
            Some(s) => !proven && (s.band != Band::Act || s.operation == "BLOCKED" || state.llm_next),
        };
        let (operation, target, action, by, confidence, band) = if let Some(field) = focused {
            ("TYPE_TEXT".to_string(), None, field, "rule", None, None)
        } else if needs_llm {
            let encoded_for_llm = encoded_for_llm.expect("every decision route encodes the page for the LLM");
            state.llm_next = false;
            state.stats.llm_calls += 1;
            state.stats.fallbacks += 1;
            let asked = Instant::now();
            let picked = llm_role::fallback(ports.llm.as_ref(), state.settings.fallback_model.as_deref(), &encoded_for_llm, &obs, &state.spec.goal, &state.history, &guesses).await;
            state.stats.timing.llm += ms_since(asked);
            match picked {
                Ok((op, target)) => {
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

        // Clicking something that is already on turns it off (a like, a
        // checkbox). When the page already proves the goal, that click would
        // undo it: the task is done, whatever the model wanted next.
        let already_on = operation == "CLICK" && action.get("checked").and_then(Value::as_str) == Some("true");
        let operation = if already_on && verify_done(ports, &mut state, &route).await.0 { "DONE".to_string() } else { operation };

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
                        let asked = Instant::now();
                        out.answer = llm_role::answer(ports.llm.as_ref(), state.settings.fallback_model.as_deref(), &q, &read).await.ok();
                        state.stats.timing.answer += ms_since(asked);
                    }
                    out.stats = Stats { elapsed_ms: elapsed_ms(&state), ..state.stats.clone() };
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
        let dialog_type = obs.pointer("/dialog/type").and_then(Value::as_str);
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
            Tier::Auto | Tier::Logged => {
                if let Some(reason) = model_raises_risk(ports, &mut state, &route, &obs, &operation, &action).await {
                    let pending = Pending {
                        approval_id: new_id("apv"),
                        observation_id,
                        action_id,
                        operation,
                        label: label.clone(),
                        kind,
                        text: None,
                        reject_action: None,
                    };
                    return park(state, "needs_approval", format!("Needs the person's approval: {reason}"), Some(pending));
                }
            }
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
                    let asked = Instant::now();
                    let written = llm_role::text_value(ports.llm.as_ref(), state.settings.text_model.as_deref(), &context).await;
                    state.stats.timing.text += ms_since(asked);
                    match written {
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

        // Going in circles: the same action from the same page state already
        // led back here (Search → empty results → New search → Search …).
        // The LLM gets the step; if it picks the same action, stop — a page
        // change alone is not progress.
        let key = cycle_key(&obs, &operation, &action, text.as_deref());
        if state.tried.contains(&key) {
            if by == "llm" {
                return park(state, "blocked", format!("Going in circles: \"{label}\" was already tried from this same page"), None);
            }
            state.history.push(HistoryItem {
                action: format!("{operation} \"{label}\" was already done from this same page and led back here"),
                kind: "rejected".into(),
                text: None,
                page_changed: Some(false),
            });
            state.llm_next = true;
            continue;
        }
        state.tried.push(key);

        let steps_before = state.stats.steps;
        if let Err(done) = execute(ports, &mut state, &operation, &action_id, &label, &kind, text.as_deref(), observation_id, by, confidence, band, decision_ms).await {
            return done;
        }
        if let Some(last) = state.steps.last_mut() {
            last.target = target;
        }
        if operation == "CLICK" && fingerprint_unfocused(&obs) == fingerprint_unfocused(&state.observation) {
            state.type_into = action.get("node").and_then(Value::as_i64).filter(|node| fill_action(&obs, *node).is_some());
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
        /// The chat that opened tab t1, as the runtime records it.
        pub owner: std::sync::Mutex<Option<String>>,
    }

    impl FakeBrowser {
        pub fn new(responsive: bool) -> Arc<FakeBrowser> {
            Arc::new(FakeBrowser {
                clicked: std::sync::Mutex::new(false),
                seq: AtomicU64::new(0),
                responsive,
                owner: std::sync::Mutex::new(None),
            })
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
        async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, PortError> {
            if path == "/v1/sessions" && method == "GET" {
                let owner = self.owner.lock().unwrap().clone();
                let tabs: Vec<Value> = owner.into_iter().map(|o| json!({ "id": "t1", "owner": o, "closed": false })).collect();
                return Ok(json!({ "sessions": [{ "id": "s1", "tabs": tabs }] }));
            }
            if path == "/v1/sessions" {
                return Ok(json!({ "id": "s1" }));
            }
            if path.ends_with("/tabs") {
                *self.owner.lock().unwrap() = body.and_then(|b| b["owner"].as_str().map(str::to_string));
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

    /// Search leads to an empty results page whose only way on is "New
    /// search", which leads back to the identical start page.
    struct CycleSite {
        on_results: std::sync::Mutex<bool>,
        seq: AtomicU64,
    }

    impl CycleSite {
        fn page(&self) -> Value {
            let id = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
            let (url, text, label) = if *self.on_results.lock().unwrap() {
                ("https://shop.test/results", "Enter a search term", "New search")
            } else {
                ("https://shop.test/", "Find books", "Search")
            };
            json!({
                "observation_id": id, "tab_id": "t1", "url": url, "title": "Shop", "text": text,
                "actions": [{"id": "e1", "node": 1, "kind": "click", "role": "button", "label": label, "value": ""}],
                "dialog": null, "viewport": {"scroll_y": 0}
            })
        }
    }

    #[async_trait]
    impl BrowserPort for CycleSite {
        async fn call(&self, _method: &str, path: &str, _body: Option<Value>) -> Result<Value, PortError> {
            if path == "/v1/sessions" {
                return Ok(json!({ "id": "s1" }));
            }
            if path.ends_with("/tabs") {
                return Ok(json!({ "tab": { "id": "t1" }, "observation": self.page() }));
            }
            if path.ends_with("/act") {
                let mut on = self.on_results.lock().unwrap();
                *on = !*on;
                drop(on);
                return Ok(json!({ "executed": "e1", "observation": self.page() }));
            }
            Ok(self.page())
        }
    }

    /// Confidently clicks whatever the page offers.
    struct ClickDecider;

    #[async_trait]
    impl Decider for ClickDecider {
        async fn ask(&self, request: &AskRequest) -> Result<Value, String> {
            let ops: Vec<String> = request.questions.get("operation").and_then(|q| q.get("criteria")).and_then(|c| c.as_object())
                .map(|o| o.iter().map(|(k, _)| k.clone()).collect()).unwrap_or_default();
            let ops: Vec<&str> = ops.iter().map(String::as_str).collect();
            Ok(json!({ "operation": choice(&ops, "CLICK", 0.97), "click_target": choice(&["1"], "1", 1.0) }))
        }
    }

    /// An LLM that, asked for a better step, picks the same click again.
    struct SameClickLlm;

    #[async_trait]
    impl Llm for SameClickLlm {
        async fn complete(&self, _m: Option<&str>, _s: &str, _u: &str, _t: u32) -> Result<String, String> {
            Ok(r#"{"operation":"CLICK","target":"1","reason":"try again"}"#.into())
        }
    }

    #[tokio::test]
    async fn loop_leaves_a_cycle_to_the_llm_and_stops_when_it_repeats() {
        let site = Arc::new(CycleSite { on_results: std::sync::Mutex::new(false), seq: AtomicU64::new(0) });
        let ports = Ports { browser: site, decider: Arc::new(ClickDecider), llm: Arc::new(SameClickLlm) };
        let out = start(&ports, BrowserSettings::default(), spec("Search for books")).await;
        assert_eq!(out.status, "blocked", "{}", out.message);
        assert!(out.message.contains("circles"), "{}", out.message);
        // Search and New search ran once each; the repeat was never executed.
        assert_eq!(out.stats.steps, 2);
        assert_eq!(out.stats.llm_calls, 1, "only the repeat went to the LLM: {:?}", out.steps);
        assert!(out.steps.iter().all(|s| s.by == "jev"));
    }

    /// A search box: clicking into it changes nothing, typing fills it, and
    /// Search shows results for what was typed.
    struct SearchBox {
        typed: std::sync::Mutex<Option<String>>,
        searched: std::sync::Mutex<bool>,
        seq: AtomicU64,
    }

    impl SearchBox {
        fn page(&self) -> Value {
            let id = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
            let typed = self.typed.lock().unwrap().clone().unwrap_or_default();
            if *self.searched.lock().unwrap() {
                return json!({
                    "observation_id": id, "tab_id": "t1", "url": format!("https://shop.test/results?q={typed}"),
                    "title": "Shop", "text": format!("Results for {typed}: 3 found"), "actions": [],
                    "dialog": null, "viewport": {"scroll_y": 0}
                });
            }
            json!({
                "observation_id": id, "tab_id": "t1", "url": "https://shop.test/", "title": "Shop", "text": "Find books",
                "actions": [
                    {"id": "e1", "node": 1, "kind": "fill", "role": "searchbox", "label": "Query", "value": typed},
                    {"id": "e2", "node": 1, "kind": "click", "role": "searchbox", "label": "Open Query", "value": typed},
                    {"id": "e3", "node": 2, "kind": "click", "role": "button", "label": "Search", "value": ""}
                ],
                "dialog": null, "viewport": {"scroll_y": 0}
            })
        }
    }

    #[async_trait]
    impl BrowserPort for SearchBox {
        async fn call(&self, _method: &str, path: &str, body: Option<Value>) -> Result<Value, PortError> {
            if path == "/v1/sessions" {
                return Ok(json!({ "id": "s1" }));
            }
            if path.ends_with("/tabs") {
                return Ok(json!({ "tab": { "id": "t1" }, "observation": self.page() }));
            }
            if path.ends_with("/act") {
                let body = body.unwrap_or_default();
                match body["action_id"].as_str() {
                    Some("e1") => *self.typed.lock().unwrap() = body["text"].as_str().map(str::to_string),
                    Some("e3") => *self.searched.lock().unwrap() = true,
                    _ => {}
                }
                return Ok(json!({ "executed": body["action_id"], "observation": self.page() }));
            }
            Ok(self.page())
        }
    }

    /// Clicks into an empty query box (the "open, then type" habit), searches
    /// once it holds text, and says DONE on results.
    struct OpenThenSearch;

    #[async_trait]
    impl Decider for OpenThenSearch {
        async fn ask(&self, request: &AskRequest) -> Result<Value, String> {
            if request.questions.get("effect").is_some() {
                return Ok(json!({ "effect": choice(&["view", "adjust", "commit"], "view", 0.96) }));
            }
            let text = serde_json::to_string(&request.state).unwrap();
            if request.questions.get("c1").is_some() {
                let ok = text.contains("Results for books");
                return Ok(json!({ "c1": { "type": "noul", "noul": if ok { 0.95 } else { 0.05 }, "confidence": 0.9 } }));
            }
            let ops: Vec<String> = request.questions.get("operation").and_then(|q| q.get("criteria")).and_then(|c| c.as_object())
                .map(|o| o.iter().map(|(k, _)| k.clone()).collect()).unwrap_or_default();
            let ops: Vec<&str> = ops.iter().map(String::as_str).collect();
            if text.contains("Results") {
                return Ok(json!({ "operation": choice(&ops, "DONE", 0.95) }));
            }
            let clicks: Vec<String> = request.questions.get("click_target").and_then(|q| q.get("criteria")).and_then(|c| c.as_object())
                .map(|o| o.iter().map(|(k, _)| k.clone()).collect()).unwrap_or_default();
            let clicks: Vec<&str> = clicks.iter().map(String::as_str).collect();
            let criteria = serde_json::to_string(&request.questions).unwrap();
            let target = if criteria.contains("= 'books'") { clicks[1] } else { clicks[0] };
            Ok(json!({ "operation": choice(&ops, "CLICK", 0.95), "click_target": choice(&clicks, target, 0.9) }))
        }
    }

    struct WritesBooks;

    #[async_trait]
    impl Llm for WritesBooks {
        async fn complete(&self, _m: Option<&str>, _s: &str, _u: &str, _t: u32) -> Result<String, String> {
            Ok(r#"{"text":"books"}"#.into())
        }
    }

    #[tokio::test]
    async fn clicking_into_a_text_field_leads_to_typing_in_it() {
        let site = Arc::new(SearchBox { typed: Default::default(), searched: Default::default(), seq: AtomicU64::new(0) });
        let ports = Ports { browser: site, decider: Arc::new(OpenThenSearch), llm: Arc::new(WritesBooks) };
        let mut s = spec("Search for books");
        s.done_criteria = vec!["Results for books are shown".into()];
        let out = start(&ports, BrowserSettings::default(), s).await;
        assert_eq!(out.status, "done", "{}: {:?}", out.message, out.steps);
        let ops: Vec<(&str, &str)> = out.steps.iter().map(|s| (s.operation.as_str(), s.by)).collect();
        assert_eq!(ops, [("CLICK", "jev"), ("TYPE_TEXT", "rule"), ("CLICK", "jev")]);
        assert_eq!(out.steps[1].text.as_deref(), Some("books"));
    }

    #[test]
    fn quoted_criteria_are_checked_literally() {
        let page = "3 flights from Zurich to London · Economy class";
        assert_eq!(missing_quote("The flights shown are \"Business class\"", page).as_deref(), Some("Business class"));
        assert_eq!(missing_quote("The page lists \"London\" flights", page), None);
        assert_eq!(missing_quote("Shows “economy CLASS”", page), None, "case-insensitive, curly quotes");
        assert_eq!(missing_quote("The page does not show \"Error\"", page), None, "negations go to the models");
        assert_eq!(missing_quote("Business class flights are shown", page), None, "nothing quoted");
    }

    /// The same decisions, but the model judges the click irreversible.
    pub struct SeesRisk;

    #[async_trait]
    impl Decider for SeesRisk {
        async fn ask(&self, request: &AskRequest) -> Result<Value, String> {
            if request.questions.get("effect").is_some() {
                return Ok(json!({ "effect": choice(&["view", "adjust", "commit"], "commit", 0.9) }));
            }
            FakeDecider.ask(request).await
        }
    }

    #[tokio::test]
    async fn a_decision_model_can_add_a_pause_but_never_remove_one() {
        let ports = Ports { browser: FakeBrowser::new(true), decider: Arc::new(SeesRisk), llm: Arc::new(NoLlm) };
        let mut task = spec("Search for books");
        task.owner = "risk-chat".into();
        let out = start(&ports, BrowserSettings::default(), task).await;
        assert_eq!(out.status, "needs_approval", "{}", out.message);
        assert!(out.message.contains("decision model"), "{}", out.message);
        let id = out.pending.as_ref().and_then(|p| p["approval_id"].as_str()).unwrap().to_string();
        let listed = pending_approvals();
        assert!(listed.iter().any(|a| a["approval_id"] == id.as_str() && a["action"] == "Search"), "{listed:?}");
        // Declining ends the wait and leaves nothing pending.
        let declined = approve(&ports, &id, false).await.expect("known approval");
        assert_ne!(declined.status, "needs_approval");
        assert!(!pending_approvals().iter().any(|a| a["approval_id"] == id.as_str()));
    }

    /// Every click lands on an overlay, and a live counter changes the text each time.
    struct CoveredSite {
        seq: AtomicU64,
    }

    impl CoveredSite {
        fn page(&self) -> Value {
            let id = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
            json!({
                "observation_id": id, "tab_id": "t1", "url": "https://shop.test/", "title": "Shop",
                "text": format!("Find books · {id} people are looking at this"),
                "actions": [{"id": "e1", "node": 1, "kind": "click", "role": "button", "label": "Search", "value": ""}],
                "dialog": null, "viewport": {"scroll_y": 0}
            })
        }
    }

    #[async_trait]
    impl BrowserPort for CoveredSite {
        async fn call(&self, _method: &str, path: &str, _body: Option<Value>) -> Result<Value, PortError> {
            if path == "/v1/sessions" {
                return Ok(json!({ "id": "s1" }));
            }
            if path.ends_with("/tabs") {
                return Ok(json!({ "tab": { "id": "t1" }, "observation": self.page() }));
            }
            if path.ends_with("/act") {
                return Err(PortError::new(409, "target_covered", "something covers the target"));
            }
            Ok(self.page())
        }
    }

    /// With no decision model the LLM picks every step; a page that refuses
    /// every input must still end the task, with a bounded number of calls.
    #[tokio::test]
    async fn an_llm_only_loop_on_a_page_that_refuses_input_stops() {
        let ports = Ports { browser: Arc::new(CoveredSite { seq: AtomicU64::new(0) }), decider: Arc::new(FakeDecider), llm: Arc::new(SameClickLlm) };
        let settings = BrowserSettings { decision_backend: crate::browser_agent::settings::DecisionBackend::LlmOnly, ..BrowserSettings::default() };
        let out = start(&ports, settings, spec("Search for books")).await;
        assert_eq!(out.status, "needs_user", "{}", out.message);
        assert!(out.stats.llm_calls <= 6, "bounded ({} LLM calls)", out.stats.llm_calls);
        assert_eq!(out.stats.steps, 0, "nothing was executed");
    }

    /// An LLM that does each of its jobs the obvious way and notes which it was asked for.
    struct Scripted {
        asked: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Scripted {
        fn new() -> (Arc<Scripted>, Arc<std::sync::Mutex<Vec<String>>>) {
            let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
            (Arc::new(Scripted { asked: asked.clone() }), asked)
        }
    }

    #[async_trait]
    impl Llm for Scripted {
        async fn complete(&self, _m: Option<&str>, system: &str, user: &str, _t: u32) -> Result<String, String> {
            let (job, answer) = if system.starts_with("List the concrete") {
                ("criteria", r#"{"criteria":["Search results are shown"]}"#.to_string())
            } else if system.contains("\"satisfied\"") {
                let asked: Value = serde_json::from_str(user).unwrap();
                let verdicts = vec![user.contains("Results"); asked["criteria"].as_array().unwrap().len()];
                ("verify", json!({ "satisfied": verdicts }).to_string())
            } else if user.contains("Results for books") {
                ("step", r#"{"operation":"DONE","target":null}"#.to_string())
            } else {
                ("step", r#"{"operation":"CLICK","target":"1"}"#.to_string())
            };
            self.asked.lock().unwrap().push(job.to_string());
            Ok(answer)
        }
    }

    /// The fake shop behind a real wait on every call — so what was started
    /// alongside the task gets its turn — noting each click.
    struct Waiting {
        shop: Arc<FakeBrowser>,
        log: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl BrowserPort for Waiting {
        async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, PortError> {
            tokio::task::yield_now().await;
            if path.ends_with("/act") {
                self.log.lock().unwrap().push("click".to_string());
            }
            self.shop.call(method, path, body).await
        }
    }

    /// With no criteria given an LLM writes them — while the browser works,
    /// not as one more call between the last step and the answer.
    #[tokio::test]
    async fn criteria_are_written_while_the_task_runs() {
        let (llm, log) = Scripted::new();
        let ports = Ports { browser: Arc::new(Waiting { shop: FakeBrowser::new(true), log: log.clone() }), decider: Arc::new(FakeDecider), llm };
        let mut task = spec("Search for books");
        task.done_criteria.clear();
        let out = start(&ports, BrowserSettings::default(), task).await;
        assert_eq!(out.status, "done", "{}", out.message);
        assert_eq!(*log.lock().unwrap(), ["criteria", "click"], "the criteria were asked for before the first click");
        assert_eq!(out.stats.llm_calls, 1);
        assert_eq!(out.evidence[0]["criterion"], "Search results are shown");
    }

    /// A post whose like is already on; clicking it turns it off again.
    struct LikedPost {
        liked: std::sync::Mutex<bool>,
        seq: AtomicU64,
    }

    impl LikedPost {
        fn page(&self) -> Value {
            let id = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
            let liked = *self.liked.lock().unwrap();
            json!({
                "observation_id": id, "tab_id": "t1", "url": "https://clips.test/", "title": "Clips",
                "text": if liked { "Morning market\nYou liked this clip" } else { "Morning market" },
                "actions": [{"id": "e1", "node": 1, "kind": "click", "role": "button", "label": "Like clip", "value": "", "checked": liked.to_string()}],
                "dialog": null, "viewport": {"scroll_y": 0}
            })
        }
    }

    #[async_trait]
    impl BrowserPort for LikedPost {
        async fn call(&self, _method: &str, path: &str, _body: Option<Value>) -> Result<Value, PortError> {
            if path == "/v1/sessions" {
                return Ok(json!({ "id": "s1" }));
            }
            if path.ends_with("/tabs") {
                return Ok(json!({ "tab": { "id": "t1" }, "observation": self.page() }));
            }
            if path.ends_with("/act") {
                let mut liked = self.liked.lock().unwrap();
                *liked = !*liked;
                drop(liked);
                return Ok(json!({ "executed": "e1", "observation": self.page() }));
            }
            Ok(self.page())
        }
    }

    /// Clicks the like on every page; says the goal is met when the page says so.
    struct AlwaysLikes;

    #[async_trait]
    impl Decider for AlwaysLikes {
        async fn ask(&self, request: &AskRequest) -> Result<Value, String> {
            let text = serde_json::to_string(&request.state).unwrap();
            if request.questions.get("effect").is_some() {
                return Ok(json!({ "effect": choice(&["view", "adjust", "commit"], "adjust", 0.9) }));
            }
            if request.questions.get("c1").is_some() {
                let liked = text.contains("You liked this clip");
                return Ok(json!({ "c1": { "type": "noul", "noul": if liked { 0.95 } else { 0.05 }, "confidence": 0.9 } }));
            }
            ClickDecider.ask(request).await
        }
    }

    /// A model asked to like a clip clicks its like button — also when the
    /// like is already on, which would take it back. The page already proves
    /// the goal then, and the click is not made.
    #[tokio::test]
    async fn a_toggle_that_already_satisfies_the_goal_is_not_clicked_off() {
        let site = Arc::new(LikedPost { liked: std::sync::Mutex::new(true), seq: AtomicU64::new(0) });
        let ports = Ports { browser: site.clone(), decider: Arc::new(AlwaysLikes), llm: Arc::new(NoLlm) };
        let mut task = spec("Like the clip");
        task.done_criteria = vec!["The clip is liked".into()];
        let out = start(&ports, BrowserSettings::default(), task.clone()).await;
        assert_eq!(out.status, "done", "{}", out.message);
        assert_eq!(out.stats.steps, 0, "nothing was clicked: {:?}", out.steps);
        assert!(*site.liked.lock().unwrap(), "the like is still on");

        // Not liked yet: the click is the step to take, then the task is done.
        let site = Arc::new(LikedPost { liked: std::sync::Mutex::new(false), seq: AtomicU64::new(0) });
        let ports = Ports { browser: site.clone(), decider: Arc::new(AlwaysLikes), llm: Arc::new(NoLlm) };
        let out = start(&ports, BrowserSettings::default(), task).await;
        assert_eq!((out.status.as_str(), out.stats.steps), ("done", 1), "{}: {:?}", out.message, out.steps);
        assert!(*site.liked.lock().unwrap());
    }

    /// Decides like the fake, and notes which checkpoints it was asked to have ready.
    struct Warmed {
        log: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Decider for Warmed {
        async fn ask(&self, request: &AskRequest) -> Result<Value, String> {
            FakeDecider.ask(request).await
        }

        async fn warm(&self, model: Option<&str>) {
            self.log.lock().unwrap().push(format!("warm {}", model.unwrap_or("default")));
        }
    }

    /// A checkpoint unloaded while idle takes a second or more to come back:
    /// that happens while the page opens, not on the first step after it.
    #[tokio::test]
    async fn decision_checkpoints_load_while_the_page_opens() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ports = |log: &Arc<std::sync::Mutex<Vec<String>>>| Ports {
            browser: Arc::new(Waiting { shop: FakeBrowser::new(true), log: log.clone() }),
            decider: Arc::new(Warmed { log: log.clone() }),
            llm: Arc::new(NoLlm),
        };
        let out = start(&ports(&log), BrowserSettings::default(), spec("Search for books")).await;
        assert_eq!(out.status, "done", "{}", out.message);
        let seen = log.lock().unwrap().clone();
        let click = seen.iter().position(|e| e == "click").expect("the task clicked");
        assert_eq!(seen[..click], ["warm laya-browser", "warm default"], "asked for before the first step, one at a time: {seen:?}");
        assert_eq!(out.stats.decisions, 4, "warming is not a decision");

        // With the LLM picking every step there is no checkpoint to load.
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let settings = BrowserSettings { decision_backend: crate::browser_agent::settings::DecisionBackend::LlmOnly, ..BrowserSettings::default() };
        start(&ports(&log), settings, spec("Search for books")).await;
        assert!(!log.lock().unwrap().iter().any(|e| e.starts_with("warm")), "{:?}", log.lock().unwrap());
    }

    /// Says DONE on every page, never sure of it; counts the checks it is asked for.
    struct UnsureDone {
        checks: AtomicU64,
    }

    #[async_trait]
    impl Decider for UnsureDone {
        async fn ask(&self, request: &AskRequest) -> Result<Value, String> {
            if request.questions.get("c1").is_some() {
                self.checks.fetch_add(1, Ordering::SeqCst);
                return FakeDecider.ask(request).await;
            }
            let ops: Vec<String> = request.questions.get("operation").and_then(|q| q.get("criteria")).and_then(|c| c.as_object())
                .map(|o| o.iter().map(|(k, _)| k.clone()).collect()).unwrap_or_default();
            let ops: Vec<&str> = ops.iter().map(String::as_str).collect();
            Ok(json!({ "operation": choice(&ops, "DONE", 0.4), "click_target": choice(&["1"], "1", 1.0) }))
        }
    }

    /// The check is what accepts a DONE, whoever proposes it: when the page
    /// already proves the goal, an unsure decision model needs no LLM to agree.
    #[tokio::test]
    async fn an_unsure_done_is_checked_before_an_llm_is_asked() {
        // A page that already shows the results.
        let shop = FakeBrowser::new(true);
        *shop.clicked.lock().unwrap() = true;
        let (llm, asked) = Scripted::new();
        let ports = Ports { browser: shop, decider: Arc::new(UnsureDone { checks: AtomicU64::new(0) }), llm };
        let out = start(&ports, BrowserSettings::default(), spec("Search for books")).await;
        assert_eq!(out.status, "done", "{}", out.message);
        assert!(asked.lock().unwrap().is_empty(), "no LLM call: {:?}", asked.lock().unwrap());
        assert_eq!(out.stats.fallbacks, 0);

        // A page that does not: the LLM is asked, and the page is checked once, not per opinion.
        let decider = Arc::new(UnsureDone { checks: AtomicU64::new(0) });
        let (llm, asked) = Scripted::new();
        let ports = Ports { browser: FakeBrowser::new(false), decider: decider.clone(), llm };
        let mut task = spec("Search for books");
        task.max_steps = Some(1);
        let out = start(&ports, BrowserSettings::default(), task).await;
        assert_ne!(out.status, "done", "{}", out.message);
        assert!(asked.lock().unwrap().iter().any(|job| job == "step"), "the LLM got the step");
        assert_eq!(decider.checks.load(Ordering::SeqCst), 1, "one page, one check");
    }

    /// A decision runtime whose checkpoint was never installed.
    struct NotInstalled;

    #[async_trait]
    impl Decider for NotInstalled {
        async fn ask(&self, _request: &AskRequest) -> Result<Value, String> {
            Err("the decision runtime answered 404 Not Found: {\"error\":\"model `laya-browser` is not installed\"}".into())
        }
    }

    /// Without its decision model the loop still works — every step an LLM
    /// call — and says so, instead of just being slow.
    #[tokio::test]
    async fn a_missing_decision_model_is_named_in_the_outcome() {
        let (llm, asked) = Scripted::new();
        let without_model = Ports { browser: FakeBrowser::new(true), decider: Arc::new(NotInstalled), llm };
        let mut task = spec("Search for books");
        task.done_criteria = vec!["Search results are shown".into(), "The count of results is shown".into()];
        let out = start(&without_model, BrowserSettings::default(), task).await;
        assert_eq!(out.status, "done", "{}", out.message);
        assert!(out.steps.iter().all(|s| s.by == "llm"), "{:?}", out.steps);
        assert_eq!(out.notes.len(), 1, "said once: {:?}", out.notes);
        assert!(out.notes[0].contains("`laya-browser` is not installed") && out.notes[0].contains("Settings"), "{}", out.notes[0]);
        // Both criteria went to the LLM in one call.
        assert_eq!(asked.lock().unwrap().iter().filter(|job| *job == "verify").count(), 1);
        assert_eq!(out.evidence.len(), 2);
        assert!(serde_json::to_value(&out).unwrap()["notes"].is_array());
        // A task that ran on its decision model has nothing to say.
        let out = start(&ports(FakeBrowser::new(true)), BrowserSettings::default(), spec("Search for books")).await;
        assert!(serde_json::to_value(&out).unwrap().get("notes").is_none());
    }

    /// The outcome says where the time went.
    #[tokio::test]
    async fn the_outcome_accounts_for_its_time() {
        let out = start(&ports(FakeBrowser::new(true)), BrowserSettings::default(), spec("Search for books")).await;
        let json = serde_json::to_value(&out).unwrap();
        for part in ["open", "decide", "llm", "risk", "text", "act", "page", "verify", "answer"] {
            assert!(json["stats"]["timing"][part].is_u64(), "{part} missing from {}", json["stats"]["timing"]);
        }
        assert!(json["steps"][0]["act_ms"].is_u64());
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
