//! JSON-RPC 2.0 over stdio, newline-delimited — the framing the Agent Client
//! Protocol uses (one JSON object per line; no `Content-Length` headers,
//! unlike LSP).

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// Outbound writer: everything the agent sends goes through one channel so
/// lines never interleave.
#[derive(Clone)]
pub struct Outbound {
    tx: mpsc::UnboundedSender<String>,
    next_id: Arc<AtomicI64>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
}

impl Outbound {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self { tx, next_id: Arc::new(AtomicI64::new(1)), pending: Arc::new(Mutex::new(HashMap::new())) },
            rx,
        )
    }

    fn send_raw(&self, v: &Value) -> Result<()> {
        let line = serde_json::to_string(v)?;
        self.tx.send(line).map_err(|_| anyhow!("stdout closed"))
    }

    pub fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send_raw(&serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    pub fn respond(&self, id: &Value, result: Value) -> Result<()> {
        self.send_raw(&serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }

    pub fn respond_error(&self, id: &Value, code: i64, message: &str) -> Result<()> {
        self.send_raw(&serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
    }

    /// Agent → client request (permission, fs). Resolves with the `result`
    /// value, or an error when the client answered with `error`.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send_raw(&serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        let resp = rx.await.map_err(|_| anyhow!("client disconnected"))?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("{method}: {err}"));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Route a response line from the client to its waiting request.
    /// Returns `false` when it was not for a pending request.
    pub fn resolve(&self, msg: &Value) -> bool {
        let Some(id) = msg.get("id").and_then(|v| v.as_i64()) else { return false };
        if msg.get("method").is_some() {
            return false;
        }
        match self.pending.lock().unwrap().remove(&id) {
            Some(tx) => {
                let _ = tx.send(msg.clone());
                true
            }
            None => false,
        }
    }
}

/// Pump outbound lines to stdout.
pub async fn write_loop(mut rx: mpsc::UnboundedReceiver<String>) {
    let mut out = tokio::io::stdout();
    while let Some(line) = rx.recv().await {
        if out.write_all(line.as_bytes()).await.is_err() {
            break;
        }
        if out.write_all(b"\n").await.is_err() {
            break;
        }
        let _ = out.flush().await;
    }
}

/// One parsed inbound message.
#[derive(Debug, Clone)]
pub enum Inbound {
    Request { id: Value, method: String, params: Value },
    Notification { method: String, params: Value },
    Response(Value),
}

pub fn parse_line(line: &str) -> Option<Inbound> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let method = v.get("method").and_then(|m| m.as_str()).map(str::to_string);
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    match (v.get("id").cloned(), method) {
        (Some(id), Some(method)) => Some(Inbound::Request { id, method, params }),
        (None, Some(method)) => Some(Inbound::Notification { method, params }),
        (Some(_), None) => Some(Inbound::Response(v)),
        (None, None) => None,
    }
}

/// Read stdin line by line into parsed messages.
pub async fn read_loop(tx: mpsc::UnboundedSender<Inbound>) {
    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        match parse_line(&line) {
            Some(m) => {
                if tx.send(m).is_err() {
                    break;
                }
            }
            None => tracing::warn!("[acp] unparseable line ignored"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_three_shapes() {
        match parse_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}"#).unwrap() {
            Inbound::Request { id, method, params } => {
                assert_eq!(id, serde_json::json!(1));
                assert_eq!(method, "initialize");
                assert_eq!(params["protocolVersion"], 1);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(parse_line(r#"{"jsonrpc":"2.0","method":"session/cancel","params":{}}"#).unwrap(), Inbound::Notification { .. }));
        assert!(matches!(parse_line(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#).unwrap(), Inbound::Response(_)));
        assert!(parse_line("not json").is_none());
    }

    #[tokio::test]
    async fn request_resolves_from_a_response_line() {
        let (out, mut rx) = Outbound::new();
        let out2 = out.clone();
        let waiter = tokio::spawn(async move { out2.request("session/request_permission", serde_json::json!({})).await });
        let sent = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&sent).unwrap();
        let id = v["id"].as_i64().unwrap();
        assert!(out.resolve(&serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"outcome":{"outcome":"selected","optionId":"allow-once"}}})));
        let r = waiter.await.unwrap().unwrap();
        assert_eq!(r["outcome"]["optionId"], "allow-once");
    }
}
