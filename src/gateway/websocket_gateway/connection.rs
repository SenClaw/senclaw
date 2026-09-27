// ===== Connection handler =====

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use tokio::sync::Mutex;

use super::helpers::{send_error, send_json};
use super::state::{WsClient, WsState};

pub(crate) async fn handle_connection(
    ws: WebSocket,
    clients: Arc<Mutex<Vec<WsClient>>>,
    last_known_states: Arc<Mutex<HashMap<String, String>>>,
    pending_interactions: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    token: Option<String>,
    state: Arc<WsState>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    let (mut ws_sender, mut ws_receiver) = ws.split();

    // Register client.
    let auto_auth = token.is_none();
    if auto_auth {
        tracing::warn!(
            "[WsGateway] GATEWAY_TOKEN not set — client auto-authenticated. \
             Set GATEWAY_TOKEN via env for production."
        );
    }
    // Register the client at a STABLE index. Reuse a tombstoned slot if one is
    // free (keeps the `Vec` from growing unbounded) — otherwise append. Either
    // way the returned index never shifts for the life of this connection, so
    // every later `guard.get(client_idx)` stays correct even as other clients
    // come and go. See `WsClient::dead`.
    let client_idx: usize = {
        let mut guard = clients.lock().await;
        let new_client = WsClient {
            sender: tx.clone(),
            authenticated: auto_auth,
            is_admin: false,
            subscriptions: HashSet::new(),
            dead: false,
        };
        if let Some(pos) = guard.iter().position(|c| c.dead) {
            guard[pos] = new_client;
            pos
        } else {
            guard.push(new_client);
            guard.len() - 1
        }
    };

    if auto_auth {
        let _ = tx.send(Message::Text(r#"{"type":"auth:ok"}"#.to_string().into()));
        super::handlers::replay_event_notification_snapshot(&tx, &state.db).await;
    }

    // Forward channel messages → WebSocket sink.
    let mut rx = rx;
    let forward_handle = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Read loop.
    while let Some(Ok(msg)) = ws_receiver.next().await {
        match msg {
            Message::Text(text) => {
                let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) else {
                    send_error(&clients, client_idx, "Invalid JSON").await;
                    continue;
                };
                handle_message(
                    client_idx,
                    &parsed,
                    &clients,
                    &last_known_states,
                    &pending_interactions,
                    &token,
                    &state,
                )
                .await;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    forward_handle.abort();
    {
        // Tombstone this slot instead of removing it — `Vec::remove` would shift
        // every later client down and invalidate their cached `client_idx`,
        // misrouting their subscriptions and history. Clearing subscriptions +
        // `authenticated=false` makes every broadcast skip this dead slot, and a
        // future connection can reuse it. See `WsClient::dead`.
        let mut guard = clients.lock().await;
        if let Some(client) = guard.get_mut(client_idx) {
            client.dead = true;
            client.authenticated = false;
            client.is_admin = false;
            client.subscriptions.clear();
        }
    }
}

// ===== Message dispatch =====

async fn handle_message(
    client_idx: usize,
    msg: &serde_json::Value,
    clients: &Arc<Mutex<Vec<WsClient>>>,
    last_known_states: &Arc<Mutex<HashMap<String, String>>>,
    pending_interactions: &Arc<Mutex<HashMap<String, serde_json::Value>>>,
    token: &Option<String>,
    state: &Arc<WsState>,
) {
    let msg_type = msg["type"].as_str().unwrap_or("");

    let sender = {
        let guard = clients.lock().await;
        guard.get(client_idx).map(|c| c.sender.clone())
    };
    let Some(sender) = sender else { return };

    match msg_type {
        "connect" => {
            super::handlers::handle_connect(clients, client_idx, &sender, token, msg, state).await
        }
        "notification:read" => {
            super::handlers::handle_notification_read(clients, client_idx, &sender, state, msg)
                .await
        }
        "subscribe" => {
            super::handlers::handle_subscribe(
                clients,
                client_idx,
                &sender,
                last_known_states,
                pending_interactions,
                state,
                msg,
            )
            .await
        }
        "unsubscribe" => {
            super::handlers::handle_unsubscribe(clients, client_idx, &sender, msg).await
        }
        "list:groups" => {
            super::handlers::handle_list_groups(clients, client_idx, &sender, state).await
        }
        "register:group" => {
            super::handlers::handle_register_group(clients, client_idx, &sender, state, msg).await
        }
        "unregister:group" => {
            super::handlers::handle_unregister_group(clients, client_idx, &sender, state, msg).await
        }
        "update:group" => {
            super::handlers::handle_update_group(clients, client_idx, &sender, state, msg).await
        }
        "message" => {
            super::handlers::handle_message_send(clients, client_idx, &sender, state, msg).await
        }
        "permission:response" => {
            super::handlers::handle_permission_response(clients, client_idx, &sender, state, msg)
                .await
        }
        "permission:rule:add" => {
            super::handlers::handle_tool_rule_add(clients, client_idx, &sender, state, msg).await
        }
        "permission:rule:remove" => {
            super::handlers::handle_tool_rule_remove(clients, client_idx, &sender, state, msg).await
        }
        "permission:rule:update" => {
            super::handlers::handle_tool_rule_update(clients, client_idx, &sender, state, msg).await
        }
        "permission:accept-all" => {
            super::handlers::handle_tool_accept_all(clients, client_idx, &sender, state, msg).await
        }
        "plan:exit:response" => {
            super::handlers::handle_plan_exit_response(clients, client_idx, &sender, state, msg)
                .await
        }
        "plan:list" => {
            super::handlers::handle_plan_list(clients, client_idx, &sender, state, msg).await
        }
        "plan:get" => {
            super::handlers::handle_plan_get(clients, client_idx, &sender, state, msg).await
        }
        "notifications:list" => {
            super::handlers::handle_notifications_list(clients, client_idx, &sender, state, msg)
                .await
        }
        "notifications:pending" => {
            super::handlers::handle_notifications_pending(clients, client_idx, &sender, state, msg)
                .await
        }
        "question:response" => {
            super::handlers::handle_question_response(clients, client_idx, &sender, state, msg)
                .await
        }
        "form:response" => {
            super::handlers::handle_form_response(clients, client_idx, &sender, state, msg).await
        }
        "list:tasks" => {
            super::handlers::handle_list_tasks(clients, client_idx, &sender, state, msg).await
        }
        "list:task-logs" => {
            super::handlers::handle_task_logs(clients, client_idx, &sender, state, msg).await
        }
        "manage:task" => {
            super::handlers::handle_manage_task(clients, client_idx, &sender, state, msg).await
        }
        "register:feishu-app" => {
            super::handlers::handle_register_feishu_app(clients, client_idx, &sender, state, msg)
                .await
        }
        "unregister:feishu-app" => {
            super::handlers::handle_unregister_feishu_app(clients, client_idx, &sender, state, msg)
                .await
        }
        "list:feishu-apps" => {
            super::handlers::handle_list_feishu_apps(clients, client_idx, &sender, state).await
        }
        "list:dispatch" => {
            super::handlers::handle_list_dispatch(clients, client_idx, &sender, state).await
        }
        "dismiss:todos" => {
            super::handlers::handle_todos_dismiss(clients, client_idx, &sender, state, msg).await
        }
        "agent:control" => {
            super::handlers::handle_agent_control(clients, client_idx, &sender, state, msg).await
        }
        "agent:mode" => {
            super::handlers::handle_agent_mode(clients, client_idx, &sender, state, msg).await
        }
        "list:channels" => {
            super::entity_handlers::handle_list_channels(clients, client_idx, &sender, state).await
        }
        "list:agents" => {
            super::entity_handlers::handle_list_agents(clients, client_idx, &sender, state).await
        }
        "list:bindings" => {
            super::entity_handlers::handle_list_bindings(clients, client_idx, &sender, state).await
        }
        "register:channel" => {
            super::entity_handlers::handle_register_channel(
                clients, client_idx, &sender, state, msg,
            )
            .await
        }
        "register:agent" => {
            super::entity_handlers::handle_register_agent(clients, client_idx, &sender, state, msg)
                .await
        }
        "register:binding" => {
            super::entity_handlers::handle_register_binding(
                clients, client_idx, &sender, state, msg,
            )
            .await
        }
        "unregister:channel" => {
            super::entity_handlers::handle_unregister_channel(
                clients, client_idx, &sender, state, msg,
            )
            .await
        }
        "unregister:agent" => {
            super::entity_handlers::handle_unregister_agent(
                clients, client_idx, &sender, state, msg,
            )
            .await
        }
        "unregister:binding" => {
            super::entity_handlers::handle_unregister_binding(
                clients, client_idx, &sender, state, msg,
            )
            .await
        }
        "update:channel" => {
            super::entity_handlers::handle_update_channel(clients, client_idx, &sender, state, msg)
                .await
        }
        "update:agent" => {
            super::entity_handlers::handle_update_agent(clients, client_idx, &sender, state, msg)
                .await
        }
        "update:binding" => {
            super::entity_handlers::handle_update_binding(clients, client_idx, &sender, state, msg)
                .await
        }
        _ => {
            send_json(
                &sender,
                &serde_json::json!({"type": "error", "message": format!("Unknown message type: {msg_type}")}),
            );
        }
    }
}
