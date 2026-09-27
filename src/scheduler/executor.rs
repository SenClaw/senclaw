//! Task executor implementations.
//!
//! Handles the 5 context modes: notify / script / script-agent / isolated / group.
//! Mirrors `src-old/scheduler/TaskScheduler.ts` executor behaviour.

use std::process::Command;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::{debug, info, warn};

use crate::db::Db;
use crate::scheduler::task_scheduler::TaskExecutor;
use crate::types::{AgentApi, ContextMode, RunStatus, ScheduledTask, TaskRunLogInsert};

/// Executor that handles each context mode appropriately.
pub struct DefaultTaskExecutor {
    db: Arc<Db>,
    /// Used by `ContextMode::Group` to dispatch the scheduled prompt into the
    /// owning chat session. `None` falls back to a stub log (useful in tests).
    agent_api: Option<Arc<dyn AgentApi>>,
    /// Used by `ContextMode::Watch` to re-invoke the probe tool without an LLM.
    /// `None` degrades every watch to its agent-turn fallback, which is correct
    /// rather than fatal: the watch still fires, it just costs a turn.
    mcp_manager: Option<Arc<crate::mcp::manager::McpManager>>,
}

impl DefaultTaskExecutor {
    pub fn new(db: Arc<Db>) -> Self {
        Self {
            db,
            agent_api: None,
            mcp_manager: None,
        }
    }

    pub fn with_agent_api(mut self, api: Arc<dyn AgentApi>) -> Self {
        self.agent_api = Some(api);
        self
    }

    pub fn with_mcp_manager(mut self, mgr: Arc<crate::mcp::manager::McpManager>) -> Self {
        self.mcp_manager = Some(mgr);
        self
    }
}

/// Run a scheduled shell command, honouring Settings → Sandbox → "script".
///
/// Enforcement ON: the command runs inside a throwaway OS sandbox (write-jail,
/// kill-enforced 10-minute ceiling; network per the policy switch). When the
/// switch is on but the engine cannot run, the task FAILS rather than silently
/// dropping to a raw shell. Enforcement OFF: raw `bash -c`, the historical
/// behaviour.
///
/// Returns `(stdout, stderr, exit_code, isolation)` — `isolation` is `"none"`
/// on the legacy path so run logs always say what confined the script.
async fn run_script_command(cmd: &str) -> anyhow::Result<(String, String, Option<i32>, String)> {
    let policy = crate::sandbox::policy::current();
    if policy.scheduler_script {
        let run = crate::sandbox::policy::run_once_sandboxed(
            "bash",
            cmd,
            policy.scheduler_network,
            Some(crate::sandbox::backend::MAX_TIMEOUT_MS as i64),
        )
        .await?;
        let mut stderr = run.stderr;
        if run.timed_out {
            if !stderr.is_empty() {
                stderr.push('\n');
            }
            stderr.push_str("(killed: sandbox deadline reached)");
        }
        return Ok((
            run.stdout,
            stderr,
            run.exit_code.map(|c| c as i32),
            run.isolation,
        ));
    }
    let output = Command::new("bash").arg("-c").arg(cmd).output()?;
    Ok((
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
        output.status.code(),
        "none".to_string(),
    ))
}

#[async_trait]
impl TaskExecutor for DefaultTaskExecutor {
    async fn execute(&self, task: ScheduledTask) {
        let task_id = task.id.clone();

        let result = match task.context_mode {
            ContextMode::Notify => self.execute_notify(&task).await,
            ContextMode::Script => self.execute_script(&task).await,
            ContextMode::ScriptAgent => self.execute_script_agent(&task).await,
            ContextMode::Isolated => {
                info!(
                    task_id = %task.id,
                    group_folder = %task.group_folder,
                    "[TaskScheduler] isolated task (will be dispatched as a fresh session when agent pool is wired)"
                );
                Ok(format!("[isolated] task queued: {}", task.prompt))
            }
            ContextMode::Group => self.execute_group(&task).await,
            ContextMode::Watch => self.execute_watch(&task).await,
        };

        let now = chrono::Utc::now().to_rfc3339();
        match result {
            Ok(output) => {
                debug!(task_id = %task_id, "[TaskScheduler] completed");
                if let Err(e) = self.db.insert_task_run_log(&TaskRunLogInsert {
                    task_id: task_id.clone(),
                    run_at: now,
                    duration_ms: None,
                    status: RunStatus::Success,
                    result: Some(output),
                    error: None,
                }) {
                    warn!(task_id = %task_id, error = %e, "[TaskScheduler] failed to record result");
                }
            }
            Err(e) => {
                warn!(task_id = %task_id, error = %e, "[TaskScheduler] failed");
                let err_msg = format!("{e:#}");
                if let Err(e2) = self.db.insert_task_run_log(&TaskRunLogInsert {
                    task_id: task_id.clone(),
                    run_at: now,
                    duration_ms: None,
                    status: RunStatus::Error,
                    result: None,
                    error: Some(err_msg),
                }) {
                    warn!(task_id = %task_id, error = %e2, "[TaskScheduler] failed to record error");
                }
            }
        }
    }
}

impl DefaultTaskExecutor {
    /// Notify mode: just record the task result.
    async fn execute_notify(&self, task: &ScheduledTask) -> anyhow::Result<String> {
        info!(
            task_id = %task.id,
            "[TaskScheduler] notify: {}",
            task.prompt
        );
        Ok(format!("[notify] {}", task.prompt))
    }

    /// Script mode: execute a shell command.
    async fn execute_script(&self, task: &ScheduledTask) -> anyhow::Result<String> {
        let cmd = task.script_command.as_deref().unwrap_or(&task.prompt);
        info!(
            task_id = %task.id,
            command = %cmd,
            "[TaskScheduler] script"
        );

        let (stdout, stderr, exit_code, _isolation) = run_script_command(cmd).await?;

        let mut result = String::new();
        if !stdout.is_empty() {
            result.push_str(&stdout);
        }
        if !stderr.is_empty() {
            if !result.is_empty() {
                result.push('\n');
            }
            result.push_str(&stderr);
        }

        if exit_code != Some(0) {
            result.push_str(&format!("\nExit code: {}", exit_code.unwrap_or(-1)));
        }

        Ok(result)
    }

    /// Script-agent mode: shell output is fed back to the agent (stub).
    async fn execute_script_agent(&self, task: &ScheduledTask) -> anyhow::Result<String> {
        let cmd = task.script_command.as_deref().unwrap_or(&task.prompt);
        info!(
            task_id = %task.id,
            command = %cmd,
            "[TaskScheduler] script-agent"
        );

        let (stdout, stderr, exit_code, _isolation) = run_script_command(cmd).await?;

        let mut result = format!("Script output:\n{stdout}");
        if !stderr.is_empty() {
            result.push_str(&format!("\n\nStderr:\n{stderr}"));
        }
        if exit_code != Some(0) {
            result.push_str(&format!("\n\nExit code: {}", exit_code.unwrap_or(-1)));
        }

        // In full implementation: feed this output to the agent for interpretation.
        info!(
            task_id = %task.id,
            "[TaskScheduler] script-agent output ready (agent feed-back will be wired when agent pool is integrated)"
        );

        Ok(result)
    }

    /// Watch mode: re-check a condition, and resume the chat only once it holds.
    ///
    /// The tick is deliberately cheap — one MCP tool call, no LLM — so a watch
    /// polling for an hour costs the tool calls and nothing else. An agent turn
    /// is spent only on the tick that resolves, or on the one that gives up.
    ///
    /// Retiring the row is this function's job. The poll loop advances
    /// `next_run` *before* handing over, so a watch that just returns `Ok` is
    /// automatically re-armed; ending one means marking it completed here.
    async fn execute_watch(&self, task: &ScheduledTask) -> anyhow::Result<String> {
        let Some(raw) = task.watch_json.as_deref() else {
            self.retire_watch(task);
            anyhow::bail!("watch task has no watch config");
        };
        let mut cfg = match crate::scheduler::watch::WatchConfig::parse(raw) {
            Ok(c) => c,
            Err(e) => {
                // An unparseable config cannot be fixed by polling it again.
                self.retire_watch(task);
                anyhow::bail!("watch config unreadable: {e}");
            }
        };

        // Deadline first: an expired watch must not spend a tool call.
        if let Some(reason) = cfg.exhausted(chrono::Utc::now()) {
            return self
                .finish_watch(task, &cfg.render_timeout(&reason), &reason)
                .await;
        }

        // No probe declared → the agent-turn fallback. Wake the chat and let the
        // agent check by whatever means the job actually needs, and decide for
        // itself whether to keep waiting.
        if cfg.is_agent_fallback() || self.mcp_manager.is_none() {
            cfg.checks += 1;
            self.persist_watch(task, &cfg);
            info!(
                task_id = %task.id,
                checks = cfg.checks,
                "[TaskScheduler] watch: agent fallback, waking chat"
            );
            return self.dispatch_into_chat(task, &cfg.resume_prompt).await;
        }

        let tool = cfg.tool.clone().unwrap_or_default();
        let manager = self.mcp_manager.as_ref().expect("checked above");
        let probe = manager.call_external_tool(&tool, cfg.args.clone()).await;

        match probe {
            Err(e) => {
                let err = e.to_string();
                match cfg.record_error(&err) {
                    Some(reason) => {
                        self.finish_watch(task, &cfg.render_timeout(&reason), &reason)
                            .await
                    }
                    None => {
                        // Transient: a stopped session Space App is the resting
                        // state, not a fault. Keep the watch alive.
                        self.persist_watch(task, &cfg);
                        warn!(
                            task_id = %task.id,
                            streak = cfg.error_streak,
                            error = %err,
                            "[TaskScheduler] watch: probe failed, retrying"
                        );
                        Ok(format!("watch probe error ({}): {err}", cfg.error_streak))
                    }
                }
            }
            Ok(raw_result) => {
                // A JSON-RPC call can succeed while the tool itself refuses.
                // Without this the refusal unwraps to a plain string, no path
                // resolves on it, and the watch reads "not finished yet" —
                // re-asking a question already answered "no" until the deadline.
                if let Some(msg) = crate::scheduler::watch::tool_error_text(&raw_result) {
                    let full = format!("{tool} rejected the call: {msg}");
                    return match cfg.record_error_kind(&full, true) {
                        Some(reason) => {
                            warn!(
                                task_id = %task.id,
                                error = %full,
                                "[TaskScheduler] watch: tool rejected the probe, giving up"
                            );
                            self.persist_watch(task, &cfg);
                            self.finish_watch(task, &cfg.render_timeout(&reason), &reason)
                                .await
                        }
                        None => {
                            self.persist_watch(task, &cfg);
                            Ok(format!("watch probe rejected: {full}"))
                        }
                    };
                }
                let value = crate::scheduler::watch::normalize_tool_result(&raw_result);
                match cfg.record_probe(&value) {
                    crate::scheduler::watch::WatchOutcome::Done { prompt } => {
                        info!(
                            task_id = %task.id,
                            checks = cfg.checks,
                            "[TaskScheduler] watch: condition met, resuming chat"
                        );
                        // Persist the final counters before retiring. Without
                        // this the resolving probe is never written back, and a
                        // watch that answered on its first check is recorded as
                        // having made zero — which is exactly what made a
                        // premature match look like it had never run at all.
                        self.persist_watch(task, &cfg);
                        self.finish_watch(task, &prompt, "condition met").await
                    }
                    crate::scheduler::watch::WatchOutcome::Pending { checks } => {
                        self.persist_watch(task, &cfg);
                        debug!(
                            task_id = %task.id,
                            checks,
                            "[TaskScheduler] watch: not ready yet"
                        );
                        Ok(format!("watch pending (check {checks})"))
                    }
                    crate::scheduler::watch::WatchOutcome::GaveUp { prompt, reason } => {
                        self.persist_watch(task, &cfg);
                        self.finish_watch(task, &prompt, &reason).await
                    }
                }
            }
        }
    }

    /// Retire the watch, then say `prompt` into its chat. Retiring first means a
    /// slow agent turn cannot let the next poll pick the same watch up again.
    async fn finish_watch(
        &self,
        task: &ScheduledTask,
        prompt: &str,
        reason: &str,
    ) -> anyhow::Result<String> {
        self.retire_watch(task);
        let reply = self.dispatch_into_chat(task, prompt).await?;
        Ok(format!("watch finished ({reason}): {reply}"))
    }

    fn retire_watch(&self, task: &ScheduledTask) {
        if let Err(e) = self
            .db
            .update_task_status(&task.id, crate::types::TaskStatus::Completed)
        {
            warn!(task_id = %task.id, error = %e, "[TaskScheduler] watch: retire failed");
        }
    }

    /// Persist the mutated counters. A failure here is logged, not fatal: the
    /// watch simply re-checks from a stale count, which is far better than
    /// dropping the watch entirely.
    fn persist_watch(&self, task: &ScheduledTask, cfg: &crate::scheduler::watch::WatchConfig) {
        match cfg.to_json() {
            Ok(json) => {
                if let Err(e) = self.db.update_task_watch_json(&task.id, &json) {
                    warn!(task_id = %task.id, error = %e, "[TaskScheduler] watch: persist failed");
                }
            }
            Err(e) => {
                warn!(task_id = %task.id, error = %e, "[TaskScheduler] watch: serialise failed")
            }
        }
    }

    /// Group mode: dispatch the prompt as an agent run on the schedule's chat
    /// session. Agent replies stream through `broadcast_reply` and land in the
    /// existing chat history (channel_messages + WS push), so the recurring
    /// schedule's chat view shows live output.
    async fn execute_group(&self, task: &ScheduledTask) -> anyhow::Result<String> {
        self.dispatch_into_chat(task, &task.prompt).await
    }

    /// Wake the task's chat session with `prompt` and return the agent's reply.
    ///
    /// Shared by `group` and `watch`: both mean "say this into that chat as an
    /// agent turn", and a watch that resolved is exactly a group run whose
    /// prompt was written by the probe rather than by the user.
    async fn dispatch_into_chat(
        &self,
        task: &ScheduledTask,
        prompt: &str,
    ) -> anyhow::Result<String> {
        let api = match &self.agent_api {
            Some(a) => a,
            None => {
                info!(
                    task_id = %task.id,
                    chat_jid = %task.chat_jid,
                    "[TaskScheduler] group task: agent api not wired, logging only"
                );
                return Ok(format!("[group:stub] {prompt}"));
            }
        };
        let group = match self.db.get_group(&task.chat_jid) {
            Ok(Some(g)) => g,
            Ok(None) => {
                // Self-heal: the schedule's chat session can go missing (e.g. an
                // older build's config reconciliation wiped it). Recreate a
                // minimal binding from the task so the recurring schedule keeps
                // running instead of failing forever with "chat session not found".
                warn!(
                    task_id = %task.id,
                    chat_jid = %task.chat_jid,
                    "[TaskScheduler] group task: chat session missing — recreating from task"
                );
                let now = chrono::Utc::now().to_rfc3339();
                let binding = crate::types::GroupBinding {
                    jid: task.chat_jid.clone(),
                    folder: task.group_folder.clone(),
                    name: prompt.chars().take(60).collect::<String>(),
                    channel: String::new(),
                    group_type: "chat".into(),
                    requires_trigger: false,
                    allowed_tools: None,
                    allowed_paths: None,
                    allowed_work_dirs: None,
                    bot_token: None,
                    max_messages: None,
                    llm_config_id: None,
                    last_active: Some(now.clone()),
                    added_at: now,
                };
                self.db.upsert_group(&binding)?;
                binding
            }
            Err(e) => anyhow::bail!("db error: {e}"),
        };
        info!(
            task_id = %task.id,
            chat_jid = %task.chat_jid,
            "[TaskScheduler] group task: dispatching to agent"
        );
        api.process_and_wait(&task.chat_jid, &group, prompt).await?;
        let reply = api
            .get_last_reply_text(&task.chat_jid)
            .unwrap_or_else(|| "(no reply)".into());
        Ok(reply)
    }
}
