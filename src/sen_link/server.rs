//! Inbound A2A: the Agent Card, the JSON-RPC endpoint peers call, and the
//! pairing handshake. Only the LAN listener serves them (`listener.rs`), and
//! all three answer 404 while SLink is disabled. These paths live outside
//! `/api`, so the daemon's API token gate does not apply; `/a2a`
//! authenticates the peer token itself and `/a2a/pair` requires a live
//! one-time invite code.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use super::a2a::{self, TaskState};
use super::lan::{self, Endpoint, Host};
use super::store::{clean_daemon_id, random_token, sha256_hex, Peer};
use super::{tasks, LinkState};
use crate::kanban::api::now;

fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// The Agent Card. A daemon that is not discoverable answers like one that is
/// off, so a probe or a scan finds nothing; pairing by typed address still works.
pub async fn agent_card(State(st): State<Arc<LinkState>>, headers: HeaderMap) -> Response {
    let s = st.settings();
    if !s.enabled || !s.discoverable {
        return not_found();
    }
    // The address the caller used is the one that works for the caller.
    let url = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| lan::parse_endpoint(h, s.listen_port).ok())
        .map(|e| e.url(""));
    Json(a2a::agent_card(&a2a::AgentCardInfo {
        name: &s.name,
        description: &s.description,
        url: url.as_deref(),
        version: env!("CARGO_PKG_VERSION"),
        daemon_id: st.store.daemon_id(),
        pairable: st.store.has_live_invite(now()),
    }))
    .into_response()
}

/// Body of `POST /a2a/pair`.
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairRequest {
    pub code: String,
    pub name: String,
    /// Port of the caller's own SLink listener. With the address this call
    /// came from it tells us where to send the caller tasks.
    #[serde(default)]
    pub port: Option<u16>,
    /// The caller's id (`PeerStore::daemon_id`).
    #[serde(default)]
    pub daemon_id: Option<String>,
    /// Token we present when calling the caller.
    #[serde(default)]
    pub token: Option<String>,
}

fn internal_err(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Redeem an invite and record the caller as a peer. `source` is the address
/// the request came from; it is the peer's address unless it is paired already.
pub fn pair_with(st: &LinkState, req: PairRequest, source: IpAddr) -> Result<Value, (StatusCode, String)> {
    let s = st.settings();
    match st.store.redeem_invite(&req.code, now()) {
        Ok(true) => {}
        Ok(false) => return Err((StatusCode::FORBIDDEN, "invalid or expired invite code".into())),
        Err(e) => return Err(internal_err(e)),
    }
    let theirs = random_token();
    let daemon_id = req.daemon_id.as_deref().and_then(clean_daemon_id);
    // The same daemon pairing again replaces its record (and keeps the id its
    // tasks are filed under, and what we let it do), instead of adding a twin.
    let known = daemon_id.as_deref().and_then(|d| st.store.by_daemon_id(d));
    let name: String = req.name.trim().chars().take(80).collect();
    let peer = Peer {
        id: known.as_ref().map_or_else(|| uuid::Uuid::new_v4().to_string(), |k| k.id.clone()),
        name: if name.is_empty() { "peer".into() } else { name },
        addr: req
            .port
            .filter(|p| *p != 0)
            .map(|port| Endpoint { host: Host::Ip(source.to_canonical()), port }.to_string()),
        outbound_token: req.token.filter(|t| !t.is_empty()),
        inbound_token_hash: sha256_hex(&theirs),
        auto_run: known.as_ref().is_some_and(|k| k.auto_run),
        assignee: known.and_then(|k| k.assignee),
        daemon_id,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let id = peer.id.clone();
    st.store.upsert(peer).map_err(internal_err)?;
    tracing::info!(peer = %id, "[slink] paired by invite");
    Ok(json!({ "peerId": id, "name": s.name, "token": theirs, "daemonId": st.store.daemon_id() }))
}

pub async fn pair(State(st): State<Arc<LinkState>>, ConnectInfo(from): ConnectInfo<SocketAddr>, Json(req): Json<PairRequest>) -> Response {
    if !st.settings().enabled {
        return not_found();
    }
    match pair_with(&st, req, from.ip()) {
        Ok(v) => Json(v).into_response(),
        Err((code, e)) => (code, Json(json!({ "error": e }))).into_response(),
    }
}

/// Answer one JSON-RPC request body for `peer`.
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
            let board = tasks::ensure_board(db, s.effective_board_title(), s.workspace_dir.as_deref(), now()).map_err(internal)?;
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
            let Some((board, _)) = tasks::find_board(db, s.effective_board_title()).map_err(internal)? else {
                return Ok(json!({ "tasks": [], "nextPageToken": "", "pageSize": 0, "totalSize": 0 }));
            };
            let cards = db.list_cards(board, None, None, Some(&tasks::tenant_of(&peer.id))).map_err(internal)?;
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
            addr: None,
            outbound_token: None,
            inbound_token_hash: String::new(),
            auto_run,
            assignee: Some("claude-code".into()),
            daemon_id: None,
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

    fn req(code: &str, port: Option<u16>, daemon_id: Option<&str>) -> PairRequest {
        PairRequest { code: code.into(), name: "  Mac mini  ".into(), port, daemon_id: daemon_id.map(str::to_string), token: Some("slk_theirs".into()) }
    }

    #[test]
    fn pairing_records_the_caller_at_the_address_it_came_from() {
        let (_t, st) = state();
        let (code, _) = st.store.create_invite(now()).unwrap();
        let from: IpAddr = "192.168.1.9".parse().unwrap();
        let reply = pair_with(&st, req(&code, Some(18791), Some("6f1c-0a")), from).unwrap();

        let p = st.store.get(reply["peerId"].as_str().unwrap()).unwrap();
        assert_eq!(p.addr.as_deref(), Some("192.168.1.9:18791"));
        assert_eq!(p.name, "Mac mini");
        assert_eq!(p.daemon_id.as_deref(), Some("6f1c-0a"));
        assert_eq!(p.outbound_token.as_deref(), Some("slk_theirs"));
        assert!(!p.auto_run, "a new peer waits in Triage until a person trusts it");
        assert_eq!(reply["daemonId"], st.store.daemon_id());
        // The token we handed back is the one that now opens /a2a for that peer.
        assert_eq!(st.store.by_inbound_token(reply["token"].as_str().unwrap()).unwrap().id, p.id);
        // One code, one pairing.
        assert_eq!(pair_with(&st, req(&code, Some(18791), None), from).unwrap_err().0, StatusCode::FORBIDDEN);
        assert_eq!(st.store.peers().len(), 1);
    }

    #[test]
    fn the_source_address_is_read_as_the_plain_ip_and_a_missing_port_leaves_no_address() {
        let (_t, st) = state();
        let (code, _) = st.store.create_invite(now()).unwrap();
        let mapped: IpAddr = "::ffff:192.168.1.9".parse().unwrap();
        let r = pair_with(&st, req(&code, Some(7000), None), mapped).unwrap();
        assert_eq!(st.store.get(r["peerId"].as_str().unwrap()).unwrap().addr.as_deref(), Some("192.168.1.9:7000"));

        let (code, _) = st.store.create_invite(now()).unwrap();
        let r = pair_with(&st, req(&code, None, Some("<b>")), mapped).unwrap();
        let p = st.store.get(r["peerId"].as_str().unwrap()).unwrap();
        assert_eq!((p.addr, p.daemon_id), (None, None), "no port: nowhere to send; unprintable id: dropped");
        let (code, _) = st.store.create_invite(now()).unwrap();
        assert!(pair_with(&st, req(&code, Some(0), None), mapped).unwrap()["peerId"].is_string());
    }

    #[test]
    fn a_wrong_or_stale_code_pairs_nobody() {
        let (_t, st) = state();
        let from: IpAddr = "10.0.0.2".parse().unwrap();
        assert_eq!(pair_with(&st, req("NOPE2345", Some(1), None), from).unwrap_err().0, StatusCode::FORBIDDEN);
        assert!(st.store.peers().is_empty());
    }

    #[test]
    fn pairing_again_replaces_the_record_and_keeps_what_the_operator_allowed() {
        let (_t, st) = state();
        let from: IpAddr = "192.168.1.9".parse().unwrap();
        let (code, _) = st.store.create_invite(now()).unwrap();
        let first = pair_with(&st, req(&code, Some(18791), Some("6f1c-0a")), from).unwrap();
        let id = first["peerId"].as_str().unwrap().to_string();
        st.store.update(&id, |p| {
            p.auto_run = true;
            p.assignee = Some("claude-code".into());
        })
        .unwrap();

        let new_ip: IpAddr = "192.168.1.77".parse().unwrap();
        let (code, _) = st.store.create_invite(now()).unwrap();
        let again = pair_with(&st, req(&code, Some(18791), Some("6f1c-0a")), new_ip).unwrap();
        assert_eq!(again["peerId"], first["peerId"]);
        assert_ne!(again["token"], first["token"]);
        let peers = st.store.peers();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].addr.as_deref(), Some("192.168.1.77:18791"));
        assert!(peers[0].auto_run && peers[0].assignee.as_deref() == Some("claude-code"));
        assert!(st.store.by_inbound_token(first["token"].as_str().unwrap()).is_none(), "the old token stops working");
    }

    /// An install from before the rename: board and setting both say `sen-link`.
    #[test]
    fn an_old_board_keeps_working_and_is_renamed_by_the_next_task() {
        let t = tempfile::tempdir().unwrap();
        let cfg = t.path().join("config.json");
        std::fs::write(&cfg, r#"{"senLink": {"enabled": true, "boardTitle": "sen-link"}}"#).unwrap();
        let st = LinkState::new_at(
            cfg,
            Arc::new(crate::kanban::db::Db::open(&t.path().join("kanban.db")).unwrap()),
            PeerStore::new(t.path().join("peers.json")),
        );
        let a = peer("a", false);
        let old = tasks::ensure_board(&st.kanban, "sen-link", None, 1).unwrap();
        let card = tasks::create(&st.kanban, old, &tasks::NewTask { peer_id: "a", peer_name: "A", text: "Old task", assignee: None, auto_run: false, worktree: false }, 2).unwrap();

        // The old task is still listed, before anything renames the board.
        let listed = handle(&st, &a, "ListTasks", &json!({})).unwrap();
        assert_eq!((listed["totalSize"].as_u64(), listed["tasks"][0]["id"].as_str()), (Some(1), Some(card.id.to_string().as_str())));

        // A new task lands on the same board, which now carries the new name.
        let r = handle(&st, &a, "SendMessage", &json!({ "message": a2a::user_message("New task", None, json!({})) })).unwrap();
        assert_eq!(r["task"]["status"]["state"], "TASK_STATE_SUBMITTED");
        let boards = st.kanban.list_boards().unwrap();
        assert_eq!(boards.iter().map(|b| (b.title.as_str(), b.card_count)).collect::<Vec<_>>(), vec![("SLink", 2)]);
        assert_eq!(handle(&st, &a, "ListTasks", &json!({})).unwrap()["totalSize"], 2);
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
