//! REST surface for background tasks. See `docs/background-tasks-design.md` §9.
//!
//! The permission model here is the same one the MCP server enforces: read and
//! operate are global (the user must be able to see and stop *everything*
//! running in the background, core upkeep included), while authoring and
//! editing are scoped by what owns the task — an App's config lives in its
//! manifest and would be reverted by a reinstall, and a native job's body is
//! Rust.

use std::sync::Arc;

use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use uuid::Uuid;

use super::core::{AppError, UiState};
use crate::db::background::BackgroundTaskFilter;
use crate::db::Db;
use crate::types::{
    BackgroundContinuity, BackgroundJobKind, BackgroundOwnerKind, BackgroundPromptKind,
    BackgroundTask, BackgroundTaskStatus, BackgroundTrigger, BackgroundVisibility, OverlapPolicy,
};

fn internal(e: impl std::fmt::Display) -> AppError {
    AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn bad(msg: impl std::fmt::Display) -> AppError {
    AppError(StatusCode::BAD_REQUEST, msg.to_string())
}

fn db(s: &UiState) -> Result<&Arc<Db>, AppError> {
    s.db.as_ref()
        .ok_or_else(|| internal("database not available"))
}

fn scheduler(s: &UiState) -> Result<&Arc<crate::background::BackgroundScheduler>, AppError> {
    s.background_scheduler
        .as_ref()
        .ok_or_else(|| internal("background scheduler not running"))
}

fn task_or_404(s: &UiState, id: &str) -> Result<BackgroundTask, AppError> {
    db(s)?
        .get_background_task(id)
        .map_err(internal)?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("no such task: {id}")))
}

// ─── Wire shapes ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct ListQuery {
    owner_kind: Option<String>,
    owner_id: Option<String>,
    status: Option<String>,
    #[serde(default)]
    include_internal: bool,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateBody {
    title: String,
    prompt: String,
    trigger_type: String,
    trigger_value: Option<String>,
    description: Option<String>,
    prompt_kind: Option<String>,
    context_url: Option<String>,
    persona: Option<String>,
    agent_folder: Option<String>,
    workspace_dir: Option<String>,
    #[serde(default)]
    tools: Vec<String>,
    mcp: Option<serde_json::Value>,
    model_id: Option<String>,
    max_turns: Option<i64>,
    timeout_secs: Option<i64>,
    continuity: Option<String>,
    overlap_policy: Option<String>,
    #[serde(default)]
    catch_up: bool,
    max_failures: Option<i64>,
    /// Start paused. The UI sets this for outward-facing tasks.
    #[serde(default)]
    paused: bool,
    /// Deliver an OS notification instead of running an agent ("nhắc/thông báo").
    #[serde(default)]
    notify: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateBody {
    title: Option<String>,
    description: Option<String>,
    prompt: Option<String>,
    prompt_kind: Option<String>,
    context_url: Option<String>,
    persona: Option<String>,
    tools: Option<Vec<String>>,
    model_id: Option<String>,
    max_turns: Option<i64>,
    timeout_secs: Option<i64>,
    continuity: Option<String>,
    overlap_policy: Option<String>,
    catch_up: Option<bool>,
    max_failures: Option<i64>,
    trigger_type: Option<String>,
    trigger_value: Option<String>,
    notify: Option<bool>,
    /// `active` | `paused`. Other transitions are the scheduler's business.
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RunsQuery {
    limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StatsQuery {
    window: Option<String>,
    owner_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RunQuery {
    #[serde(default = "yes")]
    include_activity: bool,
}

fn yes() -> bool {
    true
}

// ─── Quick task: natural language → task spec (AI) ────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct ParseBody {
    text: String,
}

/// Turn a one-line description ("mỗi sáng 9h dọn tri thức") into a task spec.
///
/// Returns the create-body fields for the UI to review and submit — it does NOT
/// create the task. Review matters: a background task runs unattended, and a
/// cron the model got wrong is invisible until it fires (or doesn't).
pub(crate) async fn parse_quick(
    State(s): State<Arc<UiState>>,
    Json(b): Json<ParseBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let text = b.text.trim();
    if text.is_empty() {
        return Err(bad("nhập mô tả task đã"));
    }

    // The model needs "now" to resolve relative times like "2 phút nữa" into an
    // absolute RFC3339 for a `once` trigger.
    let now_local = chrono::Local::now();
    let system = format!(
        "Bạn chuyển một câu tiếng Việt mô tả công việc tự động thành JSON cho một \
BACKGROUND TASK (chạy ngầm, KHÔNG trả lời ai). Chỉ trả JSON, không thêm chữ nào.\n\
Bây giờ là: {now_readable} (giờ địa phương, ISO {now_iso}).\n\
Dạng JSON:\n\
{{\"title\": string ngắn ≤60 ký tự,\n\
  \"prompt\": string — chỉ dẫn tự chứa, nói rõ việc cần làm và khi nào coi là xong,\n\
  \"trigger_type\": \"cron\" | \"interval\" | \"once\" | \"manual\",\n\
  \"trigger_value\": với cron là biểu thức 5 trường (giờ địa phương) vd \"0 9 * * *\"; \
với interval là mili-giây (số); với once là mốc ISO-8601 tuyệt đối; manual thì bỏ trống,\n\
  \"prompt_kind\": \"static\" (mặc định),\n\
  \"continuity\": \"thread\" nếu việc đụng tới người (nhắn khách…) để khỏi lặp, ngược lại \"fresh\",\n\
  \"notify\": true nếu việc CHỈ là thông báo/nhắc người dùng (\"nhắc tôi…\", \"thông báo…\", \
\"báo tôi…\"). Khi notify=true thì prompt chính là NỘI DUNG thông báo (không phải chỉ dẫn)}}\n\
Quy tắc: \"mỗi sáng 9h\"→cron \"0 9 * * *\"; \"mỗi 30 phút\"→interval 1800000; \
\"2 phút nữa\"/\"sau 2 phút\"→once, tính mốc ISO từ giờ hiện tại.",
        now_readable = now_local.format("%A %d/%m/%Y %H:%M"),
        now_iso = now_local.to_rfc3339(),
    );

    let r = crate::gateway::ui_server::llm_config::chat_completion(
        &s.config.paths.global_config_path,
        None,
        &system,
        text,
        500,
        None,
    )
    .await
    .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e))?;
    crate::gateway::ui_server::llm_config::record_completion(
        &s.usage_recorder,
        "web:background-draft",
        "",
        &r,
    );
    let answer = r.text;

    let spec = extract_json_object(&answer).ok_or_else(|| {
        // Show a preview so a misbehaving model is debuggable rather than opaque.
        let preview: String = answer.chars().take(160).collect();
        AppError(
            StatusCode::BAD_GATEWAY,
            format!("AI không trả về JSON đọc được. Model trả: {preview}"),
        )
    })?;

    // Normalize + validate the model's output so the UI always gets a usable
    // draft, even when the model is sloppy.
    let get = |k: &str| {
        spec.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let prompt = get("prompt").unwrap_or(text).to_owned();
    let title = get("title")
        .map(|t| t.chars().take(60).collect::<String>())
        .unwrap_or_else(|| {
            prompt
                .lines()
                .next()
                .unwrap_or("Task nền")
                .chars()
                .take(60)
                .collect()
        });
    let trigger_type = get("trigger_type").unwrap_or("manual").to_owned();
    // trigger_value may arrive as a string OR (for interval) a number.
    let trigger_value = spec
        .get("trigger_value")
        .map(|v| match v {
            serde_json::Value::String(s) => s.trim().to_owned(),
            serde_json::Value::Number(n) => n.to_string(),
            _ => String::new(),
        })
        .filter(|s| !s.is_empty());

    let notify = spec
        .get("notify")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    Ok(Json(serde_json::json!({
        "title": title,
        "prompt": prompt,
        "trigger_type": trigger_type,
        "trigger_value": trigger_value,
        "prompt_kind": get("prompt_kind").unwrap_or("static"),
        "continuity": get("continuity").unwrap_or("fresh"),
        "notify": notify,
    })))
}

/// Pull the JSON object out of an LLM reply.
///
/// The naive `first{`..`last}` slice breaks on the two things local models
/// actually do: wrap the answer in `<think>…</think>` (which contains its own
/// braces, so the slice spans reasoning + JSON and never parses) and add prose
/// around a ```json fence. So: strip think blocks, then scan for every balanced
/// `{…}` span (respecting strings/escapes) and keep the last one that parses as
/// an object — the JSON almost always comes after the reasoning.
fn extract_json_object(raw: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    let cleaned = strip_think_blocks(raw);
    let bytes = cleaned.as_bytes();
    let mut best: Option<serde_json::Map<String, serde_json::Value>> = None;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = matching_brace(&bytes[i..]) {
                if let Ok(serde_json::Value::Object(m)) =
                    serde_json::from_str::<serde_json::Value>(&cleaned[i..=i + end])
                {
                    best = Some(m); // keep the last valid object
                    i += end + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    best
}

/// Byte offset (relative to `bytes[0] == '{'`) of the `}` that closes it,
/// tracking string state so a brace inside a JSON string doesn't miscount.
fn matching_brace(bytes: &[u8]) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Remove `<think>…</think>` / `<thinking>…</thinking>` reasoning blocks that
/// local models (Qwen3, R1, …) emit before the answer. Single pass over both tag
/// families, handling repeats and an unclosed (truncated) final block.
fn strip_think_blocks(raw: &str) -> String {
    const TAGS: [(&str, &str); 2] = [("<think>", "</think>"), ("<thinking>", "</thinking>")];
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    loop {
        // The earliest opening tag of either family in what's left.
        let next = TAGS
            .iter()
            .filter_map(|&(o, c)| rest.find(o).map(|i| (i, o, c)))
            .min_by_key(|&(i, _, _)| i);
        let Some((start, open, close)) = next else {
            break;
        };
        out.push_str(&rest[..start]);
        match rest[start + open.len()..].find(close) {
            Some(rel) => rest = &rest[start + open.len() + rel + close.len()..],
            None => {
                rest = ""; // unclosed block — drop the truncated tail
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

// ─── Handlers ─────────────────────────────────────────────────────────────────

pub(crate) async fn list(
    State(s): State<Arc<UiState>>,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let filter = BackgroundTaskFilter {
        owner_kind: q.owner_kind,
        owner_id: q.owner_id,
        status: q.status,
        include_internal: q.include_internal,
        limit: q.limit.map(|l| l.clamp(1, 500)),
        offset: q.offset.map(|o| o.max(0)),
    };
    let d = db(&s)?;
    // `total` is the unpaged count, so the pager knows how many pages exist.
    let total = d.count_background_tasks(&filter).map_err(internal)?;
    let tasks = d.list_background_tasks(&filter).map_err(internal)?;
    Ok(Json(serde_json::json!({ "tasks": tasks, "total": total })))
}

pub(crate) async fn create(
    State(s): State<Arc<UiState>>,
    Json(b): Json<CreateBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let d = db(&s)?;
    let cfg = &s.config.background;

    if b.title.trim().is_empty() {
        return Err(bad("title is required"));
    }
    let trigger_type = BackgroundTrigger::parse(&b.trigger_type);
    let prompt_kind = b
        .prompt_kind
        .as_deref()
        .map(BackgroundPromptKind::parse)
        .unwrap_or(BackgroundPromptKind::Static);
    if prompt_kind == BackgroundPromptKind::Template && b.context_url.is_none() {
        return Err(bad("prompt_kind 'template' requires context_url"));
    }
    if !trigger_type.is_one_shot() && b.trigger_value.is_none() {
        return Err(bad(format!(
            "trigger_type '{}' requires trigger_value",
            trigger_type.as_str()
        )));
    }

    // Quota (design §10 guard 4). Owner for a UI-created task is `ui`.
    let owner_id = b.agent_folder.clone().unwrap_or_else(|| "ui".to_owned());
    let total = d
        .count_background_tasks_by_owner(&owner_id, false)
        .map_err(internal)?;
    if total >= cfg.max_tasks_per_owner {
        return Err(bad(format!(
            "owner '{owner_id}' already has {total} background tasks (max {})",
            cfg.max_tasks_per_owner
        )));
    }

    let now = Utc::now().to_rfc3339();
    let id = Uuid::new_v4().to_string();
    let status = if b.paused {
        BackgroundTaskStatus::Paused
    } else {
        BackgroundTaskStatus::Active
    };

    let mut task = BackgroundTask {
        id: id.clone(),
        owner_kind: BackgroundOwnerKind::User,
        owner_id,
        owner_key: b
            .title
            .trim()
            .to_lowercase()
            .replace(char::is_whitespace, "-"),
        title: b.title.trim().to_owned(),
        description: b.description,
        job_kind: BackgroundJobKind::Prompt,
        native_job: None,
        prompt_kind,
        prompt: Some(b.prompt),
        context_url: b.context_url,
        persona: b.persona,
        agent_folder: b.agent_folder,
        workspace_dir: b.workspace_dir,
        use_tools: b.tools,
        mcp_json: b.mcp.map(|v| v.to_string()),
        model_id: b.model_id,
        max_turns: b.max_turns,
        timeout_secs: b.timeout_secs,
        continuity: b
            .continuity
            .as_deref()
            .map(BackgroundContinuity::parse)
            .unwrap_or(BackgroundContinuity::Fresh),
        memory_folder: None,
        trigger_type,
        trigger_value: b.trigger_value,
        next_run: None,
        last_run: None,
        overlap_policy: b
            .overlap_policy
            .as_deref()
            .map(OverlapPolicy::parse)
            .unwrap_or(OverlapPolicy::Skip),
        catch_up: b.catch_up,
        max_failures: b.max_failures.unwrap_or(5),
        consecutive_failures: 0,
        visibility: BackgroundVisibility::Normal,
        notify: b.notify,
        status,
        created_at: now.clone(),
        updated_at: now,
    };

    // A `once` task fires at its stated time; everything else gets its first
    // window computed from the trigger.
    task.next_run = first_next_run(&s, &task)?;
    d.upsert_background_task(&task).map_err(internal)?;

    Ok(Json(serde_json::json!({
        "id": id,
        "status": task.status.as_str(),
        "next_run": task.next_run,
    })))
}

pub(crate) async fn detail(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let task = task_or_404(&s, &id)?;
    let runs = db(&s)?.list_background_runs(&id, 20).map_err(internal)?;
    Ok(Json(serde_json::json!({ "task": task, "runs": runs })))
}

pub(crate) async fn update(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Json(b): Json<UpdateBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut task = task_or_404(&s, &id)?;

    // Status is the one thing every owner allows: the user must be able to
    // pause core upkeep and an App's duties, even though they can't rewrite
    // them. Everything else is user-owned only.
    let status_only = b.title.is_none()
        && b.prompt.is_none()
        && b.trigger_type.is_none()
        && b.trigger_value.is_none()
        && b.tools.is_none()
        && b.persona.is_none();
    if !status_only && !task.owner_kind.is_editable() {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            format!(
                "task is owned by {} '{}' — only its status can be changed here{}",
                task.owner_kind.as_str(),
                task.owner_id,
                if task.owner_kind == BackgroundOwnerKind::App {
                    "; edit it in the app's manifest and reinstall"
                } else {
                    ""
                }
            ),
        ));
    }

    if let Some(v) = b.title {
        task.title = v;
    }
    if let Some(v) = b.description {
        task.description = Some(v);
    }
    if let Some(v) = b.prompt {
        task.prompt = Some(v);
    }
    if let Some(v) = b.prompt_kind {
        task.prompt_kind = BackgroundPromptKind::parse(&v);
    }
    if let Some(v) = b.context_url {
        task.context_url = Some(v);
    }
    if let Some(v) = b.persona {
        task.persona = Some(v);
    }
    if let Some(v) = b.tools {
        task.use_tools = v;
    }
    if let Some(v) = b.model_id {
        task.model_id = Some(v);
    }
    if let Some(v) = b.max_turns {
        task.max_turns = Some(v);
    }
    if let Some(v) = b.timeout_secs {
        task.timeout_secs = Some(v);
    }
    if let Some(v) = b.continuity {
        task.continuity = BackgroundContinuity::parse(&v);
    }
    if let Some(v) = b.overlap_policy {
        task.overlap_policy = OverlapPolicy::parse(&v);
    }
    if let Some(v) = b.catch_up {
        task.catch_up = v;
    }
    if let Some(v) = b.notify {
        task.notify = v;
    }
    if let Some(v) = b.max_failures {
        task.max_failures = v;
    }

    let trigger_changed = b.trigger_type.is_some() || b.trigger_value.is_some();
    if let Some(v) = b.trigger_type {
        task.trigger_type = BackgroundTrigger::parse(&v);
    }
    if let Some(v) = b.trigger_value {
        task.trigger_value = Some(v);
    }

    let mut resumed = false;
    if let Some(v) = b.status {
        let next = BackgroundTaskStatus::parse(&v);
        // Resuming clears the failure counter — otherwise a task the user
        // deliberately un-paused would re-quarantine on its very next failure.
        resumed =
            next == BackgroundTaskStatus::Active && task.status != BackgroundTaskStatus::Active;
        task.status = next;
        if resumed {
            task.consecutive_failures = 0;
        }
    }

    if trigger_changed || resumed {
        task.next_run = first_next_run(&s, &task)?;
    }
    task.updated_at = Utc::now().to_rfc3339();
    let d = db(&s)?;
    d.upsert_background_task(&task).map_err(internal)?;
    // The upsert's ON CONFLICT deliberately preserves the live columns
    // (status / next_run / consecutive_failures) so an App reinstall can't
    // silently re-enable a paused task. A user edit is not a reinstall, so those
    // columns — pause/resume, a re-armed schedule — must be written explicitly.
    d.advance_background_next_run(&task.id, task.next_run.as_deref(), task.status)
        .map_err(internal)?;
    if resumed {
        d.reset_background_failures(&task.id).map_err(internal)?;
    }

    if let Ok(sch) = scheduler(&s) {
        sch.notify_task_changed(&task);
    }
    Ok(Json(serde_json::json!({
        "success": true,
        "next_run": task.next_run,
        "status": task.status.as_str(),
    })))
}

pub(crate) async fn delete(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let task = task_or_404(&s, &id)?;
    if !task.owner_kind.is_editable() {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            match task.owner_kind {
                BackgroundOwnerKind::App => format!(
                    "task belongs to app '{}' — uninstall the app to remove it",
                    task.owner_id
                ),
                _ => format!(
                    "task is core upkeep ('{}') — pause it instead of deleting",
                    task.owner_id
                ),
            },
        ));
    }
    // Stop an in-flight run first; otherwise it keeps writing activity rows for
    // a task that no longer exists.
    if let Ok(sch) = scheduler(&s) {
        sch.cancel_task_runs(&id);
    }
    db(&s)?.delete_background_task(&id).map_err(internal)?;
    Ok(Json(serde_json::json!({ "success": true })))
}

pub(crate) async fn run_now(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    task_or_404(&s, &id)?;
    let run_id = scheduler(&s)?.run_now(&id).await.map_err(bad)?;
    Ok(Json(serde_json::json!({ "run_id": run_id })))
}

pub(crate) async fn runs(
    State(s): State<Arc<UiState>>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<RunsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let runs = db(&s)?
        .list_background_runs(&id, q.limit.unwrap_or(50).clamp(1, 500))
        .map_err(internal)?;
    Ok(Json(serde_json::json!({ "runs": runs })))
}

/// The background-session viewer: one run plus its transcript.
pub(crate) async fn run_detail(
    State(s): State<Arc<UiState>>,
    AxumPath(run_id): AxumPath<String>,
    Query(q): Query<RunQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let run = db(&s)?
        .get_background_run(&run_id)
        .map_err(internal)?
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("no such run: {run_id}")))?;
    let activity = if q.include_activity {
        db(&s)?
            .get_background_activity(&run_id, 2000)
            .map_err(internal)?
    } else {
        Vec::new()
    };
    Ok(Json(
        serde_json::json!({ "run": run, "activity": activity }),
    ))
}

pub(crate) async fn cancel_run(
    State(s): State<Arc<UiState>>,
    AxumPath(run_id): AxumPath<String>,
) -> Result<Json<serde_json::Value>, AppError> {
    let cancelled = scheduler(&s)?.cancel_run(&run_id).await;
    if !cancelled {
        return Err(bad("run is not in flight"));
    }
    Ok(Json(serde_json::json!({ "success": true })))
}

pub(crate) async fn stats(
    State(s): State<Arc<UiState>>,
    Query(q): Query<StatsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let window = q.window.as_deref().unwrap_or("7d");
    let since = window_start(window)?;
    let d = db(&s)?;
    let totals = d
        .background_totals(&since, q.owner_id.as_deref())
        .map_err(internal)?;
    let by_task = d.background_task_stats(&since).map_err(internal)?;
    let attention = d.background_attention().map_err(internal)?;
    Ok(Json(serde_json::json!({
        "window": window,
        "since": since,
        "totals": totals,
        "by_task": by_task,
        "attention": attention,
    })))
}

// ─── helpers ──────────────────────────────────────────────────────────────────

/// First `next_run` for a newly created or re-armed task.
///
/// `once` fires at its stated timestamp; everything else asks the scheduler for
/// the next window, so the cron/interval semantics live in exactly one place.
fn first_next_run(s: &UiState, task: &BackgroundTask) -> Result<Option<String>, AppError> {
    match task.trigger_type {
        BackgroundTrigger::Once => {
            let raw = task
                .trigger_value
                .as_deref()
                .ok_or_else(|| bad("trigger_type 'once' requires an RFC3339 trigger_value"))?;
            let at = chrono::DateTime::parse_from_rfc3339(raw)
                .map_err(|_| bad(format!("trigger_value '{raw}' is not a valid RFC3339 time")))?;
            Ok(Some(at.with_timezone(&Utc).to_rfc3339()))
        }
        BackgroundTrigger::OnInstall | BackgroundTrigger::Manual => Ok(None),
        _ => {
            let sch = scheduler(s)?;
            let next = sch.rearm(task);
            if next.is_none() {
                return Err(bad(format!(
                    "trigger_value '{}' is not a valid {} expression",
                    task.trigger_value.as_deref().unwrap_or(""),
                    task.trigger_type.as_str()
                )));
            }
            Ok(next)
        }
    }
}

#[cfg(test)]
mod quick_parse_tests {
    use super::extract_json_object;

    #[test]
    fn extracts_fenced_json() {
        let raw = "```json\n{\"title\": \"Dọn tri thức\", \"trigger_type\": \"cron\"}\n```";
        let m = extract_json_object(raw).unwrap();
        assert_eq!(m["title"], "Dọn tri thức");
        assert_eq!(m["trigger_type"], "cron");
    }

    #[test]
    fn extracts_json_with_prose_around_it() {
        let raw = "Đây là JSON:\n{\"prompt\": \"x\", \"trigger_type\": \"interval\"}\nXong.";
        let m = extract_json_object(raw).unwrap();
        assert_eq!(m["trigger_type"], "interval");
    }

    #[test]
    fn returns_none_on_non_json() {
        assert!(extract_json_object("không có json ở đây").is_none());
    }

    #[test]
    fn extracts_json_after_a_think_block_with_braces() {
        // The naive first{..last} slice fails here: the think block has its own
        // braces, so the span would run from the reasoning to the real JSON.
        let raw = "<think>I should return {title, prompt}. Let me build it.</think>\n                   {\"title\": \"Dọn tri thức\", \"trigger_type\": \"cron\"}";
        let m = extract_json_object(raw).unwrap();
        assert_eq!(m["title"], "Dọn tri thức");
        assert_eq!(m["trigger_type"], "cron");
    }

    #[test]
    fn a_brace_inside_a_string_does_not_break_matching() {
        let raw = r#"{"prompt": "dùng {{customers}} rồi nhắn", "trigger_type": "manual"}"#;
        let m = extract_json_object(raw).unwrap();
        assert_eq!(m["trigger_type"], "manual");
    }

    #[test]
    fn picks_the_last_valid_object_when_reasoning_has_json_ish_text() {
        let raw =
            "Ví dụ {\"x\": 1} nhưng đáp án là:\n{\"title\": \"T\", \"trigger_type\": \"once\"}";
        let m = extract_json_object(raw).unwrap();
        assert_eq!(m["title"], "T");
    }
}

fn window_start(window: &str) -> Result<String, AppError> {
    let dur = match window {
        "24h" => chrono::Duration::hours(24),
        "7d" => chrono::Duration::days(7),
        "30d" => chrono::Duration::days(30),
        other => {
            return Err(bad(format!(
                "unknown window '{other}' (use 24h, 7d or 30d)"
            )))
        }
    };
    Ok((Utc::now() - dur).to_rfc3339())
}
