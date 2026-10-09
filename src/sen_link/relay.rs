//! sen-link through the SenClaw hub, for peers that cannot reach each other
//! directly (NAT, no public URL).
//!
//! Every invite made while `senLink.hubUrl` is set mints its own hub channel
//! and a fresh 32-byte key. The hub admits at most two connections per
//! channel, so a channel is exactly one pair of daemons; frames on it are
//! AES-GCM encrypted with the pair's key and the hub relays ciphertext only.
//! The invite travels as one connection string (`senlink:…`) holding the hub
//! URL, the channel, its access token, the key and the code.
//!
//! On the channel each side is both client and server: a [`Frame::Req`] names
//! `/a2a` or `/a2a/pair` with a body, and is answered with a [`Frame::Resp`].
//! Requests go through the same code as the HTTP routes ([`server::rpc_reply`],
//! [`server::pair_with`]), so peer tokens are checked exactly as over HTTP —
//! and a peer token is only accepted on its own peer's channel.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{oneshot, watch};

use super::store::RelayInfo;
use super::{server, LinkState};
use crate::clawhub::relay_client::{RelayClient, RelayInboundMessage, RelayInboundPayload, RelayMessageHandler};
use crate::util::crypto::Crypto;

/// How long a request waits for the channel to come up.
const CONNECT_WAIT: Duration = Duration::from_secs(15);
/// How long a request waits for its answer.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// How often the set of open channels is reconciled with the store.
const RECONCILE_EVERY: Duration = Duration::from_secs(30);
/// A request older or newer than this (sender clock vs ours) is refused, and
/// ids seen within it are remembered: a hub replaying a captured frame gets
/// neither a stale one through nor a fresh one twice.
const REPLAY_WINDOW_MS: i64 = 120_000;
/// Largest answer sent back over a channel. The hub drops any connection that
/// sends a frame over 8 MiB, and encryption + base64 + JSON add ~40 %.
const MAX_RESP_BYTES: usize = 5 * 1024 * 1024;
/// Requests served at once per channel; the rest wait.
const MAX_CONCURRENT_SERVES: usize = 8;
const CONNECT_PREFIX: &str = "senlink:";

/// One message on a pair channel (encrypted before it leaves the process).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Frame {
    Req {
        id: String,
        /// Sender's clock, ms since the epoch (replay window).
        ts: i64,
        path: String,
        /// The caller's peer token (absent for `/a2a/pair`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
        body: Value,
    },
    Resp {
        id: String,
        status: u16,
        body: Value,
    },
}

/// What an invite connection string carries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Connect {
    pub hub: String,
    pub cid: String,
    pub tok: String,
    pub key: String,
    pub code: String,
}

impl Connect {
    pub fn relay(&self) -> RelayInfo {
        RelayInfo {
            hub_url: self.hub.clone(),
            channel_id: self.cid.clone(),
            access_token: self.tok.clone(),
            key: self.key.clone(),
        }
    }
}

pub fn encode_connect(relay: &RelayInfo, code: &str) -> String {
    let c = Connect {
        hub: relay.hub_url.clone(),
        cid: relay.channel_id.clone(),
        tok: relay.access_token.clone(),
        key: relay.key.clone(),
        code: code.to_string(),
    };
    let raw = serde_json::to_vec(&c).unwrap_or_default();
    format!("{CONNECT_PREFIX}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw))
}

pub fn decode_connect(s: &str) -> Result<Connect> {
    let body = s.trim().strip_prefix(CONNECT_PREFIX).ok_or_else(|| anyhow!("not a senlink: connection string"))?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(body.trim_end_matches('='))
        .context("connection string is not valid base64")?;
    let c: Connect = serde_json::from_slice(&raw).context("connection string is not valid")?;
    if !(c.hub.starts_with("http://") || c.hub.starts_with("https://")) {
        bail!("connection string names no hub URL");
    }
    Ok(c)
}

/// A fresh random channel key, base64.
pub fn new_key() -> String {
    use rand::RngCore;
    let mut k = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut k);
    base64::engine::general_purpose::STANDARD.encode(k)
}

/// Mint a channel on the hub (`POST /v1/channels/register`).
pub async fn register_channel(http: &reqwest::Client, hub_url: &str) -> Result<RelayInfo> {
    let v: Value = http
        .post(format!("{}/v1/channels/register", hub_url.trim_end_matches('/')))
        .send()
        .await
        .with_context(|| format!("reach hub {hub_url}"))?
        .error_for_status()
        .context("hub refused to register a channel")?
        .json()
        .await
        .context("hub reply is not JSON")?;
    let (Some(cid), Some(tok)) = (v["channel_id"].as_str(), v["access_token"].as_str()) else {
        bail!("hub reply carried no channel");
    };
    Ok(RelayInfo { hub_url: hub_url.trim_end_matches('/').to_string(), channel_id: cid.into(), access_token: tok.into(), key: new_key() })
}

/// One open pair channel.
pub struct Link {
    pub info: RelayInfo,
    sender_id: String,
    crypto: Crypto,
    client: tokio::sync::RwLock<Option<RelayClient>>,
    up: watch::Sender<bool>,
    pending: Mutex<HashMap<String, oneshot::Sender<(u16, Value)>>>,
    /// Request ids served within the replay window, with their timestamps.
    seen: Mutex<HashMap<String, i64>>,
    serving: tokio::sync::Semaphore,
    stopped: AtomicBool,
    /// Wakes the supervisor out of its wait so it drops the client.
    stop: tokio_util::sync::CancellationToken,
}

impl Link {
    async fn send(&self, frame: &Frame) -> Result<()> {
        let text = serde_json::to_string(frame)?;
        let guard = self.client.read().await;
        let client = guard.as_ref().ok_or_else(|| anyhow!("hub channel is not connected"))?;
        client.send_message(&text).await
    }

    /// Send a request and wait for its answer.
    pub async fn request(&self, path: &str, token: Option<&str>, body: Value) -> Result<(u16, Value)> {
        let mut up = self.up.subscribe();
        if !*up.borrow() {
            tokio::time::timeout(CONNECT_WAIT, up.wait_for(|v| *v))
                .await
                .map_err(|_| anyhow!("hub channel did not connect within {}s", CONNECT_WAIT.as_secs()))?
                .map_err(|_| anyhow!("hub channel closed"))?;
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), tx);
        let frame = Frame::Req {
            id: id.clone(),
            ts: chrono::Utc::now().timestamp_millis(),
            path: path.into(),
            token: token.map(str::to_string),
            body,
        };
        if let Err(e) = self.send(&frame).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(_)) => bail!("hub channel closed before the peer answered"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                bail!("peer did not answer within {}s", CALL_TIMEOUT.as_secs())
            }
        }
    }

    /// Whether a request is fresh: inside the window and not seen before.
    /// Records it when it is.
    fn accept(&self, id: &str, ts: i64, now: i64) -> bool {
        if (now - ts).abs() > REPLAY_WINDOW_MS {
            return false;
        }
        let mut seen = self.seen.lock().unwrap();
        seen.retain(|_, t| (now - *t).abs() <= REPLAY_WINDOW_MS);
        seen.insert(id.to_string(), ts).is_none()
    }

    fn decrypt(&self, nonce: &str, ct: &str, tag: &str) -> Result<String> {
        let d = |s: &str| base64::engine::general_purpose::STANDARD.decode(s).context("bad base64");
        let plain = self.crypto.decrypt(&d(nonce)?, &d(ct)?, &d(tag)?)?;
        Ok(String::from_utf8(plain)?)
    }
}

/// The open channels of one daemon, keyed by channel id.
#[derive(Default)]
pub struct Links {
    map: Mutex<HashMap<String, Arc<Link>>>,
}

impl Links {
    fn get(&self, channel: &str) -> Option<Arc<Link>> {
        self.map.lock().unwrap().get(channel).cloned()
    }

    /// Channels currently open (for status and tests).
    pub fn open(&self) -> Vec<String> {
        self.map.lock().unwrap().keys().cloned().collect()
    }

    fn stop(&self, channel: &str) {
        if let Some(l) = self.map.lock().unwrap().remove(channel) {
            l.stopped.store(true, Ordering::SeqCst);
            // The supervisor wakes, drops the client, and that closes the socket.
            l.stop.cancel();
            l.up.send_replace(false);
        }
    }
}

/// The link for `info`, opening (and supervising) it when needed.
pub fn ensure(st: &Arc<LinkState>, info: &RelayInfo) -> Result<Arc<Link>> {
    if let Some(l) = st.links.get(&info.channel_id) {
        return Ok(l);
    }
    let crypto = Crypto::new_from_b64(&info.key).context("bad channel key")?;
    let (up, _) = watch::channel(false);
    let link = Arc::new(Link {
        info: info.clone(),
        sender_id: format!("senlink-{}", uuid::Uuid::new_v4().simple()),
        crypto,
        client: tokio::sync::RwLock::new(None),
        up,
        pending: Mutex::new(HashMap::new()),
        seen: Mutex::new(HashMap::new()),
        serving: tokio::sync::Semaphore::new(MAX_CONCURRENT_SERVES),
        stopped: AtomicBool::new(false),
        stop: tokio_util::sync::CancellationToken::new(),
    });
    {
        let mut map = st.links.map.lock().unwrap();
        if let Some(existing) = map.get(&info.channel_id) {
            return Ok(Arc::clone(existing));
        }
        map.insert(info.channel_id.clone(), Arc::clone(&link));
    }
    tokio::spawn(supervise(Arc::downgrade(st), Arc::clone(&link)));
    Ok(link)
}

/// Keep `link` connected until it is stopped or the daemon state goes away.
async fn supervise(st: Weak<LinkState>, link: Arc<Link>) {
    let mut backoff = 2u64;
    loop {
        if link.stopped.load(Ordering::SeqCst) || st.strong_count() == 0 {
            break;
        }
        let key = link.crypto.get_key();
        let handler = handler(st.clone(), Arc::downgrade(&link));
        match RelayClient::connect(
            link.info.hub_url.clone(),
            link.info.channel_id.clone(),
            link.sender_id.clone(),
            link.info.access_token.clone(),
            key,
            Some(handler),
        )
        .await
        {
            Ok(client) => {
                backoff = 2;
                *link.client.write().await = Some(client);
                link.up.send_replace(true);
                tracing::info!(channel = %link.info.channel_id, "[sen-link] hub channel up");
                {
                    let guard = link.client.read().await;
                    if let Some(c) = guard.as_ref() {
                        tokio::select! {
                            _ = c.closed() => {}
                            _ = link.stop.cancelled() => {}
                        }
                    }
                }
                link.up.send_replace(false);
                link.client.write().await.take();
                // Requests in flight will never be answered on the old socket.
                link.pending.lock().unwrap().clear();
            }
            Err(e) => tracing::warn!(channel = %link.info.channel_id, "[sen-link] hub connect failed: {e:#}"),
        }
        if link.stopped.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
            _ = link.stop.cancelled() => break,
        }
        backoff = (backoff * 2).min(60);
    }
}

fn handler(st: Weak<LinkState>, link: Weak<Link>) -> RelayMessageHandler {
    Arc::new(move |m: RelayInboundMessage| {
        let RelayInboundPayload::Encrypted { nonce_b64, ciphertext_b64, tag_b64 } = m.payload else {
            return;
        };
        let Some(link) = link.upgrade() else { return };
        if link.stopped.load(Ordering::SeqCst) {
            return;
        }
        let frame = match link.decrypt(&nonce_b64, &ciphertext_b64, &tag_b64).map(|t| serde_json::from_str::<Frame>(&t)) {
            Ok(Ok(f)) => f,
            _ => {
                tracing::warn!(channel = %link.info.channel_id, "[sen-link] dropped an unreadable hub frame");
                return;
            }
        };
        match frame {
            Frame::Resp { id, status, body } => {
                if let Some(tx) = link.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send((status, body));
                }
            }
            Frame::Req { id, ts, path, token, body } => {
                if !link.accept(&id, ts, chrono::Utc::now().timestamp_millis()) {
                    tracing::warn!(channel = %link.info.channel_id, "[sen-link] refused a stale or replayed hub request");
                    return;
                }
                let st = st.clone();
                tokio::spawn(async move {
                    let Ok(_permit) = link.serving.acquire().await else { return };
                    let Some(st) = st.upgrade() else { return };
                    let (mut status, mut body) = serve(&st, &link.info.channel_id, &path, token.as_deref(), body);
                    if serde_json::to_vec(&body).map_or(0, |b| b.len()) > MAX_RESP_BYTES {
                        status = 413;
                        body = json!({ "error": "answer too large for the hub relay" });
                    }
                    let _ = link.send(&Frame::Resp { id, status, body }).await;
                });
            }
        }
    })
}

/// Answer a request that arrived on `channel` — the relay twin of the
/// `/a2a` and `/a2a/pair` HTTP routes.
pub fn serve(st: &LinkState, channel: &str, path: &str, token: Option<&str>, body: Value) -> (u16, Value) {
    if !st.settings().enabled {
        return (404, json!({ "error": "sen-link is disabled" }));
    }
    match path {
        "/a2a/pair" => match serde_json::from_value::<server::PairRequest>(body) {
            Ok(req) => match server::pair_with(st, req, Some(channel)) {
                Ok(v) => (200, v),
                Err((code, e)) => (code.as_u16(), json!({ "error": e })),
            },
            Err(e) => (400, json!({ "error": format!("bad pair request: {e}") })),
        },
        "/a2a" => {
            // A peer token counts only on that peer's own channel.
            let peer = token
                .and_then(|t| st.store.by_inbound_token(t))
                .filter(|p| p.relay.as_ref().is_some_and(|r| r.channel_id == channel));
            match peer {
                Some(p) => (200, server::rpc_reply(st, &p, &body)),
                None => (401, json!({ "error": "unauthorized" })),
            }
        }
        _ => (404, json!({ "error": format!("no route {path}") })),
    }
}

/// Open the channels the store wants (live relay invites, relay peers) and
/// close the rest, now and every [`RECONCILE_EVERY`] — so removing a peer,
/// an invite expiring, or turning sen-link off closes its socket.
pub fn start(st: &Arc<LinkState>) {
    let weak = Arc::downgrade(st);
    tokio::spawn(async move {
        loop {
            let Some(st) = weak.upgrade() else { break };
            reconcile(&st);
            drop(st);
            tokio::time::sleep(RECONCILE_EVERY).await;
        }
    });
}

pub fn reconcile(st: &Arc<LinkState>) {
    let wanted: Vec<RelayInfo> = if st.settings().enabled {
        st.store.relay_channels(crate::kanban::api::now())
    } else {
        Vec::new()
    };
    for ch in st.links.open() {
        if !wanted.iter().any(|w| w.channel_id == ch) {
            st.links.stop(&ch);
        }
    }
    for info in &wanted {
        if let Err(e) = ensure(st, info) {
            tracing::warn!(channel = %info.channel_id, "[sen-link] cannot open hub channel: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_strings_round_trip_and_reject_junk() {
        let r = RelayInfo { hub_url: "https://hub.example".into(), channel_id: "c1".into(), access_token: "t1".into(), key: new_key() };
        let s = encode_connect(&r, "K7MQ2XRP");
        assert!(s.starts_with("senlink:"));
        let c = decode_connect(&s).unwrap();
        assert_eq!(c.relay(), r);
        assert_eq!(c.code, "K7MQ2XRP");
        assert!(decode_connect("http://x").is_err());
        assert!(decode_connect("senlink:@@@").is_err());
    }

    #[tokio::test]
    async fn replayed_or_stale_requests_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let st = Arc::new(LinkState::new_at(
            t.path().join("config.json"),
            Arc::new(crate::kanban::db::Db::open(&t.path().join("k.db")).unwrap()),
            super::super::store::PeerStore::new(t.path().join("p.json")),
        ));
        // A hub nobody listens on: the supervisor just keeps retrying.
        let info = RelayInfo { hub_url: "http://127.0.0.1:9".into(), channel_id: "c".into(), access_token: "t".into(), key: new_key() };
        let link = ensure(&st, &info).unwrap();
        let now = 1_000_000_000_000;
        assert!(link.accept("a", now, now));
        assert!(!link.accept("a", now, now + 1), "same id twice");
        assert!(!link.accept("b", now - REPLAY_WINDOW_MS - 1, now), "too old");
        assert!(!link.accept("c", now + REPLAY_WINDOW_MS + 1, now), "from the future");
        assert!(link.accept("a", now + 3 * REPLAY_WINDOW_MS, now + 3 * REPLAY_WINDOW_MS), "forgotten after the window");
        st.links.stop("c");
    }

    #[test]
    fn frames_have_a_stable_wire_shape() {
        let f = Frame::Req { id: "1".into(), ts: 5, path: "/a2a".into(), token: None, body: json!({}) };
        let v = serde_json::to_value(&f).unwrap();
        assert_eq!(v["kind"], "req");
        assert!(v.get("token").is_none());
        let back: Frame = serde_json::from_value(json!({"kind": "resp", "id": "1", "status": 200, "body": {"ok": true}})).unwrap();
        assert_eq!(back, Frame::Resp { id: "1".into(), status: 200, body: json!({"ok": true}) });
    }
}
