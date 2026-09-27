// ===== Individual message handlers =====

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::Message;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::gateway::command_dispatcher::dispatch_command;
use crate::gateway::group_manager::{
    delete_feishu_app, delete_telegram_bot, get_feishu_apps, save_feishu_app, save_telegram_bot,
    GroupBindingUpdate,
};
use crate::gateway::plugin_command::dispatch_plugin_command;
use crate::types::{GroupBinding, TaskStatus};
use crate::util::local_time::local_iso_string_now;

use super::helpers::{broadcast_to_all_inner, now_iso, require_admin, require_auth, send_json};
use super::state::{WsClient, WsState};
use super::wire::{to_group_info, GroupInfo};

pub(crate) async fn handle_connect(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    token: &Option<String>,
    msg: &serde_json::Value,
    state: &Arc<WsState>,
) {
    if let Some(ref required_token) = token {
        let provided = msg["token"].as_str().unwrap_or("");
        if provided != required_token {
            send_json(
                sender,
                &serde_json::json!({"type": "auth:error", "message": "Invalid token"}),
            );
            return;
        }
    }
    let mut guard = clients.lock().await;
    if let Some(client) = guard.get_mut(client_idx) {
        client.authenticated = true;
        // Every WS gateway client is a trusted local UI (web/desktop), and the
        // per-chat admin distinction was removed — every chat is admin. Grant
        // admin here, at connect, rather than lazily on the first subscribe to
        // an already-registered group. Otherwise a fresh client whose very
        // first action is "New Session" (register:group) is rejected with
        // "Admin subscription required": register:group needs admin, but the
        // only way to gain admin was subscribing to a group that already
        // exists — a chicken-and-egg that silently dropped the new session's
        // first message ("Group not found") and left the agent never running.
        client.is_admin = true;
    }
    send_json(sender, &serde_json::json!({"type": "auth:ok"}));
    replay_event_notification_snapshot(sender, &state.db).await;
}

/// On connect, replay persisted reminders and forward-looking pending items so
/// reload / reconnect still shows notifications the user missed offline.
pub(crate) async fn replay_event_notification_snapshot(
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    db: &Arc<crate::db::Db>,
) {
    let now_ms = chrono::Utc::now().timestamp_millis();
    const PENDING_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;

    match db.list_event_notifications(Some(50)) {
        Ok(rows) => {
            if !rows.is_empty() {
                tracing::info!(
                    "[WsGateway] connect snapshot: replaying {} event notification(s)",
                    rows.len()
                );
            }
            for row in rows {
                send_json(
                    sender,
                    &serde_json::json!({
                        "type": "space:event:reminder",
                        "replay": true,
                        "id": row.id,
                        "eventId": row.event_id,
                        "title": row.title,
                        "startAt": row.start_at,
                        "kind": row.kind,
                        "firedAt": row.fired_at,
                        "delayedMs": row.delayed_ms,
                        "read": row.read_at.is_some(),
                    }),
                );
            }
        }
        Err(e) => {
            tracing::warn!("[WsGateway] connect snapshot: list notifications failed: {e}");
        }
    }

    match db.list_pending_event_reminders(now_ms, PENDING_WINDOW_MS) {
        Ok(pending) => {
            if !pending.is_empty() {
                tracing::info!(
                    "[WsGateway] connect snapshot: {} pending event reminder(s)",
                    pending.len()
                );
            }
            for p in pending {
                send_json(
                    sender,
                    &serde_json::json!({
                        "type": "space:event:pending",
                        "eventId": p.event_id,
                        "title": p.title,
                        "startAt": p.start_at,
                        "triggerAt": p.trigger_at,
                        "reminderMin": p.reminder_min,
                    }),
                );
            }
        }
        Err(e) => {
            tracing::warn!("[WsGateway] connect snapshot: list pending reminders failed: {e}");
        }
    }
}

pub(crate) async fn handle_notification_read(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let Some(id) = msg["id"].as_str() else {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "id required"}),
        );
        return;
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    match state.db.mark_event_notification_read(id, now_ms) {
        Ok(0) => {
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": "notification not found"}),
            );
        }
        Ok(_) => {
            send_json(
                sender,
                &serde_json::json!({"type": "notification:read:ok", "id": id}),
            );
        }
        Err(e) => {
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": format!("mark read failed: {e}")}),
            );
        }
    }
}

pub(crate) async fn handle_subscribe(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    last_known_states: &Arc<Mutex<HashMap<String, String>>>,
    pending_interactions: &Arc<Mutex<HashMap<String, serde_json::Value>>>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let jid = msg["groupJid"].as_str().unwrap_or("").to_string();
    if jid.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "groupJid required"}),
        );
        return;
    }
    {
        let mut guard = clients.lock().await;
        if let Some(client) = guard.get_mut(client_idx) {
            client.subscriptions.insert(jid.clone());
            // Every chat is a full-privilege admin now, so subscribing to any
            // registered group upgrades the web client to admin (it then
            // receives dispatch:update / agent:todos).
            if state.group_manager.get(&state.db, &jid).is_some() {
                client.is_admin = true;
                tracing::info!("[WsGateway] client #{client_idx} subscribed to {jid} (admin)");
            }
        }
    }

    // Push current dispatch state + agent todos for admin groups.
    let is_admin = {
        let guard = clients.lock().await;
        guard.get(client_idx).map(|c| c.is_admin).unwrap_or(false)
    };
    if is_admin {
        let parents = state.api.get_dispatch_parents();
        let parent_count = parents.as_array().map(|a| a.len()).unwrap_or(0);
        tracing::info!(
            "[WsGateway] subscribe snapshot client #{client_idx}: dispatch:update with {parent_count} parent(s)"
        );
        if !parents.is_null() {
            send_json(
                sender,
                &serde_json::json!({"type": "dispatch:update", "parents": parents}),
            );

            // Send persisted dispatch activity for active/recent tasks so the
            // UI replays sub-agent tool logs on reconnect.
            if let Some(parents_arr) = parents.as_array() {
                let task_ids: Vec<String> = parents_arr
                    .iter()
                    .filter(|p| {
                        let status = p.get("status").and_then(|v| v.as_str()).unwrap_or("");
                        status == "active" || status == "queued"
                    })
                    .flat_map(|p| {
                        p.get("tasks")
                            .and_then(|v| v.as_array())
                            .into_iter()
                            .flatten()
                            .filter_map(|t| {
                                t.get("id").and_then(|v| v.as_str()).map(|s| s.to_string())
                            })
                    })
                    .collect();
                if !task_ids.is_empty() {
                    let id_refs: Vec<&str> = task_ids.iter().map(|s| s.as_str()).collect();
                    if let Ok(activity) = state.db.get_dispatch_activity(&id_refs) {
                        // Group by task_id and send one dispatch:activity:batch per task
                        let mut by_task: std::collections::HashMap<&str, Vec<serde_json::Value>> =
                            std::collections::HashMap::new();
                        for row in &activity {
                            let content: serde_json::Value = row
                                .content_json
                                .as_deref()
                                .and_then(|s| serde_json::from_str(s).ok())
                                .unwrap_or(serde_json::Value::Null);
                            by_task
                                .entry(&row.task_id)
                                .or_default()
                                .push(serde_json::json!({
                                    "entryType": row.entry_type,
                                    "toolName": row.tool_name,
                                    "title": row.title,
                                    "summary": row.summary,
                                    "content": content,
                                    "ok": row.ok,
                                    "text": row.text,
                                    "ts": row.ts,
                                }));
                        }
                        for (tid, entries) in by_task {
                            send_json(
                                sender,
                                &serde_json::json!({
                                    "type": "dispatch:activity:batch",
                                    "taskId": tid,
                                    "entries": entries,
                                }),
                            );
                        }
                        tracing::info!(
                            "[WsGateway] subscribe: sent {} persisted activity entries for {} task(s)",
                            activity.len(),
                            task_ids.len()
                        );
                    }
                }
            }
        }
        let todos = state.api.get_agent_todos();
        let mem_has_todos = matches!(&todos, serde_json::Value::Object(m) if !m.is_empty());
        if let serde_json::Value::Object(map) = &todos {
            tracing::info!(
                "[WsGateway] subscribe snapshot client #{client_idx}: agent:todos for {} agent(s)",
                map.len()
            );
            for (agent_jid, entry) in map {
                let agent_name = entry
                    .get("agentName")
                    .and_then(|v| v.as_str())
                    .unwrap_or(agent_jid);
                let todos_arr = entry
                    .get("todos")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                send_json(
                    sender,
                    &serde_json::json!({
                        "type": "agent:todos",
                        "agentJid": agent_jid,
                        "agentName": agent_name,
                        "todos": todos_arr,
                    }),
                );
            }
        } else if !todos.is_null() {
            tracing::info!(
                "[WsGateway] subscribe snapshot client #{client_idx}: agent:todos (legacy format)"
            );
            send_json(
                sender,
                &serde_json::json!({"type": "agent:todos", "todos": todos}),
            );
        }
        // Fallback: in-memory snapshot is empty (e.g. fresh daemon restart
        // before any agent has emitted). Replay from DB so the Agent Console
        // still shows the last-known todos for each known agent.
        if !mem_has_todos {
            if let Ok(rows) = state.db.get_all_agent_todos() {
                if !rows.is_empty() {
                    tracing::info!(
                        "[WsGateway] subscribe snapshot client #{client_idx}: agent:todos DB fallback for {} agent(s)",
                        rows.len()
                    );
                    for row in &rows {
                        let todos_val: serde_json::Value = serde_json::from_str(&row.todos_json)
                            .unwrap_or(serde_json::Value::Null);
                        send_json(
                            sender,
                            &serde_json::json!({
                                "type": "agent:todos",
                                "agentJid": row.agent_jid,
                                "agentName": row.agent_name,
                                "todos": todos_val,
                            }),
                        );
                    }
                }
            }
        }

        // Push agent tool rosters so the Agent Console can render every
        // currently-online agent and the tools it can use.
        let tools = state.api.get_agent_tools();
        if let serde_json::Value::Object(map) = &tools {
            for (agent_jid, entry) in map {
                let agent_name = entry
                    .get("agentName")
                    .and_then(|v| v.as_str())
                    .unwrap_or(agent_jid);
                let tools_arr = entry
                    .get("tools")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                send_json(
                    sender,
                    &serde_json::json!({
                        "type": "agent:tools",
                        "agentJid": agent_jid,
                        "agentName": agent_name,
                        "tools": tools_arr,
                    }),
                );
            }
        }

        // Replay any pending permission:request / question:request messages so the
        // Agent Console shows interactions issued before this client connected/reconnected.
        let pending = pending_interactions.lock().await.clone();
        let pending_count = pending.len();
        if pending_count > 0 {
            tracing::info!(
                "[WsGateway] subscribe snapshot client #{client_idx}: \
                 replaying {pending_count} pending interaction(s) (permissions/questions)"
            );
            for (_, interaction_msg) in &pending {
                send_json(sender, interaction_msg);
            }
        }
    }

    // Push last-known agent state (fix stale frontend state on reconnect).
    {
        let states = last_known_states.lock().await;
        if let Some(known) = states.get(&jid) {
            send_json(
                sender,
                &serde_json::json!({"type": "agent:state", "groupJid": jid, "state": known}),
            );
        }
    }

    // Push current agent mode so the frontend knows if Plan mode is active.
    if let Some(mode) = state.api.get_agent_mode(&jid) {
        if mode != "Agent" {
            send_json(
                sender,
                &serde_json::json!({
                    "type": "agent:mode:changed",
                    "groupJid": jid,
                    "mode": mode,
                }),
            );
        }
    }

    // Send current tool auto-accept rules to newly connected client.
    {
        let rules = state.api.get_tool_rules();
        if !rules.is_empty() {
            send_json(
                sender,
                &serde_json::json!({"type": "permission:rules", "rules": rules}),
            );
        }
    }

    // Load and push chat history so the Web UI shows past conversation.
    // Text messages (group_messages) and tool executions (tool_executions)
    // are stored separately but merged here in timestamp order so the client
    // sees a single chronologically-ordered list and re-renders
    // ToolGroupCard runs identically to the live path.
    {
        let text_msgs = state.db.get_group_messages(&jid, None).unwrap_or_default();
        let tool_msgs = state.db.get_tool_executions(&jid, None).unwrap_or_default();
        let widget_msgs = state.db.get_chat_widgets(&jid, None).unwrap_or_default();

        if !text_msgs.is_empty() || !tool_msgs.is_empty() || !widget_msgs.is_empty() {
            let mut history: Vec<serde_json::Value> =
                Vec::with_capacity(text_msgs.len() + tool_msgs.len() + widget_msgs.len());
            for m in &text_msgs {
                history.push(serde_json::json!({
                    "id": m.message_id,
                    "role": if m.is_bot_reply { "agent" } else { "user" },
                    "senderName": m.sender_name,
                    "text": m.content,
                    "timestamp": m.timestamp,
                }));
            }
            for t in &tool_msgs {
                // Parse content_json back to a JSON value; fall back to the
                // raw string if it doesn't parse so the UI still sees something.
                let content: serde_json::Value = serde_json::from_str(&t.content_json)
                    .unwrap_or_else(|_| serde_json::Value::String(t.content_json.clone()));
                history.push(serde_json::json!({
                    "id": format!("tool-{}", t.id),
                    "role": "tool",
                    "agentId": t.agent_id,
                    "toolName": t.tool_name,
                    "title": t.title,
                    "summary": t.summary,
                    "description": t.description,
                    "content": content,
                    "ok": t.ok,
                    "timestamp": t.timestamp,
                }));
            }
            for w in &widget_msgs {
                // Parse widget_json back to a JSON value; fall back to an empty
                // object so a malformed row can't crash the replay.
                let widget: serde_json::Value =
                    serde_json::from_str(&w.widget_json).unwrap_or_else(|_| serde_json::json!({}));
                history.push(serde_json::json!({
                    "id": w.id,
                    "role": "widget",
                    "widget": widget,
                    "timestamp": w.created_at,
                }));
            }
            // Stable sort by timestamp string (RFC3339 / ISO8601 sorts
            // lexicographically) so text and tool rows interleave correctly.
            history.sort_by(|a, b| {
                let ta = a.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
                let tb = b.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
                ta.cmp(tb)
            });

            send_json(
                sender,
                &serde_json::json!({
                    "type": "history:load",
                    "groupJid": jid,
                    "messages": history,
                }),
            );
        }
    }

    // Replay ephemeral chat events (permission/question request+resolved)
    // so a reload rebuilds in-flight UI state.
    // Note: agent:state is intentionally excluded — the current state is sent
    // via last_known_states snapshot above, and DB-replayed agent:state can be
    // stale (e.g. from before a server restart where the agent was destroyed).
    if let Ok(events) = state.db.get_chat_events(&jid, Some(200)) {
        let filtered: Vec<_> = events
            .into_iter()
            .filter(|e| e.event_type != "agent:state")
            .collect();
        if !filtered.is_empty() {
            let payload: Vec<serde_json::Value> = filtered
                .iter()
                .map(|e| {
                    let inner: serde_json::Value =
                        serde_json::from_str(&e.payload_json).unwrap_or(serde_json::Value::Null);
                    serde_json::json!({
                        "id": e.id,
                        "eventType": e.event_type,
                        "requestId": e.request_id,
                        "payload": inner,
                        "timestamp": e.timestamp,
                    })
                })
                .collect();
            send_json(
                sender,
                &serde_json::json!({
                    "type": "chat:history",
                    "groupJid": jid,
                    "events": payload,
                }),
            );
        }
    }

    send_json(
        sender,
        &serde_json::json!({"type": "subscribed", "groupJid": jid}),
    );
}

pub(crate) async fn handle_unsubscribe(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let jid = msg["groupJid"].as_str().unwrap_or("").to_string();
    let mut guard = clients.lock().await;
    if let Some(client) = guard.get_mut(client_idx) {
        client.subscriptions.remove(&jid);
    }
}

pub(crate) async fn handle_list_groups(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    // Legacy groups table
    let mut groups: Vec<GroupInfo> = state
        .group_manager
        .list(&state.db)
        .unwrap_or_default()
        .iter()
        .map(to_group_info)
        .collect();

    // Entity-model bindings (Telegram, Feishu, etc. created via Settings UI)
    // Only include bindings that have a real JID (not pending).
    let bindings = state
        .binding_manager
        .list_with_relations(&state.db)
        .unwrap_or_default();
    for br in &bindings {
        if let Some(jid) = &br.binding.jid {
            // Skip if already present from legacy groups table.
            if groups.iter().any(|g| &g.jid == jid) {
                continue;
            }
            groups.push(GroupInfo {
                jid: jid.clone(),
                folder: br.agent.folder.clone(),
                name: format!("{} ({})", br.channel.name, br.agent.name),
                channel: br.channel.platform_type.clone(),
                group_type: "chat".to_string(),
                requires_trigger: br.agent.requires_trigger,
                allowed_tools: br.agent.allowed_tools.clone(),
                allowed_paths: br.agent.allowed_paths.clone(),
                allowed_work_dirs: br.agent.allowed_work_dirs.clone(),
                max_messages: br.binding.max_messages,
                llm_config_id: None,
                agent_id: Some(br.agent.id),
                channel_id: Some(br.channel.id),
                last_activity: None,
            });
        }
    }

    // Stamp each group with its last message/response time so the sidebar
    // "recent activity" sort reflects real chat activity.
    let activity = state.db.last_activity_per_group().unwrap_or_default();
    for g in &mut groups {
        g.last_activity = activity.get(&g.jid).copied();
    }

    send_json(
        sender,
        &serde_json::json!({"type": "groups", "groups": groups}),
    );
}

pub(crate) async fn handle_register_group(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    if !require_admin(clients, client_idx, sender).await {
        return;
    }

    let folder = msg["folder"].as_str().unwrap_or("");
    let name = msg["name"].as_str().unwrap_or("");
    if folder.is_empty() || name.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "folder and name are required"}),
        );
        return;
    }

    let channel = msg["channel"].as_str().unwrap_or("");
    let bot_token = msg["botToken"].as_str().map(|s| s.to_string());
    let mut jid = msg["jid"].as_str().unwrap_or("").to_string();

    // Feishu pending binding.
    if jid.is_empty() && channel == "feishu" {
        let app_id = bot_token.as_deref().unwrap_or("").to_string();
        if app_id.is_empty() {
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": "feishu:pending binding requires App ID (botToken)"}),
            );
            return;
        }
        jid = format!("feishu:pending:{app_id}");
    }
    if jid.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "jid is required (or leave empty for feishu pending binding)"}),
        );
        return;
    }

    let mut resolved_jid = jid.clone();
    let resolved_folder = folder.to_string();
    let resolved_name = name.to_string();
    let resolved_channel = channel.to_string();
    let resolved_token = bot_token.clone();
    let token_for_check = resolved_token.clone(); // saved for check after move

    // Telegram: addBot to get botUserId for bot-aware JID.
    if resolved_channel == "telegram" && resolved_token.is_some() {
        let tok = resolved_token.as_deref().unwrap_or("");
        if let Err(e) = state.api.add_telegram_bot(tok).await {
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": format!("register:group failed: {e}")}),
            );
            return;
        }
        if let Some(bot_user_id) = state.api.get_telegram_bot_user_id(tok) {
            // Upgrade bare tg:user:{id} → tg:{botUserId}:user:{id}
            if let Some(caps) = regex::Regex::new(r"^tg:(user|group):(-?\d+)$")
                .ok()
                .and_then(|re| {
                    let c = re.captures(&resolved_jid)?;
                    Some((
                        c.get(1)?.as_str().to_string(),
                        c.get(2)?.as_str().to_string(),
                    ))
                })
            {
                resolved_jid = format!("tg:{bot_user_id}:{}:{}", caps.0, caps.1);
            }
            // Persist to config.json.
            if let Some(chat_caps) = regex::Regex::new(r"^tg:(?:\d+:)?user:(\d+)$")
                .ok()
                .and_then(|re| {
                    re.captures(&resolved_jid)
                        .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
                })
            {
                let _ = save_telegram_bot(
                    &state.config.paths.global_config_path,
                    crate::gateway::group_manager::TelegramBotConfig {
                        token: tok.to_string(),
                        admin_user_id: chat_caps,
                        folder: resolved_folder.clone(),
                        name: Some(resolved_name.clone()),
                    },
                );
            }
        }
    }

    let now = now_iso();
    let existing = state.group_manager.get(&state.db, &resolved_jid);
    let binding = GroupBinding {
        jid: resolved_jid.clone(),
        folder: resolved_folder.clone(),
        name: resolved_name.clone(),
        channel: if resolved_channel.is_empty() {
            existing
                .as_ref()
                .map(|e| e.channel.clone())
                .unwrap_or_default()
        } else {
            resolved_channel.clone()
        },
        group_type: msg["groupType"].as_str().unwrap_or("chat").to_string(),
        requires_trigger: msg["requiresTrigger"].as_bool().unwrap_or(
            existing
                .as_ref()
                .map(|e| e.requires_trigger)
                .unwrap_or(true),
        ),
        allowed_tools: if msg.get("allowedTools").is_some() {
            msg["allowedTools"].as_array().map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
        } else {
            existing.as_ref().and_then(|e| e.allowed_tools.clone())
        },
        allowed_paths: None,
        allowed_work_dirs: if msg.get("allowedWorkDirs").is_some() {
            msg["allowedWorkDirs"].as_array().map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
        } else {
            existing.as_ref().and_then(|e| e.allowed_work_dirs.clone())
        },
        bot_token: resolved_token.or(existing.as_ref().and_then(|e| e.bot_token.clone())),
        max_messages: if msg.get("maxMessages").is_some() {
            msg["maxMessages"].as_u64().map(|n| n as u32)
        } else {
            existing.as_ref().and_then(|e| e.max_messages)
        },
        llm_config_id: if msg.get("modelId").is_some() {
            msg["modelId"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        } else {
            existing.as_ref().and_then(|e| e.llm_config_id.clone())
        },
        last_active: existing.as_ref().and_then(|e| e.last_active.clone()),
        added_at: existing
            .as_ref()
            .map(|e| e.added_at.clone())
            .unwrap_or_else(|| now.clone()),
    };

    state
        .group_manager
        .register(&state.db, &state.config, &binding);

    // Telegram group bindings without dedicated token: fire-and-forget addBot.
    if resolved_channel == "telegram" && token_for_check.is_none() {
        if let Some(ref bt) = binding.bot_token {
            let bt = bt.clone();
            let api = state.api.clone();
            tokio::spawn(async move {
                if let Err(e) = api.add_telegram_bot(&bt).await {
                    tracing::error!("[WsGateway] Failed to register bot token: {e}");
                }
            });
        }
    }
    // Feishu: lazy-connect when group is registered.
    if binding.jid.starts_with("feishu:") {
        let api = state.api.clone();
        tokio::spawn(async move {
            if let Err(e) = api.ensure_feishu_channel().await {
                tracing::error!("[WsGateway] Failed to ensure FeishuChannel: {e}");
            }
        });
    }

    let info = to_group_info(&binding);
    broadcast_to_all_inner(
        clients,
        &serde_json::json!({"type": "group:registered", "group": info}),
    )
    .await;
}

pub(crate) async fn handle_unregister_group(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    if !require_admin(clients, client_idx, sender).await {
        return;
    }
    let jid = msg["jid"].as_str().unwrap_or("").to_string();
    if jid.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "jid required"}),
        );
        return;
    }
    let Some(group) = state.group_manager.get(&state.db, &jid) else {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": format!("Group not found: {jid}")}),
        );
        return;
    };
    // The structural implicit-main group cannot be unregistered — but ONLY the
    // bare main group itself, identified by its exact jid. Per-session sub-chats
    // (multi-session) legitimately SHARE the "main" agent folder while carrying
    // distinct dynamic jids like `web:main:<id>`, and MUST be deletable. Matching
    // on `folder == "main"` wrongly blocked deleting every main-folder session
    // (the "can't delete chat" bug). The main agent's directory is re-ensured at
    // every boot (see run_daemon), so losing the last main session is harmless.
    if group.jid == "main" || group.jid == "web:main" {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "Cannot unregister the main group"}),
        );
        return;
    }

    // Clean up channel-specific config entries.
    if group.channel == "telegram"
        && group.bot_token.is_some()
        && regex::Regex::new(r"^tg:(?:\d+:)?user:")
            .ok()
            .and_then(|re| re.is_match(&group.jid).then_some(()))
            .is_some()
    {
        let _ = delete_telegram_bot(
            &state.config.paths.global_config_path,
            group.bot_token.as_deref().unwrap_or(""),
        );
    }
    if group.channel == "feishu" && group.bot_token.is_some() {
        let app_id = group.bot_token.as_deref().unwrap_or("");
        let all = state.group_manager.list(&state.db).unwrap_or_default();
        let still_used = all.iter().any(|g| {
            g.jid != jid && g.channel == "feishu" && g.bot_token.as_deref() == Some(app_id)
        });
        if !still_used {
            let _ = delete_feishu_app(&state.config.paths.global_config_path, app_id);
        }
    }

    state
        .group_manager
        .unregister(&state.db, &state.config, &jid);
    broadcast_to_all_inner(
        clients,
        &serde_json::json!({"type": "group:unregistered", "jid": jid}),
    )
    .await;
}

pub(crate) async fn handle_update_group(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    if !require_admin(clients, client_idx, sender).await {
        return;
    }
    let jid = msg["jid"].as_str().unwrap_or("").to_string();
    if jid.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "jid required"}),
        );
        return;
    }

    let mut updates = GroupBindingUpdate::default();
    if let Some(v) = msg["name"].as_str() {
        updates.name = Some(v.to_string());
    }
    if let Some(v) = msg["channel"].as_str() {
        updates.channel = Some(v.to_string());
    }
    if let Some(v) = msg["groupType"].as_str() {
        updates.group_type = Some(v.to_string());
    }
    if let Some(v) = msg["requiresTrigger"].as_bool() {
        updates.requires_trigger = Some(v);
    }
    if msg.get("allowedTools").is_some() {
        updates.allowed_tools = Some(msg["allowedTools"].as_array().map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        }));
    }
    if msg.get("allowedPaths").is_some() {
        updates.allowed_paths = Some(msg["allowedPaths"].as_array().map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        }));
    }
    if msg.get("allowedWorkDirs").is_some() {
        updates.allowed_work_dirs = Some(msg["allowedWorkDirs"].as_array().map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        }));
    }
    if msg.get("botToken").is_some() {
        updates.bot_token = Some(msg["botToken"].as_str().map(String::from));
    }
    if msg.get("maxMessages").is_some() {
        updates.max_messages = Some(msg["maxMessages"].as_u64().map(|n| n as u32));
    }
    if msg.get("modelId").is_some() {
        // null / empty string clears the override (→ global active model).
        let id = msg["modelId"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        updates.llm_config_id = Some(id);
    }

    match state
        .group_manager
        .update(&state.db, &state.config, &jid, updates)
    {
        Ok(updated) => {
            // The new selection is persisted on the binding; AgentPool reads it
            // fresh from the DB and applies it to the engine on the next turn
            // (see `set_model_override` in the dispatch path).
            let info = to_group_info(&updated);
            broadcast_to_all_inner(
                clients,
                &serde_json::json!({"type": "group:updated", "group": info}),
            )
            .await;
        }
        Err(e) => {
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": format!("update:group failed: {e}")}),
            );
        }
    }
}

pub(crate) async fn handle_message_send(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let group_jid = msg["groupJid"].as_str().unwrap_or("").to_string();
    let text = msg["text"].as_str().unwrap_or("").trim().to_string();

    // Extract attachments if present. `name` is optional — a pasted image has
    // no filename — but a document without one is unlabelable, so fall back to
    // the MIME's subtype rather than dropping the attachment.
    let attachments: Vec<crate::types::MessageAttachment> = msg["attachments"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let data_url = v["dataUrl"].as_str().unwrap_or("");
                    let mime_type = v["mimeType"].as_str().unwrap_or("");
                    if data_url.is_empty() || mime_type.is_empty() {
                        return None;
                    }
                    Some(crate::types::MessageAttachment {
                        data_url: data_url.to_string(),
                        mime_type: mime_type.to_string(),
                        name: v["name"]
                            .as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    if group_jid.is_empty() || (text.is_empty() && attachments.is_empty()) {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "groupJid and text or attachments required"}),
        );
        return;
    }

    // Admin command interception.
    let is_admin = {
        let guard = clients.lock().await;
        guard.get(client_idx).map(|c| c.is_admin).unwrap_or(false)
    };
    if is_admin {
        // `/plugin ...` marketplace commands (async: git/HTTP under the hood).
        if let Some(manager) = state.marketplace_manager.clone() {
            if let Some(output) = dispatch_plugin_command(manager, &text).await {
                send_json(
                    sender,
                    &serde_json::json!({
                        "type": "agent:reply",
                        "groupJid": group_jid,
                        "text": output,
                        "ts": chrono::Utc::now().to_rfc3339(),
                    }),
                );
                return;
            }
        }

        // `/app ...` Space App update commands (self-HTTP to the daemon).
        if let Some(output) = crate::gateway::plugin_command::dispatch_app_command(&text).await {
            send_json(
                sender,
                &serde_json::json!({
                    "type": "agent:reply",
                    "groupJid": group_jid,
                    "text": output,
                    "ts": chrono::Utc::now().to_rfc3339(),
                }),
            );
            return;
        }

        let is_reset = text.trim() == "/reset"
            || text.trim() == "reset"
            || text.trim().starts_with("/reset ")
            || text.trim().starts_with("reset ");
        if let Some(output) = dispatch_command(&state.db, &text, Some(&group_jid)) {
            send_json(
                sender,
                &serde_json::json!({
                    "type": "agent:reply",
                    "groupJid": group_jid,
                    "text": output,
                    "ts": chrono::Utc::now().to_rfc3339(),
                }),
            );
            // After reset, push empty history so the frontend clears its local messages.
            if is_reset {
                send_json(
                    sender,
                    &serde_json::json!({"type": "history:load", "groupJid": group_jid, "messages": []}),
                );
            }
            return;
        }
    }

    // Pending binding check.
    if group_jid.contains(":pending:") {
        let ch = "Feishu";
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": format!("{ch} binding is not complete. Please send the first message from {ch} to complete JID binding.")}),
        );
        return;
    }

    let Some(group) = state.group_manager.get(&state.db, &group_jid) else {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": format!("Group not found: {group_jid}")}),
        );
        return;
    };

    // Auto-subscribe the sending client to this JID. Without this, a client
    // that just registered a new group and sent its first message would not
    // receive the resulting agent:reply / tool:execution events — broadcast()
    // only delivers to clients with prior subscriptions. Subscribing here
    // closes the register → send → reply race for free.
    {
        let mut guard = clients.lock().await;
        if let Some(client) = guard.get_mut(client_idx) {
            if client.subscriptions.insert(group_jid.clone()) {
                tracing::info!(
                    "[WsGateway] client #{client_idx} auto-subscribed to {group_jid} on first send"
                );
                // Every chat is admin now — auto-subscribe grants admin.
                client.is_admin = true;
            }
        }
    }

    // Persist user message to conversation history so it appears in history:load.
    let attachments_json = if attachments.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&attachments).unwrap_or_default())
    };

    let stored = crate::types::StoredMessage {
        message_id: format!("web:{}", Uuid::new_v4()),
        chat_jid: group_jid.clone(),
        sender_jid: String::new(),
        sender_name: String::new(),
        content: text.clone(),
        timestamp: local_iso_string_now(),
        is_from_me: false,
        is_bot_reply: false,
        reply_to_id: None,
        media_type: None,
        attachments: attachments_json,
    };
    let limit = state.config.agent.max_messages_per_group;
    if let Err(e) = state.db.insert_group_message(&stored, limit) {
        tracing::warn!("[WebSocketGateway] Failed to persist web message for {group_jid}: {e}");
    }

    // Sidebar "recent activity" tick — user messages sent from the UI don't
    // pass through notify_incoming, so emit the activity event here.
    broadcast_to_all_inner(
        clients,
        &serde_json::json!({
            "type": "group:activity",
            "jid": group_jid,
            "ts": chrono::Utc::now().timestamp_millis(),
        }),
    )
    .await;

    // Cowork hook: when a user message lands in a cowork group, persist
    // the message AND create a task in the team's task list so the manager
    // has a tracked work item. We ALSO prepend a team-context preamble to
    // the prompt before sending it on to the agent — this is what turns
    // the chat into a real DAG team session: the manager sees its
    // members + dispatch instructions every turn.
    let effective_text = if group.group_type == "cowork" {
        if let Some(team_id) = group_jid.strip_prefix("cowork:") {
            crate::gateway::ui_server::cowork_runtime::on_user_message(&state.db, team_id, &text);
            match crate::gateway::ui_server::cowork_runtime::team_context_preamble(
                &state.db, team_id,
            ) {
                Some(preamble) => format!("{preamble}{text}"),
                None => text.clone(),
            }
        } else {
            text.clone()
        }
    } else {
        text.clone()
    };

    // Client-supplied hidden context (the reminder dialog uses it to say which
    // event the user is answering about). It reaches the agent but is NOT the
    // stored message: `stored.content` above is the raw `text`, so history —
    // and every client that replays it — shows what the person actually typed.
    //
    // Sending it inside `text` was the bug: the optimistic bubble looked right
    // until the daemon echoed the stored message back, at which point the user
    // saw the whole internal instruction, event id and tool list in their own
    // chat bubble.
    let effective_text = apply_context_preamble(
        &effective_text,
        msg["contextPreamble"].as_str(),
    );

    state
        .api
        .enqueue_and_process(&group_jid, &group, &effective_text, &attachments);
}

/// Prepend client-supplied hidden context to the text the agent receives.
///
/// The preamble is deliberately a separate wire field rather than part of
/// `text`: only `text` is persisted as the user's message, so the transcript
/// keeps showing what the person typed. The reminder dialog uses this to tell
/// the agent which event is being discussed — an instruction block naming the
/// event id and the tools to call, which read as gibberish when it leaked into
/// the user's own chat bubble.
fn apply_context_preamble(text: &str, preamble: Option<&str>) -> String {
    match preamble.map(str::trim) {
        Some(p) if !p.is_empty() => format!("{p}\n\n{text}"),
        _ => text.to_string(),
    }
}

pub(crate) async fn handle_permission_response(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let request_id = msg["requestId"].as_str().unwrap_or("").to_string();
    let option_key = msg["optionKey"].as_str().unwrap_or("").to_string();
    if request_id.is_empty() || option_key.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "requestId and optionKey required"}),
        );
        return;
    }
    state.api.resolve_permission(&request_id, &option_key);
}

/// List the most recent fired event notifications. Used by the Sidebar
/// "history" tab so reload still surfaces reminders that fired while the
/// user was offline.
pub(crate) async fn handle_notifications_list(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let limit = msg["limit"].as_u64().map(|v| v as u32).unwrap_or(100);
    match state.db.list_event_notifications(Some(limit)) {
        Ok(rows) => {
            let items: Vec<serde_json::Value> = rows
                .iter()
                .map(|n| {
                    serde_json::json!({
                        "id": n.id,
                        "eventId": n.event_id,
                        "title": n.title,
                        "startAt": n.start_at,
                        "kind": n.kind,
                        "firedAt": n.fired_at,
                        "delayedMs": n.delayed_ms,
                        "readAt": n.read_at,
                    })
                })
                .collect();
            send_json(
                sender,
                &serde_json::json!({
                    "type": "notifications:list",
                    "notifications": items,
                }),
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "[handle_notifications_list] db error");
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": "list failed"}),
            );
        }
    }
}

/// List space-events whose reminder will fire within `windowMin` minutes
/// but hasn't been generated yet. Surfaced as the Sidebar "upcoming" tab —
/// answers "what's about to ping me?" before the notifier loop wakes.
pub(crate) async fn handle_notifications_pending(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    // Default to 24h preview — covers the working day without scrolling.
    let window_min = msg["windowMin"].as_i64().unwrap_or(24 * 60);
    let window_ms = window_min * 60_000;
    let now_ms = chrono::Utc::now().timestamp_millis();
    match state.db.list_pending_event_reminders(now_ms, window_ms) {
        Ok(rows) => {
            let items: Vec<serde_json::Value> = rows
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "eventId": p.event_id,
                        "title": p.title,
                        "startAt": p.start_at,
                        "reminderMin": p.reminder_min,
                        "triggerAt": p.trigger_at,
                    })
                })
                .collect();
            send_json(
                sender,
                &serde_json::json!({
                    "type": "notifications:pending",
                    "windowMin": window_min,
                    "items": items,
                }),
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "[handle_notifications_pending] db error");
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": "pending failed"}),
            );
        }
    }
}

pub(crate) async fn handle_plan_list(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let jid = msg["groupJid"].as_str().unwrap_or("").to_string();
    if jid.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "groupJid required"}),
        );
        return;
    }
    match state.db.list_plans_for_chat(&jid, Some(100)) {
        Ok(rows) => {
            let plans: Vec<serde_json::Value> = rows
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "id": p.id,
                        "chatJid": p.chat_jid,
                        "agentId": p.agent_id,
                        "title": p.title,
                        "filePath": p.file_path,
                        "approval": p.approval,
                        "createdAt": p.created_at,
                        "approvedAt": p.approved_at,
                    })
                })
                .collect();
            send_json(
                sender,
                &serde_json::json!({
                    "type": "plans:list",
                    "groupJid": jid,
                    "plans": plans,
                }),
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "[handle_plan_list] db error");
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": "plan list failed"}),
            );
        }
    }
}

pub(crate) async fn handle_plan_get(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let id = msg["id"].as_str().unwrap_or("");
    if id.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "id required"}),
        );
        return;
    }
    match state.db.get_plan(id) {
        Ok(Some(p)) => {
            send_json(
                sender,
                &serde_json::json!({
                    "type": "plans:get",
                    "plan": {
                        "id": p.id,
                        "chatJid": p.chat_jid,
                        "agentId": p.agent_id,
                        "title": p.title,
                        "filePath": p.file_path,
                        "contentMd": p.content_md,
                        "approval": p.approval,
                        "createdAt": p.created_at,
                        "approvedAt": p.approved_at,
                    },
                }),
            );
        }
        Ok(None) => {
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": "plan not found"}),
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "[handle_plan_get] db error");
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": "plan get failed"}),
            );
        }
    }
}

pub(crate) async fn handle_tool_rule_add(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    if let Ok(rule) = serde_json::from_value::<
        crate::agent::permission_bridge::types::ToolAutoAcceptRule,
    >(msg["rule"].clone())
    {
        state.api.add_tool_rule(rule.clone());
        send_json(
            sender,
            &serde_json::json!({"type": "permission:rule:added", "rule": rule}),
        );
    }
}

pub(crate) async fn handle_tool_rule_remove(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    if let Some(rule_id) = msg["ruleId"].as_str() {
        state.api.remove_tool_rule(rule_id);
        send_json(
            sender,
            &serde_json::json!({"type": "permission:rule:removed", "ruleId": rule_id}),
        );
    }
}

/// `dismiss:todos {agentJid}` — permanently dismiss an agent's todo list from
/// the Console. Clears the in-memory snapshot cache AND deletes the persisted
/// `agent_todos` row (both are replayed to clients on subscribe, which is why
/// a client-side-only removal reappears after reload), then broadcasts an
/// empty list so every connected client drops the group immediately.
pub(crate) async fn handle_todos_dismiss(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let Some(jid) = msg["agentJid"].as_str().filter(|s| !s.is_empty()) else {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "agentJid required"}),
        );
        return;
    };
    state.api.dismiss_agent_todos(jid);
    if let Err(e) = state.db.delete_agent_todos(jid) {
        tracing::warn!(
            error = %e, agent_jid = %jid,
            "[WsGateway] dismiss:todos: failed to delete persisted todos"
        );
    }
    tracing::info!("[WsGateway] dismiss:todos agent_jid={jid}");
    broadcast_to_all_inner(
        clients,
        &serde_json::json!({
            "type": "agent:todos",
            "agentJid": jid,
            "agentName": jid,
            "todos": [],
        }),
    )
    .await;
}

pub(crate) async fn handle_tool_rule_update(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    if let Ok(rule) = serde_json::from_value::<
        crate::agent::permission_bridge::types::ToolAutoAcceptRule,
    >(msg["rule"].clone())
    {
        state.api.update_tool_rule(rule.clone());
        send_json(
            sender,
            &serde_json::json!({"type": "permission:rule:updated", "rule": rule}),
        );
    }
}

pub(crate) async fn handle_tool_accept_all(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let enabled = msg["enabled"].as_bool().unwrap_or(false);
    state.api.set_accept_all(enabled);
    send_json(
        sender,
        &serde_json::json!({"type": "permission:accept-all:updated", "enabled": enabled}),
    );
}

pub(crate) async fn handle_question_response(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let request_id = msg["requestId"].as_str().unwrap_or("").to_string();
    let answers = &msg["answers"];
    if request_id.is_empty() || answers.is_null() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "requestId and answers required"}),
        );
        return;
    }
    let other_texts = msg.get("otherTexts");
    state
        .api
        .resolve_ask_question(&request_id, answers, other_texts);
}

pub(crate) async fn handle_form_response(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let request_id = msg["requestId"].as_str().unwrap_or("").to_string();
    if request_id.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "requestId required"}),
        );
        return;
    }
    let values = msg.get("values").cloned().unwrap_or(serde_json::json!({}));
    // Missing flag counts as submitted; explicit `false` means the user skipped.
    let submitted = msg["submitted"].as_bool().unwrap_or(true);
    state.api.resolve_form(&request_id, &values, submitted);
}

/// Handle the user's plan-exit decision from `PlanExitDialog`. Routes the
/// choice to the suspended `ExitPlanMode` tool (which otherwise blocks
/// forever) and broadcasts `plan:exit:response` so every connected client
/// closes its modal.
pub(crate) async fn handle_plan_exit_response(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let group_jid = msg["groupJid"].as_str().unwrap_or("").to_string();
    let agent_id = msg["agentId"].as_str().unwrap_or("main").to_string();
    let selected = msg["selected"]
        .as_str()
        .unwrap_or("startEditing")
        .to_string();
    if group_jid.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "groupJid required"}),
        );
        return;
    }
    tracing::info!(
        "[WsGateway] plan:exit:response client #{client_idx} jid={group_jid} agent={agent_id} selected={selected}"
    );
    // Deliver to the engine: unblocks ExitPlanMode + flips mode on approval.
    state
        .api
        .resolve_plan_exit(&group_jid, &agent_id, &selected);
    // Broadcast so all clients (the one that answered + others) dismiss the
    // dialog. Frontend listens for `plan:exit:response`.
    broadcast_to_all_inner(
        clients,
        &serde_json::json!({
            "type": "plan:exit:response",
            "groupJid": group_jid,
            "agentId": agent_id,
            "selected": selected,
        }),
    )
    .await;
    // On approval the engine flips back to Agent mode internally — mirror
    // that to clients so the mode selector + read-only UI state update.
    if matches!(selected.as_str(), "startEditing" | "clearContextAndStart") {
        broadcast_to_all_inner(
            clients,
            &serde_json::json!({
                "type": "agent:mode:changed",
                "groupJid": group_jid,
                "mode": "Agent",
            }),
        )
        .await;
    }
}

pub(crate) async fn handle_list_tasks(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    if !require_admin(clients, client_idx, sender).await {
        return;
    }
    let jid = msg["groupJid"].as_str();
    let tasks = if let Some(jid) = jid {
        if let Some(group) = state.group_manager.get(&state.db, jid) {
            state
                .db
                .get_tasks_by_group(&group.folder)
                .unwrap_or_default()
        } else {
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": format!("Group not found: {jid}")}),
            );
            return;
        }
    } else {
        state.db.list_all_tasks().unwrap_or_default()
    };
    let tasks_json: Vec<serde_json::Value> = tasks
        .iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "groupFolder": t.group_folder,
                "chatJid": t.chat_jid,
                "prompt": t.prompt,
                "scheduleType": t.schedule_type.as_str(),
                "scheduleValue": t.schedule_value,
                "contextMode": t.context_mode.as_str(),
                "scriptCommand": t.script_command,
                "nextRun": t.next_run,
                "lastRun": t.last_run,
                "lastResult": t.last_result,
                "status": t.status.as_str(),
                "createdAt": t.created_at,
            })
        })
        .collect();
    send_json(
        sender,
        &serde_json::json!({"type": "tasks", "tasks": tasks_json, "groupJid": jid}),
    );
}

pub(crate) async fn handle_task_logs(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    if !require_admin(clients, client_idx, sender).await {
        return;
    }
    let task_id = msg["taskId"].as_str().unwrap_or("").to_string();
    if task_id.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "taskId required"}),
        );
        return;
    }
    let limit = msg["limit"].as_u64().unwrap_or(20) as u32;
    let logs = state
        .db
        .get_task_run_logs(&task_id, limit)
        .unwrap_or_default();
    let logs_json: Vec<serde_json::Value> = logs
        .iter()
        .map(|l| {
            serde_json::json!({
                "id": l.id,
                "taskId": l.task_id,
                "runAt": l.run_at,
                "durationMs": l.duration_ms,
                "status": l.status.as_str(),
                "result": l.result,
                "error": l.error,
            })
        })
        .collect();
    send_json(
        sender,
        &serde_json::json!({"type": "task-logs", "taskId": task_id, "logs": logs_json}),
    );
}

pub(crate) async fn handle_manage_task(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    if !require_admin(clients, client_idx, sender).await {
        return;
    }
    let task_id = msg["taskId"].as_str().unwrap_or("").to_string();
    let action = msg["action"].as_str().unwrap_or("").to_string();
    if task_id.is_empty() || action.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "taskId and action required"}),
        );
        return;
    }
    let new_status = match action.as_str() {
        "pause" => TaskStatus::Paused,
        "resume" => TaskStatus::Active,
        "cancel" => TaskStatus::Completed,
        _ => {
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": format!("Unknown action: {action}")}),
            );
            return;
        }
    };
    let _ = state.db.update_task_status(&task_id, new_status);
    send_json(
        sender,
        &serde_json::json!({"type": "task:updated", "taskId": task_id, "status": new_status.as_str()}),
    );
}

pub(crate) async fn handle_register_feishu_app(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    if !require_admin(clients, client_idx, sender).await {
        return;
    }
    let app_id = msg["appId"].as_str().unwrap_or("");
    let app_secret = msg["appSecret"].as_str().unwrap_or("");
    if app_id.is_empty() || app_secret.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "appId and appSecret required"}),
        );
        return;
    }
    let domain = msg["domain"].as_str();
    if let Err(e) = save_feishu_app(
        &state.config.paths.global_config_path,
        app_id,
        app_secret,
        domain,
    ) {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": format!("register:feishu-app failed: {e}")}),
        );
        return;
    }
    // Hot-register into FeishuChannel at runtime.
    match state
        .api
        .add_feishu_app_runtime(app_id, app_secret, domain)
        .await
    {
        Ok(false) => {
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": format!("Feishu app {app_id} connection failed: invalid credentials or network issue. Check App ID/App Secret and ensure the app is published.")}),
            );
        }
        Err(e) => {
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": format!("register:feishu-app failed: {e}")}),
            );
        }
        Ok(true) => {
            send_json(
                sender,
                &serde_json::json!({"type": "feishu-app:registered", "appId": app_id}),
            );
        }
    }
}

pub(crate) async fn handle_unregister_feishu_app(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    if !require_admin(clients, client_idx, sender).await {
        return;
    }
    let app_id = msg["appId"].as_str().unwrap_or("").to_string();
    if app_id.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "appId required"}),
        );
        return;
    }
    let _ = delete_feishu_app(&state.config.paths.global_config_path, &app_id);
    send_json(
        sender,
        &serde_json::json!({"type": "feishu-app:unregistered", "appId": app_id}),
    );
}

pub(crate) async fn handle_list_feishu_apps(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let apps = get_feishu_apps(&state.config.paths.global_config_path);
    let list: Vec<serde_json::Value> = apps
        .iter()
        .map(|(app_id, cfg)| {
            serde_json::json!({
                "appId": app_id,
                "domain": cfg.domain.as_deref().unwrap_or("feishu"),
            })
        })
        .collect();
    send_json(
        sender,
        &serde_json::json!({"type": "feishu-apps", "apps": list}),
    );
}

pub(crate) async fn handle_list_dispatch(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    if !require_admin(clients, client_idx, sender).await {
        return;
    }
    let parents = state.api.get_dispatch_parents();
    let parent_count = parents.as_array().map(|a| a.len()).unwrap_or(0);
    tracing::info!("[WsGateway] list:dispatch client #{client_idx}: {parent_count} parent(s)");
    send_json(
        sender,
        &serde_json::json!({"type": "dispatch:update", "parents": parents}),
    );
}

pub(crate) async fn handle_agent_mode(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let group_jid = msg["groupJid"].as_str().unwrap_or("").to_string();
    let mode = msg["mode"].as_str().unwrap_or("").to_string();
    if group_jid.is_empty() || mode.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "groupJid and mode required"}),
        );
        return;
    }
    if !matches!(mode.as_str(), "Agent" | "Plan" | "Dag") {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": format!("Unknown mode: {mode}")}),
        );
        return;
    }
    tracing::info!("[WsGateway] agent:mode client #{client_idx} jid={group_jid} mode={mode}");
    state.api.set_agent_mode(&group_jid, &mode);
    // Broadcast to all authenticated clients so every tab reflects the new mode.
    broadcast_to_all_inner(
        clients,
        &serde_json::json!({
            "type": "agent:mode:changed",
            "groupJid": group_jid,
            "mode": mode,
        }),
    )
    .await;
}

pub(crate) async fn handle_agent_control(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
    msg: &serde_json::Value,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let group_jid = msg["groupJid"].as_str().unwrap_or("").to_string();
    let action = msg["action"].as_str().unwrap_or("").to_string();
    if !group_jid.is_empty() {
        let subscribed = {
            let guard = clients.lock().await;
            guard
                .get(client_idx)
                .map(|c| c.subscriptions.contains(&group_jid))
                .unwrap_or(false)
        };
        if !subscribed {
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": "Subscribe to the group before controlling its agent"}),
            );
            return;
        }
    }
    if group_jid.is_empty() || action.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "groupJid and action required"}),
        );
        return;
    }
    if state.group_manager.get(&state.db, &group_jid).is_none() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": format!("Group not found: {group_jid}")}),
        );
        return;
    }
    match action.as_str() {
        "pause" => state.api.pause_agent(&group_jid),
        "resume" => {
            let query = msg["query"].as_str();
            state.api.resume_agent(&group_jid, query);
        }
        "stop" => {
            let api = state.api.clone();
            let jid = group_jid;
            tokio::spawn(async move {
                api.stop_agent(&jid).await;
            });
        }
        // Stop the agent AND permanently delete all persisted message history for this JID.
        // After stopping, push an empty history:load so the frontend clears its local list.
        "stop_and_clear" => {
            let api = state.api.clone();
            let db = state.db.clone();
            let jid = group_jid.clone();
            let sender_clone = sender.clone();
            tokio::spawn(async move {
                api.stop_agent(&jid).await;

                // 1. Chat messages (user / agent text turns)
                let del_msg = db.delete_group_messages_for_jid(&jid).unwrap_or(0);

                // 2. Tool-execution action logs (browser actions, bash, read, etc.)
                let del_tools = db.delete_tool_executions_for_jid(&jid).unwrap_or(0);

                // 3. Ephemeral chat events (permission/question request+resolved pairs)
                let del_events = db.delete_chat_events_for_jid(&jid).unwrap_or(0);

                // 4. One-way chat widgets (chart/image/clock/weather cards)
                let del_widgets = db.delete_chat_widgets_for_jid(&jid).unwrap_or(0);

                tracing::info!(
                    "[handle_agent_control] stop_and_clear for {jid}: \
                     {del_msg} messages, {del_tools} tool actions, {del_events} chat events, \
                     {del_widgets} widgets deleted"
                );

                // Signal frontend to wipe its in-memory list.
                send_json(
                    &sender_clone,
                    &serde_json::json!({
                        "type": "history:load",
                        "groupJid": jid,
                        "messages": [],
                    }),
                );
            });
        }
        _ => {
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": format!("Unknown agent:control action: {action}")}),
            );
        }
    }
}

#[cfg(test)]
mod context_preamble_tests {
    use super::apply_context_preamble;

    #[test]
    fn preamble_reaches_the_agent() {
        let out = apply_context_preamble("Xoá nhắc nhở này", Some("[Ngữ cảnh] eventId=abc"));
        assert!(out.starts_with("[Ngữ cảnh] eventId=abc"));
        assert!(out.ends_with("Xoá nhắc nhở này"));
    }

    #[test]
    fn absent_or_blank_preamble_leaves_text_untouched() {
        // A blank field must not open the turn with two stray newlines.
        for p in [None, Some(""), Some("   \n ")] {
            assert_eq!(apply_context_preamble("chào", p), "chào");
        }
    }
}
