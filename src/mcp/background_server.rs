//! Background MCP server — lets a chat session create and manage autonomous
//! background tasks. See `docs/background-tasks-design.md` §10.
//!
//! Naming per CLAUDE.md: server `senclaw-background`, tool prefix `background_`.
//!
//! Ownership is pinned from the env the config builder injects
//! (`SENCLAW_GROUP_FOLDER` / `SENCLAW_CHAT_JID`), never from a tool parameter —
//! a deliberate departure from `senclaw-schedule`, whose ownership check is
//! client-supplied so any caller can manage another group's tasks.

use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use rmcp::ServiceExt;
use uuid::Uuid;

use crate::background::plan_next_run;
use crate::db::background::BackgroundTaskFilter;
use crate::db::Db;
use crate::mcp::schedule_server::ToolResult;
use crate::types::{
    BackgroundContinuity, BackgroundJobKind, BackgroundOwnerKind, BackgroundPromptKind,
    BackgroundTask, BackgroundTaskStatus, BackgroundTrigger, OverlapPolicy,
};

// ───────────────────────── param structs ─────────────────────────

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct CreateParams {
    /// Short human title. Optional — when omitted it is derived from the first
    /// line of the prompt. Kept non-required because models routinely skip a
    /// nicety field and a hard failure there is worse than a derived title.
    #[serde(default)]
    title: Option<String>,
    /// The instruction the task runs. Must be self-contained — a background run
    /// has no human to ask and no chat history for context.
    prompt: String,
    /// `cron` (recurring; `trigger_value` is a 5-field cron expr, local time),
    /// `interval` (recurring; `trigger_value` is milliseconds), `once`
    /// (`trigger_value` is an RFC3339 timestamp), or `manual` (run-now only).
    trigger_type: String,
    #[serde(default)]
    trigger_value: Option<String>,
    #[serde(default)]
    description: Option<String>,
    /// `static` (verbatim), `template` (GET `context_url` for `{{vars}}`; skips
    /// when empty), or `generator` (an LLM writes the real prompt each run).
    #[serde(default)]
    prompt_kind: Option<String>,
    #[serde(default)]
    context_url: Option<String>,
    /// Agent profile folder to run under (persona + skills + tools).
    #[serde(default)]
    persona: Option<String>,
    /// Tool allowlist. Empty = the persona's list, or all. Must be a subset of
    /// this chat's own allowed tools.
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    max_turns: Option<i64>,
    #[serde(default)]
    timeout_secs: Option<i64>,
    /// `fresh` (default) or `thread` (inject recent run summaries — required for
    /// anything contacting people, so it doesn't repeat itself).
    #[serde(default)]
    continuity: Option<String>,
    /// `skip` (default), `queue`, or `cancel_previous`.
    #[serde(default)]
    overlap_policy: Option<String>,
    /// Đẩy thông báo OS thay vì chạy agent (dùng cho "nhắc/thông báo X").
    #[serde(default)]
    notify: bool,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct ListParams {
    #[serde(default)]
    status: Option<String>,
    /// Include core-upkeep (system) tasks. Off by default.
    #[serde(default)]
    include_internal: bool,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct TaskIdParams {
    task_id: String,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct StatsParams {
    /// `24h`, `7d` (default) or `30d`.
    #[serde(default)]
    window: Option<String>,
}

// ───────────────────────── MCP server ─────────────────────────

#[derive(Clone)]
pub struct McpBackgroundServer {
    db: Arc<Db>,
    /// The owning chat's agent folder — becomes `owner_id`. From env, never a
    /// tool param.
    owner_id: String,
    /// The owning chat's jid, used to read its `allowed_tools` whitelist.
    chat_jid: String,
}

impl McpBackgroundServer {
    /// Build from the DB + chat env trio, or `None` when any is absent. See
    /// [`crate::mcp::wiki_server::McpWikiServer::from_env`] for why an
    /// unconfigured child is `None` rather than an error.
    pub fn from_env() -> Result<Option<Self>> {
        let (Ok(owner_id), Ok(chat_jid)) = (
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
            owner_id,
            chat_jid,
        }))
    }
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpBackgroundServer {
    #[rmcp::tool(
        description = "Create a BACKGROUND task that SenClaw runs by itself on a schedule with NO \
                       reply to anyone (unlike a chat schedule). Required: prompt (what to do) and \
                       trigger_type ('cron' with a cron trigger_value, 'interval' with milliseconds, \
                       'once' with an RFC3339 time, or 'manual'). Everything else is optional."
    )]
    fn background_create(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            CreateParams,
        >,
    ) -> String {
        self.create(p).content
    }

    #[rmcp::tool(description = "List background tasks (title, owner, trigger, next run, status).")]
    fn background_list(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            ListParams,
        >,
    ) -> String {
        self.list(p).content
    }

    #[rmcp::tool(description = "Get one background task's config plus its recent runs.")]
    fn background_get(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            TaskIdParams,
        >,
    ) -> String {
        self.get(&p.task_id).content
    }

    #[rmcp::tool(description = "Pause a background task (stop it firing; keep its config).")]
    fn background_pause(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            TaskIdParams,
        >,
    ) -> String {
        self.set_status(&p.task_id, BackgroundTaskStatus::Paused)
            .content
    }

    #[rmcp::tool(
        description = "Resume a paused background task. Recomputes the next run and clears its \
                       consecutive-failure count."
    )]
    fn background_resume(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            TaskIdParams,
        >,
    ) -> String {
        self.resume(&p.task_id).content
    }

    #[rmcp::tool(
        description = "Delete a background task. Only user-created tasks can be deleted here; an \
                       app's tasks go away when the app is uninstalled, and core upkeep can only be \
                       paused. Run history is kept."
    )]
    fn background_delete(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            TaskIdParams,
        >,
    ) -> String {
        self.delete(&p.task_id).content
    }

    #[rmcp::tool(
        description = "Run a background task now, out of schedule. It fires on the next scheduler \
                       tick (within a few seconds)."
    )]
    fn background_run_now(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            TaskIdParams,
        >,
    ) -> String {
        self.run_now(&p.task_id).content
    }

    #[rmcp::tool(
        description = "Background task statistics over a window: run counts, success rate (skips \
                       excluded), and which tasks need attention."
    )]
    fn background_stats(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            StatsParams,
        >,
    ) -> String {
        self.stats(p.window.as_deref().unwrap_or("7d")).content
    }
}

impl McpBackgroundServer {
    fn create(&self, p: CreateParams) -> ToolResult {
        let cfg = crate::config::Config::from_env().background;

        if p.prompt.trim().is_empty() {
            return ToolResult::err("prompt is required".into());
        }
        // Title is a nicety — derive it from the prompt when the model didn't
        // send one (mirrors recurring_create deriving its label from prompt).
        let title = p
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| derive_title(&p.prompt));
        let trigger_type = BackgroundTrigger::parse(&p.trigger_type);
        let prompt_kind = p
            .prompt_kind
            .as_deref()
            .map(BackgroundPromptKind::parse)
            .unwrap_or(BackgroundPromptKind::Static);

        if prompt_kind == BackgroundPromptKind::Template && p.context_url.is_none() {
            return ToolResult::err("prompt_kind 'template' requires context_url".into());
        }
        if !trigger_type.is_one_shot() && p.trigger_value.is_none() {
            return ToolResult::err(format!(
                "trigger_type '{}' requires trigger_value",
                trigger_type.as_str()
            ));
        }

        // Guard 1 — no privilege escalation: a task's tools must be a subset of
        // this chat's own allowed_tools. A chat with a narrow allowlist cannot
        // mint a background task that has everything. Literal comparison
        // (design open-question 9); globs are compared as-is.
        if let Ok(Some(group)) = self.db.get_group(&self.chat_jid) {
            if let Some(allowed) = group.allowed_tools.filter(|a| !a.is_empty()) {
                let escalated: Vec<&String> =
                    p.tools.iter().filter(|t| !allowed.contains(*t)).collect();
                if !escalated.is_empty() {
                    return ToolResult::err(format!(
                        "these tools are outside this chat's allowlist, so a background task can't \
                         use them: {}",
                        escalated
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
        }

        // Guard 4 — quota.
        match self
            .db
            .count_background_tasks_by_owner(&self.owner_id, false)
        {
            Ok(n) if n >= cfg.max_tasks_per_owner => {
                return ToolResult::err(format!(
                    "you already have {n} background tasks (max {})",
                    cfg.max_tasks_per_owner
                ))
            }
            Err(e) => return ToolResult::err(format!("quota check: {e}")),
            _ => {}
        }

        // Guard 3 — an outward-facing or self-writing task is created PAUSED,
        // enabled out-of-band in the UI. An injection can reach the prompt but
        // not the UI toggle.
        let outward = prompt_kind == BackgroundPromptKind::Generator
            || p.tools.iter().any(|t| is_outward_facing(t));
        let status = if outward {
            BackgroundTaskStatus::Paused
        } else {
            BackgroundTaskStatus::Active
        };

        let now = Utc::now().to_rfc3339();
        let id = Uuid::new_v4().to_string();
        let mut task = BackgroundTask {
            id: id.clone(),
            owner_kind: BackgroundOwnerKind::User,
            owner_id: self.owner_id.clone(),
            owner_key: format!("chat-{}", &id[..8]),
            title,
            description: p.description,
            job_kind: BackgroundJobKind::Prompt,
            native_job: None,
            prompt_kind,
            prompt: Some(p.prompt),
            context_url: p.context_url,
            persona: p.persona.clone(),
            agent_folder: p.persona,
            workspace_dir: None,
            use_tools: p.tools,
            mcp_json: None,
            model_id: p.model_id,
            max_turns: p.max_turns,
            timeout_secs: p.timeout_secs,
            continuity: p
                .continuity
                .as_deref()
                .map(BackgroundContinuity::parse)
                .unwrap_or(BackgroundContinuity::Fresh),
            memory_folder: None,
            trigger_type,
            trigger_value: p.trigger_value,
            next_run: None,
            last_run: None,
            overlap_policy: p
                .overlap_policy
                .as_deref()
                .map(OverlapPolicy::parse)
                .unwrap_or(OverlapPolicy::Skip),
            catch_up: false,
            max_failures: 5,
            consecutive_failures: 0,
            visibility: crate::types::BackgroundVisibility::Normal,
            notify: p.notify,
            status,
            created_at: now.clone(),
            updated_at: now,
        };

        task.next_run = match self.compute_first_run(&task) {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };

        if let Err(e) = self.db.upsert_background_task(&task) {
            return ToolResult::err(format!("create: {e}"));
        }

        let mut msg = format!(
            "Created background task \"{}\" ({}).",
            task.title,
            task.trigger_type.as_str()
        );
        if outward {
            msg.push_str(
                "\n\n⚠ It is PAUSED because it can act outside this machine. Enable it in the \
                 Background screen after reviewing it — it will not run until then.",
            );
        } else if let Some(nr) = &task.next_run {
            msg.push_str(&format!("\nFirst run: {nr}."));
        }
        msg.push_str(&format!("\nid: {id}"));
        ToolResult::ok(msg)
    }

    fn list(&self, p: ListParams) -> ToolResult {
        match self.db.list_background_tasks(&BackgroundTaskFilter {
            owner_kind: None,
            owner_id: None,
            status: p.status,
            include_internal: p.include_internal,
            ..Default::default()
        }) {
            Ok(tasks) => {
                let rows: Vec<serde_json::Value> = tasks
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "id": t.id,
                            "title": t.title,
                            "owner": t.owner_kind.as_str(),
                            "trigger": format!("{} {}", t.trigger_type.as_str(),
                                                t.trigger_value.as_deref().unwrap_or("")),
                            "next_run": t.next_run,
                            "status": t.status.as_str(),
                            "consecutive_failures": t.consecutive_failures,
                        })
                    })
                    .collect();
                ToolResult::ok(serde_json::json!({ "tasks": rows }).to_string())
            }
            Err(e) => ToolResult::err(format!("list: {e}")),
        }
    }

    fn get(&self, id: &str) -> ToolResult {
        match self.db.get_background_task(id) {
            Ok(Some(t)) => {
                let runs = self.db.list_background_runs(id, 10).unwrap_or_default();
                ToolResult::ok(
                    serde_json::json!({
                        "task": {
                            "id": t.id, "title": t.title, "status": t.status.as_str(),
                            "owner": t.owner_kind.as_str(), "prompt": t.prompt,
                            "prompt_kind": t.prompt_kind.as_str(),
                            "trigger_type": t.trigger_type.as_str(),
                            "trigger_value": t.trigger_value, "next_run": t.next_run,
                            "last_run": t.last_run, "persona": t.persona,
                            "continuity": t.continuity.as_str(),
                            "consecutive_failures": t.consecutive_failures,
                        },
                        "recent_runs": runs.iter().map(|r| serde_json::json!({
                            "status": r.status.as_str(), "started_at": r.started_at,
                            "duration_ms": r.duration_ms,
                            "result": r.result, "error": r.error,
                        })).collect::<Vec<_>>(),
                    })
                    .to_string(),
                )
            }
            Ok(None) => ToolResult::err(format!("no such task: {id}")),
            Err(e) => ToolResult::err(format!("get: {e}")),
        }
    }

    fn set_status(&self, id: &str, status: BackgroundTaskStatus) -> ToolResult {
        match self.db.get_background_task(id) {
            Ok(Some(_)) => match self.db.set_background_task_status(id, status) {
                Ok(()) => ToolResult::ok(format!("task {id} → {}", status.as_str())),
                Err(e) => ToolResult::err(format!("update status: {e}")),
            },
            Ok(None) => ToolResult::err(format!("no such task: {id}")),
            Err(e) => ToolResult::err(format!("lookup: {e}")),
        }
    }

    fn resume(&self, id: &str) -> ToolResult {
        let task = match self.db.get_background_task(id) {
            Ok(Some(t)) => t,
            Ok(None) => return ToolResult::err(format!("no such task: {id}")),
            Err(e) => return ToolResult::err(format!("lookup: {e}")),
        };
        let next = plan_next_run(&task, Utc::now());
        match self.db.resume_background_task(id, next.as_deref()) {
            Ok(()) => ToolResult::ok(format!("task {id} resumed")),
            Err(e) => ToolResult::err(format!("resume: {e}")),
        }
    }

    fn delete(&self, id: &str) -> ToolResult {
        match self.db.get_background_task(id) {
            Ok(Some(t)) if t.owner_kind != BackgroundOwnerKind::User => {
                ToolResult::err(match t.owner_kind {
                    BackgroundOwnerKind::App => format!(
                        "task belongs to app '{}' — uninstall the app to remove it",
                        t.owner_id
                    ),
                    _ => "task is core upkeep — pause it instead of deleting".into(),
                })
            }
            Ok(Some(_)) => match self.db.delete_background_task(id) {
                Ok(()) => ToolResult::ok(format!("task {id} deleted")),
                Err(e) => ToolResult::err(format!("delete: {e}")),
            },
            Ok(None) => ToolResult::err(format!("no such task: {id}")),
            Err(e) => ToolResult::err(format!("lookup: {e}")),
        }
    }

    /// Cross-process run-now: the scheduler runs inside the daemon, so we can't
    /// execute inline here. Rewind `next_run` to now and the daemon picks it up
    /// on its next tick (a few seconds).
    fn run_now(&self, id: &str) -> ToolResult {
        let task = match self.db.get_background_task(id) {
            Ok(Some(t)) => t,
            Ok(None) => return ToolResult::err(format!("no such task: {id}")),
            Err(e) => return ToolResult::err(format!("lookup: {e}")),
        };
        // A paused task shouldn't silently start running from a run-now.
        let status = if task.status == BackgroundTaskStatus::Active {
            BackgroundTaskStatus::Active
        } else {
            return ToolResult::err(format!(
                "task is {} — resume it before running now",
                task.status.as_str()
            ));
        };
        let now = (Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        match self.db.advance_background_next_run(id, Some(&now), status) {
            Ok(()) => ToolResult::ok(format!(
                "task {id} will run on the next scheduler tick (within a few seconds)"
            )),
            Err(e) => ToolResult::err(format!("run-now: {e}")),
        }
    }

    fn stats(&self, window: &str) -> ToolResult {
        let dur = match window {
            "24h" => chrono::Duration::hours(24),
            "30d" => chrono::Duration::days(30),
            _ => chrono::Duration::days(7),
        };
        let since = (Utc::now() - dur).to_rfc3339();
        let totals = match self.db.background_totals(&since, None) {
            Ok(t) => t,
            Err(e) => return ToolResult::err(format!("stats: {e}")),
        };
        let attention = self.db.background_attention().unwrap_or_default();
        ToolResult::ok(
            serde_json::json!({
                "window": window,
                "runs": totals.runs,
                "success": totals.success,
                "errors": totals.error + totals.timeout,
                "skipped": totals.skipped,
                "success_rate": totals.success_rate,
                "attention": attention.iter().map(|a| serde_json::json!({
                    "title": a.title, "status": a.status,
                    "consecutive_failures": a.consecutive_failures,
                    "last_error": a.last_error,
                })).collect::<Vec<_>>(),
            })
            .to_string(),
        )
    }

    /// First `next_run`: `once` fires at its stated instant, everything else
    /// asks the pure scheduler function so cron/interval semantics live in one
    /// place (mirrors the REST handler's `first_next_run`).
    fn compute_first_run(&self, task: &BackgroundTask) -> Result<Option<String>, String> {
        match task.trigger_type {
            BackgroundTrigger::Once => {
                let raw = task
                    .trigger_value
                    .as_deref()
                    .ok_or("trigger_type 'once' requires an RFC3339 trigger_value")?;
                let at = chrono::DateTime::parse_from_rfc3339(raw)
                    .map_err(|_| format!("trigger_value '{raw}' is not a valid RFC3339 time"))?;
                Ok(Some(at.with_timezone(&Utc).to_rfc3339()))
            }
            BackgroundTrigger::OnInstall | BackgroundTrigger::Manual => Ok(None),
            _ => {
                let next = plan_next_run(task, Utc::now());
                if next.is_none() {
                    return Err(format!(
                        "trigger_value '{}' is not a valid {} expression",
                        task.trigger_value.as_deref().unwrap_or(""),
                        task.trigger_type.as_str()
                    ));
                }
                Ok(next)
            }
        }
    }
}

/// A background task always has a title; when the model omits one, take the
/// first non-empty line of the prompt, clamped to 60 chars on a char boundary
/// (so Vietnamese/multibyte text never splits mid-character).
fn derive_title(prompt: &str) -> String {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("Background task");
    let clamped: String = line.chars().take(60).collect();
    if clamped.is_empty() {
        "Background task".to_owned()
    } else {
        clamped
    }
}

/// A tool that can act outside this machine. Kept in sync with the desktop
/// editor's heuristic (design §10 guard 3). Deliberately broad: the cost of a
/// false positive is one extra click to enable; the cost of a miss is an
/// injection planting a task that emails or posts on its own, forever.
fn is_outward_facing(tool: &str) -> bool {
    let n = tool.to_lowercase();
    n.contains("send")
        || n.contains("browser")
        || n.contains("post")
        || n.contains("mail")
        || n.contains("message")
        || n.contains("crm_")
        || n.contains("moltbook")
}

/// Start the background MCP server over stdio. Reads config from the env set by
/// [`crate::mcp::helper::background_mcp_config`].
pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server = McpBackgroundServer::from_env()?
        .context("SENCLAW_DB_PATH / SENCLAW_GROUP_FOLDER / SENCLAW_CHAT_JID not set")?;
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{derive_title, is_outward_facing};

    #[test]
    fn derive_title_uses_first_prompt_line() {
        assert_eq!(
            derive_title("Dọn dẹp tri thức\nvà báo cáo"),
            "Dọn dẹp tri thức"
        );
    }

    #[test]
    fn derive_title_skips_leading_blank_lines() {
        assert_eq!(
            derive_title("\n\n  Rà khách quá hạn  \n"),
            "Rà khách quá hạn"
        );
    }

    #[test]
    fn derive_title_clamps_on_a_char_boundary() {
        // 80 multibyte chars must clamp to 60 chars without panicking.
        let long = "á".repeat(80);
        assert_eq!(derive_title(&long).chars().count(), 60);
    }

    #[test]
    fn derive_title_never_empty() {
        assert_eq!(derive_title("   "), "Background task");
        assert_eq!(derive_title(""), "Background task");
    }

    #[test]
    fn outward_facing_covers_the_dangerous_tools() {
        for t in [
            "mcp__senclaw-send__send_message",
            "mcp__senclaw-browser__browser_navigate",
            "mcp__crm-mcp__crm_log_interaction",
            "mcp__moltbook-mcp__moltbook_post",
            "SendReply",
        ] {
            assert!(is_outward_facing(t), "{t} should be outward-facing");
        }
    }

    #[test]
    fn read_only_tools_are_not_outward_facing() {
        for t in [
            "Read",
            "Grep",
            "mcp__senclaw-memory__memory_search",
            "mcp__senclaw-cognitive__cog_recall",
        ] {
            assert!(!is_outward_facing(t), "{t} should be inward");
        }
    }
}
