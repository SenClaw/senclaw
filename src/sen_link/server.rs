//! Inbound A2A: the Agent Card, the JSON-RPC endpoint peers call, and the
//! pairing handshake. All three answer 404 while sen-link is disabled.
//! These paths live outside `/api`, so the daemon's API token gate does not
//! apply; `/a2a` authenticates the peer token itself and `/a2a/pair` requires
//! a live one-time invite code.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use super::a2a::{self, TaskState};
use super::store::{random_token, sha256_hex, Peer};
use super::{tasks, LinkState};
use crate::kanban::api::now;

fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

pub async fn agent_card(State(st): State<Arc<LinkState>>) -> Response {
    let s = st.settings();
    if !s.enabled {
        return not_found();
    }
    Json(a2a::agent_card(&a2a::AgentCardInfo {
        name: &s.name,
        description: &s.description,
        url: s.public_url.as_deref(),
        version: env!("CARGO_PKG_VERSION"),
    }))
    .into_response()
}

/// Body of `POST /a2a/pair`.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairRequest {
    pub code: String,
    pub name: String,
    /// The caller's base URL, so we can send it tasks too.
    #[serde(default)]
    pub url: Option<String>,
    /// Token we present when calling the caller.
    #[serde(default)]
    pub token: Option<String>,
}

/// Redeem an invite and record the caller as a peer. `channel` is the hub
/// channel the request arrived on (relay pairing): only that channel's invite
/// matches, and the new peer keeps the channel. Shared by HTTP and the relay.
pub fn pair_with(st: &LinkState, req: PairRequest, channel: Option<&str>) -> Result<Value, (StatusCode, String)> {
    let s = st.settings();
    let invite = match st.store.redeem_invite(&req.code, now(), channel) {
        Ok(Some(i)) => i,
        Ok(None) => return Err((StatusCode::FORBIDDEN, "invalid or expired invite code".into())),
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    };
    let theirs = random_token();
    let peer = Peer {
        id: uuid::Uuid::new_v4().to_string(),
        name: req.name.trim().chars().take(80).collect(),
        url: req.url.filter(|u| u.starts_with("http://") || u.starts_with("https://")),
        outbound_token: req.token.filter(|t| !t.is_empty()),
        inbound_token_hash: sha256_hex(&theirs),
        auto_run: false,
        assignee: None,
        relay: invite.relay,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let id = peer.id.clone();
    st.store.upsert(peer).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    tracing::info!(peer = %id, relay = channel.is_some(), "[sen-link] paired by invite");
    Ok(json!({ "peerId": id, "name": s.name, "token": theirs, "url": s.public_url }))
}

pub async fn pair(State(st): State<Arc<LinkState>>, Json(req): Json<PairRequest>) -> Response {
    if !st.settings().enabled {
        return not_found();
    }
    match pair_with(&st, req, None) {
        Ok(v) => Json(v).into_response(),
        Err((code, e)) => (code, Json(json!({ "error": e }))).into_response(),
    }
}

/// Answer one JSON-RPC request body for `peer`. Shared by HTTP and the relay.
pub fn rpc_reply(st: &LinkState, peer: &Peer, req: &Value) -> Value {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = req["method"].as_str() else {
        return a2a::rpc_error(&id, a2a::INVALID_REQUEST, "missing method");
    };
    match handle(st, peer, a2a::canonical_method(method), &req["params"]) {
        Ok(result) => a2a::rpc_result(&id, result),
        Err((code, msg)) => a2a::rpc_error(&id, code, &msg),
    }
}

/// `POST /a2a` — JSON-RPC 2.0, A2A v1.0 methods.
pub async fn rpc(State(st): State<Arc<LinkState>>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if !st.settings().enabled {
        return not_found();
    }
    let peer = match crate::gateway::ui_server::auth::token_from_headers(&headers).and_then(|t| st.store.by_inbound_token(&t)) {
        Some(p) => p,
        None => return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized" }))).into_response(),
    };
    match serde_json::from_slice::<Value>(&body) {
        Ok(req) => Json(rpc_reply(&st, &peer, &req)).into_response(),
        Err(e) => Json(a2a::rpc_error(&Value::Null, a2a::PARSE_ERROR, &e.to_string())).into_response(),
    }
}

type RpcResult = Result<Value, (i64, String)>;

fn internal(e: impl std::fmt::Display) -> (i64, String) {
    (a2a::INTERNAL_ERROR, e.to_string())
}

/// Dispatch one call for `peer`. Pure over the kanban DB, so it is testable.
pub fn handle(st: &LinkState, peer: &Peer, method: &str, params: &Value) -> RpcResult {
    let db = &st.kanban;
    let task_id = || -> Result<String, (i64, String)> {
        params["id"]
            .as_str()
            .map(str::to_string)
            .ok_or((a2a::INVALID_PARAMS, "missing params.id".to_string()))
    };
    match method {
        "SendMessage" => {
            let msg = &params["message"];
            let text = a2a::message_text(msg);
            if text.trim().is_empty() {
                return Err((a2a::INVALID_PARAMS, "message has no text part".into()));
            }
            // A follow-up on an existing task becomes a comment; a blocked
            // card goes back to the queue (Ready if the peer may auto-run).
            if let Some(tid) = msg["taskId"].as_str() {
                let card = tasks::owned_card(db, &peer.id, tid).ok_or((a2a::TASK_NOT_FOUND, format!("task {tid} not found")))?;
                db.add_comment(card.id, &peer.name, &text, "comment", now()).map_err(internal)?;
                let role = db.card_role(card.id).map_err(internal)?.unwrap_or_default();
                let comments = db.comments_of_card(card.id).map_err(internal)?;
                // Only a task waiting for input goes back to the queue; a
                // canceled one stays canceled.
                if tasks::state_for(&role, &comments) == TaskState::InputRequired {
                    let role = if peer.auto_run { "ready" } else { "triage" };
                    if let Some(col) = db.column_by_role(card.board_id, role).map_err(internal)? {
                        db.move_card(card.id, col, 0, now()).map_err(internal)?;
                    }
                }
                let card = db.card_row(card.id).map_err(internal)?.ok_or((a2a::TASK_NOT_FOUND, "task vanished".to_string()))?;
                return Ok(json!({ "task": tasks::task_for(db, &card).map_err(internal)? }));
            }
            let s = st.settings();
            let board = tasks::ensure_board(db, &s.board_title, s.workspace_dir.as_deref(), now()).map_err(internal)?;
            let meta = &msg["metadata"];
            // Who works on it is the operator's choice (per peer, then the
            // default) — never the remote peer's.
            let assignee = peer.assignee.clone().or_else(|| s.default_assignee.clone());
            let card = tasks::create(
                db,
                board,
                &tasks::NewTask {
                    peer_id: &peer.id,
                    peer_name: &peer.name,
                    text: &text,
                    assignee: assignee.as_deref(),
                    auto_run: peer.auto_run,
                    worktree: meta["worktree"].as_bool().unwrap_or(false),
                },
                now(),
            )
            .map_err(internal)?;
            tracing::info!(peer = %peer.id, card = card.id, "[sen-link] task received");
            Ok(json!({ "task": tasks::task_for(db, &card).map_err(internal)? }))
        }
        "GetTask" => {
            let tid = task_id()?;
            let card = tasks::owned_card(db, &peer.id, &tid).ok_or((a2a::TASK_NOT_FOUND, format!("task {tid} not found")))?;
            tasks::task_for(db, &card).map_err(internal)
        }
        "ListTasks" => {
            let s = st.settings();
            let Some(board) = db.list_boards().map_err(internal)?.into_iter().find(|b| b.title == s.board_title) else {
                return Ok(json!({ "tasks": [], "nextPageToken": "", "pageSize": 0, "totalSize": 0 }));
            };
            let cards = db.list_cards(board.id, None, None, Some(&tasks::tenant_of(&peer.id))).map_err(internal)?;
            let list: Vec<Value> = cards.iter().filter_map(|c| tasks::task_for(db, c).ok()).collect();
            let n = list.len();
            Ok(json!({ "tasks": list, "nextPageToken": "", "pageSize": n, "totalSize": n }))
        }
        "CancelTask" => {
            let tid = task_id()?;
            let card = tasks::owned_card(db, &peer.id, &tid).ok_or((a2a::TASK_NOT_FOUND, format!("task {tid} not found")))?;
            let role = db.card_role(card.id).map_err(internal)?.unwrap_or_default();
            let comments = db.comments_of_card(card.id).map_err(internal)?;
            if tasks::state_for(&role, &comments).is_terminal() {
                return Err((a2a::TASK_NOT_CANCELABLE, format!("task {tid} already finished")));
            }
            if role == "in_progress" {
                // A running worker cannot be stopped from here; say so rather than pretend.
                return Err((a2a::TASK_NOT_CANCELABLE, format!("task {tid} is running")));
            }
            db.add_comment(card.id, &peer.name, "canceled by the requesting peer", "cancel", now()).map_err(internal)?;
            if let Some(col) = db.column_by_role(card.board_id, "blocked").map_err(internal)? {
                db.move_card(card.id, col, 0, now()).map_err(internal)?;
            }
            let card = db.card_row(card.id).map_err(internal)?.ok_or((a2a::TASK_NOT_FOUND, "task vanished".to_string()))?;
            tasks::task_for(db, &card).map_err(internal)
        }
        other => Err((a2a::METHOD_NOT_FOUND, format!("method `{other}` is not supported"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sen_link::store::PeerStore;

    fn state() -> (tempfile::TempDir, LinkState) {
        let t = tempfile::tempdir().unwrap();
        let cfg_path = t.path().join("config.json");
        std::fs::write(&cfg_path, r#"{"senLink": {"enabled": true}}"#).unwrap();
        let st = LinkState::new_at(
            cfg_path,
            Arc::new(crate::kanban::db::Db::open(&t.path().join("kanban.db")).unwrap()),
            PeerStore::new(t.path().join("peers.json")),
        );
        (t, st)
    }

    fn peer(id: &str, auto_run: bool) -> Peer {
        Peer {
            id: id.into(),
            name: format!("peer-{id}"),
            url: None,
            outbound_token: None,
            inbound_token_hash: String::new(),
            auto_run,
            assignee: Some("claude-code".into()),
            relay: None,
            created_at: String::new(),
        }
    }

    #[test]
    fn a_task_round_trips_and_stays_private_to_its_peer() {
        let (_t, st) = state();
        let a = peer("a", false);
        let msg = json!({ "message": a2a::user_message("Write the release notes", None, json!({})) });
        let r = handle(&st, &a, "SendMessage", &msg).unwrap();
        let tid = r["task"]["id"].as_str().unwrap().to_string();
        assert_eq!(r["task"]["status"]["state"], "TASK_STATE_SUBMITTED");
        assert_eq!(r["task"]["metadata"]["assignee"], "claude-code");

        let got = handle(&st, &a, "GetTask", &json!({ "id": tid })).unwrap();
        assert_eq!(got["id"], tid);
        assert_eq!(handle(&st, &a, "ListTasks", &json!({})).unwrap()["totalSize"], 1);

        let b = peer("b", false);
        assert_eq!(handle(&st, &b, "GetTask", &json!({ "id": tid })).unwrap_err().0, a2a::TASK_NOT_FOUND);
        assert_eq!(handle(&st, &b, "ListTasks", &json!({})).unwrap()["totalSize"], 0);

        let c = handle(&st, &a, "CancelTask", &json!({ "id": tid })).unwrap();
        assert_eq!(c["status"]["state"], "TASK_STATE_CANCELED");
        assert_eq!(handle(&st, &a, "CancelTask", &json!({ "id": tid })).unwrap_err().0, a2a::TASK_NOT_CANCELABLE);
        // A follow-up does not revive a canceled task.
        let f = json!({ "message": a2a::user_message("please do it anyway", Some(&tid), json!({})) });
        let after = handle(&st, &a, "SendMessage", &f).unwrap();
        assert_eq!(after["task"]["status"]["state"], "TASK_STATE_CANCELED");
    }

    #[test]
    fn the_peer_cannot_pick_the_assignee() {
        let (_t, st) = state();
        let mut a = peer("a", true);
        a.assignee = None;
        let msg = json!({ "message": a2a::user_message("x", None, json!({ "assignee": "claude-code" })) });
        let r = handle(&st, &a, "SendMessage", &msg).unwrap();
        assert!(r["task"]["metadata"]["assignee"].is_null());
    }

    #[test]
    fn a_follow_up_requeues_a_blocked_task() {
        let (_t, st) = state();
        let a = peer("a", true);
        let r = handle(&st, &a, "SendMessage", &json!({ "message": a2a::user_message("Do X", None, json!({})) })).unwrap();
        let tid: i64 = r["task"]["id"].as_str().unwrap().parse().unwrap();
        let card = st.kanban.card_row(tid).unwrap().unwrap();
        let blocked = st.kanban.column_by_role(card.board_id, "blocked").unwrap().unwrap();
        st.kanban.move_card(tid, blocked, 0, 1).unwrap();
        st.kanban.add_comment(tid, "dispatcher", "need the bucket name", "block", 1).unwrap();
        let before = handle(&st, &a, "GetTask", &json!({ "id": tid.to_string() })).unwrap();
        assert_eq!(before["status"]["state"], "TASK_STATE_INPUT_REQUIRED");

        let f = json!({ "message": a2a::user_message("bucket is sen-assets", Some(&tid.to_string()), json!({})) });
        handle(&st, &a, "SendMessage", &f).unwrap();
        assert_eq!(st.kanban.card_role(tid).unwrap().as_deref(), Some("ready"));
    }

    #[test]
    fn unknown_methods_and_empty_messages_are_refused() {
        let (_t, st) = state();
        let a = peer("a", false);
        assert_eq!(handle(&st, &a, "SubscribeToTask", &json!({})).unwrap_err().0, a2a::METHOD_NOT_FOUND);
        let empty = json!({ "message": { "parts": [] } });
        assert_eq!(handle(&st, &a, "SendMessage", &empty).unwrap_err().0, a2a::INVALID_PARAMS);
    }
}
