//! Inbound A2A: the Agent Card, the sealed JSON-RPC endpoint peers call, and
//! the two steps of the pairing handshake. Only the LAN listener serves them
//! (`listener.rs`), and all of them answer 404 while SLink is disabled. These
//! paths live outside `/api`, so the daemon's API token gate does not apply:
//! `/a2a` opens only what is sealed under a pair key (`seal.rs`) and
//! `/a2a/pair/*` only completes for whoever knows the live invite's code
//! (`pairing.rs`).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use axum::Json;
use serde_json::{json, Value};

use super::a2a::{self, TaskState};
use super::lan::{self, Endpoint, Host};
use super::store::{clean_daemon_id, random_token, sha256_hex, Peer};
use super::{pairing, seal, tasks, LinkState};
use crate::kanban::api::now;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

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

/// The header that names the sender of a sealed request: its daemon id, which
/// is no secret. The receiver uses it to find the pair key to try.
pub const FROM_HEADER: &str = "x-slink-from";

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized" }))).into_response()
}

fn error_body(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({ "error": msg.into() }))).into_response()
}

fn internal_err(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

const NO_INVITE: &str = "invalid or expired invite code";

/// `POST /a2a/pair/begin` — the first message of the pairing handshake. There
/// is one live invite at a time, so there is one password it can be for.
pub async fn pair_begin(State(st): State<Arc<LinkState>>, Json(b): Json<pairing::Begin>) -> Response {
    if !st.settings().enabled {
        return not_found();
    }
    let Some(invite) = st.store.live_invite(now()) else {
        return error_body(StatusCode::FORBIDDEN, NO_INVITE);
    };
    let Ok(msg_a) = B64.decode(&b.msg) else {
        return error_body(StatusCode::BAD_REQUEST, "the handshake message is not base64");
    };
    let (msg_b, keys) = match pairing::respond(&invite.code, &msg_a) {
        Ok(x) => x,
        Err(e) => return error_body(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let Some(sid) = st.sessions.insert(keys, invite.id) else {
        return error_body(StatusCode::TOO_MANY_REQUESTS, "too many pairings are in flight; try again in a minute");
    };
    Json(pairing::BeginReply { sid, msg: B64.encode(msg_b) }).into_response()
}

/// `POST /a2a/pair/finish` — the second message: A's offer, sealed under the
/// key the code made. Opening it is the proof that A knew the code.
pub async fn pair_finish(State(st): State<Arc<LinkState>>, ConnectInfo(from): ConnectInfo<SocketAddr>, Json(f): Json<pairing::Finish>) -> Response {
    if !st.settings().enabled {
        return not_found();
    }
    match finish_pairing(&st, f, from.ip()) {
        Ok(sealed) => Json(sealed).into_response(),
        Err((code, msg)) => error_body(code, msg),
    }
}

/// Finish a handshake: check the code, record the caller as a peer, and seal
/// what it needs to know. `source` is the address the request came from — the
/// peer's address, unless it is paired already.
pub fn finish_pairing(st: &LinkState, f: pairing::Finish, source: IpAddr) -> Result<seal::Sealed, (StatusCode, String)> {
    let refused = || (StatusCode::FORBIDDEN, NO_INVITE.to_string());
    let pending = st.sessions.take(&f.sid).ok_or_else(refused)?;
    // The invite this began for must still be the one that can be redeemed.
    let invite = st.store.live_invite(now()).filter(|i| i.id == pending.invite_id).ok_or_else(refused)?;
    let plain = match seal::open(&pending.keys.a_to_b, &pairing::offer_aad(&f.sid), &f.sealed) {
        Ok(p) => p,
        Err(_) => {
            // Sealed under another key: the sender did not know the code. That
            // was a guess, and the invite only allows so many.
            let still_good = st.store.note_wrong_code(&invite.id).map_err(internal_err)?;
            st.discovery_wake.notify_one();
            tracing::warn!(from = %source, still_good, "[slink] a pairing attempt used a wrong code");
            return Err(refused());
        }
    };
    let offer: pairing::Offer = serde_json::from_slice(&plain).map_err(|_| (StatusCode::BAD_REQUEST, "malformed offer".to_string()))?;
    if !st.store.redeem_invite(&invite.id, now()).map_err(internal_err)? {
        return Err(refused());
    }
    // The announcement stops saying a code can be redeemed.
    st.discovery_wake.notify_one();

    let s = st.settings();
    let theirs = random_token();
    let daemon_id = offer.daemon_id.as_deref().and_then(clean_daemon_id);
    // The same daemon pairing again replaces its record (and keeps the id its
    // tasks are filed under, and what we let it do), instead of adding a twin.
    let known = daemon_id.as_deref().and_then(|d| st.store.by_daemon_id(d));
    let name: String = offer.name.trim().chars().take(80).collect();
    // Pairing again keeps what the operator allowed this peer to read, as it keeps auto-run.
    let knowledge = known.as_ref().map(|k| k.knowledge.clone()).unwrap_or_default();
    let app_sync = known.as_ref().map(|k| k.app_sync.clone()).unwrap_or_default();
    let peer = Peer {
        id: known.as_ref().map_or_else(|| uuid::Uuid::new_v4().to_string(), |k| k.id.clone()),
        name: if name.is_empty() { "peer".into() } else { name },
        addr: offer.port.filter(|p| *p != 0).map(|port| Endpoint { host: Host::Ip(source.to_canonical()), port }.to_string()),
        outbound_token: Some(offer.token).filter(|t| !t.is_empty()),
        inbound_token_hash: sha256_hex(&theirs),
        auto_run: known.as_ref().is_some_and(|k| k.auto_run),
        assignee: known.and_then(|k| k.assignee),
        daemon_id,
        key: Some(seal::encode_key(&pending.keys.pair)),
        knowledge,
        app_sync,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let accept = pairing::Accept { peer_id: peer.id.clone(), name: s.name, daemon_id: st.store.daemon_id().to_string(), token: theirs };
    st.store.upsert(peer).map_err(internal_err)?;
    tracing::info!(peer = %accept.peer_id, "[slink] paired by invite");
    seal::seal_json(&pending.keys.b_to_a, &pairing::accept_aad(&f.sid), &accept).map_err(internal_err)
}

/// Answer one JSON-RPC request body for `peer`.
pub async fn rpc_reply(st: &LinkState, peer: &Peer, req: &Value) -> Value {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = req["method"].as_str() else {
        return a2a::rpc_error(&id, a2a::INVALID_REQUEST, "missing method");
    };
    let method = a2a::canonical_method(method);
    // App sync talks to local apps over HTTP, so it is the one family of calls
    // that has to wait on something.
    let result = if method.starts_with("appsync/") {
        match &st.app_sync {
            Some(a) => a.handle(peer, method, &req["params"]).await,
            None => Err((a2a::METHOD_NOT_FOUND, format!("method `{method}` is not supported"))),
        }
    } else {
        handle(st, peer, method, &req["params"])
    };
    match result {
        Ok(result) => a2a::rpc_result(&id, result),
        Err((code, msg)) => a2a::rpc_error(&id, code, &msg),
    }
}

/// `POST /a2a` — a sealed JSON-RPC 2.0 request (A2A v1.0 methods) and its
/// sealed answer. Anything that is not exactly that is a bare 401: a wrong
/// sender, a wrong key, a replay, a stale clock and a wrong token look alike.
pub async fn rpc(State(st): State<Arc<LinkState>>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if !st.settings().enabled {
        return not_found();
    }
    match serve_sealed(&st, &headers, &body).await {
        Some(sealed) => Json(sealed).into_response(),
        None => unauthorized(),
    }
}

async fn serve_sealed(st: &LinkState, headers: &HeaderMap, body: &[u8]) -> Option<seal::Sealed> {
    let from = headers.get(FROM_HEADER).and_then(|v| v.to_str().ok()).and_then(clean_daemon_id)?;
    let peer = st.store.by_daemon_id(&from)?;
    let key = seal::decode_key(peer.key.as_deref()?).ok()?;
    let own = st.store.daemon_id().to_string();
    let sealed: seal::Sealed = serde_json::from_slice(body).ok()?;
    let req: seal::Request = seal::open_json(&key, &seal::aad("req", &from, &own), &sealed).ok()?;
    if !st.replay.accept(&peer.id, &req.id, req.ts, chrono::Utc::now().timestamp_millis()) {
        tracing::warn!(peer = %peer.id, "[slink] refused a stale or replayed request");
        return None;
    }
    // The token is the second lock: a sealed request without it is still refused.
    if !crate::gateway::ui_server::auth::ct_eq(&peer.inbound_token_hash, &sha256_hex(&req.token)) {
        return None;
    }
    let mut rpc = rpc_reply(st, &peer, &req.rpc).await;
    if serde_json::to_vec(&rpc).map_or(0, |b| b.len()) > seal::MAX_REPLY_BYTES {
        rpc = a2a::rpc_error(req.rpc.get("id").unwrap_or(&Value::Null), a2a::INTERNAL_ERROR, "the answer is too large to send");
    }
    seal::seal_json(&key, &seal::aad("resp", &own, &from), &seal::Reply { id: req.id, rpc }).ok()
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
        super::knowledge::METHOD_RECALL => match &st.knowledge {
            Some(k) => k.answer(&peer.id, &peer.knowledge, params).map_err(super::knowledge::RecallError::into_rpc),
            None => Err((a2a::METHOD_NOT_FOUND, format!("method `{method}` is not supported"))),
        },
        other => Err((a2a::METHOD_NOT_FOUND, format!("method `{other}` is not supported"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sen_link::store::PeerStore;
    use crate::sen_link::testkit::{node, paired, Node};
    use crate::sen_link::{client, lan, pairing};

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
            key: None,
            knowledge: Default::default(),
            app_sync: Default::default(),
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

    #[tokio::test]
    async fn pairing_records_the_caller_at_the_address_it_came_from_and_both_sides_hold_one_key() {
        let (a, b) = (node("Mac A", json!({})).await, node("Mac B", json!({})).await);
        let (at_a, at_b) = paired(&a, &b).await;

        assert_eq!((at_b.name.as_str(), at_b.addr.clone()), ("Mac A", Some(format!("127.0.0.1:{}", a.port))));
        assert_eq!(at_b.daemon_id.as_deref(), Some(a.st.store.daemon_id()));
        assert_eq!(at_a.daemon_id.as_deref(), Some(b.st.store.daemon_id()));
        assert!(!at_b.auto_run, "a new peer waits in Triage until a person trusts it");
        let key = seal::decode_key(at_a.key.as_deref().unwrap()).unwrap();
        assert_eq!(at_a.key, at_b.key, "both sides derived the same pair key");
        assert_eq!(key.len(), 32);
        // The invite is spent.
        assert!(!b.st.store.has_live_invite(now()));
    }

    async fn try_code(a: &Node, ep: &Endpoint, code: &str) -> Result<client::Paired, client::PairError> {
        let offer = pairing::Offer { name: "A".into(), port: Some(a.port), daemon_id: Some(a.st.store.daemon_id().into()), token: "slk_a".into() };
        client::pair(&a.st.http, ep, code, &offer).await
    }

    #[tokio::test]
    async fn a_wrong_code_pairs_nobody_and_the_third_wrong_one_kills_the_invite() {
        let (a, b) = (node("Mac A", json!({})).await, node("Mac B", json!({})).await);
        let ep = lan::parse_endpoint(&format!("127.0.0.1:{}", b.port), lan::DEFAULT_PORT).unwrap();

        // No invite at all: nothing to guess.
        let e = try_code(&a, &ep, "K7MQ2XRP").await.unwrap_err();
        assert!(e.reached && e.to_string().contains("invalid or expired invite code"), "{e}");

        // Two wrong guesses, and the invite still works.
        let (code, _) = b.st.store.create_invite(now()).unwrap();
        for wrong in ["AAAAAAAA", "BBBBBBBB"] {
            let e = try_code(&a, &ep, wrong).await.unwrap_err();
            assert!(e.reached && e.to_string().contains("invalid or expired invite code"), "{e}");
        }
        assert!(b.st.store.has_live_invite(now()));
        assert!(b.st.store.peers().is_empty(), "a wrong code recorded nobody");
        assert!(try_code(&a, &ep, &code).await.is_ok(), "the right code still pairs after two wrong ones");

        // Three wrong guesses kill it: even the right code is no use after that.
        let (code, _) = b.st.store.create_invite(now()).unwrap();
        for wrong in ["AAAAAAAA", "BBBBBBBB", "CCCCCCCC"] {
            assert!(try_code(&a, &ep, wrong).await.is_err());
        }
        assert!(!b.st.store.has_live_invite(now()), "the invite is gone");
        assert!(try_code(&a, &ep, &code).await.is_err(), "the right code, too late");
    }

    #[tokio::test]
    async fn pairing_again_replaces_the_record_keeps_what_the_operator_allowed_and_retires_the_old_key() {
        let (a, b) = (node("Mac A", json!({})).await, node("Mac B", json!({})).await);
        let (at_a, at_b) = paired(&a, &b).await;
        let shared = super::super::knowledge::KnowledgePolicy { facts: true, profiles: vec!["main".into()], ..Default::default() };
        b.st.store
            .update(&at_b.id, |p| {
                p.auto_run = true;
                p.assignee = Some("claude-code".into());
                p.knowledge = shared.clone();
            })
            .unwrap();
        a.st.store.update(&at_a.id, |p| p.knowledge = shared.clone()).unwrap();

        let (at_a2, at_b2) = paired(&a, &b).await;
        assert_eq!((&at_a2.knowledge, &at_b2.knowledge), (&shared, &shared), "what a peer may read survives pairing again, on both sides");
        assert_eq!((at_a2.id.as_str(), at_b2.id.as_str()), (at_a.id.as_str(), at_b.id.as_str()), "the same peers, not twins");
        assert_eq!((b.st.store.peers().len(), a.st.store.peers().len()), (1, 1));
        assert!(at_b2.auto_run && at_b2.assignee.as_deref() == Some("claude-code"));
        assert_ne!(at_a2.key, at_a.key, "a new pairing is a new key");

        // The old key no longer opens anything at B: a request sealed under it is refused.
        let old = seal::decode_key(at_a.key.as_deref().unwrap()).unwrap();
        let (status, _) = raw_request(&b, &a, &old, at_a.outbound_token.as_deref().unwrap(), json!({ "jsonrpc": "2.0", "id": 1, "method": "ListTasks", "params": {} }), None).await;
        assert_eq!(status, 401);
    }

    /// A request sealed the way `client::call` seals one, sent raw so a test can
    /// bend it. `ts` overrides the clock.
    async fn raw_request(to: &Node, from: &Node, key: &seal::Key, token: &str, rpc: Value, ts: Option<i64>) -> (u16, Value) {
        let (own, theirs) = (from.st.store.daemon_id(), to.st.store.daemon_id());
        let req = seal::Request {
            id: uuid::Uuid::new_v4().to_string(),
            ts: ts.unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
            token: token.into(),
            rpc,
        };
        let body = serde_json::to_vec(&seal::seal_json(key, &seal::aad("req", own, theirs), &req).unwrap()).unwrap();
        post_raw(to, Some(own), body).await
    }

    async fn post_raw(to: &Node, from_header: Option<&str>, body: Vec<u8>) -> (u16, Value) {
        let mut r = to.st.http.post(format!("http://127.0.0.1:{}/a2a", to.port)).header("content-type", "application/json");
        if let Some(f) = from_header {
            r = r.header(FROM_HEADER, f);
        }
        let resp = r.body(body).send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn only_a_request_sealed_under_the_pair_key_with_the_right_token_is_answered() {
        let (a, b) = (node("Mac A", json!({})).await, node("Mac B", json!({})).await);
        let (at_a, at_b) = paired(&a, &b).await;
        let key = seal::decode_key(at_b.key.as_deref().unwrap()).unwrap();
        let token = at_a.outbound_token.clone().unwrap();
        let list = json!({ "jsonrpc": "2.0", "id": 7, "method": "ListTasks", "params": {} });
        let own = a.st.store.daemon_id().to_string();

        // The real thing: answered, and the answer is sealed for us and carries our request's id.
        let req = seal::Request { id: "r-1".into(), ts: chrono::Utc::now().timestamp_millis(), token: token.clone(), rpc: list.clone() };
        let wire = serde_json::to_vec(&seal::seal_json(&key, &seal::aad("req", &own, b.st.store.daemon_id()), &req).unwrap()).unwrap();
        let (status, answer) = post_raw(&b, Some(&own), wire.clone()).await;
        assert_eq!(status, 200);
        let sealed: seal::Sealed = serde_json::from_value(answer.clone()).unwrap();
        assert!(answer.get("result").is_none() && answer.get("jsonrpc").is_none(), "nothing readable: {answer}");
        let reply: seal::Reply = seal::open_json(&key, &seal::aad("resp", b.st.store.daemon_id(), &own), &sealed).unwrap();
        assert_eq!((reply.id.as_str(), reply.rpc["id"].as_i64(), reply.rpc["result"]["totalSize"].as_i64()), ("r-1", Some(7), Some(0)));

        // The same bytes again: a replay.
        assert_eq!(post_raw(&b, Some(&own), wire).await.0, 401, "a captured request cannot run twice");

        // Everything else is the same bare refusal.
        assert_eq!(raw_request(&b, &a, &key, &token, list.clone(), Some(chrono::Utc::now().timestamp_millis() - 3 * 60_000)).await.0, 401, "a stale clock");
        assert_eq!(raw_request(&b, &a, &key, &token, list.clone(), Some(chrono::Utc::now().timestamp_millis() + 3 * 60_000)).await.0, 401, "a clock from the future");
        assert_eq!(raw_request(&b, &a, &key, "slk_not_the_token", list.clone(), None).await.0, 401, "the right key, the wrong token");
        assert_eq!(raw_request(&b, &a, &[9u8; 32], &token, list.clone(), None).await.0, 401, "another key");
        assert_eq!(post_raw(&b, None, b"{}".to_vec()).await.0, 401, "no sender");
        assert_eq!(post_raw(&b, Some("someone-else"), b"{}".to_vec()).await.0, 401, "an unknown sender");
        assert_eq!(post_raw(&b, Some(&own), serde_json::to_vec(&list).unwrap()).await.0, 401, "a plain JSON-RPC request, as before the conversation was sealed");
        assert_eq!(post_raw(&b, Some(&own), b"not json".to_vec()).await.0, 401);

        // An envelope for another direction or another receiver does not open here.
        let req = seal::Request { id: "r-2".into(), ts: chrono::Utc::now().timestamp_millis(), token: token.clone(), rpc: list.clone() };
        let turned = serde_json::to_vec(&seal::seal_json(&key, &seal::aad("resp", &own, b.st.store.daemon_id()), &req).unwrap()).unwrap();
        assert_eq!(post_raw(&b, Some(&own), turned).await.0, 401, "sealed as an answer");
        let elsewhere = serde_json::to_vec(&seal::seal_json(&key, &seal::aad("req", &own, "a-third-daemon"), &req).unwrap()).unwrap();
        assert_eq!(post_raw(&b, Some(&own), elsewhere).await.0, 401, "sealed for another daemon");
    }

    #[tokio::test]
    async fn a_peer_paired_before_calls_were_sealed_can_neither_call_nor_be_called() {
        let (a, b) = (node("Mac A", json!({})).await, node("Mac B", json!({})).await);
        let (at_a, at_b) = paired(&a, &b).await;
        let key = seal::decode_key(at_b.key.as_deref().unwrap()).unwrap();
        let token = at_a.outbound_token.clone().unwrap();
        // B forgets the key, as if the record were from an older build.
        b.st.store.update(&at_b.id, |p| p.key = None).unwrap();
        let (status, _) = raw_request(&b, &a, &key, &token, json!({ "jsonrpc": "2.0", "id": 1, "method": "ListTasks", "params": {} }), None).await;
        assert_eq!(status, 401);
        let e = client::call(&b.st, &b.st.store.get(&at_b.id).unwrap(), "ListTasks", json!({})).await.unwrap_err();
        assert!(e.to_string().contains("pair it again"), "{e}");
        assert!(crate::sen_link::store::PeerView::from(&b.st.store.get(&at_b.id).unwrap()).legacy);
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
