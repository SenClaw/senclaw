use crate::util::crypto::Crypto;
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use futures::{SinkExt, StreamExt};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{mpsc, Notify};
use tokio::time::{self, Duration, MissedTickBehavior};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use tracing::info;

pub type RelayMessageHandler = Arc<dyn Fn(RelayInboundMessage) + Send + Sync + 'static>;

#[derive(Debug, Clone)]
pub struct RelayInboundMessage {
    pub channel_id: String,
    pub sender_id: String,
    pub message_id: String,
    pub payload: RelayInboundPayload,
}

#[derive(Debug, Clone)]
pub enum RelayInboundPayload {
    Encrypted {
        nonce_b64: String,
        ciphertext_b64: String,
        tag_b64: String,
    },
    Control {
        control_type: i32,
        metadata: String,
    },
    Ping,
    Pong,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RelayFrame {
    #[serde(rename = "type")]
    frame_type: String,
    channel_id: String,
    sender_id: String,
    timestamp: i64,
    message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    control_type: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    nonce: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ciphertext: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tag: Option<String>,
}

pub struct RelayClient {
    crypto: Arc<Crypto>,
    channel_id: String,
    sender_id: String,
    outbound_tx: mpsc::Sender<RelayFrame>,
    /// Fired once when either the inbound or outbound socket task exits (i.e. the
    /// underlying WebSocket dropped). Lets a supervisor reconnect — the socket has
    /// no self-healing of its own.
    closed: Arc<Notify>,
}

impl RelayClient {
    /// Resolves when the underlying WebSocket has dropped. A fresh `connect()` is
    /// required afterwards; this client instance is dead once it fires.
    pub async fn closed(&self) {
        self.closed.notified().await;
    }
}

impl RelayClient {
    pub async fn connect(
        hub_url: String,
        channel_id: String,
        sender_id: String,
        access_token: String,
        encryption_key: [u8; 32],
        handler: Option<RelayMessageHandler>,
    ) -> Result<Self> {
        let ws_url = build_ws_url(&hub_url, &channel_id, &access_token)?;
        info!("Connecting to WebSocket relay at {}...", ws_url);
        let (ws_stream, _) = connect_async(ws_url.as_str())
            .await
            .map_err(|e| anyhow!("Failed to connect to relay websocket: {e}"))?;
        info!("WebSocket channel established.");

        let crypto = Arc::new(Crypto::new(encryption_key));
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<RelayFrame>(100);
        let (mut ws_write, mut ws_read) = ws_stream.split();
        let closed = Arc::new(Notify::new());
        let closed_inbound = Arc::clone(&closed);
        let closed_outbound = Arc::clone(&closed);

        info!(
            "Establishing WebSocket stream for channel_id: {}",
            channel_id
        );
        // Send initial handshake message to unblock the stream
        let handshake_msg = RelayFrame {
            frame_type: "ping".to_string(),
            channel_id: channel_id.clone(),
            sender_id: sender_id.clone(),
            timestamp: chrono::Utc::now().timestamp_millis(),
            message_id: format!("handshake-{}", uuid::Uuid::new_v4()),
            control_type: None,
            metadata: None,
            nonce: None,
            ciphertext: None,
            tag: None,
        };
        ws_write
            .send(WsMessage::Text(serde_json::to_string(&handshake_msg)?))
            .await?;

        let cid_clone = channel_id.clone();
        let sid_clone = sender_id.clone();
        let handler_inbound = handler.clone();
        let pong_tx = outbound_tx.clone();
        tokio::spawn(async move {
            info!(
                "Inbound relay stream processor started for channel: {}",
                cid_clone
            );
            while let Some(frame) = ws_read.next().await {
                let frame = match frame {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!("Inbound relay stream read error for {}: {}", cid_clone, e);
                        break;
                    }
                };
                let msg = match frame {
                    WsMessage::Text(text) => match serde_json::from_str::<RelayFrame>(&text) {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("Failed to decode inbound relay JSON: {}", e);
                            continue;
                        }
                    },
                    WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
                    WsMessage::Binary(_) => continue,
                    WsMessage::Close(_) => break,
                    _ => continue,
                };
                if msg.frame_type == "encrypted" {
                    info!(
                        "Relay stream received message: {} from {}",
                        msg.message_id, msg.sender_id
                    );
                }
                if msg.channel_id != cid_clone {
                    tracing::warn!(
                        "Received message for wrong channel_id: expected {}, got {}",
                        cid_clone,
                        msg.channel_id
                    );
                    continue;
                }

                let inbound_payload = match msg.frame_type.as_str() {
                    "encrypted" => RelayInboundPayload::Encrypted {
                        nonce_b64: msg.nonce.unwrap_or_default(),
                        ciphertext_b64: msg.ciphertext.unwrap_or_default(),
                        tag_b64: msg.tag.unwrap_or_default(),
                    },
                    "control" => RelayInboundPayload::Control {
                        control_type: msg.control_type.unwrap_or_default(),
                        metadata: msg.metadata.unwrap_or_default(),
                    },
                    "ping" => {
                        let pong = RelayFrame {
                            frame_type: "pong".to_string(),
                            channel_id: msg.channel_id.clone(),
                            // Reply as current client identity, not the sender we received from.
                            sender_id: sid_clone.clone(),
                            timestamp: chrono::Utc::now().timestamp_millis(),
                            message_id: format!("pong-{}", uuid::Uuid::new_v4()),
                            control_type: None,
                            metadata: None,
                            nonce: None,
                            ciphertext: None,
                            tag: None,
                        };
                        let _ = pong_tx.try_send(pong);
                        RelayInboundPayload::Ping
                    }
                    "pong" => RelayInboundPayload::Pong,
                    _ => continue,
                };

                if let Some(ref h) = handler_inbound {
                    h(RelayInboundMessage {
                        channel_id: msg.channel_id,
                        sender_id: msg.sender_id,
                        message_id: msg.message_id,
                        payload: inbound_payload,
                    });
                }
            }
            tracing::warn!("Inbound relay stream closed for channel: {}", cid_clone);
            closed_inbound.notify_one();
        });

        let cid_outbound = channel_id.clone();
        let sid_outbound = sender_id.clone();
        tokio::spawn(async move {
            // 20s < the hub's ~50s idle cutoff. The protocol-level Ping alone is
            // not enough — the hub (and any proxy in front of it) only counts
            // application frames, so send a JSON "ping" frame as well.
            let mut ping_tick = time::interval(Duration::from_secs(20));
            ping_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ping_tick.tick() => {
                        if let Err(e) = ws_write.send(WsMessage::Ping(Vec::new())).await {
                            tracing::warn!(
                                "Relay websocket ping failed for channel {}: {}",
                                cid_outbound,
                                e
                            );
                            break;
                        }
                        let keepalive = RelayFrame {
                            frame_type: "ping".to_string(),
                            channel_id: cid_outbound.clone(),
                            sender_id: sid_outbound.clone(),
                            timestamp: chrono::Utc::now().timestamp_millis(),
                            message_id: format!("keepalive-{}", uuid::Uuid::new_v4()),
                            control_type: None,
                            metadata: None,
                            nonce: None,
                            ciphertext: None,
                            tag: None,
                        };
                        match serde_json::to_string(&keepalive) {
                            Ok(payload) => {
                                if let Err(e) = ws_write.send(WsMessage::Text(payload)).await {
                                    tracing::warn!(
                                        "Relay keepalive ping failed for channel {}: {}",
                                        cid_outbound,
                                        e
                                    );
                                    break;
                                }
                            }
                            Err(e) => tracing::error!("Failed to encode relay keepalive: {}", e),
                        }
                    }
                    maybe_msg = outbound_rx.recv() => {
                        let Some(msg) = maybe_msg else { break; };
                        let payload = match serde_json::to_string(&msg) {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::error!("Failed to encode relay outbound JSON: {}", e);
                                continue;
                            }
                        };
                        if let Err(e) = ws_write.send(WsMessage::Text(payload)).await {
                            tracing::warn!(
                                "Outbound relay stream closed for channel {}: {}",
                                cid_outbound,
                                e
                            );
                            continue;
                        }
                    }
                }
            }
            closed_outbound.notify_one();
        });

        Ok(Self {
            crypto,
            channel_id,
            sender_id,
            outbound_tx,
            closed,
        })
    }

    pub async fn send_message(&self, text: &str) -> Result<()> {
        let (nonce, ciphertext, tag) = self.crypto.encrypt(text.as_bytes())?;

        info!(
            "Sending relay message ({} bytes encrypted)",
            ciphertext.len()
        );
        let msg = RelayFrame {
            frame_type: "encrypted".to_string(),
            channel_id: self.channel_id.clone(),
            sender_id: self.sender_id.clone(),
            timestamp: chrono::Utc::now().timestamp_millis(),
            message_id: uuid::Uuid::new_v4().to_string(),
            control_type: None,
            metadata: None,
            nonce: Some(base64::engine::general_purpose::STANDARD.encode(nonce)),
            ciphertext: Some(base64::engine::general_purpose::STANDARD.encode(ciphertext)),
            tag: Some(base64::engine::general_purpose::STANDARD.encode(tag)),
        };

        self.outbound_tx.send(msg).await?;
        Ok(())
    }

    pub async fn send_control(&self, control_type: i32, metadata: String) -> Result<()> {
        let msg = RelayFrame {
            frame_type: "control".to_string(),
            channel_id: self.channel_id.clone(),
            sender_id: self.sender_id.clone(),
            timestamp: chrono::Utc::now().timestamp_millis(),
            message_id: uuid::Uuid::new_v4().to_string(),
            control_type: Some(control_type),
            metadata: Some(metadata),
            nonce: None,
            ciphertext: None,
            tag: None,
        };

        self.outbound_tx.send(msg).await?;
        Ok(())
    }

    pub fn decrypt_payload(
        &self,
        nonce_b64: &str,
        ciphertext_b64: &str,
        tag_b64: &str,
    ) -> Result<String> {
        let nonce = base64::engine::general_purpose::STANDARD
            .decode(nonce_b64)
            .context("invalid nonce b64")?;
        let ciphertext = base64::engine::general_purpose::STANDARD
            .decode(ciphertext_b64)
            .context("invalid ciphertext b64")?;
        let tag = base64::engine::general_purpose::STANDARD
            .decode(tag_b64)
            .context("invalid tag b64")?;
        let plaintext = self.crypto.decrypt(&nonce, &ciphertext, &tag)?;
        String::from_utf8(plaintext).map_err(|e| anyhow!("Invalid UTF-8: {}", e))
    }
}

fn build_ws_url(hub_url: &str, channel_id: &str, access_token: &str) -> Result<Url> {
    let trimmed = hub_url.trim();
    let base = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{}", trimmed)
    };
    let mut url = Url::parse(&base).context("invalid relay URL")?;

    // Relay endpoint is fixed on the same host/port as hub API.
    url.set_path("/v1/relay/ws");
    match url.scheme() {
        "http" => {
            url.set_scheme("ws")
                .map_err(|_| anyhow!("failed to set ws scheme"))?;
        }
        "https" => {
            url.set_scheme("wss")
                .map_err(|_| anyhow!("failed to set wss scheme"))?;
        }
        "ws" | "wss" => {}
        s => return Err(anyhow!("unsupported relay URL scheme: {}", s)),
    }

    url.query_pairs_mut()
        .append_pair("channel_id", channel_id)
        .append_pair("access_token", access_token);
    Ok(url)
}
