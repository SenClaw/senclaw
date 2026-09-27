//! App Connector channel adapter using WebSocket relay.
//!
//! JID format: `app:{channel_id}:user:{sender_id}`
//!
//! Control message types (ControlMessage.type):
//!   0  PING            — server keepalive
//!   1  PONG
//!   2  ACK
//!   3  TYPING_START    — server → app
//!   4  TYPING_STOP     — server → app
//!   5  DISCONNECT
//!   6  AGENT_LIST_REQ  — app → server: request list of available agents
//!   7  AGENT_LIST_RESP — server → app: JSON array of agents
//!   8  AGENT_SELECT    — app → server: bind sender to a specific agent folder
//!   9  HISTORY_REQ     — app → server: request message history
//!  10  HISTORY_RESP    — server → app: JSON array of stored messages
//!  11  API_REQ         — app → server: tunnel a REST call over the relay
//!  12  API_RESP        — server → app: REST result for an API_REQ
//!  13  API_EVENT       — server → app: pushed event (server-initiated)
//!  14–19 session management (see the CTRL_SESSION_* constants)
//!  20  FILE_SEND       — server → app: a file the agent sent

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tracing::{error, info, warn};

use crate::channels::{Channel, MessageCallback, MetadataCallback};
use crate::clawhub::relay_client::{
    RelayClient, RelayInboundMessage, RelayInboundPayload, RelayMessageHandler,
};
use crate::types::{ChatType, IncomingMessage};
use chrono;

pub const CTRL_AGENT_LIST_REQ: i32 = 6;
pub const CTRL_AGENT_LIST_RESP: i32 = 7;
pub const CTRL_AGENT_SELECT: i32 = 8;
pub const CTRL_HISTORY_REQ: i32 = 9;
pub const CTRL_HISTORY_RESP: i32 = 10;
/// app → server: tunnel a REST call. metadata = `{requestId, method, path, body?}`
pub const CTRL_API_REQ: i32 = 11;
/// server → app: REST result. metadata = `{requestId, status, body}`
pub const CTRL_API_RESP: i32 = 12;
/// server → app: pushed event (no request). metadata = `{topic, data}`
pub const CTRL_API_EVENT: i32 = 13;
/// app → server: request the sender's session list. metadata = `{}`
pub const CTRL_SESSION_LIST_REQ: i32 = 14;
/// server → app: JSON array `[{jid,name,folder,groupType,lastActivity,active}]`
pub const CTRL_SESSION_LIST_RESP: i32 = 15;
/// app → server: create a new session group. metadata = `{name, folder, mode?}`
pub const CTRL_SESSION_CREATE: i32 = 16;
/// app → server: rename/rebind a session. metadata = `{jid, name?, folder?}`
pub const CTRL_SESSION_UPDATE: i32 = 17;
/// app → server: delete a session. metadata = `{jid}`
pub const CTRL_SESSION_DELETE: i32 = 18;
/// app → server: set the active session (routes new messages). metadata = `{jid, folder?, mode?}`
pub const CTRL_SESSION_SELECT: i32 = 19;
/// server → app: a file the agent sent.
/// metadata = `{jid, name, mime, size, caption?, dataUrl}`.
/// A client that does not know this type ignores the frame, so an old app
/// misses the file rather than breaking on it.
pub const CTRL_FILE_SEND: i32 = 20;

/// Largest file pushed over the relay. The payload is base64 inside one
/// control frame, so it arrives ~33 % larger; 8 MiB keeps a frame comfortably
/// under what the hub and every WebSocket client accept. Bigger files are
/// refused with a message naming the limit rather than silently truncated.
pub const MAX_RELAY_FILE_BYTES: usize = 8 * 1024 * 1024;

/// Called when the app sends a control frame that requires server handling.
/// Arguments: (sender_id, control_type, metadata_json)
pub type ControlCallback = Arc<dyn Fn(String, i32, String) + Send + Sync + 'static>;

pub struct AppChannel {
    hub_url: String,
    channel_id: String,
    access_token: String,
    encryption_key: [u8; 32],
    handlers: Arc<RwLock<Vec<MessageCallback>>>,
    meta_handlers: Arc<RwLock<Vec<MetadataCallback>>>,
    control_handler: Arc<RwLock<Option<ControlCallback>>>,
    client: Mutex<Option<Arc<RelayClient>>>,
    connected: AtomicBool,
    /// Per-sender active session jid (`sender_id → group jid`). New messages
    /// from a sender are filed under their active session's group; absence =
    /// the default single-session jid `app:<cid>:user:<sender>`.
    active_sessions: Arc<RwLock<std::collections::HashMap<String, String>>>,
}

impl AppChannel {
    pub fn new(
        hub_url: String,
        channel_id: String,
        access_token: String,
        encryption_key: [u8; 32],
    ) -> Self {
        Self {
            hub_url,
            channel_id,
            access_token,
            encryption_key,
            handlers: Arc::new(RwLock::new(Vec::new())),
            meta_handlers: Arc::new(RwLock::new(Vec::new())),
            control_handler: Arc::new(RwLock::new(None)),
            client: Mutex::new(None),
            connected: AtomicBool::new(false),
            active_sessions: Arc::new(RwLock::new(std::collections::HashMap::new())),
        }
    }

    /// The relay channel id string used to build `app:<cid>:user:<sender>` jids.
    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    /// The default (single-session) jid for a sender.
    pub fn default_session_jid(&self, sender_id: &str) -> String {
        format!("app:{}:user:{}", self.channel_id, sender_id)
    }

    /// The sender's currently active session jid, or their default jid.
    pub fn active_session_for(&self, sender_id: &str) -> String {
        self.active_sessions
            .read()
            .unwrap()
            .get(sender_id)
            .cloned()
            .unwrap_or_else(|| self.default_session_jid(sender_id))
    }

    /// Route this sender's subsequent messages to session [jid].
    pub fn set_active_session(&self, sender_id: &str, jid: &str) {
        self.active_sessions
            .write()
            .unwrap()
            .insert(sender_id.to_string(), jid.to_string());
    }

    pub fn parse_jid(jid: &str) -> Option<(String, String)> {
        let parts: Vec<&str> = jid.split(':').collect();
        if parts.len() >= 4 && parts[0] == "app" && parts[2] == "user" {
            Some((parts[1].to_string(), parts[3].to_string()))
        } else {
            None
        }
    }

    /// Register a handler for app-initiated control frames (types 6–10).
    pub fn set_control_handler(&self, handler: ControlCallback) {
        *self.control_handler.write().unwrap() = Some(handler);
    }

    /// Send a control frame to the app (e.g. AGENT_LIST_RESP, HISTORY_RESP).
    pub async fn send_control(&self, control_type: i32, metadata: String) -> Result<()> {
        let lock = self.client.lock().await;
        if let Some(ref client) = *lock {
            client.send_control(control_type, metadata).await
        } else {
            Err(anyhow!("AppChannel not connected"))
        }
    }

    /// Runs relay handshake and inbound processing. Idempotent if already connected.
    async fn establish_relay(this: &AppChannel) -> Result<()> {
        if this.is_connected() {
            return Ok(());
        }

        let hub_url = this.hub_url.clone();
        let channel_id = this.channel_id.clone();
        let access_token = this.access_token.clone();
        let encryption_key = this.encryption_key;

        let handlers = Arc::clone(&this.handlers);
        let control_handler = Arc::clone(&this.control_handler);
        let active_sessions = Arc::clone(&this.active_sessions);
        let cid = channel_id.clone();

        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel(100);

        let handler: RelayMessageHandler = Arc::new(move |msg: RelayInboundMessage| {
            let _ = msg_tx.try_send(msg);
        });

        let client = RelayClient::connect(
            hub_url,
            channel_id.clone(),
            "senclaw-daemon".to_string(),
            access_token,
            encryption_key,
            Some(handler),
        )
        .await?;

        let client_arc = Arc::new(client);
        let client_for_inbound = Arc::clone(&client_arc);

        tokio::spawn(async move {
            while let Some(msg) = msg_rx.recv().await {
                match msg.payload {
                    RelayInboundPayload::Encrypted {
                        nonce_b64,
                        ciphertext_b64,
                        tag_b64,
                    } => {
                        info!(
                            "[AppChannel] Received message: {} from {}",
                            msg.message_id, msg.sender_id
                        );
                        match client_for_inbound.decrypt_payload(
                            &nonce_b64,
                            &ciphertext_b64,
                            &tag_b64,
                        ) {
                            Ok(text) => {
                                info!("[AppChannel] Decrypted from {}: {}", msg.sender_id, text);
                                // File under the sender's active session (multi-session);
                                // falls back to the default single-session jid.
                                let chat_jid = active_sessions
                                    .read()
                                    .unwrap()
                                    .get(&msg.sender_id)
                                    .cloned()
                                    .unwrap_or_else(|| {
                                        format!("app:{}:user:{}", cid, msg.sender_id)
                                    });
                                let incoming = IncomingMessage {
                                    id: msg.message_id,
                                    chat_jid: chat_jid.clone(),
                                    sender_name: msg.sender_id.clone(),
                                    sender_jid: format!("app:{}:user:{}", cid, msg.sender_id),
                                    content: text,
                                    timestamp: chrono::Utc::now().to_rfc3339(),
                                    is_from_me: false,
                                    chat_type: ChatType::Private,
                                    mentions_bot_username: Some(true),
                                    bot_token: None,
                                    native_msg_id: None,
                                    // The relay carries images as `data:` URLs inside the
                                    // message body, which InputBuilder picks up from the text.
                                    attachments: Vec::new(),
                                };
                                let h_list = handlers.read().unwrap();
                                for h in h_list.iter() {
                                    h(incoming.clone());
                                }
                            }
                            Err(e) => error!("[AppChannel] Decrypt failed: {e}"),
                        }
                    }
                    RelayInboundPayload::Control {
                        control_type,
                        metadata,
                    } => {
                        let ctrl_type = control_type;
                        // Server-sent keepalives (PING/PONG/ACK) — ignore silently.
                        // App-initiated types (≥ AGENT_LIST_REQ) are forwarded to the handler.
                        if ctrl_type >= CTRL_AGENT_LIST_REQ {
                            info!(
                                "[AppChannel] Control type={} from {}",
                                ctrl_type, msg.sender_id
                            );
                            if let Some(ref h) = *control_handler.read().unwrap() {
                                h(msg.sender_id.clone(), ctrl_type, metadata.clone());
                            }
                        }
                    }
                    RelayInboundPayload::Ping | RelayInboundPayload::Pong => {}
                }
            }
        });

        *this.client.lock().await = Some(client_arc);
        this.connected.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Connect to the hub relay without blocking daemon startup, then **supervise** the
    /// connection forever: the underlying WebSocket has no self-healing, so when it drops
    /// (Cloudflare idle cutoff, hub redeploy, network blip) this loop reconnects with
    /// exponential backoff. Without this the daemon silently falls off the channel after the
    /// first drop while the app keeps reconnecting alone — i.e. "daemon stops responding".
    pub fn connect_nonblocking(this: Arc<AppChannel>) {
        info!(
            "[AppChannel] Scheduling relay connection for channel {} at {}",
            this.channel_id, this.hub_url
        );
        tokio::spawn(async move {
            let mut backoff_secs = 2u64;
            let mut logged_first_failure = false;
            loop {
                match Self::establish_relay(this.as_ref()).await {
                    Ok(()) => {
                        info!(
                            "[AppChannel] Connected to relay for channel {}",
                            this.channel_id
                        );
                        backoff_secs = 2;
                        logged_first_failure = false;

                        // Block until the socket drops, then fall through to reconnect.
                        // Grab the close future, releasing the client lock before awaiting.
                        let closed = {
                            let lock = this.client.lock().await;
                            lock.as_ref().map(Arc::clone)
                        };
                        if let Some(client) = closed {
                            client.closed().await;
                        }
                        warn!(
                            "[AppChannel] Relay connection dropped for channel {}; reconnecting",
                            this.channel_id
                        );
                        // Reset live state so establish_relay re-runs the handshake.
                        this.connected.store(false, Ordering::SeqCst);
                        *this.client.lock().await = None;
                        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                    }
                    Err(e) => {
                        if !logged_first_failure {
                            warn!(
                                "[AppChannel] Relay not reachable ({e}); retrying in background for channel {}",
                                this.channel_id
                            );
                            logged_first_failure = true;
                        } else {
                            error!(
                                "[AppChannel] Failed to connect to relay: {e}; retrying in {backoff_secs}s"
                            );
                        }
                        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
                        backoff_secs = (backoff_secs * 2).min(60);
                    }
                }
            }
        });
    }
}

#[async_trait]
impl Channel for AppChannel {
    fn id(&self) -> &'static str {
        "app"
    }

    async fn connect(&self) -> Result<()> {
        // Single attempt; does not retry (use `Arc<AppChannel>` + `connect_nonblocking` for prod).
        Self::establish_relay(self).await
    }

    async fn disconnect(&self) -> Result<()> {
        *self.client.lock().await = None;
        self.connected.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    async fn send_message(
        &self,
        chat_jid: &str,
        text: &str,
        _bot_token: Option<&str>,
    ) -> Result<()> {
        let lock = self.client.lock().await;
        if let Some(ref client) = *lock {
            info!("[AppChannel] Sending to {}: {}", chat_jid, text);
            client.send_message(text).await
        } else {
            Err(anyhow!("AppChannel not connected"))
        }
    }

    async fn set_typing(
        &self,
        _chat_jid: &str,
        typing: bool,
        _bot_token: Option<&str>,
    ) -> Result<()> {
        let lock = self.client.lock().await;
        if let Some(ref client) = *lock {
            let ctrl = if typing { 3 } else { 4 };
            client.send_control(ctrl, String::new()).await
        } else {
            Err(anyhow!("AppChannel not connected"))
        }
    }

    async fn send_file(
        &self,
        chat_jid: &str,
        file_path: &str,
        caption: Option<&str>,
        _bot_token: Option<&str>,
    ) -> Result<()> {
        use base64::{engine::general_purpose::STANDARD, Engine as _};

        let bytes = tokio::fs::read(file_path)
            .await
            .map_err(|e| anyhow!("read {file_path}: {e}"))?;
        if bytes.len() > MAX_RELAY_FILE_BYTES {
            return Err(anyhow!(
                "{file_path} is {} bytes; the relay carries at most {MAX_RELAY_FILE_BYTES}",
                bytes.len()
            ));
        }
        let name = std::path::Path::new(file_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file")
            .to_string();
        let mime = crate::channels::media::sniff_mime(&bytes)
            .unwrap_or_else(|| crate::channels::media::mime_for_name(&name));
        let meta = serde_json::json!({
            "jid": chat_jid,
            "name": name,
            "mime": mime,
            "size": bytes.len(),
            "caption": caption,
            "dataUrl": format!("data:{mime};base64,{}", STANDARD.encode(&bytes)),
        })
        .to_string();

        info!(
            "[AppChannel] Sending file {name} ({} bytes, {mime}) to {chat_jid}",
            bytes.len()
        );
        self.send_control(CTRL_FILE_SEND, meta).await
    }

    fn owns_jid(&self, chat_jid: &str) -> bool {
        if let Some((cid, _)) = Self::parse_jid(chat_jid) {
            cid == self.channel_id
        } else {
            false
        }
    }

    fn on_message(&self, handler: MessageCallback) {
        self.handlers.write().unwrap().push(handler);
    }

    fn on_metadata(&self, handler: MetadataCallback) {
        self.meta_handlers.write().unwrap().push(handler);
    }
}

// Allow Arc<AppChannel> to be used in the channels Vec<Box<dyn Channel>>.
#[async_trait]
impl Channel for Arc<AppChannel> {
    fn id(&self) -> &'static str {
        self.as_ref().id()
    }
    async fn connect(&self) -> Result<()> {
        AppChannel::connect_nonblocking(Arc::clone(self));
        Ok(())
    }
    async fn disconnect(&self) -> Result<()> {
        self.as_ref().disconnect().await
    }
    fn is_connected(&self) -> bool {
        self.as_ref().is_connected()
    }
    async fn send_message(&self, jid: &str, text: &str, bt: Option<&str>) -> Result<()> {
        self.as_ref().send_message(jid, text, bt).await
    }
    async fn set_typing(&self, jid: &str, typing: bool, bt: Option<&str>) -> Result<()> {
        self.as_ref().set_typing(jid, typing, bt).await
    }
    async fn send_file(
        &self,
        jid: &str,
        path: &str,
        cap: Option<&str>,
        bt: Option<&str>,
    ) -> Result<()> {
        self.as_ref().send_file(jid, path, cap, bt).await
    }
    fn owns_jid(&self, jid: &str) -> bool {
        self.as_ref().owns_jid(jid)
    }
    fn on_message(&self, h: MessageCallback) {
        self.as_ref().on_message(h)
    }
    fn on_metadata(&self, h: MetadataCallback) {
        self.as_ref().on_metadata(h)
    }
}
