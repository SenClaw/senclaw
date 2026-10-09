//! `/mcp/{core|kanban}` — MCP Streamable HTTP in front of SenClaw's own stdio
//! MCP servers, so an interactive Claude Code can `claude mcp add` SenClaw:
//!
//! ```text
//! claude mcp add --transport http --scope user senclaw \
//!   http://127.0.0.1:18788/mcp/core --header "Authorization: Bearer $SENCLAW_TOKEN"
//! ```
//!
//! Each MCP session (`initialize` → `Mcp-Session-Id`) owns one child process
//! (`senclaw core-server` scoped to a chat, or `senclaw kanban-server`);
//! JSON-RPC requests are relayed line by line and answered as plain JSON
//! (the spec's non-streaming response form). There is no server-to-client
//! stream: `GET` answers 405, which the spec allows.
//!
//! Auth is always required, loopback included (any local web page can POST to
//! loopback): the daemon API token, or a live Claude Code run token. A session
//! only answers the token that opened it.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::oneshot;

use crate::mcp::helper::McpServerConfig;

const SESSION_HEADER: &str = "mcp-session-id";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const IDLE_TTL: Duration = Duration::from_secs(30 * 60);
/// Live sessions (= child processes) per token, and in total. Opening one past
/// the cap closes that token's (or anyone's) least recently used session.
const MAX_SESSIONS_PER_OWNER: usize = 8;
const MAX_SESSIONS: usize = 32;

/// Who is calling.
enum Caller {
    /// The daemon API token: may pick the chat with `?chat=`.
    Api,
    /// A Claude Code run token: pinned to the run's chat.
    Run(Option<String>),
}

pub struct BridgeState {
    config: Arc<crate::config::Config>,
    db: Option<Arc<crate::db::Db>>,
    /// The daemon API token (accepted in addition to run tokens).
    api_token: Option<String>,
    sessions: Mutex<HashMap<String, Arc<ChildSession>>>,
}

struct ChildSession {
    owner: String,
    /// Added to the server's `initialize` instructions (chat scope for `core`).
    instructions: Option<String>,
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    /// Ids sent to the child are rewritten to this counter, so two clients'
    /// equal JSON-RPC ids on one session can never collide.
    next_id: std::sync::atomic::AtomicU64,
    child: tokio::sync::Mutex<Child>,
    last_used: Mutex<Instant>,
}

impl ChildSession {
    async fn write(&self, msg: &Value) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(msg)?;
        line.push(b'\n');
        let mut w = self.stdin.lock().await;
        w.write_all(&line).await?;
        w.flush().await
    }

    async fn request(&self, msg: &Value) -> Value {
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let internal = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let key = Value::from(internal).to_string();
        let mut out = msg.clone();
        out["id"] = Value::from(internal);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(key.clone(), tx);
        if let Err(e) = self.write(&out).await {
            self.pending.lock().unwrap().remove(&key);
            return rpc_error(id, -32603, &format!("MCP server unavailable: {e}"));
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(mut v)) => {
                v["id"] = id;
                v
            }
            Ok(Err(_)) => rpc_error(id, -32603, "MCP server exited"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&key);
                rpc_error(id, -32603, "MCP request timed out")
            }
        }
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn token_hash(t: &str) -> String {
    hex::encode(Sha256::digest(t.as_bytes()))
}

/// Whether `msg` is a request (has both `method` and `id`).
fn is_request(msg: &Value) -> bool {
    msg.get("method").is_some() && msg.get("id").is_some_and(|i| !i.is_null())
}

impl BridgeState {
    pub fn new(config: Arc<crate::config::Config>, db: Option<Arc<crate::db::Db>>, api_token: Option<String>) -> Arc<Self> {
        let s = Arc::new(Self { config, db, api_token, sessions: Mutex::new(HashMap::new()) });
        let weak = Arc::downgrade(&s);
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_secs(60));
            loop {
                t.tick().await;
                let Some(s) = weak.upgrade() else { break };
                s.reap_idle();
            }
        });
        s
    }

    fn reap_idle(&self) {
        let mut map = self.sessions.lock().unwrap();
        map.retain(|_, s| s.last_used.lock().unwrap().elapsed() < IDLE_TTL);
    }

    fn authorize(&self, token: &str) -> Option<Caller> {
        if let Some(t) = self.api_token.as_deref() {
            if crate::gateway::ui_server::auth::ct_eq(t, token) {
                return Some(Caller::Api);
            }
        }
        super::registry::by_token(token).map(|e| Caller::Run(e.mcp_chat_jid))
    }

    /// Insert `s`, first closing least-recently-used sessions over the caps.
    fn insert_capped(&self, sid: String, s: Arc<ChildSession>) {
        let mut map = self.sessions.lock().unwrap();
        let oldest = |map: &HashMap<String, Arc<ChildSession>>, owner: Option<&str>| {
            map.iter()
                .filter(|(_, v)| owner.is_none_or(|o| v.owner == o))
                .min_by_key(|(_, v)| *v.last_used.lock().unwrap())
                .map(|(k, _)| k.clone())
        };
        while map.values().filter(|v| v.owner == s.owner).count() >= MAX_SESSIONS_PER_OWNER {
            let Some(k) = oldest(&map, Some(&s.owner)) else { break };
            map.remove(&k);
        }
        while map.len() >= MAX_SESSIONS {
            let Some(k) = oldest(&map, None) else { break };
            map.remove(&k);
        }
        map.insert(sid, s);
    }

    /// The child to spawn, plus the scope note for its `initialize` reply.
    fn server_config(&self, kind: &str, chat: Option<&str>) -> Result<(McpServerConfig, Option<String>), String> {
        match kind {
            "kanban" => Ok((super::senclaw_mcp::kanban_server(), None)),
            "core" => {
                let db = self.db.as_ref().ok_or("daemon database unavailable")?;
                let st = super::settings::load(&self.config.paths.global_config_path);
                let jid = chat.map(str::to_string).or(st.mcp_chat_jid);
                let g = super::senclaw_mcp::resolve_group(db, jid.as_deref()).ok_or("no chat group to scope SenClaw tools to")?;
                Ok((super::senclaw_mcp::core_server(&self.config, &g), Some(super::senclaw_mcp::scope_instructions(&g))))
            }
            other => Err(format!("unknown SenClaw MCP server `{other}` (core, kanban)")),
        }
    }

    fn spawn(&self, cfg: &McpServerConfig, owner: String, instructions: Option<String>) -> std::io::Result<Arc<ChildSession>> {
        let mut child = tokio::process::Command::new(&cfg.command)
            .args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>> = Arc::default();
        let session = Arc::new(ChildSession {
            owner,
            instructions,
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Arc::clone(&pending),
            next_id: std::sync::atomic::AtomicU64::new(1),
            child: tokio::sync::Mutex::new(child),
            last_used: Mutex::new(Instant::now()),
        });
        let weak = Arc::downgrade(&session);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                if is_request(&msg) {
                    // Server → client requests (sampling, elicitation) cannot be
                    // relayed without a stream; refuse so the server never hangs.
                    if let Some(s) = weak.upgrade() {
                        let id = msg["id"].clone();
                        let _ = s.write(&rpc_error(id, -32601, "not supported over the SenClaw HTTP bridge")).await;
                    }
                    continue;
                }
                if let Some(id) = msg.get("id") {
                    if let Some(tx) = pending.lock().unwrap().remove(&id.to_string()) {
                        let _ = tx.send(msg);
                    }
                }
            }
            // EOF: dropping the senders fails every waiting request.
            pending.lock().unwrap().clear();
        });
        Ok(session)
    }
}

pub fn router(state: Arc<BridgeState>) -> Router {
    Router::new()
        .route("/mcp/:server", post(post_mcp).get(get_mcp).delete(delete_mcp))
        .with_state(state)
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized" }))).into_response()
}

async fn get_mcp() -> Response {
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}

async fn delete_mcp(State(st): State<Arc<BridgeState>>, headers: HeaderMap) -> Response {
    let Some(token) = crate::gateway::ui_server::auth::token_from_headers(&headers) else {
        return unauthorized();
    };
    let Some(sid) = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut map = st.sessions.lock().unwrap();
    match map.get(sid) {
        Some(s) if s.owner == token_hash(&token) => {
            map.remove(sid);
            StatusCode::OK.into_response()
        }
        Some(_) => unauthorized(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn post_mcp(
    State(st): State<Arc<BridgeState>>,
    Path(server): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(token) = crate::gateway::ui_server::auth::token_from_headers(&headers) else {
        return unauthorized();
    };
    let Some(caller) = st.authorize(&token) else {
        return unauthorized();
    };
    let owner = token_hash(&token);
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(rpc_error(Value::Null, -32700, &format!("parse error: {e}")))).into_response(),
    };
    let batch = parsed.is_array();
    let msgs: Vec<Value> = match parsed {
        Value::Array(a) => a,
        v => vec![v],
    };
    let initializing = msgs.iter().any(|m| m["method"] == "initialize");

    let (session, new_sid) = match headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) {
        Some(sid) => match st.sessions.lock().unwrap().get(sid).cloned() {
            Some(s) if s.owner == owner => (s, None),
            Some(_) => return unauthorized(),
            None => return (StatusCode::NOT_FOUND, Json(rpc_error(Value::Null, -32001, "session not found"))).into_response(),
        },
        None if initializing => {
            let chat = match caller {
                Caller::Api => q.get("chat").cloned(),
                Caller::Run(jid) => jid,
            };
            let (cfg, note) = match st.server_config(&server, chat.as_deref()) {
                Ok(c) => c,
                Err(e) => return (StatusCode::NOT_FOUND, Json(json!({ "error": e }))).into_response(),
            };
            let s = match st.spawn(&cfg, owner, note) {
                Ok(s) => s,
                Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": format!("spawn MCP server: {e}") }))).into_response(),
            };
            let sid = uuid::Uuid::new_v4().to_string();
            st.insert_capped(sid.clone(), Arc::clone(&s));
            (s, Some(sid))
        }
        None => return (StatusCode::BAD_REQUEST, Json(rpc_error(Value::Null, -32600, "missing Mcp-Session-Id"))).into_response(),
    };
    *session.last_used.lock().unwrap() = Instant::now();

    let mut replies = Vec::new();
    for m in &msgs {
        if is_request(m) {
            let mut reply = session.request(m).await;
            if m["method"] == "initialize" {
                add_instructions(&mut reply, session.instructions.as_deref());
            }
            replies.push(reply);
        } else if let Err(e) = session.write(m).await {
            replies.push(rpc_error(Value::Null, -32603, &format!("MCP server unavailable: {e}")));
        }
    }
    let mut resp = if replies.is_empty() {
        StatusCode::ACCEPTED.into_response()
    } else if batch {
        Json(Value::Array(replies)).into_response()
    } else {
        Json(replies.into_iter().next().unwrap()).into_response()
    };
    if let Some(sid) = new_sid {
        if let Ok(v) = HeaderValue::from_str(&sid) {
            resp.headers_mut().insert(SESSION_HEADER, v);
        }
    }
    resp
}

/// Append `note` to an `initialize` result's `instructions`.
fn add_instructions(reply: &mut Value, note: Option<&str>) {
    let Some(note) = note else { return };
    let Some(result) = reply.get_mut("result").and_then(Value::as_object_mut) else { return };
    let merged = match result.get("instructions").and_then(Value::as_str) {
        Some(existing) if !existing.is_empty() => format!("{existing}\n\n{note}"),
        _ => note.to_string(),
    };
    result.insert("instructions".into(), Value::String(merged));
}

impl Drop for ChildSession {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.try_lock() {
            let _ = c.start_kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_told_apart_from_notifications_and_replies() {
        assert!(is_request(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})));
        assert!(!is_request(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})));
        assert!(!is_request(&json!({"jsonrpc": "2.0", "id": 1, "result": {}})));
        assert!(!is_request(&json!({"jsonrpc": "2.0", "id": null, "method": "x"})));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sessions_are_capped_per_token() {
        let st = BridgeState::new(Arc::new(crate::config::Config::from_env()), None, None);
        let mut cfg = McpServerConfig::new("echo", "");
        cfg.command = "cat".into();
        cfg.args = vec![];
        for i in 0..(MAX_SESSIONS_PER_OWNER + 3) {
            let s = st.spawn(&cfg, "owner-a".into(), None).unwrap();
            st.insert_capped(format!("s{i}"), s);
        }
        let map = st.sessions.lock().unwrap();
        assert_eq!(map.len(), MAX_SESSIONS_PER_OWNER);
        assert!(!map.contains_key("s0"));
        assert!(map.contains_key(&format!("s{}", MAX_SESSIONS_PER_OWNER + 2)));
    }

    #[test]
    fn the_scope_note_joins_existing_instructions() {
        let mut r = json!({"result": {"instructions": "A"}});
        add_instructions(&mut r, Some("B"));
        assert_eq!(r["result"]["instructions"], "A\n\nB");
        let mut e = json!({"error": {}});
        add_instructions(&mut e, Some("B"));
        assert!(e.get("result").is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_session_relays_requests_to_its_child() {
        // `cat` echoes each request line back: a reply with the same id.
        let st = BridgeState::new(Arc::new(crate::config::Config::from_env()), None, Some("tok".into()));
        let mut cfg = McpServerConfig::new("echo", "");
        cfg.command = "cat".into();
        cfg.args = vec![];
        let s = st.spawn(&cfg, token_hash("tok"), None).unwrap();
        // `cat` echoes the request back; an echoed line carrying `method` and
        // `id` is a server→client request and is refused, so send a
        // reply-shaped message to exercise id rewriting and routing.
        let reply = s.request(&json!({"jsonrpc": "2.0", "id": "client-7", "result": {"ok": true}})).await;
        assert_eq!(reply["id"], "client-7");
        assert_eq!(reply["result"]["ok"], true);
        assert!(matches!(st.authorize("tok"), Some(Caller::Api)));
        assert!(st.authorize("nope").is_none());
    }
}
