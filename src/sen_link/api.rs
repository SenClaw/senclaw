//! `/api/sen-link/*` — settings, invites, pairing, and delegating tasks to
//! peers. Behind the daemon's API token gate like every `/api` route.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use super::settings::SenLinkSettings;
use super::store::{random_token, sha256_hex, Peer, PeerView};
use super::{a2a, client, LinkState};

pub fn routes() -> Router<Arc<LinkState>> {
    Router::new()
        .route("/api/sen-link/status", get(status))
        .route("/api/sen-link/settings", get(settings_get).put(settings_put))
        .route("/api/sen-link/invites", post(invite))
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
    (!st.settings().enabled).then(|| err(StatusCode::CONFLICT, "sen-link is disabled (senLink.enabled)"))
}

async fn status(State(st): State<Arc<LinkState>>) -> Response {
    let s = st.settings();
    let card = a2a::agent_card(&a2a::AgentCardInfo {
        name: &s.name,
        description: &s.description,
        url: s.public_url.as_deref(),
        version: env!("CARGO_PKG_VERSION"),
    });
    Json(json!({
        "settings": s,
        "agentCard": card,
        "peerCount": st.store.peers().len(),
        "hubChannels": st.links.open().len(),
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
            super::relay::reconcile(&st);
            Json(s).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn invite(State(st): State<Arc<LinkState>>) -> Response {
    if let Some(r) = need_enabled(&st) {
        return r;
    }
    let s = st.settings();
    // With a hub, the invite gets its own channel: the other daemon can pair
    // (and later talk) through it even when neither side is reachable.
    // A hub that cannot be reached still leaves a direct invite (url + code).
    let (relay, warning) = match s.hub_url.as_deref() {
        Some(hub) => match super::relay::register_channel(&st.http, hub).await {
            Ok(r) => (Some(r), None),
            Err(e) => (None, Some(format!("hub unavailable, direct invite only: {e:#}"))),
        },
        None => (None, None),
    };
    match st.store.create_invite(crate::kanban::api::now(), relay.clone()) {
        Ok((code, expires_at)) => {
            let connect = relay.as_ref().map(|r| super::relay::encode_connect(r, &code));
            if let Some(r) = &relay {
                // Listen now, so the other side can pair as soon as it has the string.
                if let Err(e) = super::relay::ensure(&st, r) {
                    return err(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
                }
            }
            Json(json!({ "code": code, "expiresAt": expires_at, "url": s.public_url, "connect": connect, "warning": warning }))
                .into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn peers(State(st): State<Arc<LinkState>>) -> Response {
    let list: Vec<PeerView> = st.store.peers().iter().map(PeerView::from).collect();
    Json(json!({ "peers": list })).into_response()
}

#[derive(Deserialize)]
struct PairBody {
    /// Direct pairing: the other daemon's URL and its invite code.
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    code: Option<String>,
    /// Hub pairing: the `senlink:` connection string of a hub invite.
    #[serde(default)]
    connect: Option<String>,
}

/// Pair with another daemon: directly (`url` + `code`) or through the hub
/// channel of its invite (`connect`).
async fn pair(State(st): State<Arc<LinkState>>, Json(b): Json<PairBody>) -> Response {
    if let Some(r) = need_enabled(&st) {
        return r;
    }
    let s = st.settings();
    let ours = random_token();
    let (reply, url, relay) = if let Some(c) = b.connect.as_deref().filter(|c| !c.trim().is_empty()) {
        let connect = match super::relay::decode_connect(c) {
            Ok(c) => c,
            Err(e) => return err(StatusCode::BAD_REQUEST, format!("{e:#}")),
        };
        match client::pair_via_hub(&st, &connect, &s.name, s.public_url.as_deref(), &ours).await {
            // Talk through the hub: that is why this pairing used it.
            Ok(v) => (v, None, Some(connect.relay())),
            Err(e) => return err(StatusCode::BAD_GATEWAY, format!("{e:#}")),
        }
    } else {
        let (Some(url), Some(code)) = (b.url.as_deref(), b.code.as_deref()) else {
            return err(StatusCode::BAD_REQUEST, "give url and code, or a senlink: connection string");
        };
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return err(StatusCode::BAD_REQUEST, "url: must start with http:// or https://");
        }
        match client::pair(&st.http, url, code, &s.name, s.public_url.as_deref(), &ours).await {
            // Prefer the URL we actually reached over the one it advertises.
            Ok(v) => (v, Some(url.trim_end_matches('/').to_string()), None),
            Err(e) => return err(StatusCode::BAD_GATEWAY, format!("{e:#}")),
        }
    };
    let peer = Peer {
        id: uuid::Uuid::new_v4().to_string(),
        name: reply["name"].as_str().unwrap_or("peer").chars().take(80).collect(),
        url,
        outbound_token: reply["token"].as_str().map(str::to_string),
        inbound_token_hash: sha256_hex(&ours),
        auto_run: false,
        assignee: None,
        relay,
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
        Ok(true) => {
            // Close its hub channel now rather than at the next reconcile.
            super::relay::reconcile(&st);
            StatusCode::NO_CONTENT.into_response()
        }
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

async fn relay(st: &Arc<LinkState>, peer: &Peer, method: &str, params: Value) -> Response {
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
