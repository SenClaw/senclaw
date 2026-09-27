//! `/api/pairings` — the chats asking to be let in, and the button that lets them.
//!
//! This is the human half of [`crate::gateway::pairing`]. The router side can
//! only ever *record* a request; every route here is behind the daemon's
//! authenticated API, which is what makes approval a decision by a person
//! rather than a side effect of somebody messaging the bot first.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;

use super::core::{AppError, UiState};
use crate::gateway::pairing;

fn db(s: &Arc<UiState>) -> Result<Arc<crate::db::Db>, AppError> {
    s.db.clone()
        .ok_or_else(|| AppError(StatusCode::SERVICE_UNAVAILABLE, "db_unset".into()))
}

#[derive(serde::Deserialize, Default)]
pub(crate) struct PairingQuery {
    /// `?pending=false` includes resolved rows. Default is pending-only: the
    /// list is a to-do, and the audit trail is the exception.
    #[serde(default)]
    all: Option<bool>,
}

/// GET /api/pairings — who is knocking.
///
/// Each row carries the sender's name and jid, not just the chat id, because
/// approving is a judgement about a person; a bare `tg:…:user:812…` gives the
/// approver nothing to judge with. Expired rows are marked rather than hidden —
/// disappearing silently reads as "the bot never got my message".
pub(crate) async fn pairings_list(
    State(s): State<Arc<UiState>>,
    Query(q): Query<PairingQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let d = db(&s)?;
    let rows = d
        .list_pairings(!q.all.unwrap_or(false))
        .map_err(|e| AppError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|p| {
            let expired = crate::db::pairings::is_expired(&p.expires_at);
            let channel_name = d
                .get_channel(p.channel_id)
                .ok()
                .flatten()
                .map(|c| c.name)
                .unwrap_or_default();
            serde_json::json!({
                "id": p.id,
                "channelId": p.channel_id,
                "channelName": channel_name,
                "chatJid": p.chat_jid,
                "chatType": p.chat_type,
                "senderJid": p.sender_jid,
                "senderName": p.sender_name,
                "code": p.code,
                "status": if expired && p.status == "pending" { "expired" } else { &p.status },
                "expired": expired,
                "createdAt": p.created_at,
                "expiresAt": p.expires_at,
                "resolvedAt": p.resolved_at,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "pairings": items })))
}

/// Shared tail of both approve routes: tell the chat it got in, and tell every
/// open Settings page the row is gone.
async fn announce_approval(s: &Arc<UiState>, a: &pairing::Approved) {
    if let Some(api) = s.agent_api.as_ref() {
        api.send_channel_message(
            &a.pairing.chat_jid,
            "✅ Đã được duyệt. Bạn có thể bắt đầu trò chuyện.",
            a.pairing.bot_token.as_deref(),
        );
        api.broadcast_event(serde_json::json!({
            "type": "pairing:resolved",
            "id": a.pairing.id,
            "status": "approved",
        }));
    }
}

/// POST /api/pairings/:id/approve — let this chat in.
///
/// The refusal cases (expired, already handled, channel has no agent) come back
/// as a 400 with their own wording: they are three different problems with
/// three different fixes, and a generic "approve failed" just makes the person
/// click again.
pub(crate) async fn pairing_approve(
    State(s): State<Arc<UiState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, AppError> {
    let approved = pairing::approve(&*db(&s)?, id)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    announce_approval(&s, &approved).await;
    Ok(Json(serde_json::json!({
        "success": true,
        "chatJid": approved.pairing.chat_jid,
        "agentFolder": approved.agent_folder,
        "filledPending": approved.filled_pending,
    })))
}

#[derive(serde::Deserialize)]
pub(crate) struct ApproveCodeBody {
    code: String,
}

/// POST /api/pairings/approve-code — approve by the code a person typed.
///
/// The shape the Settings text box and the CLI both want: the approver is
/// reading an 8-character code off a phone, not a row id.
pub(crate) async fn pairing_approve_code(
    State(s): State<Arc<UiState>>,
    Json(body): Json<ApproveCodeBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let approved = pairing::approve_by_code(&*db(&s)?, &body.code)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    announce_approval(&s, &approved).await;
    Ok(Json(serde_json::json!({
        "success": true,
        "chatJid": approved.pairing.chat_jid,
        "agentFolder": approved.agent_folder,
        "filledPending": approved.filled_pending,
    })))
}

/// POST /api/pairings/:id/reject — turn this request away.
///
/// Deliberately silent toward the chat: an unknown party that just got told
/// "no" learns that a human is reading, which is more than a stranger needs.
/// They stay unbound, which is the default anyway.
pub(crate) async fn pairing_reject(
    State(s): State<Arc<UiState>>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, AppError> {
    let p = pairing::reject(&*db(&s)?, id)
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e.to_string()))?;
    if let Some(api) = s.agent_api.as_ref() {
        api.broadcast_event(serde_json::json!({
            "type": "pairing:resolved",
            "id": p.id,
            "status": "rejected",
        }));
    }
    Ok(Json(serde_json::json!({ "success": true, "id": p.id })))
}
