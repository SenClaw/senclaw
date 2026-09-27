// ===== Entity CRUD handlers =====

use std::sync::Arc;

use axum::extract::ws::Message;
use tokio::sync::Mutex;

use crate::util::local_time::local_iso_string_now;

use super::helpers::{broadcast_to_all_inner, require_admin, require_auth, send_json};
use super::state::{WsClient, WsState};
use super::wire::{
    to_agent_info, to_binding_with_relations, to_channel_info, AgentInfoWire,
    BindingWithRelationsWire, ChannelInfoWire,
};

pub(crate) async fn handle_list_channels(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let channels: Vec<ChannelInfoWire> = state
        .channel_manager
        .list(&state.db)
        .unwrap_or_default()
        .iter()
        .map(|c| to_channel_info(c))
        .collect();
    send_json(
        sender,
        &serde_json::json!({"type": "channels", "channels": channels}),
    );
}

pub(crate) async fn handle_list_agents(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let agents: Vec<AgentInfoWire> = state
        .agent_manager
        .list(&state.db)
        .unwrap_or_default()
        .iter()
        .map(|a| to_agent_info(a))
        .collect();
    send_json(
        sender,
        &serde_json::json!({"type": "agents", "agents": agents}),
    );
}

pub(crate) async fn handle_list_bindings(
    clients: &Arc<Mutex<Vec<WsClient>>>,
    client_idx: usize,
    sender: &tokio::sync::mpsc::UnboundedSender<Message>,
    state: &Arc<WsState>,
) {
    if !require_auth(clients, client_idx, sender).await {
        return;
    }
    let bindings: Vec<BindingWithRelationsWire> = state
        .binding_manager
        .list_with_relations(&state.db)
        .unwrap_or_default()
        .iter()
        .map(|b| to_binding_with_relations(b))
        .collect();
    send_json(
        sender,
        &serde_json::json!({"type": "bindings", "bindings": bindings}),
    );
}

pub(crate) async fn handle_register_channel(
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
    let platform_type = msg["platformType"].as_str().unwrap_or("");
    let name = msg["name"].as_str().unwrap_or("");
    if platform_type.is_empty() || name.is_empty() {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "platformType and name are required"}),
        );
        return;
    }
    let credentials = msg["credentials"].clone();
    let now = local_iso_string_now();
    match state.channel_manager.create(
        &state.db,
        platform_type,
        name,
        &credentials.to_string(),
        &now,
    ) {
        Ok(ch) => {
            let wire = to_channel_info(&ch);
            send_json(
                sender,
                &serde_json::json!({"type": "channel:registered", "channel": wire}),
            );
            broadcast_to_all_inner(
                clients,
                &serde_json::json!({"type": "channel:registered", "channel": wire}),
            )
            .await;
        }
        Err(e) => {
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": format!("{e}")}),
            );
        }
    }
}

pub(crate) async fn handle_register_agent(
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
    let requires_trigger = msg["requiresTrigger"].as_bool().unwrap_or(true);
    let allowed_tools: Option<Vec<String>> = msg["allowedTools"].as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });
    let allowed_work_dirs: Option<Vec<String>> = msg["allowedWorkDirs"].as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });
    let core_prompt = msg["corePrompt"].as_str().unwrap_or("");
    let model_id = msg["modelId"].as_str();
    let now = local_iso_string_now();
    match state.agent_manager.create(
        &state.db,
        &state.config,
        &state.group_manager,
        folder,
        name,
        requires_trigger,
        allowed_tools.as_ref(),
        allowed_work_dirs.as_ref(),
        core_prompt,
        model_id,
        &now,
    ) {
        Ok(a) => {
            let wire = to_agent_info(&a);
            send_json(
                sender,
                &serde_json::json!({"type": "agent:registered", "agent": wire}),
            );
            broadcast_to_all_inner(
                clients,
                &serde_json::json!({"type": "agent:registered", "agent": wire}),
            )
            .await;
        }
        Err(e) => {
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": format!("{e}")}),
            );
        }
    }
}

pub(crate) async fn handle_register_binding(
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
    let agent_id = msg["agentId"].as_i64().unwrap_or(0);
    let channel_id = msg["channelId"].as_i64().unwrap_or(0);
    if agent_id == 0 || channel_id == 0 {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "agentId and channelId are required"}),
        );
        return;
    }
    let jid = msg["jid"].as_str();
    let bot_token_override = msg["botTokenOverride"].as_str();
    let max_messages = msg["maxMessages"].as_u64().map(|n| n as u32);
    let now = local_iso_string_now();
    match state.binding_manager.create(
        &state.db,
        jid,
        agent_id,
        channel_id,
        bot_token_override,
        max_messages,
        &now,
    ) {
        Ok(b) => {
            // Fetch with relations for the full response
            if let Ok(Some(br)) = state
                .binding_manager
                .get_with_relations(&state.db, &b.jid.clone().unwrap_or_default())
            {
                let wire = to_binding_with_relations(&br);
                send_json(
                    sender,
                    &serde_json::json!({"type": "binding:registered", "binding": wire}),
                );
                broadcast_to_all_inner(
                    clients,
                    &serde_json::json!({"type": "binding:registered", "binding": wire}),
                )
                .await;
            } else {
                // Fallback: send just the binding without relations
                send_json(
                    sender,
                    &serde_json::json!({"type": "binding:registered", "binding": {"id": b.id}}),
                );
            }
        }
        Err(e) => {
            send_json(
                sender,
                &serde_json::json!({"type": "error", "message": format!("{e}")}),
            );
        }
    }
}

pub(crate) async fn handle_unregister_channel(
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
    let id = msg["id"].as_i64().unwrap_or(0);
    if id == 0 {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "id is required"}),
        );
        return;
    }
    if let Err(e) = state.channel_manager.delete(&state.db, id) {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": format!("{e}")}),
        );
        return;
    }
    send_json(
        sender,
        &serde_json::json!({"type": "channel:unregistered", "id": id}),
    );
    broadcast_to_all_inner(
        clients,
        &serde_json::json!({"type": "channel:unregistered", "id": id}),
    )
    .await;
}

pub(crate) async fn handle_unregister_agent(
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
    let id = msg["id"].as_i64().unwrap_or(0);
    if id == 0 {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "id is required"}),
        );
        return;
    }
    if let Err(e) = state.agent_manager.delete(&state.db, id) {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": format!("{e}")}),
        );
        return;
    }
    send_json(
        sender,
        &serde_json::json!({"type": "agent:unregistered", "id": id}),
    );
    broadcast_to_all_inner(
        clients,
        &serde_json::json!({"type": "agent:unregistered", "id": id}),
    )
    .await;
}

pub(crate) async fn handle_unregister_binding(
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
    let id = msg["id"].as_i64().unwrap_or(0);
    if id == 0 {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "id is required"}),
        );
        return;
    }
    if let Err(e) = state.binding_manager.delete(&state.db, id) {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": format!("{e}")}),
        );
        return;
    }
    send_json(
        sender,
        &serde_json::json!({"type": "binding:unregistered", "id": id}),
    );
    broadcast_to_all_inner(
        clients,
        &serde_json::json!({"type": "binding:unregistered", "id": id}),
    )
    .await;
}

pub(crate) async fn handle_update_channel(
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
    let id = msg["id"].as_i64().unwrap_or(0);
    if id == 0 {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "id is required"}),
        );
        return;
    }
    let name = msg["name"].as_str();
    let credentials = msg["credentials"]
        .as_object()
        .map(|c| serde_json::to_string(&c).unwrap_or_default());
    let now = local_iso_string_now();
    if let Err(e) = state
        .channel_manager
        .update(&state.db, id, name, credentials.as_deref(), &now)
    {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": format!("{e}")}),
        );
        return;
    }
    if let Ok(Some(ch)) = state.channel_manager.get(&state.db, id) {
        let wire = to_channel_info(&ch);
        send_json(
            sender,
            &serde_json::json!({"type": "channel:updated", "channel": wire}),
        );
        broadcast_to_all_inner(
            clients,
            &serde_json::json!({"type": "channel:updated", "channel": wire}),
        )
        .await;
    }
}

pub(crate) async fn handle_update_agent(
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
    let id = msg["id"].as_i64().unwrap_or(0);
    if id == 0 {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "id is required"}),
        );
        return;
    }
    let name = msg["name"].as_str();
    let requires_trigger = msg["requiresTrigger"].as_bool();
    let allowed_tools: Option<Vec<String>> = msg["allowedTools"].as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });
    // allowedWorkDirs: absent = don't touch; null or [] = clear to NULL
    let allowed_work_dirs: Option<Vec<String>> = if msg["allowedWorkDirs"].is_null() {
        Some(vec![])
    } else {
        msg["allowedWorkDirs"].as_array().map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
    };
    let core_prompt = msg["corePrompt"].as_str();
    // modelId: explicit null = clear; absent = don't touch; string = set
    let clear_model_id = msg["modelId"].is_null();
    let model_id = msg["modelId"].as_str();
    let now = local_iso_string_now();
    if let Err(e) = state.agent_manager.update(
        &state.db,
        &state.config,
        id,
        name,
        requires_trigger,
        allowed_tools.as_ref(),
        allowed_work_dirs.as_ref(),
        core_prompt,
        clear_model_id,
        model_id,
        &now,
    ) {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": format!("{e}")}),
        );
        return;
    }
    if let Ok(Some(a)) = state.agent_manager.get(&state.db, id) {
        let wire = to_agent_info(&a);
        send_json(
            sender,
            &serde_json::json!({"type": "agent:updated", "agent": wire}),
        );
        broadcast_to_all_inner(
            clients,
            &serde_json::json!({"type": "agent:updated", "agent": wire}),
        )
        .await;
    }
}

pub(crate) async fn handle_update_binding(
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
    let id = msg["id"].as_i64().unwrap_or(0);
    if id == 0 {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": "id is required"}),
        );
        return;
    }
    let jid = msg["jid"].as_str();
    let bot_token_override = msg["botTokenOverride"].as_str();
    let max_messages = msg["maxMessages"].as_u64().map(|n| n as u32);
    let agent_id = msg["agentId"].as_i64();
    if let Err(e) = state.binding_manager.update(
        &state.db,
        id,
        jid,
        bot_token_override,
        max_messages,
        agent_id,
    ) {
        send_json(
            sender,
            &serde_json::json!({"type": "error", "message": format!("{e}")}),
        );
        return;
    }
    if let Ok(Some(b)) = state.binding_manager.get(&state.db, id) {
        // Fetch with relations for full info
        if let Ok(Some(br)) = state
            .binding_manager
            .get_with_relations(&state.db, &b.jid.clone().unwrap_or_default())
        {
            let wire = to_binding_with_relations(&br);
            send_json(
                sender,
                &serde_json::json!({"type": "binding:updated", "binding": wire}),
            );
            broadcast_to_all_inner(
                clients,
                &serde_json::json!({"type": "binding:updated", "binding": wire}),
            )
            .await;
        }
    }
}
