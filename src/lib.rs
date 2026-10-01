//! SenClaw — multi-group AI gateway (Rust port).
//!
//! Module layout mirrors the original TypeScript tree under `src-old/`.
//! The daemon boot sequence (`run_daemon`) follows `src-old/index.ts`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;

#[macro_use]
pub mod safe_log;

pub mod acp;

pub mod agent;
pub mod apps;
pub mod background;
pub mod browser;
pub mod browser_agent;
pub mod build_info;
pub mod channels;
pub mod checkpoints;
pub mod clawhub;
pub mod cli;
pub mod config;
pub mod control_plane;
pub mod db;
pub mod decision;
pub mod docs;
pub mod failures;
pub mod gateway;
pub mod kanban;
pub mod kits;
pub mod local_models;
pub mod lsp;
pub mod marketplace;
pub mod mcp;
pub mod memory;
pub mod patterns;
pub mod plugins;
pub mod providers;
pub mod repo_map;
pub mod runtime;
pub mod sandbox;
pub mod scaffold;
pub mod scheduler;
pub mod security;
pub mod setup;
pub mod skills;
pub mod subagents;
pub mod tools;
pub mod trajectory;
pub mod types;
pub mod usage;
pub mod user_profile;
pub mod util;
pub mod widgets;
pub mod wiki;
pub mod workflow;
pub mod worktree;
pub mod zen_core;

use channels::Channel;

/// Boot the SenClaw daemon. Mirrors `src-old/index.ts`.
///
/// Startup sequence:
///   1. SQLite init (WAL, schema, memory tables)
///   2. GroupManager — load group bindings from DB + config.json
///   3. Channel adapters connect (Telegram → Feishu → WeChat)
///   4. AgentPool + GroupQueue + MessageRouter — blocked by sema-core
///   5. TaskScheduler — wired for standalone task execution
///   6. DispatchBridge, PersonaRegistry, VirtualWorkerPool
///   7. WebSocketGateway + UIServer — axum server
///   8. WikiManager + builtin personas
///   9. Graceful shutdown on SIGINT/SIGTERM
// ===== RealWsApi: bridges WS messages → GroupQueue → AgentPool =====

struct RealWsApi {
    group_queue: Arc<agent::group_queue::GroupQueue>,
    agent_pool: Arc<agent::agent_pool::AgentPool>,
    db: Arc<db::Db>,
}

struct RealPermissionApi {
    agent_pool: Arc<agent::agent_pool::AgentPool>,
    /// Pending virtual-agent permission responses: key = "virtual_jid::tool_name"
    virtual_perm_senders: Arc<Mutex<HashMap<String, std::sync::mpsc::SyncSender<String>>>>,
    db: Arc<db::Db>,
}

impl agent::permission_bridge::PermissionBridgeApi for RealPermissionApi {
    fn persist_tool_rule(&self, rule: &agent::permission_bridge::types::ToolAutoAcceptRule) {
        persist_tool_rule(&self.db, rule);
    }

    fn is_web_jid(&self, chat_jid: &str) -> bool {
        // virtual: jids are also "web-style" — they broadcast to admins and have no
        // channel buttons, so they follow the same code path as web: jids.
        chat_jid.starts_with("web:") || chat_jid.starts_with("virtual:")
    }

    fn respond_to_tool_permission(&self, group_jid: &str, tool_name: &str, selected: &str) {
        if group_jid.starts_with("virtual:") {
            // Deliver response to the waiting virtual agent thread via mpsc.
            let key = format!("{group_jid}::{tool_name}");
            if let Some(tx) = self.virtual_perm_senders.lock().unwrap().remove(&key) {
                let _: Result<(), std::sync::mpsc::SendError<String>> =
                    tx.send(selected.to_string());
            } else {
                tracing::warn!(
                    "[RealPermissionApi] no waiting sender for virtual permission: jid={group_jid} tool={tool_name}"
                );
            }
            return;
        }
        self.agent_pool
            .respond_to_tool_permission(group_jid, tool_name, selected);
    }

    fn respond_to_ask_question(
        &self,
        group_jid: &str,
        agent_id: &str,
        answers: HashMap<String, String>,
    ) {
        self.agent_pool
            .respond_to_ask_question(group_jid, agent_id, answers);
    }

    fn respond_to_form(
        &self,
        group_jid: &str,
        agent_id: &str,
        values: HashMap<String, serde_json::Value>,
        submitted: bool,
    ) {
        self.agent_pool
            .respond_to_form(group_jid, agent_id, values, submitted);
    }
}

/// Convert a JSON object of form values into the map shape AgentPool expects.
fn form_values_map(values: &serde_json::Value) -> HashMap<String, serde_json::Value> {
    values
        .as_object()
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

#[async_trait]
impl gateway::websocket_gateway::WsGatewayApi for RealWsApi {
    fn enqueue_and_process(
        &self,
        group_jid: &str,
        group: &crate::types::GroupBinding,
        text: &str,
        attachments: &[crate::types::MessageAttachment],
    ) {
        // Mid-turn fast path: while this group's agent is processing, text-only
        // inputs go into the engine's pending queue — they get appended to the
        // running turn's tool results (or chained as the next turn) instead of
        // waiting behind the whole turn in the GroupQueue. Image messages keep
        // the full path (the queue is text-only).
        if attachments.is_empty() && self.agent_pool.queue_input_if_processing(group_jid, text) {
            return;
        }
        let agent_pool = Arc::clone(&self.agent_pool);
        let jid = group_jid.to_string();
        let g = group.clone();
        let t = text.to_string();
        let att = attachments.to_vec();
        let gq = Arc::clone(&self.group_queue);
        let jid_key = jid.clone();
        tokio::spawn(async move {
            gq.enqueue(
                &jid_key,
                Box::pin(async move {
                    let _ = types::AgentApi::process_and_wait_with_attachments(
                        agent_pool.as_ref(),
                        &jid,
                        &g,
                        &t,
                        &att,
                    )
                    .await;
                }),
            )
            .await;
        });
    }

    fn pause_agent(&self, group_jid: &str) {
        self.agent_pool.pause_agent(group_jid);
    }

    fn resolve_permission(&self, request_id: &str, option_key: &str) {
        let _ = self.agent_pool.resolve_permission(request_id, option_key);
    }

    fn add_tool_rule(&self, rule: crate::agent::permission_bridge::types::ToolAutoAcceptRule) {
        persist_tool_rule(&self.db, &rule);
        if let Some(bridge) = self.agent_pool.permission_bridge() {
            bridge.add_rule(rule);
        }
    }

    fn remove_tool_rule(&self, rule_id: &str) {
        if let Err(e) = self.db.delete_tool_rule(rule_id) {
            tracing::warn!(error = %e, rule_id, "[ToolRules] failed to delete from DB");
        }
        if let Some(bridge) = self.agent_pool.permission_bridge() {
            bridge.remove_rule(rule_id);
        }
    }

    fn update_tool_rule(&self, rule: crate::agent::permission_bridge::types::ToolAutoAcceptRule) {
        persist_tool_rule(&self.db, &rule);
        if let Some(bridge) = self.agent_pool.permission_bridge() {
            bridge.update_rule(rule);
        }
    }

    fn set_accept_all(&self, enabled: bool) {
        if let Some(bridge) = self.agent_pool.permission_bridge() {
            bridge.set_accept_all(enabled);
        }
    }

    fn get_tool_rules(&self) -> Vec<crate::agent::permission_bridge::types::ToolAutoAcceptRule> {
        self.agent_pool
            .permission_bridge()
            .map(|b| b.get_rules())
            .unwrap_or_default()
    }

    fn resolve_ask_question(
        &self,
        request_id: &str,
        answers: &serde_json::Value,
        other_texts: Option<&serde_json::Value>,
    ) {
        let _ = self
            .agent_pool
            .resolve_ask_question_batch(request_id, answers, other_texts);
    }

    fn resolve_form(&self, request_id: &str, values: &serde_json::Value, submitted: bool) {
        let _ = self
            .agent_pool
            .resolve_form(request_id, form_values_map(values), submitted);
    }

    fn resume_agent(&self, group_jid: &str, query: Option<&str>) {
        self.agent_pool.resume_agent(group_jid, query);
    }

    async fn stop_agent(&self, group_jid: &str) {
        self.agent_pool.stop_agent(group_jid).await;
    }

    fn clear_llm_session_history(&self, group_jid: &str) {
        self.agent_pool.clear_llm_session_history(group_jid);
    }

    fn compact_agent(&self, group_jid: &str) {
        self.agent_pool.force_compact(group_jid);
    }

    fn set_agent_mode(&self, group_jid: &str, mode: &str) {
        self.agent_pool.set_agent_mode(group_jid, mode);
    }

    fn get_agent_mode(&self, group_jid: &str) -> Option<String> {
        self.agent_pool.get_agent_mode(group_jid)
    }

    fn resolve_plan_exit(&self, group_jid: &str, agent_id: &str, selected: &str) {
        tracing::info!(
            "[Plans] resolve_plan_exit jid={group_jid} agent={agent_id} selected={selected}"
        );
        self.agent_pool
            .resolve_plan_exit(group_jid, agent_id, selected);
        // Persist the approval outcome on the most recent pending plan for
        // this chat so the Plan History panel reflects accepted/rejected
        // state (the row was inserted as "pending" at request time).
        let approval = match selected {
            "startEditing" | "clearContextAndStart" => selected,
            _ => "cancelled",
        };
        if let Ok(plans) = self.db.list_plans_for_chat(group_jid, Some(1)) {
            if let Some(p) = plans.first() {
                if p.approval == "pending" {
                    let ts = chrono::Utc::now().to_rfc3339();
                    if let Err(e) = self.db.update_plan_approval(&p.id, approval, &ts) {
                        tracing::warn!(error = %e, plan_id = %p.id, "[Plan] update approval failed");
                    }
                }
            }
        }
    }

    /// Snapshot of all dispatch parents — sent to admin clients on subscribe.
    fn get_dispatch_parents(&self) -> serde_json::Value {
        let bridge = self.agent_pool.dispatch_bridge_snapshot();
        let parents = match bridge {
            Some(b) => b.get_parents(),
            None => Vec::new(),
        };
        serde_json::to_value(
            parents
                .iter()
                .map(dispatch_parent_to_json)
                .collect::<Vec<_>>(),
        )
        .unwrap_or(serde_json::Value::Null)
    }

    fn dismiss_agent_todos(&self, agent_jid: &str) {
        self.agent_pool.dismiss_cached_todos(agent_jid);
    }

    /// Snapshot of cached agent todos — sent to admin clients on subscribe.
    fn get_agent_todos(&self) -> serde_json::Value {
        let cached = self.agent_pool.get_all_cached_todos();
        let map: serde_json::Map<String, serde_json::Value> = cached
            .into_iter()
            .map(|(jid, entry)| {
                (
                    jid,
                    serde_json::to_value(entry).unwrap_or(serde_json::Value::Null),
                )
            })
            .collect();
        serde_json::Value::Object(map)
    }

    /// Snapshot of per-agent tool rosters — sent to admin clients on subscribe
    /// so the Agent Console can render currently-online agents and their tools.
    fn get_agent_tools(&self) -> serde_json::Value {
        let cached = self.agent_pool.get_all_cached_tools();
        let map: serde_json::Map<String, serde_json::Value> = cached
            .into_iter()
            .map(|(jid, entry)| {
                (
                    jid,
                    serde_json::to_value(entry).unwrap_or(serde_json::Value::Null),
                )
            })
            .collect();
        serde_json::Value::Object(map)
    }
}

fn dispatch_parent_to_json(p: &agent::dispatch_bridge::DispatchParent) -> serde_json::Value {
    serde_json::json!({
        "id": p.id,
        "goal": p.goal,
        "adminFolder": p.admin_folder,
        "sharedWorkspace": p.shared_workspace,
        "status": p.status,
        "createdAt": p.created_at,
        "completedAt": p.completed_at,
        "tasks": p.tasks.iter().map(|t| serde_json::json!({
            "id": t.id,
            "label": t.label,
            "agentId": t.agent_id,
            "agentJid": t.agent_jid,
            "dependsOn": t.depends_on,
            "prompt": t.prompt,
            "status": t.status.label(),
            "result": t.result,
            "createdAt": t.created_at,
            "startedAt": t.started_at,
            "timeoutAt": t.timeout_at,
            "completedAt": t.completed_at,
            "isVirtual": t.is_virtual,
            "personaName": t.persona_name,
        })).collect::<Vec<_>>(),
    })
}

// ===== WsAgentEventSink: forwards AgentPool events → WebSocket gateway =====

struct WsAgentEventSink {
    gateway: Arc<gateway::websocket_gateway::WebSocketGateway>,
    db: Arc<db::Db>,
}

/// `true` when the tool name corresponds to a Space-calendar mutation
/// (create/update/delete/set_reminder). Matches the MCP-prefixed names
/// emitted by `senclaw-space` plus the bare tool names used in tests. Used
/// to kick the frontend calendar so it re-fetches after an agent mutates
/// events through the MCP — without this the UI keeps showing stale data
/// while the row is already in the DB.
fn is_space_mutation_tool(tool_name: &str) -> bool {
    const MUTATIONS: &[&str] = &[
        "event_create",
        "event_update",
        "event_delete",
        "event_set_reminder",
        "event_set_renotify",
    ];
    // The MCP routes through `mcp__<server>__<tool>` (TS) or
    // `mcp__senclaw-space__<tool>` (Rust) — accept both.
    if let Some(suffix) = tool_name.rsplit("__").next() {
        if MUTATIONS.contains(&suffix) {
            return true;
        }
    }
    // Some adapters lowercase the server name — keep a broad check.
    MUTATIONS.iter().any(|m| tool_name.ends_with(m))
}

#[cfg(test)]
mod space_mutation_classifier_tests {
    use super::is_space_mutation_tool;

    #[test]
    fn accepts_mcp_prefixed_create_and_delete() {
        assert!(is_space_mutation_tool("mcp__senclaw-space__event_create"));
        assert!(is_space_mutation_tool("mcp__senclaw-space__event_update"));
        assert!(is_space_mutation_tool("mcp__senclaw-space__event_delete"));
    }

    #[test]
    fn rejects_read_only_tools() {
        // Listing / searching events MUST NOT trigger a refresh broadcast —
        // it would loop the frontend (refresh → list → refresh → ...).
        assert!(!is_space_mutation_tool("mcp__senclaw-space__event_list"));
        assert!(!is_space_mutation_tool("mcp__senclaw-space__event_search"));
        assert!(!is_space_mutation_tool("mcp__senclaw-space__event_get"));
    }

    #[test]
    fn rejects_unrelated_tools() {
        assert!(!is_space_mutation_tool("Read"));
        assert!(!is_space_mutation_tool("Bash"));
        assert!(!is_space_mutation_tool("mcp__senclaw-memory__add"));
    }
}

/// Best-effort persistence of an `ExitPlanMode` plan request. Writes the
/// markdown to `<plans_dir>/<chat>-<ts>.md` and inserts a `plans` row.
/// Both operations are logged on failure but never block.
fn persist_plan(
    db: &Arc<db::Db>,
    plans_dir: &std::path::Path,
    chat_jid: &str,
    agent_id: &str,
    content_md: &str,
) {
    if let Err(e) = std::fs::create_dir_all(plans_dir) {
        tracing::warn!(error = %e, dir = %plans_dir.display(), "[Plans] mkdir failed");
        return;
    }
    let id = uuid::Uuid::new_v4().to_string();
    let ts = chrono::Utc::now().to_rfc3339();
    // Sanitize jid for filename (replace ':' and '/').
    let safe_jid = chat_jid.replace([':', '/'], "_");
    let stem = format!("{safe_jid}-{}", ts.replace([':', '.'], "_"));
    let file_path = plans_dir.join(format!("{stem}.md"));
    if let Err(e) = std::fs::write(&file_path, content_md) {
        tracing::warn!(error = %e, path = %file_path.display(), "[Plans] write file failed");
        // continue — DB insert still useful even if disk write failed
    }
    // Derive a short title from the first markdown heading or first line.
    let title = content_md
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().trim_start_matches('#').trim().to_string())
        .unwrap_or_default();
    let plan = db::plans::StoredPlan {
        id,
        chat_jid: chat_jid.to_string(),
        agent_id: agent_id.to_string(),
        title,
        file_path: file_path.to_string_lossy().to_string(),
        content_md: content_md.to_string(),
        approval: "pending".to_string(),
        created_at: ts,
        approved_at: None,
    };
    if let Err(e) = db.insert_plan(&plan) {
        tracing::warn!(error = %e, "[Plans] DB insert failed");
    } else {
        tracing::info!(
            "[Plans] persisted plan id={} chat={} title=\"{}\" file={}",
            plan.id,
            chat_jid,
            plan.title,
            file_path.display()
        );
    }
}

/// Best-effort persistence of a tool auto-accept rule. We log and swallow
/// failures so a transient DB error never blocks the in-memory rule update.
fn persist_tool_rule(
    db: &Arc<db::Db>,
    rule: &crate::agent::permission_bridge::types::ToolAutoAcceptRule,
) {
    let json = match serde_json::to_string(rule) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, rule_id = %rule.id, "[ToolRules] serialize failed");
            return;
        }
    };
    let ts = chrono::Utc::now().to_rfc3339();
    if let Err(e) = db.upsert_tool_rule(&rule.id, &json, &ts) {
        tracing::warn!(error = %e, rule_id = %rule.id, "[ToolRules] DB upsert failed");
    }
}

/// Best-effort persistence of an ephemeral chat event. Logs and swallows
/// failures so a transient DB error never blocks the live broadcast.
fn persist_chat_event(
    db: &Arc<db::Db>,
    chat_jid: &str,
    event_type: &str,
    request_id: Option<&str>,
    payload: &serde_json::Value,
) {
    let payload_json = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    let ts = chrono::Utc::now().to_rfc3339();
    if let Err(e) = db.insert_chat_event(chat_jid, event_type, request_id, &payload_json, &ts) {
        tracing::warn!(
            error = %e, chat_jid = %chat_jid, event = %event_type,
            "[WsAgentEventSink] failed to persist chat event; live broadcast continues"
        );
    }
}

// ===== WsBackgroundEventSink: forwards background runs → WebSocket gateway =====

/// Bridges the sync [`background::BackgroundEventSink`] trait onto the async
/// gateway, same shape as [`WsAgentEventSink`].
struct WsBackgroundEventSink {
    gateway: Arc<gateway::websocket_gateway::WebSocketGateway>,
}

impl background::BackgroundEventSink for WsBackgroundEventSink {
    fn run_started(
        &self,
        task: &types::BackgroundTask,
        run_id: &str,
        trigger: types::BackgroundTriggerKind,
    ) {
        let gw = Arc::clone(&self.gateway);
        let (task_id, run_id, title, trigger) = (
            task.id.clone(),
            run_id.to_string(),
            task.title.clone(),
            trigger.as_str().to_string(),
        );
        tokio::spawn(async move {
            gw.notify_background_run_started(&task_id, &run_id, &title, &trigger)
                .await;
        });
    }

    fn run_activity(&self, task_id: &str, run_id: &str, kind: &str, detail: &str) {
        let gw = Arc::clone(&self.gateway);
        let (task_id, run_id, kind, detail) = (
            task_id.to_string(),
            run_id.to_string(),
            kind.to_string(),
            detail.to_string(),
        );
        tokio::spawn(async move {
            gw.notify_background_run_activity(&task_id, &run_id, &kind, &detail)
                .await;
        });
    }

    fn run_finished(
        &self,
        task_id: &str,
        run_id: &str,
        status: types::BackgroundRunStatus,
        duration_ms: i64,
        error: Option<&str>,
    ) {
        let gw = Arc::clone(&self.gateway);
        let (task_id, run_id, status, error) = (
            task_id.to_string(),
            run_id.to_string(),
            status.as_str().to_string(),
            error.map(str::to_string),
        );
        tokio::spawn(async move {
            gw.notify_background_run_finished(
                &task_id,
                &run_id,
                &status,
                duration_ms,
                error.as_deref(),
            )
            .await;
        });
    }

    fn task_changed(&self, task: &types::BackgroundTask) {
        let gw = Arc::clone(&self.gateway);
        let task = task.clone();
        tokio::spawn(async move {
            gw.notify_background_task_changed(&task).await;
        });
    }

    fn notify(&self, title: &str, message: &str) {
        let gw = Arc::clone(&self.gateway);
        let (title, message) = (title.to_string(), message.to_string());
        let id = uuid::Uuid::new_v4().to_string();
        tokio::spawn(async move {
            gw.notify_notification(&id, &title, &message, "background")
                .await;
        });
    }
}

impl agent::agent_pool::AgentEventSink for WsAgentEventSink {
    fn notify_agent_reply(&self, chat_jid: &str, text: &str, tokens: u32) {
        let gw = Arc::clone(&self.gateway);
        let jid = chat_jid.to_string();
        let text = text.to_string();
        tokio::spawn(async move {
            gw.notify_agent_reply(&jid, &text, tokens).await;
        });
    }

    fn notify_agent_delta(&self, chat_jid: &str, delta: &str) {
        let gw = Arc::clone(&self.gateway);
        let jid = chat_jid.to_string();
        let delta = delta.to_string();
        // Deliberately NOT persisted: the completed `agent:reply` is the record
        // of the turn. Writing a row per token would bloat the chat log and
        // replay duplicated text on history load.
        tokio::spawn(async move {
            gw.notify_agent_delta(&jid, &delta).await;
        });
    }

    fn notify_agent_state(&self, chat_jid: &str, state: &str) {
        let gw = Arc::clone(&self.gateway);
        let db = Arc::clone(&self.db);
        let jid = chat_jid.to_string();
        let state = state.to_string();
        tokio::spawn(async move {
            persist_chat_event(
                &db,
                &jid,
                "agent:state",
                None,
                &serde_json::json!({"state": state}),
            );
            gw.notify_agent_state(&jid, &state).await;
        });
    }

    fn notify_permission_request(
        &self,
        chat_jid: &str,
        request_id: &str,
        payload: agent::permission_bridge::PermissionPayload,
    ) {
        let gw = Arc::clone(&self.gateway);
        let db = Arc::clone(&self.db);
        let jid = chat_jid.to_string();
        let req = request_id.to_string();
        let payload = serde_json::to_value(&payload).unwrap_or(serde_json::Value::Null);
        tokio::spawn(async move {
            persist_chat_event(&db, &jid, "permission:request", Some(&req), &payload);
            gw.notify_permission_request(&jid, &req, &payload).await;
        });
    }

    fn notify_ask_question_request(
        &self,
        chat_jid: &str,
        request_id: &str,
        payload: agent::permission_bridge::AskQuestionPayload,
    ) {
        let gw = Arc::clone(&self.gateway);
        let db = Arc::clone(&self.db);
        let jid = chat_jid.to_string();
        let req = request_id.to_string();
        let payload = serde_json::to_value(&payload).unwrap_or(serde_json::Value::Null);
        tokio::spawn(async move {
            persist_chat_event(&db, &jid, "question:request", Some(&req), &payload);
            gw.notify_ask_question_request(&jid, &req, &payload).await;
        });
    }

    fn notify_form_request(
        &self,
        chat_jid: &str,
        request_id: &str,
        payload: agent::permission_bridge::FormPayload,
    ) {
        let gw = Arc::clone(&self.gateway);
        let db = Arc::clone(&self.db);
        let jid = chat_jid.to_string();
        let req = request_id.to_string();
        let payload = serde_json::to_value(&payload).unwrap_or(serde_json::Value::Null);
        tokio::spawn(async move {
            persist_chat_event(&db, &jid, "form:request", Some(&req), &payload);
            gw.notify_form_request(&jid, &req, &payload).await;
        });
    }

    fn notify_form_resolved(
        &self,
        chat_jid: &str,
        request_id: &str,
        values: std::collections::HashMap<String, serde_json::Value>,
    ) {
        let gw = Arc::clone(&self.gateway);
        let db = Arc::clone(&self.db);
        let jid = chat_jid.to_string();
        let req = request_id.to_string();
        let values = serde_json::to_value(&values).unwrap_or(serde_json::Value::Null);
        tokio::spawn(async move {
            persist_chat_event(&db, &jid, "form:resolved", Some(&req), &values);
            gw.notify_form_resolved(&jid, &req, &values).await;
        });
    }

    fn notify_permission_resolved(
        &self,
        chat_jid: &str,
        request_id: &str,
        option_key: &str,
        option_label: &str,
    ) {
        let gw = Arc::clone(&self.gateway);
        let db = Arc::clone(&self.db);
        let jid = chat_jid.to_string();
        let req = request_id.to_string();
        let key = option_key.to_string();
        let label = option_label.to_string();
        tokio::spawn(async move {
            persist_chat_event(
                &db,
                &jid,
                "permission:resolved",
                Some(&req),
                &serde_json::json!({"key": key, "label": label}),
            );
            gw.notify_permission_resolved(&jid, &req, &key, &label)
                .await;
        });
    }

    fn notify_ask_question_resolved(
        &self,
        chat_jid: &str,
        request_id: &str,
        answers: std::collections::HashMap<String, String>,
    ) {
        let gw = Arc::clone(&self.gateway);
        let db = Arc::clone(&self.db);
        let jid = chat_jid.to_string();
        let req = request_id.to_string();
        let answers = serde_json::to_value(&answers).unwrap_or(serde_json::Value::Null);
        tokio::spawn(async move {
            persist_chat_event(&db, &jid, "question:resolved", Some(&req), &answers);
            gw.notify_ask_question_resolved(&jid, &req, &answers).await;
        });
    }

    fn notify_agent_todos(
        &self,
        agent_jid: &str,
        agent_name: &str,
        todos: &[agent::agent_pool::TodoSnapshot],
    ) {
        tracing::info!(
            "[WsAgentEventSink] notify_agent_todos jid={agent_jid} name={agent_name} count={}",
            todos.len()
        );
        let gw = Arc::clone(&self.gateway);
        let db = Arc::clone(&self.db);
        let jid = agent_jid.to_string();
        let name = agent_name.to_string();
        let todos = serde_json::to_value(todos).unwrap_or(serde_json::Value::Null);
        tokio::spawn(async move {
            // Persist before broadcasting so an admin reconnecting right
            // after the emit still sees the list across a daemon restart.
            let todos_json = serde_json::to_string(&todos).unwrap_or_else(|_| "[]".to_string());
            let ts = chrono::Utc::now().to_rfc3339();
            if let Err(e) = db.upsert_agent_todos(&jid, &name, &todos_json, &ts) {
                tracing::warn!(
                    error = %e, agent_jid = %jid,
                    "[WsAgentEventSink] failed to persist agent_todos; live broadcast continues"
                );
            }
            gw.notify_agent_todos(&jid, &name, &todos).await;
        });
    }

    fn notify_agent_compacting(&self, chat_jid: &str, is_compacting: bool) {
        let gw = Arc::clone(&self.gateway);
        let jid = chat_jid.to_string();
        tokio::spawn(async move {
            gw.notify_agent_compacting(&jid, is_compacting).await;
        });
    }

    fn notify_agent_tools(
        &self,
        agent_jid: &str,
        agent_name: &str,
        tools: &[agent::agent_pool::AgentToolInfo],
    ) {
        let gw = Arc::clone(&self.gateway);
        let jid = agent_jid.to_string();
        let name = agent_name.to_string();
        let tools = serde_json::to_value(tools).unwrap_or(serde_json::Value::Null);
        tokio::spawn(async move {
            gw.notify_agent_tools(&jid, &name, &tools).await;
        });
    }

    fn notify_agent_usage(&self, agent_jid: &str, usage: crate::zen_core::ConversationUsageData) {
        let gw = Arc::clone(&self.gateway);
        let jid = agent_jid.to_string();
        tokio::spawn(async move {
            gw.notify_agent_usage(&jid, &usage).await;
        });
    }
}

// ===== App channel control flow wiring =====

/// Build the sender's session list (the default session plus any `:s-*`
/// sub-sessions) and push it to the app as a `SESSION_LIST_RESP` control frame.
/// Ensures the default session group exists so the list is never empty.
async fn send_app_session_list(
    app: &Arc<channels::app::AppChannel>,
    db: &Arc<db::Db>,
    gm: &Arc<gateway::group_manager::GroupManager>,
    cfg: &Arc<config::Config>,
    sender_id: &str,
) {
    use channels::app::CTRL_SESSION_LIST_RESP;

    let default_jid = app.default_session_jid(sender_id);
    if gm.get(db, &default_jid).is_none() {
        gateway::group_manager::ensure_app_group(db, gm, cfg, &default_jid);
    }

    let sub_prefix = format!("{default_jid}:");
    let active = app.active_session_for(sender_id);
    let last_activity = db.last_activity_per_group().unwrap_or_default();

    let mut sessions: Vec<serde_json::Value> = gm
        .list(db)
        .unwrap_or_default()
        .into_iter()
        .filter(|g| g.jid == default_jid || g.jid.starts_with(&sub_prefix))
        .map(|g| {
            let ts = last_activity.get(&g.jid).copied();
            serde_json::json!({
                "jid": g.jid,
                "name": g.name,
                "folder": g.folder,
                "groupType": g.group_type,
                "lastActivity": ts,
                "active": g.jid == active,
            })
        })
        .collect();

    // Freshest first; sessions with no activity yet sink to the bottom.
    sessions.sort_by(|a, b| {
        let av = a["lastActivity"].as_i64().unwrap_or(0);
        let bv = b["lastActivity"].as_i64().unwrap_or(0);
        bv.cmp(&av)
    });

    let json = serde_json::to_string(&sessions).unwrap_or_default();
    let _ = app.send_control(CTRL_SESSION_LIST_RESP, json).await;
}

/// Wire AGENT_LIST_REQ / AGENT_SELECT / HISTORY_REQ handlers onto an AppChannel.
/// Called before `connect()` so the handler is in place when the first control
/// frame arrives.
fn wire_app_channel_controls(
    app: &Arc<channels::app::AppChannel>,
    db: Arc<db::Db>,
    gm: Arc<gateway::group_manager::GroupManager>,
    cfg: Arc<config::Config>,
    db_channel_id: i64,
    api_bridge: Arc<gateway::ui_server::ApiBridgeState>,
    agent_pool_cell: Arc<std::sync::OnceLock<Arc<agent::agent_pool::AgentPool>>>,
) {
    use channels::app::{
        CTRL_AGENT_LIST_REQ, CTRL_AGENT_LIST_RESP, CTRL_AGENT_SELECT, CTRL_API_REQ, CTRL_API_RESP,
        CTRL_HISTORY_REQ, CTRL_HISTORY_RESP, CTRL_SESSION_CREATE, CTRL_SESSION_DELETE,
        CTRL_SESSION_LIST_REQ, CTRL_SESSION_SELECT, CTRL_SESSION_UPDATE,
    };

    let app_for_cb = Arc::clone(app);

    app.set_control_handler(Arc::new(move |sender_id, ctrl_type, metadata| {
        let app = Arc::clone(&app_for_cb);
        let db = Arc::clone(&db);
        let gm = Arc::clone(&gm);
        let cfg = Arc::clone(&cfg);
        let api_bridge = Arc::clone(&api_bridge);

        match ctrl_type {
            // ── Agent list ──────────────────────────────────────────────────
            t if t == CTRL_AGENT_LIST_REQ => {
                tokio::spawn(async move {
                    // Debug: dump all bindings in DB to verify data exists
                    let all_bindings = db.list_bindings_with_relations().unwrap_or_default();
                    tracing::info!(
                        "[AppChannel] DEBUG AGENT_LIST_REQ from={} db_channel_id={} | total_bindings_in_db={}",
                        sender_id, db_channel_id, all_bindings.len()
                    );
                    for bwr in &all_bindings {
                        tracing::info!(
                            "[AppChannel] DEBUG binding: id={} channel_id={} channel_type={} channel_name={} agent_folder={} agent_name={}",
                            bwr.binding.id,
                            bwr.binding.channel_id,
                            bwr.channel.platform_type,
                            bwr.channel.name,
                            bwr.agent.folder,
                            bwr.agent.name,
                        );
                    }

                    // Only agents explicitly bound to this channel in the DB.
                    let bindings = db.list_bindings_for_channel(db_channel_id).unwrap_or_default();
                    tracing::info!(
                        "[AppChannel] list_bindings_for_channel({}): {} result(s)",
                        db_channel_id, bindings.len()
                    );

                    let payload: Vec<serde_json::Value> = bindings
                        .iter()
                        .map(|bwr| {
                            serde_json::json!({
                                "folder":  bwr.agent.folder,
                                "name":    bwr.agent.name,
                            })
                        })
                        .collect();
                    let json = serde_json::to_string(&payload).unwrap_or_default();
                    tracing::info!(
                        "[AppChannel] AGENT_LIST_RESP → {} ({} agent(s))",
                        sender_id, payload.len()
                    );
                    let _ = app.send_control(CTRL_AGENT_LIST_RESP, json).await;
                });
            }

            // ── Agent select — validate against channel<->agent bindings ────
            t if t == CTRL_AGENT_SELECT => {
                let agent_pool_cell = Arc::clone(&agent_pool_cell);
                tokio::spawn(async move {
                    let val: serde_json::Value =
                        serde_json::from_str(&metadata).unwrap_or_default();
                    let folder = val["folder"].as_str().unwrap_or("").to_string();

                    // Validate: folder must be bound to this channel.
                    let bindings = db.list_bindings_for_channel(db_channel_id).unwrap_or_default();
                    let target = bindings.iter().find(|bwr| bwr.agent.folder == folder);

                    let Some(bwr) = target else {
                        tracing::warn!(
                            "[AppChannel] AGENT_SELECT: folder '{}' not bound to channel {} (sender={})",
                            folder, db_channel_id, sender_id
                        );
                        return;
                    };
                    let target_folder = bwr.agent.folder.clone();
                    let target_name   = bwr.agent.name.clone();

                    // Rebind the sender's ACTIVE session group (multi-session);
                    // falls back to the default single-session jid.
                    let chat_jid = app.active_session_for(&sender_id);
                    if gm.get(&db, &chat_jid).is_none() {
                        gateway::group_manager::ensure_app_group(&db, &gm, &cfg, &chat_jid);
                    }

                    if let Some(mut binding) = gm.get(&db, &chat_jid) {
                        binding.folder = target_folder.clone();
                        binding.name   = target_name.clone();
                        gm.register(&db, &cfg, &binding);
                        tracing::info!(
                            "[AppChannel] AGENT_SELECT: {} → folder={} ({})",
                            chat_jid, target_folder, target_name
                        );
                    } else {
                        tracing::warn!(
                            "[AppChannel] AGENT_SELECT: no group binding for {}", chat_jid
                        );
                    }

                    // Optional agent mode (Agent | Plan | Dag) carried by the app.
                    let mode = val["mode"].as_str().unwrap_or("");
                    if !mode.is_empty() {
                        if let Some(pool) = agent_pool_cell.get() {
                            pool.set_agent_mode(&chat_jid, mode);
                            tracing::info!(
                                "[AppChannel] AGENT_SELECT: mode={} for {}", mode, chat_jid
                            );
                        }
                    }
                });
            }

            // ── History request ─────────────────────────────────────────────
            t if t == CTRL_HISTORY_REQ => {
                tokio::spawn(async move {
                    let val: serde_json::Value =
                        serde_json::from_str(&metadata).unwrap_or_default();
                    
                    // Support pagination
                    let page = val["page"].as_u64().unwrap_or(1) as u32;
                    let page_size = val["pageSize"].as_u64().unwrap_or(20) as u32;
                    let offset = (page.saturating_sub(1)) * page_size;

                    // History for the sender's ACTIVE session (multi-session).
                    let chat_jid = app.active_session_for(&sender_id);

                    let messages = db
                        .get_group_messages_paginated(&chat_jid, page_size, offset)
                        .unwrap_or_default();

                    let payload: Vec<serde_json::Value> = messages
                        .iter()
                        .map(|m| {
                            // Keep mobile protocol explicit: only "user" or "agent".
                            // Non-bot messages are treated as user-side messages.
                            let role = if m.is_bot_reply { "agent" } else { "user" };

                            serde_json::json!({
                                "id":        m.message_id,
                                "sender":    m.sender_name,
                                "content":   m.content,
                                "timestamp": m.timestamp,
                                "isFromMe":  m.is_from_me,
                                "isBotReply": m.is_bot_reply,
                                "role":      role,
                            })
                        })
                        .collect();

                    let json = serde_json::to_string(&payload).unwrap_or_default();
                    tracing::info!(
                        "[AppChannel] HISTORY_RESP → {} ({} message(s), page={}, pageSize={})",
                        sender_id, payload.len(), page, page_size
                    );
                    let _ = app.send_control(CTRL_HISTORY_RESP, json).await;
                });
            }

            // ── Session list ────────────────────────────────────────────────
            t if t == CTRL_SESSION_LIST_REQ => {
                tokio::spawn(async move {
                    send_app_session_list(&app, &db, &gm, &cfg, &sender_id).await;
                });
            }

            // ── Session create — a new `:s-*` sub-session group ──────────────
            t if t == CTRL_SESSION_CREATE => {
                let agent_pool_cell = Arc::clone(&agent_pool_cell);
                tokio::spawn(async move {
                    let val: serde_json::Value =
                        serde_json::from_str(&metadata).unwrap_or_default();
                    let name = val["name"].as_str().unwrap_or("New session").to_string();
                    let want_folder = val["folder"].as_str().unwrap_or("").to_string();

                    // Folder must be bound to this channel; else use the first
                    // bound agent, else a per-device fallback.
                    let bindings =
                        db.list_bindings_for_channel(db_channel_id).unwrap_or_default();
                    let folder = if bindings.iter().any(|b| b.agent.folder == want_folder) {
                        want_folder
                    } else {
                        bindings
                            .first()
                            .map(|b| b.agent.folder.clone())
                            .unwrap_or_else(|| format!("app-{sender_id}"))
                    };

                    let sid = uuid::Uuid::new_v4().simple().to_string();
                    let jid = format!("{}:s-{}", app.default_session_jid(&sender_id), &sid[..8]);
                    let now = chrono::Utc::now().to_rfc3339();
                    let binding = crate::types::GroupBinding {
                        jid: jid.clone(),
                        folder,
                        name,
                        channel: "app".to_string(),
                        group_type: "chat".to_string(),
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
                    gm.register(&db, &cfg, &binding);
                    app.set_active_session(&sender_id, &jid);

                    let mode = val["mode"].as_str().unwrap_or("");
                    if !mode.is_empty() {
                        if let Some(pool) = agent_pool_cell.get() {
                            pool.set_agent_mode(&jid, mode);
                        }
                    }
                    tracing::info!(
                        "[AppChannel] SESSION_CREATE {} for {}", jid, sender_id
                    );
                    send_app_session_list(&app, &db, &gm, &cfg, &sender_id).await;
                });
            }

            // ── Session update — rename / rebind folder ──────────────────────
            t if t == CTRL_SESSION_UPDATE => {
                tokio::spawn(async move {
                    let val: serde_json::Value =
                        serde_json::from_str(&metadata).unwrap_or_default();
                    let jid = val["jid"].as_str().unwrap_or("").to_string();
                    let default_jid = app.default_session_jid(&sender_id);
                    let owns = jid == default_jid
                        || jid.starts_with(&format!("{default_jid}:"));
                    if jid.is_empty() || !owns {
                        return;
                    }
                    let mut updates =
                        gateway::group_manager::GroupBindingUpdate::default();
                    if let Some(n) = val["name"].as_str() {
                        updates.name = Some(n.to_string());
                    }
                    if let Some(f) = val["folder"].as_str() {
                        if !f.is_empty() {
                            updates.folder = Some(f.to_string());
                        }
                    }
                    if gm.get(&db, &jid).is_none() {
                        gateway::group_manager::ensure_app_group(&db, &gm, &cfg, &jid);
                    }
                    let _ = gm.update(&db, &cfg, &jid, updates);
                    send_app_session_list(&app, &db, &gm, &cfg, &sender_id).await;
                });
            }

            // ── Session delete — sub-sessions only (default is permanent) ────
            t if t == CTRL_SESSION_DELETE => {
                tokio::spawn(async move {
                    let val: serde_json::Value =
                        serde_json::from_str(&metadata).unwrap_or_default();
                    let jid = val["jid"].as_str().unwrap_or("").to_string();
                    let default_jid = app.default_session_jid(&sender_id);
                    let owns = jid == default_jid
                        || jid.starts_with(&format!("{default_jid}:"));
                    // The default session can't be deleted — it's the fallback.
                    if jid.is_empty() || !owns || jid == default_jid {
                        return;
                    }
                    gm.unregister(&db, &cfg, &jid);
                    if app.active_session_for(&sender_id) == jid {
                        app.set_active_session(&sender_id, &default_jid);
                    }
                    tracing::info!(
                        "[AppChannel] SESSION_DELETE {} for {}", jid, sender_id
                    );
                    send_app_session_list(&app, &db, &gm, &cfg, &sender_id).await;
                });
            }

            // ── Session select — set active + optional folder/mode ───────────
            t if t == CTRL_SESSION_SELECT => {
                let agent_pool_cell = Arc::clone(&agent_pool_cell);
                tokio::spawn(async move {
                    let val: serde_json::Value =
                        serde_json::from_str(&metadata).unwrap_or_default();
                    let jid = val["jid"].as_str().unwrap_or("").to_string();
                    let default_jid = app.default_session_jid(&sender_id);
                    let owns = jid == default_jid
                        || jid.starts_with(&format!("{default_jid}:"));
                    if jid.is_empty() || !owns {
                        return;
                    }
                    // The default session may not be registered until the first
                    // message — materialize it so history/routing line up.
                    if jid == default_jid && gm.get(&db, &jid).is_none() {
                        gateway::group_manager::ensure_app_group(&db, &gm, &cfg, &jid);
                    }
                    app.set_active_session(&sender_id, &jid);

                    // Optional folder rebind (switch agent for this session);
                    // never touches the session's own display name.
                    if let Some(folder) = val["folder"].as_str() {
                        if !folder.is_empty() {
                            let bindings = db
                                .list_bindings_for_channel(db_channel_id)
                                .unwrap_or_default();
                            if let Some(b) =
                                bindings.iter().find(|b| b.agent.folder == folder)
                            {
                                let mut updates =
                                    gateway::group_manager::GroupBindingUpdate::default();
                                updates.folder = Some(b.agent.folder.clone());
                                let _ = gm.update(&db, &cfg, &jid, updates);
                            }
                        }
                    }

                    let mode = val["mode"].as_str().unwrap_or("");
                    if !mode.is_empty() {
                        if let Some(pool) = agent_pool_cell.get() {
                            pool.set_agent_mode(&jid, mode);
                        }
                    }
                    tracing::info!(
                        "[AppChannel] SESSION_SELECT {} for {}", jid, sender_id
                    );
                    send_app_session_list(&app, &db, &gm, &cfg, &sender_id).await;
                });
            }

            // ── REST tunnel — replay through the UI router ───────────────────
            t if t == CTRL_API_REQ => {
                tokio::spawn(async move {
                    let req: gateway::ui_server::ApiRequest = match serde_json::from_str(&metadata)
                    {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(
                                "[AppChannel] API_REQ parse error from {}: {e}",
                                sender_id
                            );
                            return;
                        }
                    };
                    tracing::info!(
                        "[AppChannel] API_REQ {} {} (id={}) from {}",
                        req.method, req.path, req.request_id, sender_id
                    );
                    let resp = gateway::ui_server::dispatch_api(api_bridge.as_ref(), req).await;
                    let json = serde_json::to_string(&resp).unwrap_or_default();
                    tracing::info!(
                        "[AppChannel] API_RESP → {} (id={}, status={}, {} bytes)",
                        sender_id, resp.request_id, resp.status, resp.body.len()
                    );
                    let _ = app.send_control(CTRL_API_RESP, json).await;
                });
            }

            _ => {
                tracing::debug!("[AppChannel] Unhandled control type={} from {}", ctrl_type, sender_id);
            }
        }
    }));
}

#[cfg(unix)]
fn raise_fd_limit() {
    // A generous ceiling; macOS enforces `kern.maxfilesperproc` on top of this,
    // so we try descending candidates until one is accepted.
    const CANDIDATES: [u64; 4] = [1_048_576, 262_144, 65_536, 10_240];
    unsafe {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            tracing::warn!("[SenClaw] getrlimit(RLIMIT_NOFILE) failed; leaving fd limit as-is");
            return;
        }
        let old_soft = lim.rlim_cur;
        for &target in CANDIDATES.iter() {
            // Never exceed the hard limit (unless it's "infinity").
            let want = if lim.rlim_max == libc::RLIM_INFINITY {
                target
            } else {
                target.min(lim.rlim_max as u64)
            };
            if (want as libc::rlim_t) <= lim.rlim_cur {
                continue;
            }
            let mut new_lim = lim;
            new_lim.rlim_cur = want as libc::rlim_t;
            if libc::setrlimit(libc::RLIMIT_NOFILE, &new_lim) == 0 {
                tracing::info!(
                    old = old_soft as u64,
                    new = want,
                    "[SenClaw] raised open-file (RLIMIT_NOFILE) soft limit"
                );
                return;
            }
        }
        tracing::warn!(
            soft = old_soft as u64,
            "[SenClaw] could not raise RLIMIT_NOFILE; watch for 'Too many open files'"
        );
    }
}

/// One-shot sweep restricting every file under `~/.senclaw/` that holds a
/// secret to owner-only (`0600`).
///
/// The individual write paths restrict themselves now, but a file only gets
/// re-written when something changes it — an install that never touches its
/// LLM config would keep `config.json` world-readable forever. This runs on
/// every boot and is a no-op once the modes are already right.
///
/// Deliberately *not* exhaustive: state files with no secrets in them
/// (`marketplace.json`, `workspace-state-*.json`, window geometry) are left
/// alone. Widening this to the whole directory would be easy and wrong — it
/// would fight any file another tool legitimately shares.
fn harden_data_dir_permissions(cfg: &config::Config) {
    use util::file_perms::{restrict_best_effort, restrict_sqlite};

    restrict_sqlite(&cfg.paths.db_path);
    restrict_sqlite(&cfg.paths.cognitive_db_path);
    restrict_best_effort(&cfg.paths.global_config_path);

    // Sibling files of config.json that carry credentials or MCP env blocks.
    if let Some(home) = cfg.paths.global_config_path.parent() {
        for name in ["oauth.json", "api_token", "project-config.json", "mcp.json"] {
            restrict_best_effort(&home.join(name));
        }
    }
}

pub async fn run_daemon(cfg: config::Config) -> Result<()> {
    // ===== 0. Setup wizard =====
    setup::run_setup_if_needed(&cfg.paths.global_config_path);

    tracing::info!("[SenClaw] Starting...");

    #[cfg(unix)]
    raise_fd_limit();

    // Before the first spawn: on Windows, children otherwise outlive a
    // `TerminateProcess` of the daemon and keep their ports.
    util::process_tree::bind_children_to_daemon();

    // ===== 0b. Lock down secret-bearing files =====
    // The write paths now chmod 0600 themselves, but that only covers files
    // written after this build. Existing installs already have `config.json`
    // (every LLM apiKey), `senclaw.db` (bot tokens, Space-App tokens, full
    // chat history) and the MCP config sitting at 0644 from the default
    // umask — readable by any other account on the machine. Sweep once on
    // boot so an upgrade fixes them without the user doing anything.
    harden_data_dir_permissions(&cfg);

    // ===== 0c. Workspace layout =====
    // `~/senclaw/agents/` → `profiles/`, per-profile `.sema/` → `.sen/`.
    gateway::group_manager::migrate_legacy_layout(&cfg);

    // ===== 1. Database =====
    let db = Arc::new(db::Db::open(&cfg).context("open database")?);
    tracing::info!("[SenClaw] DB initialized: {}", cfg.paths.db_path.display());

    // Load enabled MCP tool aliases (Plugins → Alias) into the process-wide
    // registry so `resolve_tool_by_name` applies renames/overrides from the
    // very first agent turn.
    tools::tool_alias::reload_from_db(&db);

    // ===== 1a-0. Token usage accounting =====
    // One recorder shared by every LLM call path (agent loop, bridge,
    // cognitive, embeddings). Non-blocking; batch-flushed to llm_usage_log.
    // Failure ledger writer: one row per run of consecutive failures of the
    // same tool, plus whether it was ever fixed and by whom. Records only.
    failures::start(Arc::clone(&db));
    let usage_recorder = usage::UsageRecorder::start(Arc::clone(&db));
    usage::set_global(Arc::clone(&usage_recorder));
    usage::aggregate::start(Arc::clone(&db));
    tracing::info!("[SenClaw] usage recorder started");

    // ===== 1a. Boot cleanup of stale pending interactions =====
    // `chat_events` persists permission:request / question:request rows
    // so the UI can replay them on reconnect. When the daemon was killed
    // mid-prompt (or the user just hasn't run senclaw in days), those
    // rows linger and the Agent Console resurrects a ghost approval the
    // agent has no memory of. Wipe anything unresolved that's older than
    // 1 hour — recent requests survive so an active session reconnect
    // still works.
    {
        let cutoff = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        match db.cleanup_stale_pending_interactions(&cutoff) {
            Ok(n) if n > 0 => tracing::info!(
                rows_deleted = n,
                "[SenClaw] cleaned up stale pending permission/question rows on boot"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "[SenClaw] stale-pending cleanup failed"),
        }
    }

    // ===== 1b. MemoryManager =====
    let _memory_mgr = memory::manager::init(Arc::clone(&db), &cfg);
    tracing::info!("[SenClaw] MemoryManager initialized");

    // Flipped to `true` once the UI HTTP server (7c) is listening. A `local:`
    // LLM is reached through that server (`/api/runtimes/models/:key/v1`), so a
    // boot task that may call the LLM waits for it first.
    let (http_ready_tx, http_ready_rx) = tokio::sync::watch::channel(false);

    // ===== 1c. Cognitive layer (graph + Hebbian + decay ticker) =====
    {
        use crate::memory::cognitive::{llm::LlmClient, llm_openai::create_cognitive_llm};
        struct DormantLlm;
        #[async_trait]
        impl LlmClient for DormantLlm {
            async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
                anyhow::bail!("cognify LLM not configured: set SENCLAW_OPENAI_API_KEY")
            }
        }
        let llm: Arc<dyn LlmClient> =
            create_cognitive_llm(&cfg).unwrap_or_else(|| Arc::new(DormantLlm));
        match memory::cognitive::init_daemon(Arc::clone(&db), &cfg, llm) {
            Some(sys) => {
                tracing::info!(
                    edges = sys.stats().map(|s| s.edges).unwrap_or(0),
                    "[SenClaw] Cognitive system booted (decay ticker running, 300s)"
                );
                // Persona ingest: every agent's SOUL.md becomes nodes/edges
                // in the cognitive graph so the agent can recall its own
                // identity via CogRecall (scoped to Persona(folder, "soul")).
                // Spawn in background — slow first-time LLM calls shouldn't
                // delay daemon readiness.
                let sys_clone = Arc::clone(&sys);
                let profiles_dir = cfg.paths.profiles_dir.clone();
                let profiles_dir_for_watch = profiles_dir.clone();
                let sys_for_watch = Arc::clone(&sys);
                let mut http_ready = http_ready_rx;
                tokio::spawn(async move {
                    // Ingesting before a `local:` LLM can answer skips every
                    // section's extraction, and a chunk without edges is
                    // retried only on the next boot, which would fail the
                    // same way. An `Err` (server never came up) still ingests.
                    let _ = http_ready.wait_for(|up| *up).await;
                    memory::cognitive::ingest_all_souls(sys_clone, profiles_dir).await;
                });
                // Then start the mtime-poll watcher so external edits to
                // SOUL.md (vim, VS Code, git pull, …) trigger re-ingest
                // without needing the API write hook.
                memory::cognitive::spawn_soul_watcher(
                    sys_for_watch,
                    profiles_dir_for_watch,
                    std::time::Duration::from_secs(30),
                );

                // Periodic maintenance: cleanup junk + merge duplicate
                // entities. Cadence comes from CognitiveConfig (0 disables).
                let interval = std::time::Duration::from_secs(
                    cfg.cognitive
                        .maintenance_interval_hours
                        .saturating_mul(3600),
                );
                memory::cognitive::start_maintenance_ticker(
                    Arc::clone(&sys.graph),
                    memory::cognitive::MaintenanceConfig { interval },
                );
            }
            None => tracing::info!(
                "[SenClaw] Cognitive system dormant — no embedding provider configured"
            ),
        }
    }

    // ===== 1c. Ensure main agent directory =====
    // Ensure main agent skeleton exists (missing dirs + SOUL.md/MEMORY.md templates),
    // avoiding the case where the user accidentally deletes the main group.
    // Matches TypeScript: ensureAgentDirs('main') before GroupManager creation.
    gateway::group_manager::ensure_agent_dirs(
        &cfg,
        &cfg.telegram.agent_folder,
        &cfg.telegram.agent_folder,
    );
    tracing::info!("[SenClaw] Main agent directory ensured");

    // ===== 1d. Soul Core =====
    // `USER.md` / `TOOLS.md` / `AGENTS.md` at `~/.senclaw/`. Global, outside
    // `profiles_dir`, so every agent profile shares one answer to "who is my
    // human". Kept OUTSIDE the cognitive block above on purpose: that branch
    // only runs when an embedding provider is configured, which is right for
    // persona ingest and wrong here — the profile has nothing to do with the
    // graph, and an install without embeddings must still notice edits.
    user_profile::ensure_exists(&cfg);
    user_profile::reload(&cfg.paths.user_profile_path);
    user_profile::spawn_watcher(
        Arc::new(cfg.clone()),
        std::time::Duration::from_secs(30),
        None,
    );
    tracing::info!(
        path = %cfg.paths.user_profile_path.display(),
        "[SenClaw] Soul Core ready"
    );

    // OAuth account store. Installed before anything can resolve a model
    // profile, because an OAuth-backed LlmConfig reads its bearer token from
    // here synchronously. The background task then keeps tokens ahead of
    // expiry so a refresh never stalls a user-visible reply.
    let oauth_manager = providers::oauth::init(providers::oauth::store::default_path(
        &cfg.paths.global_config_path,
    ));
    let oauth_accounts = oauth_manager.accounts_redacted().len();
    if oauth_accounts > 0 {
        tracing::info!("[SenClaw] OAuth accounts loaded: {oauth_accounts}");
    }
    providers::oauth::spawn_background_refresher(Arc::clone(&oauth_manager));

    // ===== 2. GroupManager & Other Managers =====
    // Load group bindings from DB; reconcile with config.json
    let gm = Arc::new(gateway::group_manager::GroupManager::new());
    let am = Arc::new(gateway::agent_manager::AgentManager::new());
    let bm = Arc::new(gateway::binding_manager::BindingManager::new());
    let cm = Arc::new(gateway::channel_manager::ChannelManager::new());
    // Sync groups from config.json into DB on startup
    let (sync_added, sync_updated, sync_removed) =
        gateway::group_manager::sync_groups_from_config(&db, &gm, &cfg);
    if sync_added > 0 || sync_updated > 0 || sync_removed > 0 {
        tracing::info!(
            "[SenClaw] Group sync: +{sync_added} added, ~{sync_updated} updated, -{sync_removed} removed"
        );
    }
    let groups = db.list_groups()?;
    tracing::info!("[SenClaw] GroupManager: {} group(s) loaded", groups.len());

    // ===== 2b. PersonaRegistry =====
    let persona_registry = {
        let reg =
            agent::persona_registry::PersonaRegistry::new(cfg.paths.virtual_agents_dir.clone());
        let reg = Arc::new(std::sync::Mutex::new(reg));
        // Spawn file watcher for hot-reload
        agent::persona_registry::PersonaRegistry::spawn_watcher(Arc::clone(&reg));
        reg
    };
    tracing::info!(
        "[SenClaw] PersonaRegistry: {} persona(s) loaded",
        persona_registry.lock().unwrap().list().len()
    );

    // ===== 3. Channel adapters =====
    let mut channels: Vec<Box<dyn channels::Channel>> = Vec::new();

    // Lazily-populated handle that lets app-channel relay clients tunnel REST
    // calls through the UI router. Filled in once `UiState` is built (step 7).
    let api_bridge = Arc::new(gateway::ui_server::ApiBridgeState::new());

    // App channels collected for the WS-gateway → relay event forwarder (step 8).
    let mut app_channels: Vec<Arc<channels::app::AppChannel>> = Vec::new();

    // Lazy handle to the AgentPool (built later in step 4) so the relay
    // AGENT_SELECT handler can pin a group's agent mode (Agent/Plan/Dag).
    let app_agent_pool_cell =
        Arc::new(std::sync::OnceLock::<Arc<agent::agent_pool::AgentPool>>::new());

    // 3a. Telegram
    let tg = channels::telegram::TelegramChannel::new(cfg.telegram.bot_token.clone());
    match tg.connect().await {
        Ok(()) => {
            if tg.is_connected() {
                tracing::info!("[SenClaw] TelegramChannel connected");
            } else {
                tracing::warn!(
                    "[SenClaw] TelegramChannel not connected (token missing or invalid)"
                );
            }
        }
        Err(e) => {
            tracing::error!(
                "[SenClaw] TelegramChannel connect failed, continuing without Telegram: {e}"
            );
        }
    }
    channels.push(Box::new(tg));

    // 3e. Reconcile channel adapters from DB channels table.
    // Entity migration creates channels from legacy groups; config.json may also
    // have entries. This step ensures any channel stored in the DB that isn't
    // already covered by a config-based adapter gets initialized.
    match cm.list(&db) {
        Ok(db_channels) => {
            for ch_record in &db_channels {
                let creds: serde_json::Value =
                    serde_json::from_str(&ch_record.credentials_json).unwrap_or_default();

                // Skip if a running adapter already covers this exact channel.
                // For Telegram we check by bot token so multiple bots can coexist.
                let already_running = {
                    let platform = ch_record.platform_type.as_str();
                    if platform == "telegram" {
                        let db_token = creds["botToken"].as_str().unwrap_or("").trim();
                        let effective = if db_token.is_empty() {
                            cfg.telegram.bot_token.as_str()
                        } else {
                            db_token
                        };
                        // Already running if a connected Telegram adapter was started with the same token.
                        channels.iter().any(|adapter| {
                            adapter.id() == "telegram"
                                && adapter.is_connected()
                                && !effective.is_empty()
                                && effective == cfg.telegram.bot_token.as_str()
                        })
                    } else {
                        channels
                            .iter()
                            .any(|adapter| adapter.id() == platform && adapter.is_connected())
                    }
                };
                if already_running {
                    continue;
                }

                match ch_record.platform_type.as_str() {
                    "telegram" => {
                        let token = creds["botToken"].as_str().unwrap_or("").trim().to_string();
                        // Use global default token if credentials didn't specify one.
                        let effective_token = if token.is_empty() {
                            cfg.telegram.bot_token.clone()
                        } else {
                            token
                        };
                        if effective_token.is_empty() {
                            tracing::warn!(
                                "[SenClaw] Telegram channel id={} has no bot token (set SENCLAW_TELEGRAM_BOT_TOKEN or enter token in channel settings)",
                                ch_record.id
                            );
                        } else {
                            // Re-use the existing TelegramChannel adapter if available,
                            // otherwise create a new one for this token.
                            let tg_new =
                                channels::telegram::TelegramChannel::new(effective_token.clone());
                            match tg_new.add_bot(&effective_token).await {
                                Ok(()) if tg_new.is_connected() => {
                                    tracing::info!(
                                        "[SenClaw] TelegramChannel from DB (id={}) connected",
                                        ch_record.id
                                    );
                                    channels.push(Box::new(tg_new));
                                }
                                Ok(()) => {
                                    tracing::warn!(
                                        "[SenClaw] TelegramChannel from DB (id={}) did not connect",
                                        ch_record.id
                                    );
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "[SenClaw] TelegramChannel from DB (id={}) failed: {e}",
                                        ch_record.id
                                    );
                                }
                            }
                        }
                    }
                    "feishu" => {
                        let app_id = creds["appId"].as_str().unwrap_or("");
                        let app_secret = creds["appSecret"].as_str().unwrap_or("");
                        let domain = creds["domain"].as_str();
                        if !app_id.is_empty() && !app_secret.is_empty() {
                            let feishu = channels::feishu::FeishuChannel::new(
                                app_id.to_string(),
                                app_secret.to_string(),
                                domain.map(|s| s.to_string()),
                            );
                            match feishu.connect().await {
                                Ok(()) if feishu.is_connected() => {
                                    tracing::info!(
                                        "[SenClaw] FeishuChannel from DB (id={}) connected",
                                        ch_record.id
                                    );
                                    channels.push(Box::new(feishu));
                                }
                                Ok(()) => {
                                    tracing::warn!(
                                        "[SenClaw] FeishuChannel from DB (id={}) not connected",
                                        ch_record.id
                                    );
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "[SenClaw] FeishuChannel from DB (id={}) failed: {e}",
                                        ch_record.id
                                    );
                                }
                            }
                        }
                    }
                    "app" | "senclaw" => {
                        let hub_url = creds["hubUrl"].as_str().unwrap_or("http://localhost:50051");
                        let channel_id = creds["channelId"].as_str().unwrap_or("");
                        let enc_key_b64 = creds["encryptionKey"].as_str().unwrap_or("");
                        let access_token = creds["accessToken"].as_str().unwrap_or("");
                        if !channel_id.is_empty()
                            && !enc_key_b64.is_empty()
                            && !access_token.is_empty()
                        {
                            if let Ok(crypto) = util::crypto::Crypto::new_from_b64(enc_key_b64) {
                                let key = crypto.get_key();
                                let app_arc = Arc::new(channels::app::AppChannel::new(
                                    hub_url.to_string(),
                                    channel_id.to_string(),
                                    access_token.to_string(),
                                    key,
                                ));
                                wire_app_channel_controls(
                                    &app_arc,
                                    Arc::clone(&db),
                                    Arc::clone(&gm),
                                    Arc::new(cfg.clone()),
                                    ch_record.id,
                                    Arc::clone(&api_bridge),
                                    Arc::clone(&app_agent_pool_cell),
                                );
                                channels::app::AppChannel::connect_nonblocking(Arc::clone(
                                    &app_arc,
                                ));
                                tracing::info!(
                                    "[SenClaw] AppChannel from DB (id={}) registered (relay in background)",
                                    ch_record.id
                                );
                                app_channels.push(Arc::clone(&app_arc));
                                channels.push(Box::new(Arc::clone(&app_arc)));
                            }
                        }
                    }
                    _ => {
                        tracing::debug!(
                            "[SenClaw] Channel id={} type={}: no DB-based init needed",
                            ch_record.id,
                            ch_record.platform_type
                        );
                    }
                }
            }
            let db_init_count = db_channels
                .iter()
                .filter(|c| {
                    c.platform_type == "feishu"
                        || c.platform_type == "app"
                        || c.platform_type == "senclaw"
                })
                .count();
            if db_init_count > 0 {
                tracing::info!(
                    "[SenClaw] DB channel reconciliation: checked {} channel(s)",
                    db_init_count
                );
            }
        }
        Err(e) => {
            tracing::error!("[SenClaw] Failed to list DB channels for reconciliation: {e}");
        }
    }

    let connected_count = channels.iter().filter(|ch| ch.is_connected()).count();
    if connected_count == 0 {
        tracing::warn!("[SenClaw] No channels are connected; running in WebUI-only mode.");
    } else {
        tracing::info!("[SenClaw] {connected_count} channel(s) connected");
    }

    // Wrap channels for shared access (callbacks + shutdown).
    let channels: Arc<tokio::sync::Mutex<Vec<Box<dyn Channel>>>> =
        Arc::new(tokio::sync::Mutex::new(channels));

    // (admin group auto-creation removed — was 3e. Profiles & bindings are
    // now managed explicitly via Settings → Profile UI.)

    // ===== 3f. MCP Manager =====
    let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let user_config_dir = cfg
        .paths
        .global_config_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".senclaw")
        });
    let mcp_manager = Arc::new(mcp::manager::McpManager::new(working_dir, user_config_dir));
    if let Err(e) = mcp_manager.init().await {
        tracing::warn!("[SenClaw] MCP manager init: {e}");
    }
    tracing::info!("[SenClaw] MCP manager initialized");

    // Teach the manager to reach SenClaw's own MCP servers. They are otherwise
    // launched per chat session and so are invisible to anything calling from
    // inside the daemon — a watch probing `dispatch_status` fails with
    // "MCP server not found: senclaw-dispatch" and gives up.
    //
    // Only servers whose config carries no per-chat context belong here: one
    // process answers every caller, so a spec holding one chat's jid would
    // answer them all as that chat.
    mcp_manager
        .register_builtin_spec(mcp::helper::dispatch_mcp_config(
            &cfg.paths.dispatch_state_path.to_string_lossy(),
            "main",
            // Empty on purpose, per the note above: this one process answers
            // every caller, so a jid here would stamp one chat's id onto
            // parents created for anybody.
            "",
            None,
        ))
        .await;
    mcp_manager
        .register_builtin_spec(mcp::helper::usage_mcp_config(
            &cfg.paths.db_path.to_string_lossy(),
        ))
        .await;

    // ===== Built-in Kanban board (folded into core — src/kanban) =====
    // No separate process, port, or Space-App: the REST API is mounted on the
    // daemon UI server (/api/kanban/*, wired where the UI router is built), the
    // native Flutter screen renders it, the `kanban-server` subcommand serves
    // its MCP to agents, and the in-process MCPDispatcher drives it directly.
    let kanban_state = kanban::make_state();

    // Auto-launch + auto-register MCP servers declared by installed Space Apps
    // (manifest `mcp.autoRegister`). Lifecycle is tied to the daemon.
    //
    // Runs in the BACKGROUND: apps are launched sequentially and each can wait
    // up to ~30s for its health endpoint, so a slow or broken app must not
    // stall daemon boot (the desktop startup gate waits on the UI port, which
    // is only opened after this section used to complete). Agents that start
    // before an app finishes registering simply see its MCP tools appear late.
    let space_mcp_launcher = Arc::new(
        gateway::ui_server::space_mcp::SpaceMcpLauncher::with_api_version(cfg.space_api_version),
    );
    // Load the app-provided model registry synchronously, before anything can
    // resolve a model profile. The auto-register pass below refreshes it, but it
    // runs in the background and can take a minute — and a turn that starts
    // first would find the user's selected model missing and silently fall back
    // to a different one.
    if let Err(e) = apps::llm_provider::refresh(&db) {
        tracing::warn!("[app-llm] provider registry not loaded at boot: {e}");
    }
    {
        let apps_dir = cfg.paths.workspace_dir.join("space-apps");
        let base_url = format!("http://127.0.0.1:{}", cfg.ui_server.port);
        let launcher = Arc::clone(&space_mcp_launcher);
        let db_bg = Arc::clone(&db);
        let mgr_bg = Arc::clone(&mcp_manager);
        tokio::spawn(async move {
            launcher
                .autoregister_installed(&db_bg, &mgr_bg, &apps_dir, &base_url)
                .await;
            tracing::info!("[space-mcp] background auto-register pass complete");
        });
    }

    // Space-App supervisor: periodically health-check every enabled server app
    // and respawn any that has died or stopped responding — so a crashed app (or
    // one that came up on a broken deploy) recovers on its own.
    if cfg.space_supervise_secs > 0 {
        let apps_dir = cfg.paths.workspace_dir.join("space-apps");
        let base_url = format!("http://127.0.0.1:{}", cfg.ui_server.port);
        let launcher = Arc::clone(&space_mcp_launcher);
        let db_bg = Arc::clone(&db);
        let mgr_bg = Arc::clone(&mcp_manager);
        let interval = std::time::Duration::from_secs(cfg.space_supervise_secs.max(5));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.tick().await; // skip immediate — the auto-register pass just ran
            loop {
                tick.tick().await;
                launcher
                    .supervise(&db_bg, &mgr_bg, &apps_dir, &base_url)
                    .await;
            }
        });
        tracing::info!(
            "[space-mcp] supervisor loop started ({}s interval)",
            cfg.space_supervise_secs
        );
    }

    // Idle reaper: stop session apps that have not been used for their
    // `runtime.idleTimeoutSecs`. Without this, the first tool call of the day
    // would start an app that then stays up forever — which is the always-on
    // behaviour session mode exists to replace.
    if cfg.space_idle_sweep_secs > 0 {
        let apps_dir = cfg.paths.workspace_dir.join("space-apps");
        let launcher = Arc::clone(&space_mcp_launcher);
        let db_bg = Arc::clone(&db);
        let mgr_bg = Arc::clone(&mcp_manager);
        let interval = std::time::Duration::from_secs(cfg.space_idle_sweep_secs.max(5));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.tick().await; // the first tick fires immediately; nothing is idle yet
            loop {
                tick.tick().await;
                launcher.reap_idle(&db_bg, &mgr_bg, &apps_dir).await;
            }
        });
        tracing::info!(
            "[space-mcp] idle reaper started ({}s sweep)",
            cfg.space_idle_sweep_secs
        );
    }

    // ===== 4. GroupQueue + AgentPool =====
    let group_queue = agent::group_queue::GroupQueue::new(cfg.agent.max_concurrent);
    // Keep a typed Arc<ZenCoreApi> so we can wire late dependencies (workbench bridge).
    let zen_core_api = Arc::new(agent::agent_pool::ZenCoreApi::new(Some(Arc::clone(
        &mcp_manager,
    ))));
    zen_core_api.set_usage_recorder(Arc::clone(&usage_recorder));
    // Shadow-git checkpoints: a commit outside the project's own repo after
    // every tool that wrote into the chat's working directory, so a person
    // can diff and restore per step (src/checkpoints). Created here, at the
    // top of run_daemon, because both the tool-execution callback and the
    // UI state need it.
    let checkpoint_service = Arc::new(checkpoints::CheckpointService::new(
        Arc::clone(&db),
        cfg.paths
            .user_profile_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new(".")),
    ));
    // ===== Runtime manager =====
    //
    // The daemon links no inference code: MLX, llama.cpp, the decision model,
    // OCR, Whisper and TTS are all runtimes it installs and supervises as
    // child processes (docs/runtime-protocol.md). Constructed before the
    // agent pool and the UI router, both of which need it.
    crate::util::internal_auth::set_daemon_ui_port(cfg.ui_server.port);
    // The model picker scans this root on every `load_llm_configs`, which only
    // receives the config path.
    crate::local_models::register_root(&cfg.paths.global_config_path, cfg.paths.local_models_dir.clone());
    let runtime_manager = crate::runtime::RuntimeManager::new(crate::runtime::RuntimeManagerConfig {
        runtimes_dir: cfg.paths.runtimes_dir.clone(),
        runtime_data_dir: cfg.paths.runtime_data_dir.clone(),
        runtime_logs_dir: cfg.paths.runtime_logs_dir.clone(),
        bundled_dir: cfg.paths.bundled_runtimes_dir.clone(),
        local_models_dir: cfg.paths.local_models_dir.clone(),
        config_path: cfg.paths.global_config_path.clone(),
        home: cfg.paths.global_config_path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from(".")),
        index_url: cfg.paths.runtime_index_url.clone(),
    });
    // Stop anything a previous daemon left running (crash, `kill -9`) before
    // this one might start a competing process on the same port.
    runtime_manager.cleanup_orphans().await;
    runtime_manager.spawn_idle_sweeper();
    // The browser extension's pipe (`/browser/ext`) opens the browser runtime.
    crate::browser_agent::extension::set_runtime_manager(Arc::clone(&runtime_manager));

    let agent_pool = agent::agent_pool::AgentPool::new(zen_core_api.clone());
    // The object every user message is queued through — the WS gateway and
    // the REST code-session endpoint share it so both take the same path.
    let ws_api = Arc::new(RealWsApi {
        group_queue: Arc::clone(&group_queue),
        agent_pool: agent_pool.clone(),
        db: Arc::clone(&db),
    });
    agent_pool.set_db(Arc::clone(&db));
    agent_pool.set_config(Arc::new(cfg.clone()));
    agent_pool.set_runtime_manager(Arc::clone(&runtime_manager));
    // Load the saved permission choice. Without this the pool starts with both
    // flags false no matter what `config.json` says, and only the Web UI's save
    // handler ever set them — so every restart silently re-armed permission
    // prompts. Dispatch felt it worst: `get_skip_perms_for_virtual` reads the
    // "skip all agents" flag directly, so every DAG worker stopped at its first
    // Write and waited on an admin UI that a headless daemon does not have.
    {
        let saved = gateway::group_manager::get_admin_permissions_config(
            &cfg.paths.global_config_path,
        );
        agent_pool.set_permissions_config(agent::agent_pool::PermissionsConfig {
            skip_main_agent_permissions: saved.skip_main_agent_permissions,
            skip_all_agents_permissions: saved.skip_all_agents_permissions,
        });
        tracing::info!(
            "[SenClaw] Admin permissions loaded from config: skip_main={} skip_all={}",
            saved.skip_main_agent_permissions,
            saved.skip_all_agents_permissions
        );
    }
    // Skills hot-reload: clawhub reload-signal + manual changes to the
    // managed skills dir (cp/rm of a skill folder writes no signal).
    agent_pool.watch_skills_reload(cfg.paths.managed_skills_dir.clone());
    // Publish the pool so the relay AGENT_SELECT handler can set agent modes.
    let _ = app_agent_pool_cell.set(agent_pool.clone());

    // Initialize marketplace manager for loading MCP servers from plugins
    let marketplace_manager = Arc::new(marketplace::manager::MarketplaceManager::from_config(&cfg));
    agent_pool.set_marketplace_manager(Arc::clone(&marketplace_manager));
    tracing::info!("[SenClaw] MarketplaceManager initialized and wired to AgentPool");

    let dispatch_bridge = Arc::new(agent::dispatch_bridge::DispatchBridge::new(
        cfg.paths.dispatch_state_path.clone(),
    ));
    agent_pool.set_dispatch_bridge(
        Arc::clone(&dispatch_bridge) as Arc<dyn agent::dispatch_bridge::DispatchBridgeApi>
    );
    // Shared map for routing virtual-agent permission responses back to their waiting thread.
    let virtual_perm_senders: Arc<Mutex<HashMap<String, std::sync::mpsc::SyncSender<String>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    agent_pool.set_permission_bridge(Arc::new(agent::permission_bridge::PermissionBridge::new(
        Arc::new(RealPermissionApi {
            agent_pool: agent_pool.clone(),
            virtual_perm_senders: Arc::clone(&virtual_perm_senders),
            db: Arc::clone(&db),
        }),
        None,
    )));

    // The tool-call gate (Settings → Decision): may approve an agent's shell
    // command once, before it prompts, when the decision engine is confident
    // it only reads, tests or edits. Off unless the user turns it on; settings
    // are read per request.
    if let Some(bridge) = agent_pool.permission_bridge() {
        let zen = Arc::downgrade(&zen_core_api);
        bridge.set_tool_gate(Arc::new(decision::gate::ToolGate::new(
            cfg.paths.global_config_path.clone(),
            Arc::clone(&runtime_manager),
            Arc::clone(&db),
            Box::new(move |jid: &str| zen.upgrade().and_then(|api| api.working_dir_for(jid))),
        )));
    }

    // Seed the permission bridge with any persisted tool rules from DB so
    // the server is the source of truth across restarts and browsers.
    if let (Some(bridge), Ok(rows)) = (agent_pool.permission_bridge(), db.list_tool_rules()) {
        let mut loaded = 0usize;
        for row in &rows {
            match serde_json::from_str::<agent::permission_bridge::types::ToolAutoAcceptRule>(
                &row.rule_json,
            ) {
                Ok(rule) => {
                    bridge.add_rule(rule);
                    loaded += 1;
                }
                Err(e) => {
                    tracing::warn!(error = %e, rule_id = %row.id, "[ToolRules] DB row deserialize failed")
                }
            }
        }
        tracing::info!("[ToolRules] seeded permission bridge with {loaded} persisted rule(s)");
    }

    // ===== DailyLogger for conversation history =====
    let daily_logger = Arc::new(memory::daily_logger::DailyLogger::new(
        cfg.paths.profiles_dir.clone(),
    ));
    agent_pool.set_daily_logger(daily_logger);
    tracing::info!("[SenClaw] DailyLogger initialized");

    // ===== WorkbenchBridge =====
    // Relays workbench:* events from each per-group ZenEngine to WS clients +
    // IM text-fallback. AgentPool calls bridge.bind_engine(...) after each
    // engine is created; callbacks below are set once at startup.
    let workbench_bridge = Arc::new(agent::workbench_bridge::WorkbenchBridge::new());
    zen_core_api.set_workbench_bridge(Arc::clone(&workbench_bridge));
    agent_pool.set_workbench_bridge(Arc::clone(&workbench_bridge));
    {
        let chs = Arc::clone(&channels);
        workbench_bridge.set_send_channel_notice(Arc::new(
            move |jid: &str, text: &str, bot_token: Option<&str>| {
                let chs = Arc::clone(&chs);
                let jid = jid.to_string();
                let text = text.to_string();
                let bt = bot_token.map(|s| s.to_string());
                tokio::spawn(async move {
                    let guard = chs.lock().await;
                    for c in guard.iter() {
                        if c.owns_jid(&jid) {
                            if let Err(e) = c.send_message(&jid, &text, bt.as_deref()).await {
                                tracing::warn!(
                                    "[WorkbenchBridge] channel notify failed for {jid}: {e}"
                                );
                            }
                            break;
                        }
                    }
                });
            },
        ));
    }
    tracing::info!("[SenClaw] WorkbenchBridge initialized");

    tracing::info!(
        "[SenClaw] AgentPool (zen-core engine) + GroupQueue (max_concurrent={}) ready",
        cfg.agent.max_concurrent
    );

    // Wire reply send through the correct channel.
    {
        let chs = Arc::clone(&channels);
        agent_pool.set_send_reply(Arc::new(
            move |jid: &str, text: &str, bot_token: Option<&str>| {
                let chs = Arc::clone(&chs);
                let jid = jid.to_string();
                let text = text.to_string();
                let bt = bot_token.map(|s| s.to_string());
                tokio::spawn(async move {
                    // Egress gate. ĐÂY là đường worm Morris II lây — reply không đi qua
                    // `mcp::send_server`, nên gate đặt ở đó sẽ bỏ lọt đúng đường này.
                    // Xem docs/agent-security-hooks.md §3.1.1.
                    if !crate::security::gate(&jid, &text) {
                        tracing::warn!("[SenClaw] Reply tới {jid} bị egress guard chặn");
                        return;
                    }
                    let guard = chs.lock().await;
                    // `let _ =` here is what made a broken Telegram setup look
                    // like a silent one: the adapter answered "Bot not found"
                    // and the reply vanished with nothing in the log to find.
                    let mut delivered = false;
                    for c in guard.iter() {
                        if c.owns_jid(&jid) {
                            delivered = true;
                            if let Err(e) = c.send_message(&jid, &text, bt.as_deref()).await {
                                tracing::error!(
                                    "[SenClaw] Gửi reply tới {jid} qua kênh '{}' thất bại: {e}",
                                    c.id()
                                );
                            }
                            break;
                        }
                    }
                    if !delivered {
                        // No adapter claimed it — a bot that was removed, or one
                        // that never connected. Worth saying out loud: the agent
                        // has already produced the answer at this point.
                        tracing::error!(
                            "[SenClaw] Không kênh nào nhận jid {jid} — reply bị bỏ, \
                             kiểm tra bot token của channel tương ứng"
                        );
                    }
                });
            },
        ));
    }
    tracing::info!("[SenClaw] Reply routing wired to channels (egress guard active)");

    // Wire typing indicator through the correct channel.
    {
        let chs = Arc::clone(&channels);
        agent_pool.set_typing_fn(Arc::new(
            move |jid: &str, active: bool, bot_token: Option<&str>| {
                let chs = Arc::clone(&chs);
                let jid = jid.to_string();
                let bt = bot_token.map(|s| s.to_string());
                tokio::spawn(async move {
                    let guard = chs.lock().await;
                    for c in guard.iter() {
                        if c.owns_jid(&jid) {
                            let _ = c.set_typing(&jid, active, bt.as_deref()).await;
                            break;
                        }
                    }
                });
            },
        ));
    }

    // Start SendBridge (HTTP bridge for MCP send-server).
    let _send_bridge = {
        let chs_msg = Arc::clone(&channels);
        let chs_file = Arc::clone(&channels);
        let send_msg = Arc::new(
            move |jid: String, text: String, bot_token: Option<String>| {
                let chs = Arc::clone(&chs_msg);
                Box::pin(async move {
                    let guard = chs.lock().await;
                    for c in guard.iter() {
                        if c.owns_jid(&jid) {
                            let _ = c.send_message(&jid, &text, bot_token.as_deref()).await;
                            break;
                        }
                    }
                }) as futures::future::BoxFuture<'static, ()>
            },
        );
        let send_file = Arc::new(
            move |jid: String,
                  file_path: String,
                  caption: Option<String>,
                  bot_token: Option<String>| {
                let chs = Arc::clone(&chs_file);
                Box::pin(async move {
                    let guard = chs.lock().await;
                    for c in guard.iter() {
                        if c.owns_jid(&jid) {
                            let _ = c
                                .send_file(
                                    &jid,
                                    &file_path,
                                    caption.as_deref(),
                                    bot_token.as_deref(),
                                )
                                .await;
                            break;
                        }
                    }
                }) as futures::future::BoxFuture<'static, ()>
            },
        );
        match agent::send_bridge::SendBridge::start(send_msg, send_file).await {
            Ok(sb) => {
                tracing::info!("[SenClaw] SendBridge on port {}", sb.port());
                Some(sb)
            }
            Err(e) => {
                tracing::warn!("[SenClaw] SendBridge failed to start: {e}");
                None
            }
        }
    };

    // ===== 4b. MessageRouter =====
    // One marketplace manager shared across the chat command path (router + WS)
    // and the REST/UI panel (UiState), so `/plugin` commands and the panel
    // mutate the same on-disk sources/state.
    let marketplace_shared = Arc::new(std::sync::Mutex::new(
        marketplace::manager::MarketplaceManager::from_config(&cfg),
    ));

    // Put the MCP servers of every enabled plugin into the shared manager, so
    // their tools reach agents and show up at `GET /api/mcp-servers`. Rebuilt
    // on every plugin toggle; never written to the user's own `mcp.json`.
    marketplace::mcp_sync::sync(&marketplace_shared, &mcp_manager).await;

    // Process-wide widget registry: built-in kinds + Space-App manifest
    // `widgets[]` + enabled plugins' `widgets/widgets.json`. Read by the
    // `emit_widget` (kind `app`) and `widget_list` tools.
    widgets::init_global(widgets::WidgetRegistry::new(
        Some(Arc::clone(&db)),
        cfg.paths.global_config_path.clone(),
        Some(Arc::clone(&marketplace_shared)),
    ));

    let message_router = Arc::new(gateway::message_router::MessageRouter::new(
        Arc::clone(&gm),
        Arc::clone(&bm),
        agent_pool.clone() as Arc<dyn types::AgentApi>,
        Arc::clone(&group_queue),
        Arc::clone(&db),
        Arc::new(cfg.clone()),
    ));
    message_router
        .set_marketplace_manager(Arc::clone(&marketplace_shared))
        .await;
    // Wire incoming messages from all channels → MessageRouter
    {
        let chs = channels.lock().await;
        for ch in chs.iter() {
            let router = Arc::clone(&message_router);
            ch.on_message(Box::new(move |msg| {
                let r = Arc::clone(&router);
                tokio::spawn(async move {
                    r.handle_incoming(msg).await;
                });
            }));
        }
    }
    tracing::info!("[SenClaw] MessageRouter wired to {connected_count} channel(s)");

    // ===== 5. TaskScheduler =====
    //
    // Migration: clear legacy scheduled tasks that predate the recurring-chat
    // redesign. Schedules now own a dedicated group binding with folder prefix
    // `schedule_`; older rows have a real group folder and can no longer be
    // matched to a chat session here. Mark them completed so they stop firing.
    if let Err(e) = db.with_conn(|c| {
        c.execute(
            // `context_mode <> 'watch'` is load-bearing. A watch is armed
            // against the *chat's own* folder, never a `schedule_` one, so this
            // sweep would retire every in-flight watch on every restart — the
            // chat would simply never hear back, with nothing logged and
            // nothing to see.
            "UPDATE scheduled_tasks
             SET status = 'completed'
             WHERE status = 'active'
               AND context_mode <> 'watch'
               AND group_folder NOT LIKE 'schedule\\_%' ESCAPE '\\'",
            [],
        )?;
        Ok(())
    }) {
        tracing::warn!("[SenClaw] failed to retire legacy schedules: {e}");
    }

    let task_executor = Arc::new(
        scheduler::DefaultTaskExecutor::new(Arc::clone(&db))
            .with_agent_api(Arc::clone(&agent_pool) as Arc<dyn types::AgentApi>)
            // Watch mode probes an MCP tool directly, with no LLM in the loop.
            .with_mcp_manager(Arc::clone(&mcp_manager)),
    );
    let _task_scheduler = scheduler::task_scheduler::TaskScheduler::new(
        Arc::clone(&db),
        task_executor,
        30, // poll interval in seconds
    )
    .start();
    tracing::info!("[SenClaw] TaskScheduler started (30s poll interval)");

    // ===== 5b. VirtualWorkerPool =====
    let virtual_worker_pool = Arc::new(agent::virtual_worker_pool::VirtualWorkerPool::new(
        Arc::new(agent::virtual_worker_pool::ZenVirtualCoreApi::new(Some(
            Arc::clone(&mcp_manager),
        ))),
    ));
    // Wire permission config follow (mirrors main-agent skip-perms).
    {
        let pool = agent_pool.clone();
        virtual_worker_pool.set_permission_bind(
            move |_virtual_jid: &str, _persona_name: &str, _skip_perms: bool| {
                // Permission bridge for virtual agents: follow main-agent config.
                // Real implementation will register PermissionBridge handlers
                // on the virtual core's engine.
                None
            },
            Arc::new(move || pool.get_skip_perms_for_virtual()),
        );
    }
    // Wire virtual agent permission forwarding: when a virtual subagent needs user
    // approval, forward the request to the admin Web UI via PermissionBridge, then
    // block until the user responds (up to 10 minutes).
    {
        let pool_for_vw = agent_pool.clone();
        let senders_for_vw = Arc::clone(&virtual_perm_senders);
        virtual_worker_pool.set_virtual_permission_fn(Arc::new(
            move |virtual_jid: String,
                  tool_name: String,
                  permission_key: String,
                  title: String,
                  content: serde_json::Value,
                  options: HashMap<String, String>,
                  tx: std::sync::mpsc::SyncSender<String>| {
                let key = format!("{virtual_jid}::{tool_name}");
                senders_for_vw.lock().unwrap().insert(key, tx);
                pool_for_vw.handle_virtual_permission_request(
                    &virtual_jid,
                    &tool_name,
                    &permission_key,
                    &title,
                    &content,
                    &options,
                );
            },
        ));
    }
    // Inject browser MCP server so browser-agent virtual instances have browser tools.
    // Use zen_core::McpServerConfig (not mcp::helper) since VirtualWorkerPool uses that type.
    virtual_worker_pool.set_extra_mcp_servers(vec![{
        // Coarse identity: all virtual workers share one browser tab for now.
        // Per-persona tabs need VirtualWorkerPool to build configs per worker.
        let helper_cfg = crate::mcp::helper::browser_mcp_config(
            cfg.ws_port,
            cfg.ui_server.port,
            "virtual-worker",
            crate::browser_agent::settings::engine_at(&cfg.paths.global_config_path)
                == crate::browser_agent::settings::Engine::V2,
        );
        crate::zen_core::McpServerConfig {
            name: helper_cfg.name,
            command: helper_cfg.command,
            args: helper_cfg.args,
            env: helper_cfg.env,
            request_timeout_secs: None,
        }
    }]);
    tracing::info!("[SenClaw] VirtualWorkerPool ready (browser-mcp injected)");

    // ===== 6. WebSocketGateway + UIServer =====
    // WS and UI listen on separate ports (matching TS config).

    // 5a. WebSocket gateway
    // ===== 5b1. API access-token policy =====
    // Default posture: bind loopback, no token required. Opting into LAN
    // exposure (`SENCLAW_UI_BIND_HOST=0.0.0.0`) turns on token auth for every
    // non-loopback peer of both the HTTP UI (18788) and the WS gateway
    // (18789). Loopback peers stay exempt so the bundled desktop app and
    // Space Apps calling back into the daemon keep working unchanged.
    let ui_bind_host = cfg.ui_server.bind_host.clone();
    let api_auth = {
        let bind_is_loopback = gateway::ui_server::auth::is_loopback_host(&ui_bind_host);
        let senclaw_dir = cfg
            .paths
            .global_config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        // Resolve the token even when auth is off: local clients may read it
        // from disk ahead of a later LAN-exposed restart, and the operator can
        // switch the mode on at runtime without one.
        let token = gateway::ui_server::auth::resolve_token(
            cfg.ui_server.api_token.as_deref(),
            &senclaw_dir,
        );
        // Every caller that reaches the daemon's own HTTP API over loopback
        // needs this once `always` is in force, or `space_app_*`, the kanban
        // LLM lookup and the local-model proxy all start 401-ing the moment
        // the gate closes. In-process callers read it from here; MCP children
        // get it in their own config env (`mcp::helper`).
        util::internal_auth::set_daemon_token(&token);
        let auth = gateway::ui_server::auth::ApiAuth {
            env_mode: cfg.ui_server.auth_mode,
            env_set: cfg.ui_server.auth_mode_from_env,
            bind_is_loopback,
            token: Some(token),
            token_path: Some(senclaw_dir.join("api_token")),
            cookie_secure: cfg.ui_server.cookie_secure,
            db: Some(Arc::clone(&db)),
        };
        let (mode, _) = auth.effective_mode();
        if auth.required() {
            tracing::warn!(
                "[SenClaw] API token required (auth mode {:?}, bind host {ui_bind_host:?}) — \
                 token file: {}, override: SENCLAW_API_TOKEN",
                mode.as_str(),
                senclaw_dir.join("api_token").display()
            );
        } else if !bind_is_loopback {
            // `off` on an exposed bind is a deliberate choice, but a silent one
            // would be indistinguishable from the gate failing to engage.
            tracing::warn!(
                "[SenClaw] UI bound to non-loopback host {ui_bind_host:?} with auth mode \
                 \"off\" — the API is open to the network"
            );
        }
        Arc::new(auth)
    };

    let ws_gateway = {
        let browser_relay = Arc::new(gateway::websocket_gateway::BrowserRelay::new());

        let ws_state = Arc::new(gateway::websocket_gateway::WsState {
            config: Arc::new(cfg.clone()),
            db: Arc::clone(&db),
            group_manager: Arc::clone(&gm),
            agent_manager: Arc::clone(&am),
            binding_manager: Arc::clone(&bm),
            channel_manager: Arc::clone(&cm),
            api: ws_api.clone() as Arc<dyn gateway::websocket_gateway::WsGatewayApi>,
            agent_api: Some(agent_pool.clone() as Arc<dyn types::AgentApi>),
            browser_relay,
            marketplace_manager: Some(Arc::clone(&marketplace_shared)),
        });

        let gw = Arc::new(gateway::websocket_gateway::WebSocketGateway::new(
            cfg.ws_port,
            cfg.ui_server.ws_token.clone(),
        ));
        gw.set_db_for_cowork(Arc::clone(&db));

        // Wire full event sink: AgentPool → WebSocket gateway.
        // Forwards reply / state / todos / permission / ask-question events,
        // populating the gateway's last-known state map so newly subscribed
        // clients (Agent Console) see currently-running agents.
        agent_pool.set_agent_event_sink(Arc::new(WsAgentEventSink {
            gateway: Arc::clone(&gw),
            db: Arc::clone(&db),
        }));

        // Forward `app:*` chat events (tool executions, agent state, …) to mobile
        // relay clients as CTRL_API_EVENT frames. `agent:reply`/`incoming` are
        // skipped — those already reach the app over the encrypted chat path.
        if !app_channels.is_empty() {
            use channels::app::CTRL_API_EVENT;
            let app_chs = app_channels.clone();
            gw.set_app_event_sink(Arc::new(move |chat_jid: String, msg: serde_json::Value| {
                let topic = msg
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if topic.is_empty() || topic == "agent:reply" || topic == "incoming" {
                    return;
                }
                for app in &app_chs {
                    if app.owns_jid(&chat_jid) {
                        let app = Arc::clone(app);
                        let meta = serde_json::json!({ "topic": topic, "data": msg }).to_string();
                        tokio::spawn(async move {
                            let _ = app.send_control(CTRL_API_EVENT, meta).await;
                        });
                        break;
                    }
                }
            }));
            tracing::info!(
                "[SenClaw] App-channel event forwarder wired ({} channel(s))",
                app_channels.len()
            );
        }

        // Wire MessageRouter → WebSocket gateway for real-time incoming messages.
        message_router.set_ws_gateway(Arc::clone(&gw)).await;

        // Wire DispatchBridge → WebSocket gateway. Every state mutation pushes
        // a `dispatch:update` to admin clients so the Agent Console reflects
        // current parents/tasks without polling.
        {
            let gw_for_dispatch = Arc::clone(&gw);
            dispatch_bridge.set_ws_notify(Arc::new(move |parents: &serde_json::Value| {
                let gw = Arc::clone(&gw_for_dispatch);
                let parents = parents.clone();
                tokio::spawn(async move {
                    gw.notify_dispatch_update(&parents).await;
                });
            }));
        }

        // Wire DispatchBridge → Cowork board. Dispatch task lifecycle events
        // (registered/processing/done/error/timeout) update the CoworkTeamTask
        // rows so the board columns reflect sub-agent progress in real time.
        {
            let db_for_lifecycle = Arc::clone(&db);
            dispatch_bridge.set_task_lifecycle_callback(Arc::new(
                move |task_id: &str,
                      status: &str,
                      label: &str,
                      parent_goal: &str,
                      result: Option<String>| {
                    gateway::ui_server::cowork_runtime::on_dispatch_task_lifecycle(
                        &db_for_lifecycle,
                        task_id,
                        status,
                        label,
                        parent_goal,
                        result,
                    );
                },
            ));
        }

        // Kanban → WS live updates. A 2s watcher polls a cheap per-board change
        // signature and pushes `kanban:update {boardId}` on any change. Polling
        // (vs. write hooks) is deliberate: board writers include the SEPARATE
        // `kanban-server` stdio MCP process (dispatcher workers), which in-process
        // hooks would never see — the shared SQLite file is the one truth.
        {
            let gw_for_kanban = Arc::clone(&gw);
            let kanban_db = Arc::clone(&kanban_state.db);
            tokio::spawn(async move {
                let mut prev: std::collections::HashMap<i64, (i64, i64)> =
                    std::collections::HashMap::new();
                let mut first = true;
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
                loop {
                    tick.tick().await;
                    let sig = match kanban_db.change_signature() {
                        Ok(s) => s,
                        Err(_) => continue,
                    };
                    let cur: std::collections::HashMap<i64, (i64, i64)> =
                        sig.into_iter().map(|(id, up, n)| (id, (up, n))).collect();
                    if !first {
                        for (id, v) in &cur {
                            if prev.get(id) != Some(v) {
                                gw_for_kanban.notify_kanban_update(*id).await;
                            }
                        }
                        // A deleted board also warrants a refresh of the board list.
                        for id in prev.keys() {
                            if !cur.contains_key(id) {
                                gw_for_kanban.notify_kanban_update(*id).await;
                            }
                        }
                    }
                    prev = cur;
                    first = false;
                }
            });
        }

        // Wire WorkbenchBridge → WebSocket gateway. Forwards artifact
        // lifecycle events to subscribed clients (workbench panel renders).
        {
            let gw_new = Arc::clone(&gw);
            workbench_bridge.set_on_new(Box::new(move |chat_jid, payload| {
                let gw = Arc::clone(&gw_new);
                let jid = chat_jid.to_string();
                let artifact = serde_json::to_value(&payload.artifact).unwrap_or_default();
                let replaces = payload.replaces_id.clone();
                tokio::spawn(async move {
                    gw.notify_workbench_new(&jid, &artifact, replaces.as_deref())
                        .await;
                });
            }));
            let gw_ready = Arc::clone(&gw);
            workbench_bridge.set_on_service_ready(Box::new(move |chat_jid, payload| {
                let gw = Arc::clone(&gw_ready);
                let jid = chat_jid.to_string();
                let aid = payload.artifact_id.clone();
                let ready = payload.ready;
                tokio::spawn(async move {
                    gw.notify_workbench_service_ready(&jid, &aid, ready).await;
                });
            }));
            let gw_crashed = Arc::clone(&gw);
            workbench_bridge.set_on_service_crashed(Box::new(move |chat_jid, payload| {
                let gw = Arc::clone(&gw_crashed);
                let jid = chat_jid.to_string();
                let aid = payload.artifact_id.clone();
                let logs = payload.last_log_lines.clone();
                tokio::spawn(async move {
                    gw.notify_workbench_service_crashed(&jid, &aid, &logs).await;
                });
            }));
            let gw_stopped = Arc::clone(&gw);
            workbench_bridge.set_on_service_stopped(Box::new(move |chat_jid, payload| {
                let gw = Arc::clone(&gw_stopped);
                let jid = chat_jid.to_string();
                let aid = payload.artifact_id.clone();
                let reason = payload.reason.clone();
                tokio::spawn(async move {
                    gw.notify_workbench_service_stopped(&jid, &aid, &reason)
                        .await;
                });
            }));
        }

        // Wire ZenCoreApi → WebSocket gateway for Plan-mode exit requests.
        // When an agent calls the `ExitPlanMode` tool, the engine emits
        // `EngineEvent::PlanExitRequest` which the API forwards here. The UI
        // catches `plan:exit:request` and renders the `PlanExitDialog`.
        {
            let gw_plan = Arc::clone(&gw);
            let db_plan = Arc::clone(&db);
            let plans_dir = cfg
                .paths
                .db_path
                .parent()
                .map(|p| p.join("plans"))
                .unwrap_or_else(|| std::path::PathBuf::from("plans"));
            zen_core_api.set_on_plan_exit_request(Arc::new(move |jid, data| {
                let gw = Arc::clone(&gw_plan);
                let db = Arc::clone(&db_plan);
                let plans_dir = plans_dir.clone();
                tokio::spawn(async move {
                    // Persist the plan to disk + DB at request time so the
                    // markdown is queryable as history even if the user
                    // never clicks approve. Approval starts as "pending".
                    persist_plan(&db, &plans_dir, &jid, &data.agent_id, &data.plan_content);
                    gw.notify_plan_exit_request(
                        &jid,
                        &data.agent_id,
                        &data.plan_file_path,
                        &data.plan_content,
                        &data.options.start_editing,
                        &data.options.clear_context_and_start,
                    )
                    .await;
                });
            }));
        }

        // Tool execution → WS gateway. Each completed (or errored) tool call
        // is broadcast as a single `tool:execution` message so the chat UI
        // can group consecutive calls into a "Read 3 files, ran 1 command"
        // collapsible card (claude-code style).
        {
            let gw_tool = Arc::clone(&gw);
            let db_tool = Arc::clone(&db);
            let default_msg_limit = cfg.agent.max_messages_per_group;
            let cp_svc = Arc::clone(&checkpoint_service);
            // Weak: the callback is stored *on* zen_core_api, so a strong
            // handle here would be a reference cycle.
            let zen_weak = Arc::downgrade(&zen_core_api);
            zen_core_api.set_on_tool_execution(Arc::new(move |jid, ev| {
                let gw = Arc::clone(&gw_tool);
                let db = Arc::clone(&db_tool);
                let cp_svc = Arc::clone(&cp_svc);
                let zen_weak = zen_weak.clone();
                tokio::spawn(async move {
                    // Compute the timestamp once so the persisted row and the
                    // live wire frame agree.
                    let ts = chrono::Utc::now().to_rfc3339();
                    // Persist before broadcasting so a reload immediately
                    // after the event still includes it in `history:load`.
                    let content_json =
                        serde_json::to_string(&ev.content).unwrap_or_else(|_| "{}".to_string());
                    if let Err(e) = db.insert_tool_execution(
                        &jid,
                        &ev.agent_id,
                        &ev.tool_name,
                        &ev.title,
                        &ev.summary,
                        &ev.description,
                        &content_json,
                        ev.ok,
                        &ts,
                        default_msg_limit,
                    ) {
                        tracing::warn!(
                            error = %e,
                            jid = %jid,
                            tool = %ev.tool_name,
                            "[WsGateway] failed to persist tool execution; live broadcast continues"
                        );
                    }
                    gw.notify_tool_execution(
                        &jid,
                        &ev.agent_id,
                        &ev.tool_name,
                        &ev.title,
                        &ev.summary,
                        &ev.description,
                        &ev.content,
                        ev.ok,
                        &ts,
                    )
                    .await;

                    // When an agent mutates Space (create/update/delete
                    // events) via MCP, the calendar UI has no way to know
                    // the DB just changed. Broadcast a small kick so the
                    // CalendarView re-fetches via `/api/space/calendar/events`.
                    // Without this, screenshots like "agent says event
                    // created but calendar still empty" happen — the row IS
                    // in the DB, the UI just never reloaded.
                    if ev.ok && is_space_mutation_tool(&ev.tool_name) {
                        gw.broadcast_to_all(&serde_json::json!({
                            "type": "space:events:changed",
                            "toolName": ev.tool_name,
                        }))
                        .await;
                    }

                    // Checkpoint after the fact. Runs inside this detached
                    // task so a slow `git add` never delays the tool loop or
                    // the live broadcast above.
                    let working_dir = zen_weak.upgrade().and_then(|api| api.working_dir_for(&jid));
                    let group_type = db
                        .get_group(&jid)
                        .ok()
                        .flatten()
                        .map(|g| g.group_type)
                        .unwrap_or_default();
                    match cp_svc
                        .on_tool_event(
                            &jid,
                            working_dir.as_deref(),
                            &group_type,
                            &ev.tool_name,
                            &ev.content,
                            ev.ok,
                            &ev.summary,
                        )
                        .await
                    {
                        Ok(Some(cp)) => gw.notify_checkpoint_new(&jid, &cp).await,
                        Ok(None) => {}
                        Err(e) => tracing::warn!(
                            error = %e,
                            jid = %jid,
                            tool = %ev.tool_name,
                            "[Checkpoints] checkpoint failed; the edit itself is unaffected"
                        ),
                    }
                });
            }));
        }

        // Widget emit → persist + WS gateway. One-way `chat:widget` push
        // (display-only, no response round-trip). Persist before broadcasting
        // so a reload immediately after still replays it. Mirrors the
        // tool_execution block above.
        {
            let gw_widget = Arc::clone(&gw);
            let db_widget = Arc::clone(&db);
            let chs_widget = Arc::clone(&channels);
            let default_msg_limit = cfg.agent.max_messages_per_group;
            zen_core_api.set_on_widget_emit(Arc::new(move |jid, data| {
                let gw = Arc::clone(&gw_widget);
                let db = Arc::clone(&db_widget);
                let chs = Arc::clone(&chs_widget);
                tokio::spawn(async move {
                    // The tool's optional `chat_jid` overrides the engine's jid;
                    // otherwise the emit targets the emitting agent's chat.
                    let chat_jid = data.chat_jid.clone().unwrap_or(jid);
                    let ts = chrono::Utc::now().to_rfc3339();
                    let widget_val = serde_json::to_value(&data.widget)
                        .unwrap_or_else(|_| serde_json::json!({}));
                    let widget_json =
                        serde_json::to_string(&data.widget).unwrap_or_else(|_| "{}".to_string());
                    if let Err(e) = db.insert_chat_widget(
                        &data.id,
                        &chat_jid,
                        &widget_json,
                        &ts,
                        default_msg_limit,
                    ) {
                        tracing::warn!(
                            error = %e,
                            jid = %chat_jid,
                            "[WsGateway] failed to persist chat widget; live broadcast continues"
                        );
                    }
                    gw.notify_widget(&chat_jid, &data.id, &widget_val, &ts)
                        .await;

                    // Text fallback for messaging channels. The `chat:widget`
                    // broadcast above only reaches subscribed WS clients (web /
                    // desktop / `app:` relay) — a Telegram/Feishu/WeChat jid
                    // sees nothing, and before this the emit was a silent drop
                    // the tool still reported as success. Render one text line
                    // and push it through the owning channel. Same egress gate
                    // as `set_send_reply` (the reply path this mirrors).
                    if !(chat_jid.starts_with("web:")
                        || chat_jid.starts_with("virtual:")
                        || chat_jid.starts_with("app:"))
                    {
                        let text = crate::widgets::fallback_text(&data.widget);
                        if !text.is_empty() && crate::security::gate(&chat_jid, &text) {
                            let guard = chs.lock().await;
                            for c in guard.iter() {
                                if c.owns_jid(&chat_jid) {
                                    // No per-emit bot token: the channel's
                                    // default bot delivers the fallback.
                                    let _ = c.send_message(&chat_jid, &text, None).await;
                                    break;
                                }
                            }
                        }
                    }
                });
            }));
        }

        // Wire CoworkManager → WebSocket gateway. Every mutation fires
        // Wire DispatchBridge → AgentPool. The scheduler hands off augmented
        // prompts to sub-agents via GroupQueue + process_and_wait, mirroring
        // the inbound message path. Workspace overrides are applied before
        // enqueue so the sub-agent picks them up.
        {
            let pool = agent_pool.clone();
            let gm = Arc::clone(&gm);
            let gq = Arc::clone(&group_queue);
            let db = Arc::clone(&db);
            dispatch_bridge.set_send_to_agent(Arc::new(
                move |jid: &str, task_id: &str, prompt: &str, workspace_dir: &str| {
                    tracing::info!(
                        "[DispatchBridge] send_to_agent: jid={jid} task={task_id} ws={workspace_dir} prompt_len={}",
                        prompt.len()
                    );
                    let binding: types::GroupBinding = match gm.get(&db, jid) {
                        Some(b) => b,
                        None => {
                            tracing::warn!(
                                "[DispatchBridge] send_to_agent: no binding for {jid}, dropping task {task_id}"
                            );
                            return;
                        }
                    };
                    if !workspace_dir.is_empty() {
                        pool.set_dispatch_workspace(jid, workspace_dir);
                    }
                    pool.set_current_dispatch_task_id(jid, task_id);
                    pool.mark_dispatch_executing(jid);

                    let pool = pool.clone();
                    let gq = Arc::clone(&gq);
                    let jid_owned = jid.to_string();
                    let task_id_owned = task_id.to_string();
                    let prompt_owned = prompt.to_string();
                    tokio::spawn(async move {
                        let pool_inner = pool.clone();
                        let jid_run = jid_owned.clone();
                        let task_id_run = task_id_owned.clone();
                        gq.enqueue(
                            &jid_owned,
                            Box::pin(async move {
                                tracing::info!(
                                    "[DispatchBridge] queue task start: jid={jid_run} task={task_id_run}"
                                );
                                let result = types::AgentApi::process_and_wait(
                                    pool_inner.as_ref(),
                                    &jid_run,
                                    &binding,
                                    &prompt_owned,
                                )
                                .await;
                                match result {
                                    Ok(()) => tracing::info!(
                                        "[DispatchBridge] queue task done: jid={jid_run} task={task_id_run}"
                                    ),
                                    Err(e) => tracing::warn!(
                                        "[DispatchBridge] queue task error: jid={jid_run} task={task_id_run}: {e}"
                                    ),
                                }
                            }),
                        )
                        .await;
                    });
                },
            ));
        }
        {
            let pool = agent_pool.clone();
            dispatch_bridge.set_revert_workspace(Arc::new(move |jid: &str| {
                pool.revert_dispatch_workspace(jid);
            }));
        }
        {
            let pool = agent_pool.clone();
            dispatch_bridge.set_abort_agent(Arc::new(move |jid: &str, reason: &str| {
                let pool = pool.clone();
                let jid = jid.to_string();
                let reason = reason.to_string();
                tokio::spawn(async move {
                    tracing::warn!("[DispatchBridge] aborting {jid}: {reason}");
                    pool.destroy_inner(&jid).await;
                });
            }));
        }
        // Wire virtual-agent dispatch (Phase 5): persona registry + worker pool.
        dispatch_bridge.set_virtual_workers(
            Arc::clone(&persona_registry),
            Arc::clone(&virtual_worker_pool),
        );
        // Wire virtual-agent todos → WebSocket gateway (mirrors TS
        // virtualWorkerPool.setTodosNotify).
        {
            let gw_for_todos = Arc::clone(&gw);
            virtual_worker_pool.set_todos_notify(Arc::new(
                move |jid: &str, name: &str, todos: &[agent::virtual_worker_pool::TodoItem]| {
                    let todos = serde_json::to_value(todos).unwrap_or(serde_json::Value::Null);
                    let jid = jid.to_string();
                    let name = name.to_string();
                    let gw = Arc::clone(&gw_for_todos);
                    tokio::spawn(async move {
                        gw.notify_agent_todos(&jid, &name, &todos).await;
                    });
                },
            ));
        }

        // Wire sub-agent activity events (tool calls, messages) → WS + DB persistence,
        // and record which files each dispatch task actually wrote.
        {
            let gw_for_activity = Arc::clone(&gw);
            let db_for_activity = Arc::clone(&db);
            let bridge_for_writes = Arc::clone(&dispatch_bridge);
            virtual_worker_pool.set_activity_notify(Arc::new(
                move |task_id: &str, entry: agent::virtual_worker_pool::SubAgentActivityEntry| {
                    // This event is already scoped to one task's agent loop, so
                    // the path it reports belongs to *this* task even when every
                    // task in the DAG shares a working directory — which is what
                    // a workspace-wide snapshot diff cannot tell apart.
                    if let (Some(tool), Some(content)) =
                        (entry.tool_name.as_deref(), entry.content.as_ref())
                    {
                        if let Some((path, kind)) =
                            agent::dispatch_bridge::written_path_from_tool(tool, content)
                        {
                            bridge_for_writes.add_file_change(task_id, &path, kind);
                        }
                    }
                    let gw = Arc::clone(&gw_for_activity);
                    let db = Arc::clone(&db_for_activity);
                    let task_id = task_id.to_string();
                    let entry_clone = entry.clone();
                    tokio::spawn(async move {
                        // Persist first so history:load includes it on next reload.
                        let content_json = entry_clone
                            .content
                            .as_ref()
                            .map(|v| serde_json::to_string(v).unwrap_or_default());
                        if let Err(e) = db.insert_dispatch_activity(
                            &task_id,
                            "", // parent_id — resolved by frontend from dispatchParents
                            &entry_clone.entry_type,
                            entry_clone.tool_name.as_deref(),
                            entry_clone.title.as_deref(),
                            entry_clone.summary.as_deref(),
                            content_json.as_deref(),
                            entry_clone.ok,
                            entry_clone.text.as_deref(),
                            &entry_clone.ts,
                        ) {
                            tracing::warn!("[DispatchActivity] persist failed task={task_id}: {e}");
                        }
                        gw.notify_dispatch_activity(&task_id, &entry).await;
                    });
                },
            ));
        }
        // Initial agent sync — without this, MCP `dispatch_task` can't resolve
        // agent name → jid (state.agents stays empty) and tasks never leave
        // `registered`. Re-sync periodically to pick up groups added/removed
        // through the Web UI without needing per-handler hooks.
        {
            let groups = gm.list(&db).unwrap_or_default();
            dispatch_bridge.update_agents(&groups);
            tracing::info!(
                "[SenClaw] DispatchBridge agents synced ({} group(s))",
                groups.len()
            );
        }
        {
            let bridge_for_sync = Arc::clone(&dispatch_bridge);
            let gm_for_sync = Arc::clone(&gm);
            let db_for_sync = Arc::clone(&db);
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
                tick.tick().await; // skip immediate
                loop {
                    tick.tick().await;
                    let groups = gm_for_sync.list(&db_for_sync).unwrap_or_default();
                    bridge_for_sync.update_agents(&groups);
                }
            });
        }
        dispatch_bridge.start();

        // ===== MCPDispatcher — autonomous task execution from dispatch sources =====
        // Always constructed (so the Settings toggle can turn it on/off live); the
        // poll loop idles unless the persisted `dispatchEnabled` flag is true. The
        // env SENCLAW_DISPATCH_ENABLED just seeds that flag on first boot.
        {
            use app_space_sdk::dispatch::{
                DispatchProvider, DispatchSource, LocalDispatchSource, McpServerSpec,
            };
            // Seed the persisted flag from the env default (does not override a
            // value the user already set via Settings).
            if cfg.dispatch.enabled {
                let path = &cfg.paths.global_config_path;
                if !gateway::group_manager::get_dispatch_enabled(path) {
                    let _ = gateway::group_manager::save_dispatch_enabled(path, true);
                }
            }
            // In-process Kanban source: the worker gets the NATIVE `kanban-server`
            // stdio MCP (no HTTP bridge), and claims/finalizes go straight to the
            // built-in board's DB.
            let senclaw_exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "senclaw".into());
            let worker_mcp = McpServerSpec::Stdio {
                name: "senclaw-kanban".into(),
                command: senclaw_exe,
                args: vec!["kanban-server".into()],
                env: Default::default(),
            };
            let provider: std::sync::Arc<dyn DispatchProvider> =
                std::sync::Arc::new(kanban::dispatch::KanbanDispatchProvider::new(
                    std::sync::Arc::clone(&kanban_state.db),
                    worker_mcp,
                ));
            let sources: Vec<std::sync::Arc<dyn DispatchSource>> = vec![std::sync::Arc::new(
                LocalDispatchSource::new("kanban", provider),
            )];
            let dcfg = agent::mcp_dispatch::DispatcherConfig {
                interval_secs: cfg.dispatch.interval_secs,
                max_concurrent: cfg.dispatch.max_concurrent,
                per_assignee: cfg.dispatch.per_assignee,
                max_agent_turns: (cfg.dispatch.max_agent_turns > 0)
                    .then_some(cfg.dispatch.max_agent_turns),
                default_timeout_secs: cfg.dispatch.default_timeout_secs,
                workdir_root: cfg
                    .paths
                    .db_path
                    .parent()
                    .map(|p| p.join("mcp-dispatch"))
                    .unwrap_or_else(|| std::path::PathBuf::from(".senclaw/mcp-dispatch")),
                config_path: cfg.paths.global_config_path.clone(),
            };
            let mcp_dispatcher = agent::mcp_dispatch::MCPDispatcher::new(
                sources,
                std::sync::Arc::clone(&persona_registry),
                dcfg,
            );
            mcp_dispatcher.start();
            // Keep the Arc alive for the life of the daemon.
            let _mcp_dispatcher = mcp_dispatcher;
        }

        // Gate every WS route (`/`, `/browser`, `/browser-mcp`) at upgrade
        // time when the daemon is exposed beyond loopback. The in-band
        // `connect` token is not a sufficient gate — the message dispatcher
        // runs handlers for sockets that never authenticated.
        let ws_router = gw
            .route(ws_state)
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&api_auth),
                gateway::ui_server::auth::ws_auth_mw,
            ));
        let ws_port = cfg.ws_port;
        let ws_addr = format!("{ui_bind_host}:{ws_port}");
        tracing::info!("[SenClaw] WebSocket gateway at ws://{ws_addr}");
        tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind(&ws_addr).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!("[SenClaw] WS bind {ws_addr}: {e}");
                    return;
                }
            };
            if let Err(e) = axum::serve(
                listener,
                ws_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            {
                tracing::error!("[SenClaw] WS server error: {e}");
            }
        });
        gw
    };

    // ===== 5b2. BackgroundScheduler =====
    // Autonomous work SenClaw runs by itself: periodic upkeep, an App's standing
    // duties, unattended follow-up. Unlike TaskScheduler above, a run here is
    // *not* a chat session — no GroupBinding, no reply. Constructed after
    // ws_gateway so runs are visible live; nothing else pushes when one fires.
    //
    // See docs/background-tasks-design.md.
    let background_native = Arc::new(background::NativeRegistry::new());
    let background_scheduler = background::BackgroundScheduler::new(
        Arc::clone(&db),
        cfg.background.clone(),
        Some(Arc::clone(&persona_registry)),
        Arc::clone(&background_native),
        Some(Arc::new(WsBackgroundEventSink {
            gateway: Arc::clone(&ws_gateway),
        })),
        cfg.paths.workspace_dir.to_string_lossy().to_string(),
    );
    // Hold the handle. `_task_scheduler` and `_event_notifier` below are bound
    // to `_` locals and dropped immediately, leaving them with no abort path on
    // shutdown — don't repeat that here.
    let _background_handle = background_scheduler.start();

    // 5c. EventNotifier — polls space_events for reminders and status transitions.
    //     Wired after ws_gateway so it can push events to connected clients.
    {
        // Arc<WebSocketGateway> implements EventNotifySink; wrap in a second Arc
        // to get the Arc<dyn EventNotifySink> the EventNotifier expects.
        struct WsEventSinkWrapper(Arc<gateway::websocket_gateway::WebSocketGateway>);
        impl scheduler::EventNotifySink for WsEventSinkWrapper {
            fn notify_event_reminder(
                &self,
                notification_id: &str,
                event_id: &str,
                title: &str,
                start_at_ms: i64,
                kind: &str,
                fired_at_ms: i64,
                delayed_ms: i64,
            ) {
                let gw = Arc::clone(&self.0);
                let nid = notification_id.to_string();
                let eid = event_id.to_string();
                let t = title.to_string();
                let k = kind.to_string();
                tokio::spawn(async move {
                    gw.push_event_reminder(
                        &nid,
                        &eid,
                        &t,
                        start_at_ms,
                        &k,
                        fired_at_ms,
                        delayed_ms,
                    )
                    .await;
                });
            }
        }
        let event_sink: Arc<dyn scheduler::EventNotifySink> =
            Arc::new(WsEventSinkWrapper(Arc::clone(&ws_gateway)));
        let _event_notifier = scheduler::EventNotifier::new(
            Arc::clone(&db),
            event_sink,
            60, // poll every 60 seconds
        )
        .start();
        let tz_name = chrono::Local::now().format("%Z %z").to_string();
        tracing::info!("[SenClaw] EventNotifier started (60s poll, local TZ: {tz_name})");
    }

    // 6b. WorkflowService — saved DAGs of agent + script steps. Fully
    //     decoupled from AgentPool/DispatchBridge (isolated sessions only);
    //     state changes push to admin clients as `workflow:update`.
    let workflow_service = {
        let gw = Arc::clone(&ws_gateway);
        let on_update: workflow::executor::OnUpdate = Arc::new(move |run| {
            let gw = Arc::clone(&gw);
            let run_json = serde_json::to_value(run).unwrap_or_default();
            tokio::spawn(async move {
                gw.notify_workflow_update(&run_json).await;
            });
        });
        let svc = Arc::new(workflow::WorkflowService::new(
            workflow::WorkflowServiceOpts {
                workflows_dir: cfg.paths.workflows_dir.clone(),
                workflow_state_path: cfg.paths.workflow_state_path.clone(),
                workflow_data_dir: cfg.paths.workflow_data_dir.clone(),
                persona_registry: Arc::clone(&persona_registry),
                concurrency: None,
                skills_extra_dirs: cli::commands::workflow::default_skills_dirs(&cfg),
                extra_mcp_servers: cli::commands::workflow::default_extra_mcp_servers(&cfg),
                shell_override: cfg.workflow_shell.clone(),
                on_update: Some(on_update),
            },
        ));
        tracing::info!(
            "[SenClaw] WorkflowService ready: {} definition(s) in {}",
            svc.list_defs().len(),
            cfg.paths.workflows_dir.display()
        );
        svc
    };

    // 7b. WikiManager
    let wiki_mgr = Arc::new(wiki::manager::WikiManager::new(cfg.paths.wiki_dir.clone()));
    if let Err(e) = wiki_mgr.ensure_init().await {
        tracing::warn!("[SenClaw] Wiki init failed (non-fatal): {e}");
    } else {
        tracing::info!(
            "[SenClaw] WikiManager initialized: {}",
            cfg.paths.wiki_dir.display()
        );
    }

    // 7c. UI HTTP server
    // No idle-unload worker here: local model weights live in runtime
    // processes (llama.cpp, sen-mlx) that the runtime manager's idle sweep
    // stops once unused, and a session app is stopped outright once idle, so
    // the memory returns to the OS without anyone else sweeping for it.
    {
        struct RealUiApi {
            agent_pool: Arc<agent::agent_pool::AgentPool>,
            ws_gateway: Arc<gateway::websocket_gateway::WebSocketGateway>,
            /// The same object the WS gateway enqueues through, so a REST
            /// message (code sessions, mobile) takes the identical path.
            ws_api: Arc<RealWsApi>,
            zen: Arc<agent::agent_pool::ZenCoreApi>,
            /// Needed to read a chat's `group_type`, which decides whether a
            /// non-git folder is checkpointed at all.
            db: Arc<db::Db>,
            checkpoints: Arc<checkpoints::CheckpointService>,
        }
        impl gateway::ui_server::UiApi for RealUiApi {
            fn submit_user_message(
                &self,
                group: &types::GroupBinding,
                text: &str,
            ) -> Result<(), String> {
                use gateway::websocket_gateway::WsGatewayApi as _;
                self.ws_api.enqueue_and_process(&group.jid, group, text, &[]);
                Ok(())
            }
            fn stop_agent(&self, jid: &str) {
                let pool = self.agent_pool.clone();
                let jid = jid.to_string();
                tokio::spawn(async move { pool.stop_agent(&jid).await });
            }
            fn set_working_dir(&self, jid: &str, dir: &str) {
                use agent::agent_pool::CoreApi as _;
                self.zen.set_working_dir(jid, dir);
                // Take the checkpoint baseline now, while the tree is still
                // as the agent found it. The tool hook cannot: it fires after
                // the tool, so on a new project — where the first tool is a
                // Write, with no file to Read first — it would commit that
                // very file as the baseline and leave nothing to diff.
                let cp = Arc::clone(&self.checkpoints);
                let db = Arc::clone(&self.db);
                let jid = jid.to_string();
                let dir = dir.to_string();
                tokio::spawn(async move {
                    let group_type = db
                        .get_group(&jid)
                        .ok()
                        .flatten()
                        .map(|g| g.group_type)
                        .unwrap_or_default();
                    cp.prepare(&jid, &dir, &group_type).await;
                });
            }
            fn reload_all_skills(&self) {
                self.agent_pool.reload_all_skills();
            }
            fn reload_all_hooks(&self) {
                self.agent_pool.reload_all_hooks();
            }
            fn broadcast_checkpoint_new(&self, jid: &str, cp: &types::ChatCheckpoint) {
                let gw = Arc::clone(&self.ws_gateway);
                let jid = jid.to_string();
                let cp = cp.clone();
                tokio::spawn(async move { gw.notify_checkpoint_new(&jid, &cp).await });
            }
            fn broadcast_event(&self, event: serde_json::Value) {
                // `broadcast_to_admins` is async; UiApi is sync because most
                // of it is cheap state access. Detach rather than block the
                // HTTP handler on socket writes.
                let gw = Arc::clone(&self.ws_gateway);
                tokio::spawn(async move {
                    gw.broadcast_to_admins(&event).await;
                });
            }
            fn send_channel_message(&self, chat_jid: &str, text: &str, bot_token: Option<&str>) {
                // Same detach-don't-block reasoning as `broadcast_event`: this
                // one reaches out to Telegram's API, which is slower still.
                let pool = Arc::clone(&self.agent_pool);
                let (jid, text, tok) = (
                    chat_jid.to_string(),
                    text.to_string(),
                    bot_token.map(str::to_string),
                );
                tokio::spawn(async move {
                    pool.broadcast_reply(&jid, &text, tok.as_deref()).await;
                });
            }
            fn get_thinking_enabled(&self) -> bool {
                self.agent_pool.get_thinking_enabled()
            }
            fn set_thinking_enabled(&self, enabled: bool) {
                self.agent_pool.set_thinking_enabled(enabled);
            }
            fn get_permissions_config(&self) -> gateway::ui_server::AdminPermissionsConfig {
                let cfg = self.agent_pool.get_permissions_config();
                gateway::ui_server::AdminPermissionsConfig {
                    skip_main_agent_permissions: cfg.skip_main_agent_permissions,
                    skip_all_agents_permissions: cfg.skip_all_agents_permissions,
                }
            }
            fn set_permissions_config(&self, config: gateway::ui_server::AdminPermissionsConfig) {
                self.agent_pool
                    .set_permissions_config(agent::agent_pool::PermissionsConfig {
                        skip_main_agent_permissions: config.skip_main_agent_permissions,
                        skip_all_agents_permissions: config.skip_all_agents_permissions,
                    });
            }
            fn resolve_permission(&self, request_id: &str, option_key: &str) {
                let _ = self.agent_pool.resolve_permission(request_id, option_key);
            }
            fn resolve_ask_question(
                &self,
                request_id: &str,
                answers: &serde_json::Value,
                other_texts: Option<&serde_json::Value>,
            ) {
                let _ =
                    self.agent_pool
                        .resolve_ask_question_batch(request_id, answers, other_texts);
            }
            fn resolve_form(&self, request_id: &str, values: &serde_json::Value, submitted: bool) {
                let _ =
                    self.agent_pool
                        .resolve_form(request_id, form_values_map(values), submitted);
            }
            fn resolve_plan_exit(&self, group_jid: &str, agent_id: &str, selected: &str) {
                self.agent_pool
                    .resolve_plan_exit(group_jid, agent_id, selected);
            }
        }

        let ui_state = Arc::new(gateway::ui_server::UiState {
            config: Arc::new(cfg.clone()),
            db: Some(Arc::clone(&db)),
            group_manager: Some(Arc::clone(&gm)),
            wiki_manager: Some(Arc::clone(&wiki_mgr)),
            persona_registry: Some(Arc::clone(&persona_registry)),
            agent_api: Some(Arc::new(RealUiApi {
                agent_pool: agent_pool.clone(),
                ws_gateway: Arc::clone(&ws_gateway),
                ws_api: Arc::clone(&ws_api),
                zen: Arc::clone(&zen_core_api),
                db: Arc::clone(&db),
                checkpoints: Arc::clone(&checkpoint_service),
            })),
            mcp_manager: Some(Arc::clone(&mcp_manager)),
            marketplace_manager: Some(Arc::clone(&marketplace_shared)),
            workbench_bridge: Some(Arc::clone(&workbench_bridge)),
            dispatch_bridge: Some(Arc::clone(&dispatch_bridge)),
            space_mcp_launcher: Some(Arc::clone(&space_mcp_launcher)),
            workflow_service: Some(Arc::clone(&workflow_service)),
            virtual_worker_pool: Some(Arc::clone(&virtual_worker_pool)),
            // Share the gateway's live state map so GET /api/chat/states serves
            // the same snapshot the web WS replays on reconnect.
            agent_states: Some(Arc::clone(&ws_gateway.last_known_states)),
            background_scheduler: Some(Arc::clone(&background_scheduler)),
            usage_recorder: Some(Arc::clone(&usage_recorder)),
            checkpoints: Some(Arc::clone(&checkpoint_service)),
            runtime_manager: Some(Arc::clone(&runtime_manager)),
            ws_port: cfg.ws_port,
            ws_token: cfg.ui_server.ws_token.clone().unwrap_or_default(),
            api_auth: Arc::clone(&api_auth),
        });

        // Make the same state reachable to app-channel relay clients (REST tunnel).
        api_bridge.set(Arc::clone(&ui_state));

        // Mount the built-in Kanban board's REST API under /api/kanban/* on the
        // daemon UI server (the native Flutter screen talks to these routes).
        // Auth middleware is layered *after* the kanban nest so it covers
        // those routes too (a layer added before a nest would not wrap it).
        let ui_router = gateway::ui_server::build_router(ui_state)
            .nest(
                "/api/kanban",
                kanban::api::api_router(std::sync::Arc::clone(&kanban_state)),
            )
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&api_auth),
                gateway::ui_server::auth::http_auth_mw,
            ));
        let http_port = cfg.ui_server.port;
        let http_addr = format!("{ui_bind_host}:{http_port}");
        let listener = tokio::net::TcpListener::bind(&http_addr)
            .await
            .with_context(|| format!("bind {http_addr}"))?;
        tracing::info!("[SenClaw] Web UI at http://{http_addr}");
        // The bound listener already queues connections, so a boot task can
        // dial it before `axum::serve` below starts accepting.
        let _ = http_ready_tx.send(true);
        tokio::spawn(async move {
            if let Err(e) = axum::serve(
                listener,
                ui_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            {
                tracing::error!("[SenClaw] UI server error: {e}");
            }
        });
    }

    // 7d. Builtin personas
    subagents::builtin_personas::install_builtin_personas(&cfg.paths.virtual_agents_dir);

    // ===== 9. Graceful shutdown =====
    tracing::info!("[SenClaw] Daemon running. Press Ctrl-C to stop.");

    // SIGINT *and* SIGTERM. Only Ctrl-C was handled before, and the desktop app
    // stops the daemon with `kill -TERM` (then SIGKILL 800 ms later) — so the
    // shutdown below never ran in the way people actually quit, and every Space
    // App the daemon had launched survived it. Weeks-old orphans then held the
    // apps' ports, and each new daemon adopted them instead of launching a fresh,
    // sandboxed process.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => tracing::info!("[SenClaw] SIGINT received"),
                    _ = term.recv() => tracing::info!("[SenClaw] SIGTERM received"),
                }
            }
            Err(e) => {
                tracing::warn!("[SenClaw] cannot listen for SIGTERM ({e}); Ctrl-C only");
                tokio::signal::ctrl_c().await.ok();
            }
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await.ok();
    tracing::info!("[SenClaw] Shutting down...");

    // Disconnect all channels
    {
        let chs = channels.lock().await;
        for ch in chs.iter() {
            let id = ch.id();
            if let Err(e) = ch.disconnect().await {
                tracing::warn!("[SenClaw] Error disconnecting {id}: {e}");
            }
        }
    }

    // Stop every Space App process FIRST: they are what outlives the daemon,
    // and the window before SIGKILL is short (~800 ms from the desktop app).
    space_mcp_launcher.shutdown().await;
    // Every runtime process this daemon launched (engines: decision, OCR,
    // Whisper, TTS, GGUF/MLX models) — same reasoning as Space Apps above.
    runtime_manager.stop_all().await;

    // Flush in-flight workflow run state (running orphans reconcile on next boot)
    workflow_service.flush();

    // Drop ws_gateway to close all client connections
    drop(ws_gateway);

    tracing::info!("[SenClaw] Goodbye.");
    Ok(())
}
