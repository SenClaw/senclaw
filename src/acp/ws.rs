//! WebSocket client to the running daemon's gateway — the same frames the
//! web UI speaks (`connect`, `subscribe`, `message`, `permission:response`,
//! `agent:control`), so `senclaw acp` is a thin translator with no engine of
//! its own.

use anyhow::{anyhow, Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone)]
pub struct DaemonWs {
    tx: mpsc::UnboundedSender<Value>,
}

impl DaemonWs {
    /// Connect and authenticate. Every frame the daemon sends afterwards is
    /// delivered on the returned receiver.
    pub async fn connect(url: &str, token: Option<&str>) -> Result<(Self, mpsc::UnboundedReceiver<Value>)> {
        let (stream, _) = tokio_tungstenite::connect_async(url)
            .await
            .with_context(|| format!("connecting to {url} — is the daemon running? (`senclaw web`)"))?;
        let (mut sink, mut source) = stream.split();
        let mut hello = serde_json::json!({ "type": "connect" });
        if let Some(t) = token {
            hello["token"] = Value::String(t.to_string());
        }
        sink.send(Message::Text(hello.to_string())).await?;
        // Wait for auth:ok before anything else.
        loop {
            let Some(frame) = source.next().await else { return Err(anyhow!("gateway closed during connect")) };
            if let Ok(Message::Text(t)) = frame {
                let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                match v.get("type").and_then(|t| t.as_str()) {
                    Some("auth:ok") => break,
                    Some("auth:error") => {
                        return Err(anyhow!(
                            "gateway refused the connection: {} (set SENCLAW_WS_TOKEN)",
                            v.get("message").and_then(|m| m.as_str()).unwrap_or("")
                        ))
                    }
                    _ => continue,
                }
            }
        }
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
        let (in_tx, in_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            while let Some(v) = out_rx.recv().await {
                if sink.send(Message::Text(v.to_string())).await.is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            while let Some(frame) = source.next().await {
                match frame {
                    Ok(Message::Text(t)) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            if in_tx.send(v).is_err() {
                                break;
                            }
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
        });
        Ok((Self { tx: out_tx }, in_rx))
    }

    pub fn send(&self, v: Value) -> Result<()> {
        self.tx.send(v).map_err(|_| anyhow!("gateway connection closed"))
    }

    pub fn subscribe(&self, jid: &str) -> Result<()> {
        self.send(serde_json::json!({ "type": "subscribe", "groupJid": jid }))
    }

    /// Register a code chat pinned to `cwd`. The daemon answers with
    /// `group:registered`; callers do not need to wait for it before sending.
    pub fn register_code_group(&self, jid: &str, name: &str, cwd: &str) -> Result<()> {
        self.send(serde_json::json!({
            "type": "register:group",
            "jid": jid,
            "folder": "main",
            "name": name,
            "groupType": "code",
            "requiresTrigger": false,
            "allowedWorkDirs": [cwd],
        }))
    }

    pub fn send_message(&self, jid: &str, text: &str) -> Result<()> {
        self.send(serde_json::json!({ "type": "message", "groupJid": jid, "text": text }))
    }

    pub fn permission_response(&self, request_id: &str, option_key: &str) -> Result<()> {
        self.send(serde_json::json!({ "type": "permission:response", "requestId": request_id, "optionKey": option_key }))
    }

    pub fn stop(&self, jid: &str) -> Result<()> {
        self.send(serde_json::json!({ "type": "agent:control", "groupJid": jid, "action": "stop" }))
    }
}
