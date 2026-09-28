//! The SenClaw extension's connection: `ws://127.0.0.1:<ws port>/browser/ext`.
//!
//! The daemon — not the runtime — holds it, because the daemon is the only
//! process clients may reach and the one that can authenticate the extension:
//!
//! - **Origin.** Chrome stamps `Origin: chrome-extension://<id>` on an
//!   extension's WebSocket and no web page can forge it; anything else is
//!   refused before the upgrade. (The old `/browser` route checked nothing, so
//!   any page could take over the extension channel.)
//! - **Pairing.** An unknown extension id gets an 8-character code, shown in
//!   the side panel; a person approves it (`pair approve <CODE>` in chat, or
//!   `POST /api/browser-agent/extension/pairings/<CODE>/approve`). The token
//!   the extension then keeps is stored here only as a SHA-256.
//! - **Pipe.** `drv` frames are forwarded verbatim to the browser runtime's
//!   `/v1/drivers/extension`, opened when a task needs the extension and
//!   closed after ten idle minutes so the runtime can stop.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::runtime::manager::RuntimeManager;

const CODE_TTL: Duration = Duration::from_secs(600);
const PIPE_IDLE: Duration = Duration::from_secs(600);
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

static RUNTIME: OnceLock<Arc<RuntimeManager>> = OnceLock::new();

/// Called once at daemon start: the pipe needs the runtime manager.
pub fn set_runtime_manager(manager: Arc<RuntimeManager>) {
    let _ = RUNTIME.set(manager);
}

pub fn runtime_manager() -> Option<Arc<RuntimeManager>> {
    RUNTIME.get().cloned()
}

/// `chrome-extension://<32 × a-p>` → the id; anything else is refused.
pub fn origin_extension_id(origin: Option<&str>) -> Result<String, String> {
    let origin = origin.ok_or("no Origin header: only the SenClaw extension may connect here")?;
    let id = origin
        .strip_prefix("chrome-extension://")
        .ok_or_else(|| format!("origin {origin} is not a Chrome extension"))?
        .trim_end_matches('/');
    if id.len() == 32 && id.bytes().all(|b| (b'a'..=b'p').contains(&b)) {
        Ok(id.to_string())
    } else {
        Err(format!("{origin} is not a valid extension origin"))
    }
}

fn sha256_hex(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Paired {
    ext_id: String,
    token_sha256: String,
    paired_at: String,
}

struct Conn {
    id: u64,
    ext_id: String,
    hello: Value,
    to_ext: mpsc::UnboundedSender<String>,
    pipe: Option<mpsc::UnboundedSender<String>>,
    last_used: Instant,
}

struct PendingPair {
    ext_id: String,
    created: Instant,
    conn_id: u64,
    hello: Value,
    to_ext: mpsc::UnboundedSender<String>,
}

#[derive(Default)]
struct HubState {
    conn: Option<Conn>,
    pending: HashMap<String, PendingPair>,
}

pub struct Hub {
    store: PathBuf,
    state: Mutex<HubState>,
    next_conn: AtomicU64,
}

static HUB: LazyLock<Hub> =
    LazyLock::new(|| Hub::new(crate::control_plane::senclaw_home().join("browser").join("extensions.json")));

pub fn hub() -> &'static Hub {
    &HUB
}

impl Hub {
    pub fn new(store: PathBuf) -> Hub {
        Hub { store, state: Mutex::new(HubState::default()), next_conn: AtomicU64::new(1) }
    }

    fn paired(&self) -> Vec<Paired> {
        std::fs::read_to_string(&self.store).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    fn save_paired(&self, list: &[Paired]) -> Result<(), String> {
        if let Some(dir) = self.store.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&self.store, serde_json::to_vec_pretty(list).unwrap_or_default()).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.store, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn verify_token(&self, ext_id: &str, token: Option<&str>) -> bool {
        let Some(token) = token.filter(|t| !t.is_empty()) else { return false };
        let digest = sha256_hex(token);
        self.paired().iter().any(|p| p.ext_id == ext_id && p.token_sha256 == digest)
    }

    pub fn is_connected(&self) -> bool {
        self.state.lock().map(|s| s.conn.is_some()).unwrap_or(false)
    }

    pub fn status(&self) -> Value {
        let state = self.state.lock().expect("hub lock");
        let pending: Vec<Value> = state
            .pending
            .iter()
            .filter(|(_, p)| p.created.elapsed() < CODE_TTL)
            .map(|(code, p)| json!({ "code": code, "ext_id": p.ext_id, "age_secs": p.created.elapsed().as_secs() }))
            .collect();
        json!({
            "connected": state.conn.as_ref().map(|c| json!({ "ext_id": c.ext_id, "version": c.hello.get("v"), "chrome": c.hello.get("chrome"), "piped": c.pipe.is_some() })),
            "pending": pending,
            "paired": self.paired().iter().map(|p| json!({ "ext_id": p.ext_id, "paired_at": p.paired_at })).collect::<Vec<_>>(),
        })
    }

    fn new_code(&self) -> String {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        (0..8).map(|_| CODE_ALPHABET[rng.gen_range(0..CODE_ALPHABET.len())] as char).collect()
    }

    fn request_pairing(&self, conn_id: u64, ext_id: &str, hello: Value, to_ext: mpsc::UnboundedSender<String>) -> String {
        let code = self.new_code();
        let mut state = self.state.lock().expect("hub lock");
        state.pending.retain(|_, p| p.created.elapsed() < CODE_TTL && p.conn_id != conn_id);
        state.pending.insert(
            code.clone(),
            PendingPair { ext_id: ext_id.to_string(), created: Instant::now(), conn_id, hello, to_ext },
        );
        code
    }

    /// Whether an extension is waiting with `code` (the chat's `pair approve`
    /// shares one command with channel pairing and routes on this).
    pub fn has_pending(&self, code: &str) -> bool {
        let code = code.trim().to_ascii_uppercase();
        self.state.lock().map(|s| s.pending.get(&code).is_some_and(|p| p.created.elapsed() < CODE_TTL)).unwrap_or(false)
    }

    /// A person approved `code`: mint the token, keep only its hash, hand it to
    /// the waiting extension and make that connection the live one.
    pub fn approve_code(&self, code: &str) -> Result<String, String> {
        let code = code.trim().to_ascii_uppercase();
        let pending = {
            let mut state = self.state.lock().map_err(|_| "hub lock poisoned")?;
            let p = state.pending.remove(&code).ok_or_else(|| format!("no extension is waiting with code {code}"))?;
            if p.created.elapsed() >= CODE_TTL {
                return Err(format!("code {code} expired; open the side panel to get a new one"));
            }
            p
        };
        let token = {
            use rand::RngCore;
            let mut bytes = [0u8; 32];
            rand::thread_rng().fill_bytes(&mut bytes);
            hex::encode(bytes)
        };
        let mut paired: Vec<Paired> = self.paired().into_iter().filter(|p| p.ext_id != pending.ext_id).collect();
        paired.push(Paired { ext_id: pending.ext_id.clone(), token_sha256: sha256_hex(&token), paired_at: chrono::Utc::now().to_rfc3339() });
        self.save_paired(&paired)?;
        let _ = pending.to_ext.send(json!({ "ch": "ctl", "t": "paired", "token": token }).to_string());
        self.register(Conn {
            id: pending.conn_id,
            ext_id: pending.ext_id.clone(),
            hello: pending.hello,
            to_ext: pending.to_ext,
            pipe: None,
            last_used: Instant::now(),
        });
        Ok(pending.ext_id)
    }

    pub fn revoke(&self, ext_id: &str) -> Result<bool, String> {
        let before = self.paired();
        let after: Vec<Paired> = before.iter().filter(|p| p.ext_id != ext_id).cloned().collect();
        self.save_paired(&after)?;
        let mut state = self.state.lock().map_err(|_| "hub lock poisoned")?;
        if state.conn.as_ref().map(|c| c.ext_id == ext_id).unwrap_or(false) {
            if let Some(c) = state.conn.take() {
                let _ = c.to_ext.send(json!({ "ch": "ctl", "t": "revoked" }).to_string());
            }
        }
        Ok(after.len() != before.len())
    }

    fn register(&self, conn: Conn) {
        let mut state = self.state.lock().expect("hub lock");
        if let Some(old) = state.conn.replace(conn) {
            let _ = old.to_ext.send(json!({ "ch": "ctl", "t": "replaced" }).to_string());
        }
    }

    fn unregister(&self, conn_id: u64) {
        let mut state = self.state.lock().expect("hub lock");
        state.pending.retain(|_, p| p.conn_id != conn_id);
        if state.conn.as_ref().map(|c| c.id == conn_id).unwrap_or(false) {
            state.conn = None; // dropping the pipe sender closes the runtime pipe
        }
    }

    fn forward_to_runtime(&self, conn_id: u64, frame: String) {
        let state = self.state.lock().expect("hub lock");
        if let Some(conn) = state.conn.as_ref().filter(|c| c.id == conn_id) {
            if let Some(pipe) = &conn.pipe {
                let _ = pipe.send(frame);
            }
        }
    }

    fn clear_pipe(&self, conn_id: u64) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(conn) = state.conn.as_mut().filter(|c| c.id == conn_id) {
                conn.pipe = None;
            }
        }
    }

    /// Make sure the runtime can reach the extension; called before a task
    /// that runs in the person's Chrome.
    pub async fn ensure_pipe(&'static self, manager: Arc<RuntimeManager>) -> Result<(), String> {
        let (conn_id, hello, to_ext) = {
            let mut state = self.state.lock().map_err(|_| "hub lock poisoned")?;
            let conn = state.conn.as_mut().ok_or("the SenClaw extension is not connected")?;
            conn.last_used = Instant::now();
            if conn.pipe.as_ref().map(|p| !p.is_closed()).unwrap_or(false) {
                return Ok(());
            }
            (conn.id, conn.hello.clone(), conn.to_ext.clone())
        };
        let dial = manager
            .ensure_slot_started(sen_runtime_sdk::manifest::Slot::Browser)
            .await
            .map_err(|e| format!("the browser runtime is unavailable: {e}"))?;
        let url = format!("{}/v1/drivers/extension", dial.base_url.replacen("http://", "ws://", 1));
        let mut request = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(url.as_str())
            .map_err(|e| e.to_string())?;
        request.headers_mut().insert(
            "Authorization",
            format!("Bearer {}", dial.token).parse().map_err(|_| "bad runtime token")?,
        );
        let (ws, _) = tokio_tungstenite::connect_async(request).await.map_err(|e| format!("cannot open the runtime pipe: {e}"))?;
        let (mut sink, mut stream) = ws.split();
        let (pipe_tx, mut pipe_rx) = mpsc::unbounded_channel::<String>();
        manager.begin_request(&dial.process_key);
        tokio::spawn(async move {
            while let Some(frame) = pipe_rx.recv().await {
                if sink.send(tokio_tungstenite::tungstenite::Message::Text(frame)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        {
            let key = dial.process_key.clone();
            let manager = manager.clone();
            tokio::spawn(async move {
                while let Some(Ok(msg)) = stream.next().await {
                    if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                        let _ = to_ext.send(text.to_string());
                    }
                }
                manager.end_request(&key);
                hub().clear_pipe(conn_id);
            });
        }
        let runtime_hello = json!({
            "ch": "drv", "t": "hello",
            "ext": hello.get("ext"), "v": hello.get("v"), "scripts": hello.get("scripts"), "chrome": hello.get("chrome"),
        });
        let _ = pipe_tx.send(runtime_hello.to_string());
        {
            let mut state = self.state.lock().map_err(|_| "hub lock poisoned")?;
            if let Some(conn) = state.conn.as_mut().filter(|c| c.id == conn_id) {
                conn.pipe = Some(pipe_tx);
            }
        }
        // Give the runtime a moment to register the relay before the task opens a session.
        tokio::time::sleep(Duration::from_millis(150)).await;
        spawn_idle_sweeper();
        Ok(())
    }
}

fn spawn_idle_sweeper() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    tokio::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            if let Ok(mut state) = hub().state.lock() {
                if let Some(conn) = state.conn.as_mut() {
                    if conn.pipe.is_some() && conn.last_used.elapsed() > PIPE_IDLE {
                        conn.pipe = None;
                    }
                }
            }
        }
    });
}

/// One extension WebSocket, from upgrade to close.
pub async fn handle_socket(socket: axum::extract::ws::WebSocket, ext_id: String) {
    use axum::extract::ws::Message;
    let hub = hub();
    let (mut sink, mut stream) = socket.split();
    let (to_ext, mut rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if sink.send(Message::Text(frame)).await.is_err() {
                break;
            }
        }
    });
    let conn_id = hub.next_conn.fetch_add(1, Ordering::SeqCst);

    // The first frame must be the extension's hello, for the same id as its Origin.
    let hello = match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str::<Value>(&text).ok(),
        _ => None,
    };
    let Some(hello) = hello.filter(|h| h.get("ch").and_then(Value::as_str) == Some("ctl") && h.get("t").and_then(Value::as_str) == Some("hello")) else {
        let _ = to_ext.send(json!({ "ch": "ctl", "t": "error", "error": "expected hello" }).to_string());
        drop(to_ext);
        let _ = writer.await;
        return;
    };
    if hello.get("ext").and_then(Value::as_str) != Some(ext_id.as_str()) {
        let _ = to_ext.send(json!({ "ch": "ctl", "t": "error", "error": "hello names another extension than its Origin" }).to_string());
        drop(to_ext);
        let _ = writer.await;
        return;
    }
    let token = hello.get("token").and_then(Value::as_str);
    if hub.verify_token(&ext_id, token) {
        hub.register(Conn { id: conn_id, ext_id: ext_id.clone(), hello: hello.clone(), to_ext: to_ext.clone(), pipe: None, last_used: Instant::now() });
        let _ = to_ext.send(json!({ "ch": "ctl", "t": "welcome" }).to_string());
    } else {
        let code = hub.request_pairing(conn_id, &ext_id, hello.clone(), to_ext.clone());
        let _ = to_ext.send(
            json!({ "ch": "ctl", "t": "pair_required", "code": code, "expires_in": CODE_TTL.as_secs(),
                    "how": "Approve in SenClaw: send \"pair approve <CODE>\" in a chat, or use Settings → Browser" })
            .to_string(),
        );
    }

    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(text) => {
                let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
                match frame.get("ch").and_then(Value::as_str) {
                    Some("drv") => hub.forward_to_runtime(conn_id, text),
                    Some("ctl") if frame.get("t").and_then(Value::as_str) == Some("ping") => {
                        let _ = to_ext.send(json!({ "ch": "ctl", "t": "pong" }).to_string());
                    }
                    _ => {}
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    hub.unregister(conn_id);
    writer.abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXT: &str = "abcdefghijklmnopabcdefghijklmnop";

    #[test]
    fn origin_and_pairing() {
        // Origin: only a well-formed extension id.
        assert_eq!(origin_extension_id(Some(&format!("chrome-extension://{EXT}"))).unwrap(), EXT);
        assert!(origin_extension_id(Some("https://evil.example")).is_err());
        assert!(origin_extension_id(Some("chrome-extension://short")).is_err());
        assert!(origin_extension_id(Some("chrome-extension://ABCDEFGHIJKLMNOPABCDEFGHIJKLMNOP")).is_err());
        assert!(origin_extension_id(None).is_err());

        // Pairing: a code, then a token only the approved extension receives.
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::new(dir.path().join("extensions.json"));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let hello = json!({ "ch": "ctl", "t": "hello", "ext": EXT, "v": "0.2.0", "scripts": "sha256:x" });
        assert!(!hub.verify_token(EXT, None));
        let code = hub.request_pairing(1, EXT, hello, tx);
        assert_eq!(code.len(), 8);
        assert!(hub.approve_code("ZZZZZZZZ").is_err(), "unknown code");
        assert!(!hub.is_connected());
        assert_eq!(hub.approve_code(&code.to_lowercase()).unwrap(), EXT);
        assert!(hub.approve_code(&code).is_err(), "a code works once");
        let paired: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
        assert_eq!(paired["t"], "paired");
        let token = paired["token"].as_str().unwrap().to_string();
        assert!(hub.verify_token(EXT, Some(&token)));
        assert!(!hub.verify_token(EXT, Some("guess")));
        assert!(!hub.verify_token("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", Some(&token)), "the token is bound to its extension");
        assert!(hub.is_connected(), "the approved connection is live");
        let stored = std::fs::read_to_string(dir.path().join("extensions.json")).unwrap();
        assert!(!stored.contains(&token), "only the hash is stored");

        // Revoking forgets the token.
        assert!(hub.revoke(EXT).unwrap());
        assert!(!hub.verify_token(EXT, Some(&token)));
        assert!(!hub.is_connected());
    }
}
