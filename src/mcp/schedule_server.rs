//! Schedule MCP server. Port target: src-old/mcp/schedule-server.ts
//!
//! Tools: schedule_task, list_tasks, pause_task, cancel_task.
//! Operates directly on the SQLite `scheduled_tasks` table through [`Db`].

use anyhow::{bail, Context, Result};
use chrono::Utc;
use cron::Schedule;
use rmcp::ServiceExt;
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

use crate::db::Db;
use crate::types::{ContextMode, ScheduleType, ScheduledTask, TaskStatus};

// ===== MCP stdio server =====

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct ScheduleTaskParams {
    group_folder: String,
    chat_jid: String,
    prompt: String,
    schedule_type: String,
    schedule_value: String,
    #[serde(default)]
    context_mode: Option<String>,
    #[serde(default)]
    script_command: Option<String>,
}

/// Ownership is pinned from the chat env, never from a parameter — a watch
/// resumes a conversation, so letting a caller name someone else's chat would
/// let one session speak into another.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct ScheduleWatchParams {
    /// What the chat is told once the job finishes. Write it as an instruction
    /// to yourself in the future: you will be woken with this and nothing else,
    /// so restate what was being waited on. `{{result}}` is replaced with the
    /// probe payload.
    resume_prompt: String,
    /// Full `mcp__<server>__<tool>` name to re-call each check, e.g.
    /// `mcp__ai-office-mcp__office_get_task`. Omit when the job cannot be
    /// checked with one tool call — you are then woken every interval (at least
    /// 60 s apart, at most 10 times) by an automatic message to check it
    /// yourself, which costs a full turn each time, and you must end the watch
    /// with `schedule_watch_stop` once the job is finished or you cannot tell.
    #[serde(default)]
    tool: Option<String>,
    /// Arguments for `tool`, as a JSON object — include every one the tool
    /// requires, e.g. `{"id": 31}`. Omitting a required argument does not fail
    /// loudly: the tool answers with a rejection each check, and the watch
    /// cannot tell that apart from "not finished yet". `datagen_status` takes
    /// `{"job_id": "<id from datagen_start>"}`; without it the watch is refused.
    #[serde(default)]
    args: Option<serde_json::Value>,
    /// Dot path into the tool result, e.g. `task.status`. **Call the probe tool
    /// once before arming and read the path off its actual answer** — a guessed
    /// path resolves to nothing, which counts as "not ready", so the watch waits
    /// out its whole deadline on a job that already finished. Empty: `contains`
    /// searches the whole answer as text; `equals` / `in` read its `state` or
    /// `status` field (top level or one object down, e.g. `job.state`).
    #[serde(default)]
    done_path: Option<String>,
    /// `exists` (default), `equals`, `not_equals`, `contains`, `not_contains`,
    /// `in`, `not_in`. Case-insensitive.
    #[serde(default)]
    done_op: Option<String>,
    /// Comparison value for `equals` / `contains` and friends.
    #[serde(default)]
    done_value: Option<String>,
    /// Comparison set for `in` / `not_in`, e.g. ["done","failed","cancelled"].
    #[serde(default)]
    done_values: Vec<String>,
    /// Seconds between checks. Default 60, floor 15.
    #[serde(default)]
    interval_secs: Option<i64>,
    /// Give up after this many seconds. Default 3600, ceiling 86400.
    #[serde(default)]
    timeout_secs: Option<i64>,
    /// Short label naming what is being waited on, used in logs and in the
    /// give-up message.
    #[serde(default)]
    label: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct WatchStopParams {
    /// `watchId` from the `schedule_watch` result. Omit to stop every watch
    /// this chat has running.
    #[serde(default)]
    watch_id: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct ListTasksParams {
    group_folder: String,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct TaskActionParams {
    task_id: String,
    group_folder: String,
}

#[derive(Clone)]
pub struct McpScheduleServer {
    db: Arc<Db>,
    group_folder: String,
    chat_jid: String,
}

impl McpScheduleServer {
    /// Build from the DB + chat env trio, or `None` when any is absent. See
    /// [`crate::mcp::wiki_server::McpWikiServer::from_env`] for why an
    /// unconfigured child is `None` rather than an error.
    pub fn from_env() -> Result<Option<Self>> {
        let (Ok(group_folder), Ok(chat_jid)) = (
            std::env::var("SENCLAW_GROUP_FOLDER"),
            std::env::var("SENCLAW_CHAT_JID"),
        ) else {
            return Ok(None);
        };
        let Some(db) = crate::mcp::helper::shared_env_db()? else {
            return Ok(None);
        };
        Ok(Some(Self {
            db,
            group_folder,
            chat_jid,
        }))
    }
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpScheduleServer {
    #[rmcp::tool(
        description = "Schedule a new task. schedule_type is one of: 'cron' (recurring, schedule_value is a cron expr), 'interval' (recurring, schedule_value is milliseconds), 'once' (fire once at the ISO-8601 schedule_value, then keep the row marked completed), or 'once_delete' (fire once at the ISO-8601 schedule_value, then delete the task entirely)"
    )]
    async fn schedule_task(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            ScheduleTaskParams,
        >,
    ) -> String {
        let srv = ScheduleServer::new();
        let result = srv
            .schedule_task(
                &self.db,
                &p.group_folder,
                &p.chat_jid,
                &p.prompt,
                &p.schedule_type,
                &p.schedule_value,
                p.context_mode.as_deref(),
                p.script_command.as_deref(),
            )
            .await;
        if result.is_error {
            return result.content;
        }
        result.content
    }

    #[rmcp::tool(
        description = "Wait for a long-running job and resume THIS chat when it finishes. \
Use this instead of sleeping, instead of polling inside your turn, and instead of \
telling the user to ask you again later — after you hand work to something slow \
(an AI Office task, a dispatch/DAG run, a Space App job, a build), arm a watch and \
end your turn. SenClaw re-calls `tool` every `interval_secs` with no LLM involved, so \
waiting is nearly free, and the moment the condition holds it wakes you here with \
`resume_prompt` so you can deliver the result or carry on. It always reports back, \
including when it gives up at `timeout_secs`. Never arm one for something you can answer \
in this turn — a web search, a price, a score, a lookup: there is nothing to wait for, \
and it only wakes you again with nothing new to say. Omit `tool` only when the job \
cannot be checked with a single tool call; every wake is then a full turn of yours (at \
least 60 s apart, at most 10), and the watch ends only when you call schedule_watch_stop \
in a woken turn or it gives up — telling the user you are done does not end it."
    )]
    async fn schedule_watch(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            ScheduleWatchParams,
        >,
    ) -> String {
        let srv = ScheduleServer::new();
        srv.schedule_watch(&self.db, &self.group_folder, &self.chat_jid, p)
            .content
    }

    #[rmcp::tool(
        description = "List the watches this chat currently has running, with how many checks each has made and when it gives up. Use it when the user asks what you are waiting on, or before stopping one."
    )]
    fn schedule_watch_list(&self) -> String {
        let srv = ScheduleServer::new();
        srv.watch_list(&self.db, &self.chat_jid).content
    }

    #[rmcp::tool(
        description = "Stop a watch this chat armed — when the user says to stop waiting, when the thing being waited on is no longer relevant, or when an automatic watch check finds the job finished or impossible to determine (that is how a watch without a probe ends; saying you are done does not). Pass watchId to stop one, or omit it to stop all of this chat's watches. A stopped watch never wakes the chat, so say so plainly instead of leaving the user expecting a report."
    )]
    fn schedule_watch_stop(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            WatchStopParams,
        >,
    ) -> String {
        let srv = ScheduleServer::new();
        srv.watch_stop(&self.db, &self.chat_jid, p.watch_id.as_deref())
            .content
    }

    #[rmcp::tool(description = "List all scheduled tasks for a group")]
    fn list_tasks(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            ListTasksParams,
        >,
    ) -> String {
        let srv = ScheduleServer::new();
        let result = srv.list_tasks(&self.db, &p.group_folder);
        result.content
    }

    #[rmcp::tool(description = "Pause a scheduled task")]
    fn pause_task(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            TaskActionParams,
        >,
    ) -> String {
        let srv = ScheduleServer::new();
        let result = srv.pause_task(&self.db, &p.task_id, &p.group_folder);
        result.content
    }

    #[rmcp::tool(description = "Cancel a scheduled task")]
    fn cancel_task(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            TaskActionParams,
        >,
    ) -> String {
        let srv = ScheduleServer::new();
        let result = srv.cancel_task(&self.db, &p.task_id, &p.group_folder);
        result.content
    }

    #[rmcp::tool(
        description = "Resume a paused scheduled task. The next run time is recomputed from \
                       the task's schedule (a one-off whose time already passed fires on the \
                       next scheduler tick)."
    )]
    fn resume_task(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            TaskActionParams,
        >,
    ) -> String {
        let srv = ScheduleServer::new();
        let result = srv.resume_task(&self.db, &p.task_id, &p.group_folder);
        result.content
    }

    #[rmcp::tool(
        description = "Permanently delete a scheduled task and stop it from ever running again. \
                       Unlike cancel_task (which keeps the row with status=completed), this \
                       removes the task entirely."
    )]
    fn delete_task(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            TaskActionParams,
        >,
    ) -> String {
        let srv = ScheduleServer::new();
        let result = srv.delete_task(&self.db, &p.task_id, &p.group_folder);
        result.content
    }
}

/// Start the schedule MCP server over stdio. Reads config from environment
/// variables set by [`crate::mcp::helper::schedule_mcp_config`].
pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server = McpScheduleServer::from_env()?
        .context("SENCLAW_DB_PATH / SENCLAW_GROUP_FOLDER / SENCLAW_CHAT_JID not set")?;

    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

pub struct ScheduleServer;

/// Result returned by each tool call, compatible with MCP content format.
#[derive(Debug)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(text: String) -> Self {
        Self {
            content: text,
            is_error: false,
        }
    }

    pub fn err(text: String) -> Self {
        Self {
            content: text,
            is_error: true,
        }
    }
}

impl ScheduleServer {
    pub fn new() -> Self {
        Self
    }

    // ===== schedule_task =====

    /// Create a scheduled task. Returns JSON with `{success, taskId, nextRun}`.
    /// What this chat is waiting on.
    fn watch_list(&self, db: &Db, chat_jid: &str) -> ToolResult {
        use crate::scheduler::watch::WatchConfig;
        let tasks = match db.get_active_watches(chat_jid) {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!("Error: {e}")),
        };
        let items: Vec<serde_json::Value> = tasks
            .iter()
            .map(|t| {
                let cfg = t
                    .watch_json
                    .as_deref()
                    .and_then(|j| WatchConfig::parse(j).ok());
                serde_json::json!({
                    "watchId": t.id,
                    "label": cfg.as_ref().and_then(|c| c.label.clone()),
                    "tool": cfg.as_ref().and_then(|c| c.tool.clone()),
                    "checks": cfg.as_ref().map(|c| c.checks).unwrap_or(0),
                    "maxChecks": cfg.as_ref().map(|c| c.max_checks).unwrap_or(0),
                    "givesUpAt": cfg.as_ref().map(|c| c.deadline_at.clone()),
                    "nextCheck": t.next_run,
                })
            })
            .collect();
        ToolResult::ok(serde_json::json!({ "count": items.len(), "watches": items }).to_string())
    }

    /// Stop one watch, or all of this chat's watches when `watch_id` is absent.
    ///
    /// Ownership is the chat, not the id: stopping by a caller-supplied id
    /// alone would let one conversation cancel another's wait.
    fn watch_stop(&self, db: &Db, chat_jid: &str, watch_id: Option<&str>) -> ToolResult {
        let tasks = match db.get_active_watches(chat_jid) {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!("Error: {e}")),
        };
        let targets: Vec<&ScheduledTask> = match watch_id {
            Some(id) => tasks.iter().filter(|t| t.id == id).collect(),
            None => tasks.iter().collect(),
        };
        if targets.is_empty() {
            return ToolResult::err(match watch_id {
                Some(id) => format!("No running watch {id} in this chat"),
                None => "This chat has no running watch".into(),
            });
        }
        let mut stopped = Vec::new();
        for t in targets {
            // `stop_watch` reports whether a row actually changed; a plain
            // status update returns Ok for an id that matched nothing, and the
            // agent would then tell the user it had stopped a live watch.
            match db.stop_watch(&t.id) {
                Ok(true) => stopped.push(t.id.clone()),
                Ok(false) => tracing::warn!("[schedule] watch {} was not running", t.id),
                Err(e) => tracing::warn!("[schedule] stop watch {}: {e}", t.id),
            }
        }
        if stopped.is_empty() {
            return ToolResult::err("Could not stop any watch".into());
        }
        ToolResult::ok(
            serde_json::json!({
                "success": true,
                "stopped": stopped.len(),
                "watchIds": stopped,
                "note": "These watches will not wake this chat. Tell the user plainly that you are no longer waiting.",
            })
            .to_string(),
        )
    }

    /// Arm a watch on the calling chat. Bounds are clamped rather than
    /// rejected: a model that asks for a 2-second poll or a week-long deadline
    /// gets a sane watch, not an error it has to recover from mid-turn.
    fn schedule_watch(
        &self,
        db: &Db,
        group_folder: &str,
        chat_jid: &str,
        p: ScheduleWatchParams,
    ) -> ToolResult {
        use crate::scheduler::watch::{DoneOp, DoneWhen, WatchBounds, WatchConfig};

        if p.resume_prompt.trim().is_empty() {
            return ToolResult::err(
                "Error: resume_prompt is required — a watch that wakes you with nothing \
                 to act on is the same dead end as not waiting at all"
                    .into(),
            );
        }

        let args = match probe_args(p.args.clone()) {
            Ok(a) => a,
            Err(e) => return ToolResult::err(e),
        };
        if let Some(refusal) = p
            .tool
            .as_deref()
            .and_then(|t| crate::scheduler::watch::missing_probe_arg(t, &args))
        {
            return ToolResult::err(refusal);
        }
        let op = p.done_op.as_deref().map(DoneOp::parse).unwrap_or(DoneOp::Exists);
        let path = p.done_path.clone().unwrap_or_default();
        let agent_turn = p.tool.as_deref().map(str::trim).unwrap_or("").is_empty();
        let WatchBounds {
            interval_secs,
            timeout_secs,
            max_checks,
        } = WatchBounds::clamp(agent_turn, p.interval_secs, p.timeout_secs);
        let deadline = Utc::now() + chrono::Duration::seconds(timeout_secs);

        let cfg = WatchConfig {
            tool: p.tool.clone(),
            args,
            done_when: DoneWhen {
                path,
                op,
                value: p.done_value.clone(),
                values: p.done_values.clone(),
            },
            deadline_at: deadline.to_rfc3339(),
            max_checks,
            checks: 0,
            error_streak: 0,
            last_error: None,
            resume_prompt: p.resume_prompt.clone(),
            timeout_prompt: None,
            label: p.label.clone(),
        };
        let watch_json = match cfg.to_json() {
            Ok(j) => j,
            Err(e) => return ToolResult::err(format!("Error: {e}")),
        };

        let next_run = (Utc::now() + chrono::Duration::seconds(interval_secs)).to_rfc3339();
        let task = ScheduledTask {
            id: Uuid::new_v4().to_string(),
            group_folder: group_folder.to_owned(),
            chat_jid: chat_jid.to_owned(),
            prompt: p.resume_prompt.clone(),
            schedule_type: ScheduleType::Interval,
            schedule_value: (interval_secs * 1000).to_string(),
            context_mode: ContextMode::Watch,
            agent_mode: crate::types::AgentMode::Agent,
            script_command: None,
            watch_json: Some(watch_json),
            next_run: Some(next_run.clone()),
            last_run: None,
            last_result: None,
            status: TaskStatus::Active,
            created_at: Utc::now().to_rfc3339(),
        };
        if let Err(e) = db.insert_task(&task) {
            return ToolResult::err(format!("Error: {e}"));
        }
        ToolResult::ok(
            serde_json::json!({
                "success": true,
                "watchId": task.id,
                "firstCheck": next_run,
                "intervalSecs": interval_secs,
                "givesUpAt": cfg.deadline_at,
                "maxChecks": max_checks,
                "mode": if cfg.is_agent_fallback() { "agent-turn" } else { "probe" },
                "note": if cfg.is_agent_fallback() {
                    format!(
                        "Agent-turn watch armed. It has no probe, so every {interval_secs}s (at most {max_checks} \
times) you are woken here by an automatic message starting \"[Automatic check\"; each wake is a full turn. \
End your turn now and tell the user what you are watching and that they can stop it. In a woken turn, when the \
job has finished or you cannot tell, call schedule_watch_stop with this watchId — saying you are done does not \
end the watch. If you armed this for something you could answer right now (a search, a price, a score), call \
schedule_watch_stop now and answer instead."
                    )
                } else {
                    "Watch armed. End your turn now — you will be woken here with the result. \
The user sees it above the composer with a Stop button, and you can stop it with schedule_watch_stop; \
tell them what you are watching and that they can stop it."
                        .to_string()
                },
            })
            .to_string(),
        )
    }

    pub async fn schedule_task(
        &self,
        db: &Db,
        group_folder: &str,
        chat_jid: &str,
        prompt: &str,
        schedule_type: &str,
        schedule_value: &str,
        context_mode: Option<&str>,
        script_command: Option<&str>,
    ) -> ToolResult {
        let schedule_type = ScheduleType::parse(schedule_type);
        let resolved_mode = context_mode
            .map(|s| ContextMode::parse(s))
            .unwrap_or(ContextMode::Notify);

        if matches!(
            resolved_mode,
            ContextMode::Script | ContextMode::ScriptAgent
        ) && script_command.is_none()
        {
            return ToolResult::err(
                "Error: script_command is required for script and script-agent modes".into(),
            );
        }

        match compute_next_run(&schedule_type, schedule_value) {
            Ok(next_run) => {
                let task = ScheduledTask {
                    id: Uuid::new_v4().to_string(),
                    group_folder: group_folder.to_owned(),
                    chat_jid: chat_jid.to_owned(),
                    prompt: prompt.to_owned(),
                    schedule_type,
                    schedule_value: schedule_value.to_owned(),
                    context_mode: resolved_mode,
                    agent_mode: crate::types::AgentMode::Agent,
                    script_command: script_command.map(|s| s.to_owned()),
                    watch_json: None,
                    next_run: Some(next_run.clone()),
                    last_run: None,
                    last_result: None,
                    status: TaskStatus::Active,
                    created_at: Utc::now().to_rfc3339(),
                };
                if let Err(e) = db.insert_task(&task) {
                    return ToolResult::err(format!("Error: {e}"));
                }
                let json = serde_json::json!({
                    "success": true,
                    "taskId": task.id,
                    "nextRun": next_run,
                });
                ToolResult::ok(json.to_string())
            }
            Err(e) => ToolResult::err(format!("Error: {e}")),
        }
    }

    // ===== list_tasks =====

    /// List all scheduled tasks for the owning group folder.
    pub fn list_tasks(&self, db: &Db, group_folder: &str) -> ToolResult {
        match db.get_tasks_by_group(group_folder) {
            Ok(tasks) => {
                let json = serde_json::to_string_pretty(&tasks).unwrap_or_default();
                ToolResult::ok(json)
            }
            Err(e) => ToolResult::err(format!("Error: {e}")),
        }
    }

    // ===== pause_task =====

    pub fn pause_task(&self, db: &Db, task_id: &str, group_folder: &str) -> ToolResult {
        // validate ownership: only pause tasks belonging to this group
        match db.get_tasks_by_group(group_folder) {
            Ok(tasks) => {
                if !tasks.iter().any(|t| t.id == task_id) {
                    return ToolResult::err(format!(
                        "Task not found or not in this group: {task_id}"
                    ));
                }
            }
            Err(e) => return ToolResult::err(format!("Error: {e}")),
        }
        match db.update_task_status(task_id, TaskStatus::Paused) {
            Ok(_) => {
                let json = serde_json::json!({
                    "success": true,
                    "taskId": task_id,
                    "status": "paused",
                });
                ToolResult::ok(json.to_string())
            }
            Err(e) => ToolResult::err(format!("Error: {e}")),
        }
    }

    // ===== cancel_task =====

    pub fn cancel_task(&self, db: &Db, task_id: &str, group_folder: &str) -> ToolResult {
        match db.get_tasks_by_group(group_folder) {
            Ok(tasks) => {
                if !tasks.iter().any(|t| t.id == task_id) {
                    return ToolResult::err(format!(
                        "Task not found or not in this group: {task_id}"
                    ));
                }
            }
            Err(e) => return ToolResult::err(format!("Error: {e}")),
        }
        match db.update_task_status(task_id, TaskStatus::Completed) {
            Ok(_) => {
                let json = serde_json::json!({
                    "success": true,
                    "taskId": task_id,
                    "status": "completed",
                });
                ToolResult::ok(json.to_string())
            }
            Err(e) => ToolResult::err(format!("Error: {e}")),
        }
    }

    // ===== resume_task =====

    /// Reactivate a paused task. Recomputes `next_run` from the schedule so a
    /// long pause doesn't leave a stale fire time; a `once` task whose time
    /// already passed fires on the next scheduler tick.
    pub fn resume_task(&self, db: &Db, task_id: &str, group_folder: &str) -> ToolResult {
        let task = match db.get_tasks_by_group(group_folder) {
            Ok(tasks) => match tasks.into_iter().find(|t| t.id == task_id) {
                Some(t) => t,
                None => {
                    return ToolResult::err(format!(
                        "Task not found or not in this group: {task_id}"
                    ))
                }
            },
            Err(e) => return ToolResult::err(format!("Error: {e}")),
        };
        if task.status != TaskStatus::Paused {
            return ToolResult::err(format!(
                "Only paused tasks can be resumed (task {task_id} is {})",
                task.status.as_str()
            ));
        }
        match compute_next_run(&task.schedule_type, &task.schedule_value) {
            Ok(next_run) => {
                if let Err(e) =
                    db.advance_task_next_run(task_id, Some(&next_run), TaskStatus::Active)
                {
                    return ToolResult::err(format!("Error: {e}"));
                }
                let json = serde_json::json!({
                    "success": true,
                    "taskId": task_id,
                    "status": "active",
                    "nextRun": next_run,
                });
                ToolResult::ok(json.to_string())
            }
            Err(e) => ToolResult::err(format!("Error: {e}")),
        }
    }

    // ===== delete_task =====

    /// Hard-delete a task (row removed; run logs are kept for audit).
    pub fn delete_task(&self, db: &Db, task_id: &str, group_folder: &str) -> ToolResult {
        match db.get_tasks_by_group(group_folder) {
            Ok(tasks) => {
                if !tasks.iter().any(|t| t.id == task_id) {
                    return ToolResult::err(format!(
                        "Task not found or not in this group: {task_id}"
                    ));
                }
            }
            Err(e) => return ToolResult::err(format!("Error: {e}")),
        }
        match db.delete_task(task_id) {
            Ok(true) => {
                let json = serde_json::json!({
                    "success": true,
                    "taskId": task_id,
                    "deleted": true,
                });
                ToolResult::ok(json.to_string())
            }
            Ok(false) => ToolResult::err(format!("Task not found: {task_id}")),
            Err(e) => ToolResult::err(format!("Error: {e}")),
        }
    }
}

impl Default for ScheduleServer {
    fn default() -> Self {
        Self::new()
    }
}

// ===== next_run calculation =====

fn compute_next_run(schedule_type: &ScheduleType, value: &str) -> Result<String> {
    match schedule_type {
        ScheduleType::Cron => {
            let expr = if value.trim().split_whitespace().count() == 5 {
                format!("0 {value}")
            } else {
                value.to_owned()
            };
            let schedule = Schedule::from_str(&expr)?;
            schedule
                .upcoming(Utc)
                .next()
                .map(|t| t.to_rfc3339())
                .ok_or_else(|| anyhow::anyhow!("Cron schedule has no upcoming occurrence"))
        }
        ScheduleType::Interval => {
            let ms: i64 = value
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid interval value: {value}"))?;
            if ms <= 0 {
                bail!("Interval value must be positive: {value}");
            }
            Ok((Utc::now() + chrono::Duration::milliseconds(ms)).to_rfc3339())
        }
        ScheduleType::Once | ScheduleType::OnceDelete => {
            // value is used directly as ISO time
            let _ = chrono::DateTime::parse_from_rfc3339(value)?;
            Ok(value.to_owned())
        }
    }
}

/// The probe's arguments as the JSON object the tool takes. A model often
/// hands them over as a JSON *string* (`"{\"job_id\": \"…\"}"`); stored as is,
/// every check sent a string where the tool wants an object, every check
/// failed, and the chat was never woken. A string holding an object is
/// unwrapped; anything else that is not an object is refused while the model
/// can still fix it.
fn probe_args(args: Option<serde_json::Value>) -> Result<serde_json::Value, String> {
    match args {
        None | Some(serde_json::Value::Null) => Ok(serde_json::json!({})),
        Some(v @ serde_json::Value::Object(_)) => Ok(v),
        Some(serde_json::Value::String(s)) if s.trim().is_empty() => Ok(serde_json::json!({})),
        Some(serde_json::Value::String(s)) => match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(v @ serde_json::Value::Object(_)) => Ok(v),
            _ => Err(format!("Error: args must be a JSON object such as {{\"id\": 31}}, not the text {s:?}")),
        },
        Some(other) => Err(format!("Error: args must be a JSON object such as {{\"id\": 31}}, not {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_args_given_as_a_json_string_are_unwrapped() {
        use serde_json::json;
        assert_eq!(probe_args(None).unwrap(), json!({}));
        assert_eq!(probe_args(Some(json!({"job_id": "a1"}))).unwrap(), json!({"job_id": "a1"}));
        assert_eq!(probe_args(Some(json!("{\"job_id\": \"a1\"}"))).unwrap(), json!({"job_id": "a1"}));
        assert_eq!(probe_args(Some(json!(""))).unwrap(), json!({}));
        assert!(probe_args(Some(json!("job a1"))).unwrap_err().contains("JSON object"));
        assert!(probe_args(Some(json!([1, 2]))).is_err());
    }
    use crate::config::Config;

    fn test_config() -> Config {
        Config::from_env()
    }

    #[test]
    fn compute_next_run_interval() {
        let result = compute_next_run(&ScheduleType::Interval, "3600000").unwrap();
        // should be ~1 hour from now
        assert!(result.len() > 10);
    }

    #[test]
    fn compute_next_run_once() {
        let result = compute_next_run(&ScheduleType::Once, "2026-12-25T00:00:00+00:00").unwrap();
        assert!(result.contains("2026-12-25"));
    }

    #[test]
    fn compute_next_run_once_delete() {
        let result =
            compute_next_run(&ScheduleType::OnceDelete, "2026-12-25T00:00:00+00:00").unwrap();
        assert!(result.contains("2026-12-25"));
        assert_eq!(ScheduleType::parse("once_delete"), ScheduleType::OnceDelete);
    }

    #[test]
    fn compute_next_run_cron() {
        let result = compute_next_run(&ScheduleType::Cron, "0 9 * * *").unwrap();
        assert!(result.len() > 10);
    }

    #[test]
    fn compute_next_run_invalid_interval() {
        assert!(compute_next_run(&ScheduleType::Interval, "bad").is_err());
    }

    #[test]
    fn a_watch_armed_with_stringified_args_and_no_path_still_works() {
        let cfg = test_config();
        let db = Db::open_in_memory(&cfg).unwrap();
        let srv = ScheduleServer::new();
        let arm = |v: serde_json::Value| {
            let p: ScheduleWatchParams = serde_json::from_value(v).unwrap();
            srv.schedule_watch(&db, "team-a", "tg:group:1", p)
        };
        let base = serde_json::json!({
            "resume_prompt": "job done: {{result}}",
            "tool": "mcp__senclaw-datagen__datagen_status",
            "args": "{\"job_id\": \"abc123\"}",
            "done_op": "in",
            "done_values": ["done", "failed", "cancelled"]
        });
        let armed = arm(base.clone());
        assert!(!armed.is_error, "{}", armed.content);
        let stored: String = db
            .with_conn(|c| c.query_row("SELECT watch_json FROM scheduled_tasks WHERE context_mode='watch'", [], |r| r.get(0)).map_err(Into::into))
            .unwrap();
        let w: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(w["args"], serde_json::json!({"job_id": "abc123"}), "a stringified args object is unwrapped");
        let done: crate::scheduler::watch::DoneWhen = serde_json::from_value(w["done_when"].clone()).unwrap();
        assert!(done.evaluate(&serde_json::json!({"job": {"state": "done"}})), "no path reads job.state");

        let mut text = base;
        text["done_op"] = serde_json::json!("contains");
        text["done_value"] = serde_json::json!("done");
        assert!(!arm(text).is_error, "contains on the whole answer stays allowed");
    }

    #[test]
    fn a_watch_without_a_probe_is_capped_and_told_how_to_end_itself() {
        // 2026-10-17: a lookup ("giá vàng hôm nay") was armed as a watch with no
        // tool, 60 s, the default hour. It woke the chat 32 times and the agent
        // answered every wake "I am done" without ever stopping it.
        let cfg = test_config();
        let db = Db::open_in_memory(&cfg).unwrap();
        let srv = ScheduleServer::new();
        let armed = srv.schedule_watch(
            &db,
            "main",
            "web:main:x",
            serde_json::from_value(serde_json::json!({
                "resume_prompt": "Tôi đã tìm kiếm giá vàng. {{result}}",
                "interval_secs": 15,
                "timeout_secs": 86400,
                "label": "Giá vàng hôm nay"
            }))
            .unwrap(),
        );
        assert!(!armed.is_error, "{}", armed.content);
        let v: serde_json::Value = serde_json::from_str(&armed.content).unwrap();
        assert_eq!(v["mode"], "agent-turn");
        assert_eq!(v["intervalSecs"], 60, "a wake is a turn: the 15 s probe floor does not apply");
        assert_eq!(v["maxChecks"], 10);
        let note = v["note"].as_str().unwrap();
        assert!(note.contains("schedule_watch_stop"), "{note}");
        assert!(note.contains("does not end the watch"), "{note}");
        assert!(note.contains("answer instead"), "the arming turn is told it can back out: {note}");

        let stored: String = db
            .with_conn(|c| c.query_row("SELECT watch_json FROM scheduled_tasks WHERE context_mode='watch'", [], |r| r.get(0)).map_err(Into::into))
            .unwrap();
        let w: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(w["max_checks"], 10, "the cap is stored, so the strip shows n/10");
        let gives_up = chrono::DateTime::parse_from_rfc3339(v["givesUpAt"].as_str().unwrap()).unwrap();
        let in_secs = (gives_up.with_timezone(&Utc) - Utc::now()).num_seconds();
        assert!(in_secs <= 60 * 11, "gives up in {in_secs}s, not the 86400 that was asked for");
    }

    #[test]
    fn a_probe_watch_keeps_its_result_note_and_its_fifteen_second_floor() {
        let cfg = test_config();
        let db = Db::open_in_memory(&cfg).unwrap();
        let srv = ScheduleServer::new();
        let armed = srv.schedule_watch(
            &db,
            "main",
            "web:main:x",
            serde_json::from_value(serde_json::json!({
                "resume_prompt": "done: {{result}}",
                "tool": "mcp__ai-office-mcp__office_get_task",
                "args": {"id": 27},
                "done_path": "task.status",
                "done_op": "equals",
                "done_value": "done",
                "interval_secs": 2
            }))
            .unwrap(),
        );
        assert!(!armed.is_error, "{}", armed.content);
        let v: serde_json::Value = serde_json::from_str(&armed.content).unwrap();
        assert_eq!(v["mode"], "probe");
        assert_eq!(v["intervalSecs"], 15);
        assert_eq!(v["maxChecks"], 240);
        assert!(v["note"].as_str().unwrap().contains("woken here with the result"));
    }

    #[test]
    fn a_datagen_watch_without_a_job_id_is_refused_while_the_model_can_fix_it() {
        // The 2026-10-16 run: `args` left empty on `datagen_status`, which then
        // lists everything and has no state to wait on. The watch was armed,
        // checked five times and never resolved on a job that was already done.
        let cfg = test_config();
        let db = Db::open_in_memory(&cfg).unwrap();
        let srv = ScheduleServer::new();
        let count = || -> i64 {
            db.with_conn(|c| {
                c.query_row("SELECT COUNT(*) FROM scheduled_tasks WHERE context_mode='watch'", [], |r| r.get(0))
                    .map_err(Into::into)
            })
            .unwrap()
        };
        let arm = |args: Option<serde_json::Value>| {
            let mut v = serde_json::json!({
                "resume_prompt": "import done: {{result}}",
                "tool": "mcp__senclaw-datagen__datagen_status",
                "done_op": "in",
                "done_values": ["done", "failed", "cancelled"]
            });
            if let Some(a) = args {
                v["args"] = a;
            }
            let p: ScheduleWatchParams = serde_json::from_value(v).unwrap();
            srv.schedule_watch(&db, "team-a", "tg:group:1", p)
        };

        for unnamed in [None, Some(serde_json::json!({})), Some(serde_json::json!("{}")), Some(serde_json::json!({"job_id": " "}))] {
            let refused = arm(unnamed);
            assert!(refused.is_error, "{}", refused.content);
            assert!(refused.content.contains("job_id"), "{}", refused.content);
        }
        assert_eq!(count(), 0, "a refused watch leaves no row polling");

        assert!(!arm(Some(serde_json::json!({"job_id": "c44bde07eee7"}))).is_error);
        assert_eq!(count(), 1);
    }

    #[test]
    fn schedule_task_pause_cancel_flow() {
        let cfg = test_config();
        let db = Db::open_in_memory(&cfg).unwrap();
        let srv = ScheduleServer::new();

        let create = tokio_test::block_on(srv.schedule_task(
            &db,
            "team-a",
            "tg:group:1",
            "do thing",
            "once",
            "2026-12-25T00:00:00+00:00",
            Some("isolated"),
            None,
        ));
        assert!(!create.is_error);
        let task_id: String = serde_json::from_str::<serde_json::Value>(&create.content)
            .unwrap()
            .get("taskId")
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned();

        let list = srv.list_tasks(&db, "team-a");
        assert!(!list.is_error);

        let pause = srv.pause_task(&db, &task_id, "team-a");
        assert!(!pause.is_error);

        let cancel = srv.cancel_task(&db, &task_id, "team-a");
        assert!(!cancel.is_error);
    }

    #[test]
    fn resume_and_delete_task_flow() {
        let cfg = test_config();
        let db = Db::open_in_memory(&cfg).unwrap();
        let srv = ScheduleServer::new();

        let create = tokio_test::block_on(srv.schedule_task(
            &db,
            "team-b",
            "tg:group:2",
            "daily report",
            "cron",
            "0 9 * * *",
            Some("isolated"),
            None,
        ));
        assert!(!create.is_error);
        let task_id: String = serde_json::from_str::<serde_json::Value>(&create.content).unwrap()
            ["taskId"]
            .as_str()
            .unwrap()
            .to_owned();

        // Resume only works on paused tasks.
        let premature = srv.resume_task(&db, &task_id, "team-b");
        assert!(premature.is_error, "resuming an active task must fail");

        srv.pause_task(&db, &task_id, "team-b");
        let resume = srv.resume_task(&db, &task_id, "team-b");
        assert!(!resume.is_error, "{}", resume.content);
        let v: serde_json::Value = serde_json::from_str(&resume.content).unwrap();
        assert_eq!(v["status"], "active");
        assert!(v["nextRun"].as_str().unwrap().len() > 10);

        // Ownership enforced for both new tools.
        assert!(srv.resume_task(&db, &task_id, "other-team").is_error);
        assert!(srv.delete_task(&db, &task_id, "other-team").is_error);

        // Hard delete removes the row entirely.
        let del = srv.delete_task(&db, &task_id, "team-b");
        assert!(!del.is_error, "{}", del.content);
        let tasks = db.get_tasks_by_group("team-b").unwrap();
        assert!(tasks.is_empty(), "task row must be gone after delete_task");

        // Deleting again reports not-found.
        assert!(srv.delete_task(&db, &task_id, "team-b").is_error);
    }
}
