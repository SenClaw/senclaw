//! `/api/sen-link/*` — settings, invites, pairing, probing an address, and
//! delegating tasks to peers. On the UI server, behind the daemon's API token
//! gate like every `/api` route; the A2A surface peers call is on the LAN
//! listener instead (`listener.rs`).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::lan::{self, DEFAULT_PORT};
use super::server::PairRequest;
use super::settings::SenLinkSettings;
use super::store::{clean_daemon_id, random_token, sha256_hex, Peer, PeerView};
use super::{a2a, client, probe, LinkState};

pub fn routes() -> Router<Arc<LinkState>> {
    Router::new()
        .route("/api/sen-link/status", get(status))
        .route("/api/sen-link/settings", get(settings_get).put(settings_put))
        .route("/api/sen-link/invites", post(invite))
        .route("/api/sen-link/probe", post(probe_addr))
        .route("/api/sen-link/peers", get(peers))
        .route("/api/sen-link/peers/pair", post(pair))
        .route("/api/sen-link/peers/:id", axum::routing::patch(peer_patch).delete(peer_delete))
        .route("/api/sen-link/peers/:id/messages", post(send))
        .route("/api/sen-link/peers/:id/tasks", get(tasks_list))
        .route("/api/sen-link/peers/:id/tasks/:task_id", get(task_get))
        .route("/api/sen-link/peers/:id/tasks/:task_id/cancel", post(task_cancel))
}

fn err(code: StatusCode, e: impl std::fmt::Display) -> Response {
    (code, Json(json!({ "error": e.to_string() }))).into_response()
}

fn need_enabled(st: &LinkState) -> Option<Response> {
    (!st.settings().enabled).then(|| err(StatusCode::CONFLICT, "SLink is disabled (senLink.enabled)"))
}

/// This machine's LAN addresses, for "type this on the other machine".
fn local_addresses() -> Vec<String> {
    lan::local_addresses().iter().map(|a| a.to_string()).collect()
}

async fn status(State(st): State<Arc<LinkState>>) -> Response {
    let s = st.settings();
    let card = a2a::agent_card(&a2a::AgentCardInfo {
        name: &s.name,
        description: &s.description,
        url: None,
        version: env!("CARGO_PKG_VERSION"),
        daemon_id: st.store.daemon_id(),
        pairable: st.store.has_live_invite(crate::kanban::api::now()),
    });
    let listen = st.listen_status();
    Json(json!({
        "settings": s,
        "agentCard": card,
        "daemonId": st.store.daemon_id(),
        "peerCount": st.store.peers().len(),
        "listening": listen.addr.map(|a| a.to_string()),
        "listenError": listen.error,
        "addresses": local_addresses(),
        "migrated": st.migrated,
    }))
    .into_response()
}

async fn settings_get(State(st): State<Arc<LinkState>>) -> Response {
    Json(st.settings()).into_response()
}

async fn settings_put(State(st): State<Arc<LinkState>>, Json(body): Json<SenLinkSettings>) -> Response {
    let s = match body.validated() {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    match crate::gateway::group_manager::save_sen_link_settings(st.config_path(), &s) {
        Ok(()) => {
            // Open, move or close the LAN socket now rather than at the next look.
            st.wake.notify_one();
            Json(s).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// A one-time code, and where to type it: this machine's addresses and port.
async fn invite(State(st): State<Arc<LinkState>>) -> Response {
    if let Some(r) = need_enabled(&st) {
        return r;
    }
    let s = st.settings();
    match st.store.create_invite(crate::kanban::api::now()) {
        Ok((code, expires_at)) => {
            let listen = st.listen_status();
            Json(json!({
                "code": code,
                "expiresAt": expires_at,
                "port": listen.addr.map_or(s.listen_port, |a| a.port()),
                "addresses": local_addresses(),
                "listening": listen.addr.is_some(),
                "listenError": listen.error,
            }))
            .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
struct ProbeBody {
    addr: String,
}

/// Where a typed address lands and whether a SLink answers there.
async fn probe_addr(State(st): State<Arc<LinkState>>, Json(b): Json<ProbeBody>) -> Response {
    Json(probe::probe(&st.http, &b.addr, DEFAULT_PORT).await).into_response()
}

async fn peers(State(st): State<Arc<LinkState>>) -> Response {
    let list: Vec<PeerView> = st.store.peers().iter().map(PeerView::from).collect();
    Json(json!({ "peers": list })).into_response()
}

#[derive(Deserialize)]
struct PairBody {
    /// The other daemon: an IP or a DNS name, optionally `:port`. (`url` is what
    /// the same field was called before.)
    #[serde(alias = "url")]
    addr: String,
    code: String,
}

/// Pair with the daemon at `addr` using its invite `code`.
async fn pair(State(st): State<Arc<LinkState>>, Json(b): Json<PairBody>) -> Response {
    if let Some(r) = need_enabled(&st) {
        return r;
    }
    let ep = match lan::parse_endpoint(&b.addr, DEFAULT_PORT) {
        Ok(ep) => ep,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let s = st.settings();
    let ours = random_token();
    let req = PairRequest {
        code: b.code,
        name: s.name.clone(),
        port: Some(s.listen_port),
        daemon_id: Some(st.store.daemon_id().to_string()),
        token: Some(ours.clone()),
    };
    let reply = match client::pair(&st.http, &ep, &req).await {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    };
    let daemon_id = reply["daemonId"].as_str().and_then(clean_daemon_id);
    // The same daemon again is the same peer: new tokens and address, same id
    // (its tasks are filed under it) and the trust we gave it.
    let known = daemon_id.as_deref().and_then(|d| st.store.by_daemon_id(d));
    let name: String = reply["name"].as_str().unwrap_or("peer").chars().take(80).collect();
    let peer = Peer {
        id: known.as_ref().map_or_else(|| uuid::Uuid::new_v4().to_string(), |k| k.id.clone()),
        name: if name.trim().is_empty() { "peer".into() } else { name },
        // As typed: a name keeps following DNS.
        addr: Some(ep.to_string()),
        outbound_token: reply["token"].as_str().map(str::to_string),
        inbound_token_hash: sha256_hex(&ours),
        auto_run: known.as_ref().is_some_and(|k| k.auto_run),
        assignee: known.and_then(|k| k.assignee),
        daemon_id,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let view = PeerView::from(&peer);
    match st.store.upsert(peer) {
        Ok(()) => Json(view).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeerPatch {
    name: Option<String>,
    auto_run: Option<bool>,
    /// `""` clears it.
    assignee: Option<String>,
}

async fn peer_patch(State(st): State<Arc<LinkState>>, Path(id): Path<String>, Json(p): Json<PeerPatch>) -> Response {
    let r = st.store.update(&id, |peer| {
        if let Some(n) = p.name.filter(|n| !n.trim().is_empty()) {
            peer.name = n.trim().chars().take(80).collect();
        }
        if let Some(a) = p.auto_run {
            peer.auto_run = a;
        }
        if let Some(a) = p.assignee {
            peer.assignee = (!a.trim().is_empty()).then(|| a.trim().to_string());
        }
    });
    match r {
        Ok(Some(peer)) => Json(PeerView::from(&peer)).into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "no such peer"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn peer_delete(State(st): State<Arc<LinkState>>, Path(id): Path<String>) -> Response {
    match st.store.remove(&id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such peer"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendBody {
    text: String,
    /// Follow-up on an existing remote task.
    task_id: Option<String>,
    /// Ask the peer to run it in a git worktree.
    #[serde(default)]
    worktree: bool,
}

async fn with_peer(st: &LinkState, id: &str) -> Result<Peer, Response> {
    if let Some(r) = need_enabled(st) {
        return Err(r);
    }
    st.store.get(id).ok_or_else(|| err(StatusCode::NOT_FOUND, "no such peer"))
}

async fn relay(st: &LinkState, peer: &Peer, method: &str, params: Value) -> Response {
    match client::call(st, peer, method, params).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("{e:#}")),
    }
}

async fn send(State(st): State<Arc<LinkState>>, Path(id): Path<String>, Json(b): Json<SendBody>) -> Response {
    let peer = match with_peer(&st, &id).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    if b.text.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "text: must not be empty");
    }
    let meta = json!({ "worktree": b.worktree });
    let msg = a2a::user_message(&b.text, b.task_id.as_deref(), meta);
    relay(&st, &peer, "SendMessage", json!({ "message": msg })).await
}

async fn tasks_list(State(st): State<Arc<LinkState>>, Path(id): Path<String>) -> Response {
    match with_peer(&st, &id).await {
        Ok(p) => relay(&st, &p, "ListTasks", json!({})).await,
        Err(r) => r,
    }
}

async fn task_get(State(st): State<Arc<LinkState>>, Path((id, task_id)): Path<(String, String)>) -> Response {
    match with_peer(&st, &id).await {
        Ok(p) => relay(&st, &p, "GetTask", json!({ "id": task_id })).await,
        Err(r) => r,
    }
}

async fn task_cancel(State(st): State<Arc<LinkState>>, Path((id, task_id)): Path<(String, String)>) -> Response {
    match with_peer(&st, &id).await {
        Ok(p) => relay(&st, &p, "CancelTask", json!({ "id": task_id })).await,
        Err(r) => r,
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;
    use tower::ServiceExt;

    use super::*;
    use crate::sen_link::store::PeerStore;
    use crate::sen_link::{api_router, listener};

    /// A daemon on loopback: its own config, peers, kanban, and a LAN listener
    /// on a free port (what `listener::start` would open).
    struct Node {
        st: Arc<LinkState>,
        port: u16,
        server: JoinHandle<()>,
        _dir: tempfile::TempDir,
    }

    impl Drop for Node {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    async fn node(name: &str, extra: Value) -> Node {
        let dir = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let mut cfg = json!({ "enabled": true, "name": name, "listenHost": "127.0.0.1", "listenPort": addr.port() });
        for (k, v) in extra.as_object().into_iter().flatten() {
            cfg[k] = v.clone();
        }
        let path = dir.path().join("config.json");
        std::fs::write(&path, json!({ "senLink": cfg }).to_string()).unwrap();
        let st = Arc::new(LinkState::new_at(
            path,
            Arc::new(crate::kanban::db::Db::open(&dir.path().join("kanban.db")).unwrap()),
            PeerStore::new(dir.path().join("sen-link/peers.json")),
        ));
        st.set_listen(Some(addr), None);
        let served = Arc::clone(&st);
        let server = tokio::spawn(async move {
            let _ = listener::serve_on(listener, served).await;
        });
        Node { st, port: addr.port(), server, _dir: dir }
    }

    async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    fn cards_on_slink_board(n: &Node) -> i64 {
        n.st.kanban.list_boards().unwrap().into_iter().find(|b| b.title == "SLink").map_or(0, |b| b.card_count)
    }

    #[tokio::test]
    async fn two_daemons_pair_by_name_and_by_ip_and_delegate_tasks_both_ways() {
        let (a, b) = (node("Mac A", json!({})).await, node("Mac B", json!({})).await);
        let (api_a, api_b) = (api_router(Arc::clone(&a.st)), api_router(Arc::clone(&b.st)));

        // B makes an invite and says where to type it.
        let (s, inv) = call(&api_b, "POST", "/api/sen-link/invites", None).await;
        assert_eq!(s, StatusCode::OK, "{inv}");
        assert_eq!((inv["port"].as_u64(), inv["listening"].as_bool()), (Some(b.port as u64), Some(true)));
        assert!(inv["addresses"].is_array());

        // A pairs by a DNS name: the address is kept as typed.
        let typed = format!("localhost:{}", b.port);
        let (s, view) = call(&api_a, "POST", "/api/sen-link/peers/pair", Some(json!({ "addr": typed, "code": inv["code"] }))).await;
        assert_eq!(s, StatusCode::OK, "{view}");
        assert_eq!((view["name"].as_str(), view["addr"].as_str(), view["canSend"].as_bool(), view["legacy"].as_bool()), (Some("Mac B"), Some(typed.as_str()), Some(true), Some(false)));
        let id_at_a = view["id"].as_str().unwrap().to_string();

        // B learned A's address from the connection and A's port from the request.
        let at_b = b.st.store.peers();
        assert_eq!(at_b.len(), 1);
        assert_eq!((at_b[0].name.as_str(), at_b[0].addr.clone()), ("Mac A", Some(format!("127.0.0.1:{}", a.port))));
        assert_eq!(at_b[0].daemon_id.as_deref(), Some(a.st.store.daemon_id()));
        assert_eq!(a.st.store.peers()[0].daemon_id.as_deref(), Some(b.st.store.daemon_id()));

        // A → B: lands in B's SLink board, waiting in Triage.
        let (s, sent) = call(&api_a, "POST", &format!("/api/sen-link/peers/{id_at_a}/messages"), Some(json!({ "text": "Write the release notes" }))).await;
        assert_eq!(s, StatusCode::OK, "{sent}");
        assert_eq!(sent["task"]["status"]["state"], "TASK_STATE_SUBMITTED");
        assert_eq!(cards_on_slink_board(&b), 1);
        let (s, listed) = call(&api_a, "GET", &format!("/api/sen-link/peers/{id_at_a}/tasks"), None).await;
        assert_eq!((s, listed["totalSize"].as_u64()), (StatusCode::OK, Some(1)));
        let task_id = sent["task"]["id"].as_str().unwrap();
        let (s, got) = call(&api_a, "GET", &format!("/api/sen-link/peers/{id_at_a}/tasks/{task_id}"), None).await;
        assert_eq!((s, got["id"].as_str()), (StatusCode::OK, Some(task_id)));

        // B → A over the address it derived, with the token A gave it.
        let back = client::call(&b.st, &at_b[0], "SendMessage", json!({ "message": a2a::user_message("Review the PR", None, json!({})) })).await.unwrap();
        assert_eq!(back["task"]["status"]["state"], "TASK_STATE_SUBMITTED");
        assert_eq!(cards_on_slink_board(&a), 1);

        // Pairing again, now by IP literal, is the same peer, not a twin.
        let (_, inv2) = call(&api_b, "POST", "/api/sen-link/invites", None).await;
        let ip_form = format!("127.0.0.1:{}", b.port);
        let (s, again) = call(&api_a, "POST", "/api/sen-link/peers/pair", Some(json!({ "addr": ip_form, "code": inv2["code"] }))).await;
        assert_eq!(s, StatusCode::OK, "{again}");
        assert_eq!((again["id"].as_str(), again["addr"].as_str()), (Some(id_at_a.as_str()), Some(ip_form.as_str())));
        assert_eq!((a.st.store.peers().len(), b.st.store.peers().len()), (1, 1));
        let (s, _) = call(&api_a, "GET", &format!("/api/sen-link/peers/{id_at_a}/tasks"), None).await;
        assert_eq!(s, StatusCode::OK, "the new tokens work");
    }

    #[tokio::test]
    async fn pairing_says_why_it_failed() {
        let (a, b) = (node("A", json!({})).await, node("B", json!({})).await);
        let api_a = api_router(Arc::clone(&a.st));
        let pair = |addr: String, code: &'static str| {
            let api_a = api_a.clone();
            async move { call(&api_a, "POST", "/api/sen-link/peers/pair", Some(json!({ "addr": addr, "code": code }))).await }
        };

        let (s, e) = pair(format!("127.0.0.1:{}", b.port), "WRONG234").await;
        assert_eq!(s, StatusCode::BAD_GATEWAY);
        assert!(e["error"].as_str().unwrap().contains("pairing refused") && e["error"].as_str().unwrap().contains("invite"), "{e}");

        let (s, e) = pair(format!("127.0.0.1:{}", free_port()), "WRONG234").await;
        assert_eq!(s, StatusCode::BAD_GATEWAY);
        assert!(e["error"].as_str().unwrap().contains("reach 127.0.0.1"), "{e}");

        let (s, e) = pair("8.8.8.8".into(), "WRONG234").await;
        assert_eq!(s, StatusCode::BAD_GATEWAY);
        assert!(e["error"].as_str().unwrap().contains("not a LAN address"), "{e}");

        let (s, e) = pair("fe80::1%en0".into(), "WRONG234").await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(e["error"].as_str().unwrap().contains("scope"), "{e}");
        assert!(a.st.store.peers().is_empty());

        let off = node("Off", json!({ "enabled": false })).await;
        let (s, e) = call(&api_router(Arc::clone(&off.st)), "POST", "/api/sen-link/invites", None).await;
        assert_eq!(s, StatusCode::CONFLICT, "{e}");
    }

    #[tokio::test]
    async fn probing_an_address_tells_a_slink_from_everything_else() {
        let (a, hidden) = (node("Mac A", json!({})).await, node("Quiet", json!({ "discoverable": false })).await);
        let probe = |addr: String| {
            let st = Arc::clone(&a.st);
            async move { probe::probe(&st.http, &addr, DEFAULT_PORT).await }
        };

        let v = probe(format!("localhost:{}", a.port)).await;
        assert_eq!((v["ok"].as_bool(), v["code"].as_str(), v["reachable"].as_bool(), v["hidden"].as_bool()), (Some(true), Some("ok"), Some(true), Some(false)), "{v}");
        assert_eq!((v["card"]["name"].as_str(), v["card"]["daemonId"].as_str()), (Some("Mac A"), Some(a.st.store.daemon_id())));
        assert_eq!(v["card"]["pairable"], false);
        assert!(v["resolved"].as_array().unwrap().iter().any(|ip| ip == "127.0.0.1"));
        a.st.store.create_invite(crate::kanban::api::now()).unwrap();
        assert_eq!(probe(format!("127.0.0.1:{}", a.port)).await["card"]["pairable"], true, "a live invite shows");

        let v = probe(format!("127.0.0.1:{}", hidden.port)).await;
        assert_eq!((v["ok"].as_bool(), v["hidden"].as_bool(), v["card"].is_null()), (Some(true), Some(true), true), "{v}");

        let v = probe(format!("127.0.0.1:{}", free_port())).await;
        assert_eq!((v["ok"].as_bool(), v["code"].as_str(), v["reachable"].as_bool()), (Some(false), Some("unreachable"), Some(false)), "{v}");
        assert!(v["error"].as_str().unwrap().contains("refused"), "{v}");

        let v = probe("8.8.8.8:18791".into()).await;
        assert_eq!(v["code"], "not_lan");
        assert!(v["error"].as_str().unwrap().contains("outside the LAN"), "{v}");

        assert_eq!(probe("user@host".into()).await["code"], "bad_address");
        assert_eq!(probe("".into()).await["code"], "bad_address");

        // Some other web server on the port: a 200 that is not a card, and a 404 everywhere.
        let not_a_card = Router::new().route("/.well-known/agent-card.json", get(|| async { "<html>hello</html>" }));
        let nothing = Router::new();
        for (app, what) in [(not_a_card, "hello"), (nothing, "404")] {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = l.local_addr().unwrap().port();
            let task = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
            let v = probe(format!("127.0.0.1:{port}")).await;
            assert_eq!((v["ok"].as_bool(), v["code"].as_str(), v["reachable"].as_bool()), (Some(false), Some("not_slink"), Some(true)), "{what}: {v}");
            task.abort();
        }
    }

    #[tokio::test]
    async fn settings_are_validated_and_status_says_where_this_daemon_listens() {
        let a = node("Mac A", json!({})).await;
        let api_a = api_router(Arc::clone(&a.st));

        let mut bad = a.st.settings();
        bad.listen_port = 0;
        let (s, e) = call(&api_a, "PUT", "/api/sen-link/settings", Some(serde_json::to_value(&bad).unwrap())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");

        let mut ok = a.st.settings();
        ok.name = "Renamed".into();
        let (s, _) = call(&api_a, "PUT", "/api/sen-link/settings", Some(serde_json::to_value(&ok).unwrap())).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(a.st.settings().name, "Renamed");

        // A save from a client that still sends the old keys is accepted and drops them.
        let mut old = serde_json::to_value(&ok).unwrap();
        old["publicUrl"] = json!("http://100.64.0.2:18788");
        old["hubUrl"] = json!("https://hub.example");
        let (s, _) = call(&api_a, "PUT", "/api/sen-link/settings", Some(old)).await;
        assert_eq!(s, StatusCode::OK);
        let raw = std::fs::read_to_string(a.st.config_path()).unwrap();
        assert!(!raw.contains("publicUrl") && !raw.contains("hubUrl"), "{raw}");

        let (s, st) = call(&api_a, "GET", "/api/sen-link/status", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(st["listening"], format!("127.0.0.1:{}", a.port));
        assert_eq!((st["daemonId"].as_str(), st["peerCount"].as_u64()), (Some(a.st.store.daemon_id()), Some(0)));
        assert!(st["addresses"].is_array() && st["migrated"].as_array().unwrap().is_empty());
        assert_eq!(st["settings"]["boardTitle"], "SLink");
    }
}
