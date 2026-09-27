//! Event injection methods (called externally by daemon wiring).

use super::gateway::WebSocketGateway;
use super::wire::to_group_info;
use crate::types::GroupBinding;

impl WebSocketGateway {
    // ===== Event injection (called externally) =====

    pub async fn notify_incoming(&self, msg: &crate::types::IncomingMessage) {
        let payload = serde_json::json!({
            "type": "incoming",
            "groupJid": msg.chat_jid,
            "senderName": msg.sender_name,
            "text": msg.content,
            "timestamp": msg.timestamp,
            "isFromMe": msg.is_from_me,
        });
        self.broadcast(&msg.chat_jid, &payload).await;

        let ts_ms = chrono::DateTime::parse_from_rfc3339(&msg.timestamp)
            .map(|d| d.timestamp_millis())
            .unwrap_or_else(|_| chrono::Utc::now().timestamp_millis());
        self.notify_group_activity(&msg.chat_jid, ts_ms).await;
    }

    /// An unknown chat is asking to be let in. Pushed to every client so the
    /// Settings page shows the request without polling — the person who just
    /// messaged the bot is usually standing next to the browser.
    ///
    /// The code rides along because approving needs it and the request is only
    /// ever visible to already-authenticated UI clients.
    pub async fn notify_pairing_requested(&self, p: &crate::types::ChannelPairing) {
        self.broadcast_to_all(&serde_json::json!({
            "type": "pairing:requested",
            "pairing": p,
        }))
        .await;
    }

    /// A request was approved or turned away — clears it from every open
    /// Settings page, including the one that did not click.
    pub async fn notify_pairing_resolved(&self, id: i64, status: &str) {
        self.broadcast_to_all(&serde_json::json!({
            "type": "pairing:resolved",
            "id": id,
            "status": status,
        }))
        .await;
    }

    /// Sidebar "recent activity" tick. Goes to ALL clients (not just the
    /// chat's subscribers) so the session list can reorder live on any new
    /// message or agent response.
    /// A new shadow-git checkpoint exists for `chat_jid`. Clients showing the
    /// chat refresh their restore/diff affordances; nobody else cares.
    pub async fn notify_checkpoint_new(&self, chat_jid: &str, cp: &crate::types::ChatCheckpoint) {
        self.broadcast(
            chat_jid,
            &serde_json::json!({
                "type": "checkpoint:new",
                "groupJid": chat_jid,
                "checkpoint": cp,
            }),
        )
        .await;
    }

    pub async fn notify_group_activity(&self, chat_jid: &str, ts_ms: i64) {
        self.broadcast_to_all(&serde_json::json!({
            "type": "group:activity",
            "jid": chat_jid,
            "ts": ts_ms,
        }))
        .await;
    }

    /// Emit an incremental agent reply delta. The frontend accumulates these
    /// into a streaming bubble keyed by `groupJid`; the final `agent:reply`
    /// drops the streaming bubble and replaces it with the completed message.
    pub async fn notify_agent_delta(&self, chat_jid: &str, delta: &str) {
        let payload = serde_json::json!({
            "type": "agent:delta",
            "groupJid": chat_jid,
            "delta": delta,
            "ts": chrono::Utc::now().to_rfc3339(),
        });
        self.broadcast(chat_jid, &payload).await;
    }

    pub async fn notify_agent_reply(&self, chat_jid: &str, text: &str, tokens: u32) {
        // Cowork hook: mark the manager's pending task done + save reply
        // as result_output. Mirrors legacy CoworkManager's "task complete
        // on message_complete" path.
        if let Some(team_id) = chat_jid.strip_prefix("cowork:") {
            if let Some(db) = self.db.read().unwrap().clone() {
                crate::gateway::ui_server::cowork_runtime::on_agent_reply(&db, team_id, text);
            }
        }

        // Stamp at emit time so the client can chronologically interleave
        // agent:reply with tool:execution events (both carry `ts`).
        // Without this, agent:reply used the client's WS-arrival clock and
        // could land out of order vs server-timestamped tool events.
        let payload = serde_json::json!({
            "type": "agent:reply",
            "groupJid": chat_jid,
            "text": text,
            // Output-token cost of this assistant message (0 = unknown). The
            // chat UI shows it per-message.
            "tokens": tokens,
            "ts": chrono::Utc::now().to_rfc3339(),
        });
        self.broadcast(chat_jid, &payload).await;
        self.notify_group_activity(chat_jid, chrono::Utc::now().timestamp_millis())
            .await;
    }

    pub async fn notify_agent_state(&self, chat_jid: &str, state: &str) {
        // Cowork hook: transition the manager's task to in_progress when
        // the agent starts processing this turn.
        if state == "processing" {
            if let Some(team_id) = chat_jid.strip_prefix("cowork:") {
                if let Some(db) = self.db.read().unwrap().clone() {
                    crate::gateway::ui_server::cowork_runtime::on_agent_processing(&db, team_id);
                }
            }
        }

        self.last_known_states
            .lock()
            .await
            .insert(chat_jid.to_string(), state.to_string());
        let payload = serde_json::json!({
            "type": "agent:state",
            "groupJid": chat_jid,
            "state": state,
        });
        self.broadcast(chat_jid, &payload).await;
    }

    pub async fn notify_agent_compacting(&self, chat_jid: &str, is_compacting: bool) {
        let payload = serde_json::json!({
            "type": "agent:compacting",
            "groupJid": chat_jid,
            "isCompacting": is_compacting,
        });
        self.broadcast(chat_jid, &payload).await;
    }

    pub async fn notify_agent_usage(
        &self,
        agent_jid: &str,
        usage: &crate::zen_core::ConversationUsageData,
    ) {
        let payload = serde_json::json!({
            "type": "agent:usage",
            "agentJid": agent_jid,
            "usage": {
                "useTokens": usage.usage.use_tokens,
                "maxTokens": usage.usage.max_tokens,
                "promptTokens": usage.usage.prompt_tokens,
            },
        });
        // Subscription-filtered like `agent:reply` — this used to be
        // broadcast_to_all, which leaked every agent's context-gauge to every
        // authenticated client regardless of what they were subscribed to.
        self.broadcast(agent_jid, &payload).await;
    }

    pub async fn notify_permission_request(
        &self,
        chat_jid: &str,
        request_id: &str,
        payload: &serde_json::Value,
    ) {
        let mut msg = payload.clone();
        if let Some(obj) = msg.as_object_mut() {
            obj.insert("type".into(), "permission:request".into());
            obj.insert("groupJid".into(), chat_jid.into());
            obj.insert("requestId".into(), request_id.into());
        }
        // Store for admin subscribe snapshot replay (so reconnecting admins see pending requests).
        self.pending_interactions
            .lock()
            .await
            .insert(request_id.to_string(), msg.clone());
        if chat_jid.starts_with("virtual:") {
            self.broadcast_to_admins(&msg).await;
        } else {
            // Broadcast to group subscribers (covers users viewing that chat).
            self.broadcast(chat_jid, &msg).await;
            // Also notify admins NOT subscribed to this group so the Agent Console
            // always shows dispatch subagent permissions. Admins that ARE subscribed
            // already received it from broadcast() above — skip them to avoid duplicates.
            self.broadcast_to_admins_excluding(chat_jid, &msg).await;
        }
    }

    pub async fn notify_task_backlog(
        &self,
        task_id: &str,
        chat_jid: &str,
        prompt: &str,
        interval_ms: u64,
        overdue_ms: u64,
    ) {
        let msg = serde_json::json!({
            "type": "task:backlog",
            "taskId": task_id,
            "chatJid": chat_jid,
            "prompt": prompt,
            "intervalMs": interval_ms,
            "overdueMs": overdue_ms,
            "suggestedIntervalMs": interval_ms + overdue_ms,
        });
        tracing::info!(
            "[WsGateway] emit task:backlog task_id={task_id} chat_jid={chat_jid} \
             prompt_len={} interval_ms={interval_ms} overdue_ms={overdue_ms} \
             suggested_interval_ms={}",
            prompt.len(),
            interval_ms + overdue_ms
        );
        self.broadcast_to_admins(&msg).await;
    }

    pub async fn notify_ask_question_request(
        &self,
        chat_jid: &str,
        request_id: &str,
        payload: &serde_json::Value,
    ) {
        let mut msg = payload.clone();
        if let Some(obj) = msg.as_object_mut() {
            obj.insert("type".into(), "question:request".into());
            obj.insert("groupJid".into(), chat_jid.into());
            obj.insert("requestId".into(), request_id.into());
        }
        tracing::info!("[WsGateway] notify question:request id={request_id} chat_jid={chat_jid}");
        // Store for admin subscribe snapshot replay.
        self.pending_interactions
            .lock()
            .await
            .insert(request_id.to_string(), msg.clone());
        // Broadcast to group subscribers + admins not already subscribed (no duplicate).
        self.broadcast(chat_jid, &msg).await;
        self.broadcast_to_admins_excluding(chat_jid, &msg).await;
    }

    pub async fn notify_form_request(
        &self,
        chat_jid: &str,
        request_id: &str,
        payload: &serde_json::Value,
    ) {
        let mut msg = payload.clone();
        if let Some(obj) = msg.as_object_mut() {
            obj.insert("type".into(), "form:request".into());
            obj.insert("groupJid".into(), chat_jid.into());
            obj.insert("requestId".into(), request_id.into());
        }
        tracing::info!("[WsGateway] notify form:request id={request_id} chat_jid={chat_jid}");
        // Store for admin subscribe snapshot replay.
        self.pending_interactions
            .lock()
            .await
            .insert(request_id.to_string(), msg.clone());
        // Broadcast to group subscribers + admins not already subscribed (no duplicate).
        self.broadcast(chat_jid, &msg).await;
        self.broadcast_to_admins_excluding(chat_jid, &msg).await;
    }

    pub async fn notify_form_resolved(
        &self,
        chat_jid: &str,
        request_id: &str,
        values: &serde_json::Value,
    ) {
        // Remove from pending store.
        self.pending_interactions.lock().await.remove(request_id);
        let msg = serde_json::json!({
            "type": "form:resolved",
            "groupJid": chat_jid,
            "requestId": request_id,
            "values": values,
        });
        self.broadcast(chat_jid, &msg).await;
        self.broadcast_to_admins_excluding(chat_jid, &msg).await;
    }

    pub async fn notify_permission_resolved(
        &self,
        chat_jid: &str,
        request_id: &str,
        option_key: &str,
        option_label: &str,
    ) {
        // Remove from pending store so reconnecting admins don't see resolved requests.
        self.pending_interactions.lock().await.remove(request_id);
        let msg = serde_json::json!({
            "type": "permission:resolved",
            "groupJid": chat_jid,
            "requestId": request_id,
            "optionKey": option_key,
            "optionLabel": option_label,
        });
        if chat_jid.starts_with("virtual:") {
            self.broadcast_to_admins(&msg).await;
        } else {
            self.broadcast(chat_jid, &msg).await;
            self.broadcast_to_admins_excluding(chat_jid, &msg).await;
        }
    }

    pub async fn notify_ask_question_resolved(
        &self,
        chat_jid: &str,
        request_id: &str,
        answers: &serde_json::Value,
    ) {
        // Remove from pending store.
        self.pending_interactions.lock().await.remove(request_id);
        let msg = serde_json::json!({
            "type": "question:resolved",
            "groupJid": chat_jid,
            "requestId": request_id,
            "answers": answers,
        });
        self.broadcast(chat_jid, &msg).await;
        self.broadcast_to_admins_excluding(chat_jid, &msg).await;
    }

    /// Forward a single sub-agent activity entry (tool call, message) to admin
    /// clients. Lightweight — one entry per WS frame; frontend accumulates.
    pub async fn notify_dispatch_activity(
        &self,
        task_id: &str,
        entry: &crate::agent::virtual_worker_pool::SubAgentActivityEntry,
    ) {
        let msg = serde_json::json!({
            "type": "dispatch:activity",
            "taskId": task_id,
            "entry": entry,
        });
        self.broadcast_to_admins(&msg).await;
    }

    // ===== Background tasks =====
    //
    // The user-facing scheduler pushes nothing when a task fires — its WS
    // messages are strictly request/response. Background runs are unattended,
    // so live push is the only way anyone sees them happen.

    pub async fn notify_background_run_started(
        &self,
        task_id: &str,
        run_id: &str,
        title: &str,
        trigger: &str,
    ) {
        let msg = serde_json::json!({
            "type": "bg:run:started",
            "taskId": task_id,
            "runId": run_id,
            "title": title,
            "triggerKind": trigger,
        });
        self.broadcast_to_admins(&msg).await;
    }

    pub async fn notify_background_run_activity(
        &self,
        task_id: &str,
        run_id: &str,
        kind: &str,
        detail: &str,
    ) {
        let msg = serde_json::json!({
            "type": "bg:run:activity",
            "taskId": task_id,
            "runId": run_id,
            "kind": kind,
            "detail": detail,
        });
        self.broadcast_to_admins(&msg).await;
    }

    pub async fn notify_background_run_finished(
        &self,
        task_id: &str,
        run_id: &str,
        status: &str,
        duration_ms: i64,
        error: Option<&str>,
    ) {
        let msg = serde_json::json!({
            "type": "bg:run:finished",
            "taskId": task_id,
            "runId": run_id,
            "status": status,
            "durationMs": duration_ms,
            "error": error,
        });
        self.broadcast_to_admins(&msg).await;
    }

    /// Push a generic notification (OS notification when the app is
    /// backgrounded, plus the in-app bell). Reuses the `notification` frame the
    /// desktop already handles — same shape as calendar reminders, minus the
    /// event target.
    pub async fn notify_notification(&self, id: &str, title: &str, message: &str, kind: &str) {
        let msg = serde_json::json!({
            "type": "notification",
            "id": id,
            "title": title,
            "message": message,
            "kind": kind,
        });
        self.broadcast_to_admins(&msg).await;
    }

    pub async fn notify_background_task_changed(&self, task: &crate::types::BackgroundTask) {
        let msg = serde_json::json!({
            "type": "bg:task:changed",
            "taskId": task.id,
            "title": task.title,
            "status": task.status.as_str(),
            "nextRun": task.next_run,
            "consecutiveFailures": task.consecutive_failures,
        });
        self.broadcast_to_admins(&msg).await;
    }

    pub async fn notify_dispatch_update(&self, parents: &serde_json::Value) {
        let msg = serde_json::json!({
            "type": "dispatch:update",
            "parents": parents,
        });
        let parent_count = parents.as_array().map(|a| a.len()).unwrap_or(0);
        let task_count = parents
            .as_array()
            .map(|arr| {
                arr.iter()
                    .map(|p| {
                        p.get("tasks")
                            .and_then(|v| v.as_array())
                            .map(|t| t.len())
                            .unwrap_or(0)
                    })
                    .sum::<usize>()
            })
            .unwrap_or(0);
        tracing::info!(
            "[WsGateway] emit dispatch:update parents={parent_count} tasks={task_count}"
        );
        self.broadcast_to_admins(&msg).await;
    }

    /// Push one workflow run state change to admin clients (Workflow dock).
    pub async fn notify_workflow_update(&self, run: &serde_json::Value) {
        let msg = serde_json::json!({
            "type": "workflow:update",
            "run": run,
        });
        self.broadcast_to_admins(&msg).await;
    }

    /// Push a built-in Kanban board change (any writer: REST, dispatcher worker,
    /// or the `kanban-server` stdio MCP). The desktop Kanban screen re-fetches
    /// the board on this event, so it updates live without a manual Refresh.
    pub async fn notify_kanban_update(&self, board_id: i64) {
        let msg = serde_json::json!({
            "type": "kanban:update",
            "boardId": board_id,
        });
        tracing::debug!("[WsGateway] emit kanban:update board={board_id}");
        self.broadcast_to_admins(&msg).await;
    }

    pub async fn notify_agent_todos(
        &self,
        agent_jid: &str,
        agent_name: &str,
        todos: &serde_json::Value,
    ) {
        let msg = serde_json::json!({
            "type": "agent:todos",
            "agentJid": agent_jid,
            "agentName": agent_name,
            "todos": todos,
        });
        let todo_count = todos.as_array().map(|a| a.len()).unwrap_or(0);
        let completed_count = todos
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter(|item| {
                        item.get("status")
                            .and_then(|s| s.as_str())
                            .map(|s| s == "completed")
                            .unwrap_or(false)
                    })
                    .count()
            })
            .unwrap_or(0);
        tracing::info!(
            "[WsGateway] emit agent:todos agent_jid={agent_jid} agent_name={agent_name} \
             todos={todo_count} completed={completed_count}"
        );
        self.broadcast_to_admins(&msg).await;
    }

    pub async fn notify_agent_tools(
        &self,
        agent_jid: &str,
        agent_name: &str,
        tools: &serde_json::Value,
    ) {
        let msg = serde_json::json!({
            "type": "agent:tools",
            "agentJid": agent_jid,
            "agentName": agent_name,
            "tools": tools,
        });
        self.broadcast_to_admins(&msg).await;
    }

    /// Push a calendar event reminder to all connected UI clients.
    ///
    /// `kind` is `"reminder"` (pre-event) or `"renotify"` (ongoing re-alert).
    /// `notification_id` is the persisted `event_notifications.id` so the
    /// frontend can dedupe across the live frame and the subscribe replay,
    /// and so `notification:read` can target a specific row.
    /// `delayed_ms` is non-zero when the daemon was down past the trigger
    /// time (so the UI can render a "late" badge).
    pub async fn push_event_reminder(
        &self,
        notification_id: &str,
        event_id: &str,
        title: &str,
        start_at_ms: i64,
        kind: &str,
        fired_at_ms: i64,
        delayed_ms: i64,
    ) {
        let payload = serde_json::json!({
            "type": "space:event:reminder",
            "id": notification_id,
            "eventId": event_id,
            "title": title,
            "startAt": start_at_ms,
            "kind": kind,
            "firedAt": fired_at_ms,
            "delayedMs": delayed_ms,
        });
        tracing::info!(
            "[WsGateway] emit space:event:reminder id={notification_id} event_id={event_id} kind={kind} delayed_ms={delayed_ms}"
        );
        self.broadcast_to_all(&payload).await;
    }

    pub async fn notify_group_migrated(&self, old_jid: &str, new_binding: &GroupBinding) {
        self.broadcast_to_all(&serde_json::json!({"type": "group:unregistered", "jid": old_jid}))
            .await;
        self.broadcast_to_all(
            &serde_json::json!({"type": "group:registered", "group": to_group_info(new_binding)}),
        )
        .await;
    }

    // ===== Tool execution (chat-inline display) =====

    /// Broadcast a single tool execution event so the chat UI can render a
    /// claude-code-style tool-call card (collapsed: "Read 3 files, ran 1
    /// command"; expanded: per-call detail). Only sent to clients subscribed
    /// to `chat_jid` so we don't spam admins with irrelevant tool noise.
    pub async fn notify_tool_execution(
        &self,
        chat_jid: &str,
        agent_id: &str,
        tool_name: &str,
        title: &str,
        summary: &str,
        description: &str,
        content: &serde_json::Value,
        ok: bool,
        ts: &str,
    ) {
        let msg = serde_json::json!({
            "type": "tool:execution",
            "groupJid": chat_jid,
            "agentId": agent_id,
            "toolName": tool_name,
            "title": title,
            "summary": summary,
            "description": description,
            "content": content,
            "ok": ok,
            "ts": ts,
        });
        self.broadcast(chat_jid, &msg).await;
    }

    /// Broadcast a one-way rich widget (chart/image/clock/weather) so the chat
    /// UI can render an inline widget card. Display-only — there is no response
    /// round-trip (unlike `form:request`). Only sent to clients subscribed to
    /// `chat_jid`. Mirrors [`Self::notify_tool_execution`].
    pub async fn notify_widget(
        &self,
        chat_jid: &str,
        id: &str,
        widget: &serde_json::Value,
        ts: &str,
    ) {
        let msg = serde_json::json!({
            "type": "chat:widget",
            "groupJid": chat_jid,
            "id": id,
            "widget": widget,
            "ts": ts,
        });
        self.broadcast(chat_jid, &msg).await;
    }

    // ===== Plan mode (ExitPlanMode tool) =====

    /// Forwarded from `EngineEvent::PlanExitRequest`. UI renders the modal
    /// `PlanExitDialog` and POSTs back via `plan:exit:response` with the
    /// chosen option (`startEditing` | `clearContextAndStart`).
    pub async fn notify_plan_exit_request(
        &self,
        chat_jid: &str,
        agent_id: &str,
        plan_file_path: &str,
        plan_content: &str,
        option_start_editing: &str,
        option_clear_context: &str,
    ) {
        let msg = serde_json::json!({
            "type": "plan:exit:request",
            "groupJid": chat_jid,
            "agentId": agent_id,
            "planFilePath": plan_file_path,
            "planContent": plan_content,
            "options": {
                "startEditing": option_start_editing,
                "clearContextAndStart": option_clear_context,
            },
        });
        self.broadcast(chat_jid, &msg).await;
        self.broadcast_to_admins_excluding(chat_jid, &msg).await;
    }

    /// Server-side confirmation that the engine accepted the user's choice.
    /// Sent after `respond_to_plan_exit` completes so UI can close the modal
    /// even if it was opened in multiple browser sessions.
    pub async fn notify_plan_exit_response(&self, chat_jid: &str, agent_id: &str, selected: &str) {
        let msg = serde_json::json!({
            "type": "plan:exit:response",
            "groupJid": chat_jid,
            "agentId": agent_id,
            "selected": selected,
        });
        self.broadcast(chat_jid, &msg).await;
        self.broadcast_to_admins_excluding(chat_jid, &msg).await;
    }

    // ===== Workbench events =====

    pub async fn notify_workbench_new(
        &self,
        chat_jid: &str,
        artifact: &serde_json::Value,
        replaces_id: Option<&str>,
    ) {
        let msg = serde_json::json!({
            "type": "workbench:new",
            "groupJid": chat_jid,
            "artifact": artifact,
            "replacesId": replaces_id,
        });
        self.broadcast(chat_jid, &msg).await;
        self.broadcast_to_admins_excluding(chat_jid, &msg).await;
    }

    pub async fn notify_workbench_service_ready(
        &self,
        chat_jid: &str,
        artifact_id: &str,
        ready: bool,
    ) {
        let msg = serde_json::json!({
            "type": "workbench:service_ready",
            "groupJid": chat_jid,
            "artifactId": artifact_id,
            "ready": ready,
        });
        self.broadcast(chat_jid, &msg).await;
        self.broadcast_to_admins_excluding(chat_jid, &msg).await;
    }

    pub async fn notify_workbench_service_crashed(
        &self,
        chat_jid: &str,
        artifact_id: &str,
        last_log_lines: &str,
    ) {
        let msg = serde_json::json!({
            "type": "workbench:service_crashed",
            "groupJid": chat_jid,
            "artifactId": artifact_id,
            "lastLogLines": last_log_lines,
        });
        self.broadcast(chat_jid, &msg).await;
        self.broadcast_to_admins_excluding(chat_jid, &msg).await;
    }

    pub async fn notify_workbench_service_stopped(
        &self,
        chat_jid: &str,
        artifact_id: &str,
        reason: &str,
    ) {
        let msg = serde_json::json!({
            "type": "workbench:service_stopped",
            "groupJid": chat_jid,
            "artifactId": artifact_id,
            "reason": reason,
        });
        self.broadcast(chat_jid, &msg).await;
        self.broadcast_to_admins_excluding(chat_jid, &msg).await;
    }

    /// Push last-known agent state to a newly subscribed client.
    pub async fn push_last_known_state(
        &self,
        sender: &tokio::sync::mpsc::UnboundedSender<axum::extract::ws::Message>,
        jid: &str,
    ) {
        let states = self.last_known_states.lock().await;
        if let Some(state) = states.get(jid) {
            let msg = serde_json::json!({
                "type": "agent:state",
                "groupJid": jid,
                "state": state,
            });
            let _ = sender.send(axum::extract::ws::Message::Text(msg.to_string().into()));
        }
    }
}
