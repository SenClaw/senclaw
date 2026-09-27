//! HTTP endpoints that let the mobile app resolve pending agent interactions
//! (tool-permission requests and ask-question batches) over the relay RPC
//! tunnel. The web UI does this over the WS gateway (`permission:response` /
//! `question:response`); these routes expose the same capability via HTTP so
//! the relay bridge can reach it.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Json,
};
use serde::Deserialize;

use super::core::{AppError, UiState};

fn agent_api(s: &Arc<UiState>) -> Result<&Arc<dyn super::core::UiApi>, AppError> {
    s.agent_api.as_ref().ok_or_else(|| {
        AppError(
            StatusCode::SERVICE_UNAVAILABLE,
            "agent API unavailable".into(),
        )
    })
}

#[derive(Deserialize)]
pub(crate) struct PermissionRespondBody {
    #[serde(rename = "requestId")]
    request_id: String,
    #[serde(rename = "optionKey")]
    option_key: String,
}

pub(crate) async fn chat_permission_respond(
    State(s): State<Arc<UiState>>,
    Json(b): Json<PermissionRespondBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if b.request_id.is_empty() || b.option_key.is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "requestId and optionKey required".into(),
        ));
    }
    agent_api(&s)?.resolve_permission(&b.request_id, &b.option_key);
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(crate) struct QuestionRespondBody {
    #[serde(rename = "requestId")]
    request_id: String,
    /// `{ "<questionIndex>": optionIndex | [optionIndex, …] }` (−1 = "Other").
    answers: serde_json::Value,
    #[serde(rename = "otherTexts", default)]
    other_texts: Option<serde_json::Value>,
}

pub(crate) async fn chat_question_respond(
    State(s): State<Arc<UiState>>,
    Json(b): Json<QuestionRespondBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if b.request_id.is_empty() || b.answers.is_null() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "requestId and answers required".into(),
        ));
    }
    agent_api(&s)?.resolve_ask_question(&b.request_id, &b.answers, b.other_texts.as_ref());
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(crate) struct FormRespondBody {
    #[serde(rename = "requestId")]
    request_id: String,
    /// Structured values keyed by field `key`.
    #[serde(default)]
    values: serde_json::Value,
    /// Missing counts as submitted; explicit `false` means the user skipped.
    #[serde(default = "default_submitted")]
    submitted: bool,
}

fn default_submitted() -> bool {
    true
}

pub(crate) async fn chat_form_respond(
    State(s): State<Arc<UiState>>,
    Json(b): Json<FormRespondBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if b.request_id.is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "requestId required".into(),
        ));
    }
    let values = if b.values.is_null() {
        serde_json::json!({})
    } else {
        b.values
    };
    agent_api(&s)?.resolve_form(&b.request_id, &values, b.submitted);
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(crate) struct PlanRespondBody {
    #[serde(rename = "groupJid")]
    group_jid: String,
    #[serde(rename = "agentId", default = "default_agent_id")]
    agent_id: String,
    /// `startEditing` | `clearContextAndStart` | `cancelled`.
    selected: String,
}

fn default_agent_id() -> String {
    "main".to_string()
}

#[derive(Deserialize)]
pub(crate) struct ChatHistoryQuery {
    jid: String,
    /// Epoch-millis cursor — only messages strictly newer are returned.
    /// Optional for backward compatibility (absent = full history, capped).
    #[serde(default)]
    after_ts: Option<i64>,
    #[serde(default)]
    limit: Option<u32>,
}

/// GET /api/chat/history?jid=…&after_ts=…&limit=…
///
/// Incremental history for relay clients: returns group messages strictly
/// newer than `after_ts` (epoch ms), oldest → newest, each row carrying a
/// daemon-parsed numeric `ts` so the client can persist a stable sync cursor
/// without re-parsing the mixed-format timestamp strings.
pub(crate) async fn chat_history(
    State(s): State<Arc<UiState>>,
    Query(q): Query<ChatHistoryQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    if q.jid.is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "jid required".into()));
    }
    let db =
        s.db.as_ref()
            .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "db unavailable".into()))?;
    let limit = q.limit.unwrap_or(200).min(1000);
    let after_ms = q.after_ts.unwrap_or(-1);
    let messages = db
        .get_group_messages_after_ms(&q.jid, after_ms, limit)
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let rows: Vec<serde_json::Value> = messages
        .iter()
        .map(|(m, ms)| {
            // Keep mobile protocol explicit: only "user" or "agent" (same
            // mapping as the HISTORY_RESP relay control frames).
            let role = if m.is_bot_reply { "agent" } else { "user" };
            serde_json::json!({
                "id":         m.message_id,
                "sender":     m.sender_name,
                "content":    m.content,
                "timestamp":  m.timestamp,
                "ts":         ms,
                "isFromMe":   m.is_from_me,
                "isBotReply": m.is_bot_reply,
                "role":       role,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "jid": q.jid, "messages": rows })))
}

/// GET /api/chat/states
///
/// Authoritative per-group agent state snapshot (`jid → "processing" | "idle"
/// | …`) from the WS gateway's `last_known_states`. Relay clients call this on
/// (re)connect to reconcile a typing indicator whose `agent:state` events were
/// lost while the relay socket was down.
pub(crate) async fn chat_states(
    State(s): State<Arc<UiState>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let states = match s.agent_states.as_ref() {
        Some(m) => m.lock().await.clone(),
        None => Default::default(),
    };
    Ok(Json(serde_json::json!({ "states": states })))
}

pub(crate) async fn chat_plan_respond(
    State(s): State<Arc<UiState>>,
    Json(b): Json<PlanRespondBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if b.group_jid.is_empty() || b.selected.is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "groupJid and selected required".into(),
        ));
    }
    agent_api(&s)?.resolve_plan_exit(&b.group_jid, &b.agent_id, &b.selected);
    Ok(Json(serde_json::json!({ "ok": true })))
}
