//! Feishu/Lark WebSocket long-connection event listener.
//!
//! Protocol: protobuf-encoded `Frame` messages over WebSocket.
//! Auth: POST `/callback/ws/endpoint` with AppID/AppSecret → WS URL + ClientConfig.
//! Heartbeat: ping/pong control frames (method=0) at server-configured interval.
//! Events: data frames (method=1) with JSON payload containing `header.event_type`.
//!
//! References:
//!   - `pbbp2.proto` from the lark-websocket-protobuf crate
//!   - open-lark `ws_client/client.rs`

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, error, info, trace, warn};

// ===== Minimal protobuf Frame codec =====
// Hand-rolled for the pbbp2 Frame/Header schema to avoid a prost build dependency.

/// Protobuf wire types.
#[allow(dead_code)]
mod wire_type {
    pub const VARINT: u8 = 0;
    pub const LEN_DELIM: u8 = 2;
}

fn tag(field_num: u8, wt: u8) -> u8 {
    (field_num << 3) | wt
}

/// Write a varint to `buf`.
fn put_varint(buf: &mut Vec<u8>, mut val: u64) {
    while val >= 0x80 {
        buf.push((val as u8) | 0x80);
        val >>= 7;
    }
    buf.push(val as u8);
}

/// Read a varint starting at `*pos`, advancing the cursor. Returns `None` on overflow.
fn get_varint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut val: u64 = 0;
    let mut shift = 0;
    while *pos < data.len() {
        let byte = data[*pos];
        *pos += 1;
        val |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(val);
        }
        shift += 7;
        if shift >= 64 {
            return None; // overflow
        }
    }
    None // truncated
}

/// Write a length-delimited field: tag, then varint length, then bytes.
fn put_len_delimited(buf: &mut Vec<u8>, field_num: u8, data: &[u8]) {
    put_varint(buf, tag(field_num, wire_type::LEN_DELIM) as u64);
    put_varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

/// Write a varint field.
fn put_varint_field(buf: &mut Vec<u8>, field_num: u8, val: u64) {
    put_varint(buf, tag(field_num, wire_type::VARINT) as u64);
    put_varint(buf, val);
}

/// Protobuf key-value header.
#[derive(Debug, Clone)]
pub struct PbHeader {
    pub key: String,
    pub value: String,
}

impl PbHeader {
    fn encode(&self, buf: &mut Vec<u8>) {
        let mut inner = Vec::new();
        put_len_delimited(&mut inner, 1, self.key.as_bytes());
        put_len_delimited(&mut inner, 2, self.value.as_bytes());
        put_len_delimited(buf, 5, &inner); // field 5 on Frame
    }
}

/// Protobuf Frame — the wire format for Feishu WS.
///
/// Schema (proto2, package pbbp2):
/// ```text
/// message Frame {
///   required uint64 SeqID = 1;
///   required uint64 LogID = 2;
///   required int32  service = 3;
///   required int32  method  = 4;
///   repeated Header headers = 5;
///   optional string payload_encoding = 6;
///   optional string payload_type     = 7;
///   optional bytes  payload          = 8;
///   optional string LogIDNew         = 9;
/// }
/// message Header {
///   required string key   = 1;
///   required string value = 2;
/// }
/// ```
#[derive(Debug, Clone)]
pub struct PbFrame {
    pub seq_id: u64,
    pub log_id: u64,
    pub service: i32,
    pub method: i32,
    pub headers: Vec<PbHeader>,
    pub payload_encoding: Option<String>,
    pub payload_type: Option<String>,
    pub payload: Option<Vec<u8>>,
    pub log_id_new: Option<String>,
}

impl PbFrame {
    /// Build a ping frame for the given service ID.
    pub fn ping(service_id: i32, seq_id: u64, log_id: u64) -> Self {
        Self {
            seq_id,
            log_id,
            service: service_id,
            method: 0, // control
            headers: vec![PbHeader {
                key: "type".into(),
                value: "ping".into(),
            }],
            payload_encoding: None,
            payload_type: None,
            payload: None,
            log_id_new: None,
        }
    }

    /// Build a data frame (for auth handshake if needed).
    pub fn data(
        service_id: i32,
        seq_id: u64,
        log_id: u64,
        headers: Vec<PbHeader>,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            seq_id,
            log_id,
            service: service_id,
            method: 1, // data
            headers,
            payload_encoding: Some("json".into()),
            payload_type: None,
            payload: Some(payload),
            log_id_new: None,
        }
    }

    /// Get a header value by key.
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.key == key)
            .map(|h| h.value.as_str())
    }

    /// Encode to protobuf wire format.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(256);
        put_varint_field(&mut buf, 1, self.seq_id);
        put_varint_field(&mut buf, 2, self.log_id);
        // service: zigzag? No — int32 uses varint in proto2 (signed but wire type 0)
        put_varint_field(&mut buf, 3, self.service as u64);
        put_varint_field(&mut buf, 4, self.method as u64);
        for h in &self.headers {
            h.encode(&mut buf);
        }
        if let Some(ref s) = self.payload_encoding {
            put_len_delimited(&mut buf, 6, s.as_bytes());
        }
        if let Some(ref s) = self.payload_type {
            put_len_delimited(&mut buf, 7, s.as_bytes());
        }
        if let Some(ref p) = self.payload {
            put_len_delimited(&mut buf, 8, p);
        }
        if let Some(ref s) = self.log_id_new {
            put_len_delimited(&mut buf, 9, s.as_bytes());
        }
        buf
    }

    /// Decode from protobuf wire format.
    pub fn decode(data: &[u8]) -> Result<Self> {
        let mut seq_id = 0u64;
        let mut log_id = 0u64;
        let mut service = 0i32;
        let mut method = 0i32;
        let mut headers: Vec<PbHeader> = Vec::new();
        let mut payload_encoding = None;
        let mut payload_type = None;
        let mut payload = None;
        let mut log_id_new = None;

        let mut pos = 0;
        let len = data.len();

        while pos < len {
            let tag_val =
                get_varint(data, &mut pos).ok_or_else(|| anyhow::anyhow!("truncated tag"))?;
            let field_num = (tag_val >> 3) as u8;
            let wt = (tag_val & 0x07) as u8;

            match (field_num, wt) {
                (1, wire_type::VARINT) => {
                    seq_id = get_varint(data, &mut pos).context("SeqID")?;
                }
                (2, wire_type::VARINT) => {
                    log_id = get_varint(data, &mut pos).context("LogID")?;
                }
                (3, wire_type::VARINT) => {
                    service = get_varint(data, &mut pos).context("service")? as i32;
                }
                (4, wire_type::VARINT) => {
                    method = get_varint(data, &mut pos).context("method")? as i32;
                }
                (5, wire_type::LEN_DELIM) => {
                    let hdr_len = get_varint(data, &mut pos).context("header len")? as usize;
                    if pos + hdr_len > len {
                        anyhow::bail!("header payload truncated");
                    }
                    let hdr = decode_header(&data[pos..pos + hdr_len])?;
                    headers.push(hdr);
                    pos += hdr_len;
                }
                (6, wire_type::LEN_DELIM) => {
                    let s_len =
                        get_varint(data, &mut pos).context("payload_encoding len")? as usize;
                    if pos + s_len > len {
                        anyhow::bail!("payload_encoding truncated");
                    }
                    payload_encoding =
                        Some(String::from_utf8_lossy(&data[pos..pos + s_len]).into_owned());
                    pos += s_len;
                }
                (7, wire_type::LEN_DELIM) => {
                    let s_len = get_varint(data, &mut pos).context("payload_type len")? as usize;
                    if pos + s_len > len {
                        anyhow::bail!("payload_type truncated");
                    }
                    payload_type =
                        Some(String::from_utf8_lossy(&data[pos..pos + s_len]).into_owned());
                    pos += s_len;
                }
                (8, wire_type::LEN_DELIM) => {
                    let b_len = get_varint(data, &mut pos).context("payload len")? as usize;
                    if pos + b_len > len {
                        anyhow::bail!("payload truncated");
                    }
                    payload = Some(data[pos..pos + b_len].to_vec());
                    pos += b_len;
                }
                (9, wire_type::LEN_DELIM) => {
                    let s_len = get_varint(data, &mut pos).context("LogIDNew len")? as usize;
                    if pos + s_len > len {
                        anyhow::bail!("LogIDNew truncated");
                    }
                    log_id_new =
                        Some(String::from_utf8_lossy(&data[pos..pos + s_len]).into_owned());
                    pos += s_len;
                }
                _ => {
                    // Skip unknown field
                    if wt == wire_type::VARINT {
                        let _ = get_varint(data, &mut pos);
                    } else if wt == wire_type::LEN_DELIM {
                        let skip = get_varint(data, &mut pos).unwrap_or(0) as usize;
                        if pos + skip <= len {
                            pos += skip;
                        } else {
                            break;
                        }
                    } else {
                        break; // unknown wire type, bail
                    }
                }
            }
        }

        Ok(Self {
            seq_id,
            log_id,
            service,
            method,
            headers,
            payload_encoding,
            payload_type,
            payload,
            log_id_new,
        })
    }
}

fn decode_header(data: &[u8]) -> Result<PbHeader> {
    let mut key = String::new();
    let mut value = String::new();
    let mut pos = 0;
    let len = data.len();
    while pos < len {
        let tag_val = get_varint(data, &mut pos).context("header tag")?;
        let field_num = (tag_val >> 3) as u8;
        let wt = (tag_val & 0x07) as u8;
        match (field_num, wt) {
            (1, wire_type::LEN_DELIM) => {
                let s_len = get_varint(data, &mut pos).context("header key len")? as usize;
                if pos + s_len > len {
                    anyhow::bail!("header key truncated");
                }
                key = String::from_utf8_lossy(&data[pos..pos + s_len]).into_owned();
                pos += s_len;
            }
            (2, wire_type::LEN_DELIM) => {
                let s_len = get_varint(data, &mut pos).context("header value len")? as usize;
                if pos + s_len > len {
                    anyhow::bail!("header value truncated");
                }
                value = String::from_utf8_lossy(&data[pos..pos + s_len]).into_owned();
                pos += s_len;
            }
            _ => {
                if wt == wire_type::VARINT {
                    let _ = get_varint(data, &mut pos);
                } else if wt == wire_type::LEN_DELIM {
                    let skip = get_varint(data, &mut pos).unwrap_or(0) as usize;
                    if pos + skip <= len {
                        pos += skip;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
    }
    Ok(PbHeader { key, value })
}

// ===== WS endpoint types =====

#[derive(Debug, Deserialize)]
struct WsEndpointResponse {
    #[serde(default)]
    code: i32,
    #[serde(default)]
    msg: Option<String>,
    data: Option<WsEndpointData>,
}

#[derive(Debug, Deserialize)]
struct WsEndpointData {
    #[serde(rename = "URL")]
    url: Option<String>,
    #[serde(rename = "ClientConfig")]
    client_config: Option<ClientConfig>,
}

#[derive(Debug, Deserialize, Clone)]
struct ClientConfig {
    #[serde(rename = "ReconnectCount", default)]
    #[allow(dead_code)]
    reconnect_count: i32,
    #[serde(rename = "ReconnectInterval", default)]
    #[allow(dead_code)]
    reconnect_interval: i32,
    #[serde(rename = "ReconnectNonce", default)]
    #[allow(dead_code)]
    reconnect_nonce: i32,
    #[serde(rename = "PingInterval", default)]
    ping_interval: i32,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            reconnect_count: 3,
            reconnect_interval: 5,
            reconnect_nonce: 3,
            ping_interval: 30,
        }
    }
}

// ===== WebSocket listener =====

/// Active WebSocket connection state for one Feishu app.
pub struct WsConnection {
    /// Channel to signal shutdown.
    cancel_tx: tokio::sync::oneshot::Sender<()>,
    /// Handle for the background task.
    #[allow(dead_code)]
    handle: tokio::task::JoinHandle<()>,
}

/// Start a WebSocket event listener for a Feishu app.
///
/// Spawns a background task that:
/// 1. Fetches the WS endpoint URL via `/callback/ws/endpoint`
/// 2. Connects via `tokio-tungstenite`
/// 3. Sends periodic ping frames at the server-configured interval
/// 4. Decodes incoming data frames and calls `on_message` for each event
pub async fn start_event_listener(
    base_url: &str,
    app_id: &str,
    app_secret: &str,
    http: reqwest::Client,
    on_message: Arc<dyn Fn(Vec<u8>) + Send + Sync + 'static>,
) -> Result<WsConnection> {
    let base_url = base_url.trim_end_matches('/').to_string();
    let app_id = app_id.to_string();
    let app_secret = app_secret.to_string();

    // 1. Fetch WS endpoint
    let endpoint = fetch_ws_endpoint(&http, &base_url, &app_id, &app_secret).await?;
    let ws_url = endpoint
        .url
        .ok_or_else(|| anyhow::anyhow!("No WS URL in endpoint response"))?;
    let client_config = endpoint.client_config.unwrap_or_default();

    info!(
        "[FeishuWS:{app_id}] Connecting to {ws_url} (ping_interval={}s)",
        client_config.ping_interval
    );

    // 2. Extract service_id from URL query params
    let service_id: i32 = ws_url
        .split('?')
        .nth(1)
        .and_then(|qs| {
            qs.split('&')
                .find(|p| p.starts_with("service_id="))
                .and_then(|p| p.split('=').nth(1))
        })
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // 3. Connect
    let ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
        max_message_size: Some(2 * 1024 * 1024), // 2 MB
        max_frame_size: Some(2 * 1024 * 1024),
        ..Default::default()
    };

    let (conn, _resp) =
        tokio_tungstenite::connect_async_with_config(&ws_url, Some(ws_config), false)
            .await
            .context("WS connect")?;

    info!("[FeishuWS:{app_id}] Connected");

    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    let ping_interval = Duration::from_secs(client_config.ping_interval as u64);

    let handle = tokio::spawn(ws_event_loop(
        conn,
        service_id,
        ping_interval,
        client_config,
        on_message,
        cancel_rx,
        app_id.clone(),
    ));

    Ok(WsConnection { cancel_tx, handle })
}

impl WsConnection {
    /// Shut down the WebSocket listener.
    pub fn shutdown(self) {
        let _ = self.cancel_tx.send(());
        // Don't wait — the task will exit on its own
    }
}

async fn fetch_ws_endpoint(
    http: &reqwest::Client,
    base_url: &str,
    app_id: &str,
    app_secret: &str,
) -> Result<WsEndpointData> {
    let body = serde_json::json!({
        "AppID": app_id,
        "AppSecret": app_secret,
    });

    let resp = http
        .post(format!("{base_url}/callback/ws/endpoint"))
        .header("locale", "zh")
        .json(&body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .context("fetch WS endpoint")?;

    let ws_resp: WsEndpointResponse = resp.json().await.context("parse WS endpoint response")?;

    if ws_resp.code != 0 {
        anyhow::bail!(
            "WS endpoint error: code={}, msg={:?}",
            ws_resp.code,
            ws_resp.msg
        );
    }

    ws_resp
        .data
        .ok_or_else(|| anyhow::anyhow!("No data in WS endpoint response"))
}

async fn ws_event_loop(
    conn: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    service_id: i32,
    ping_interval: Duration,
    _client_config: ClientConfig,
    on_message: Arc<dyn Fn(Vec<u8>) + Send + Sync + 'static>,
    mut cancel_rx: tokio::sync::oneshot::Receiver<()>,
    app_id: String,
) {
    let (mut sink, mut stream) = conn.split();
    let mut ping_tick = tokio::time::interval(ping_interval);
    ping_tick.tick().await; // skip first immediate tick
    let mut ping_time = tokio::time::Instant::now();
    let mut seq_counter: u64 = 1;
    const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(120);

    loop {
        tokio::select! {
            biased;

            _ = &mut cancel_rx => {
                info!("[FeishuWS:{app_id}] Shutting down");
                let _ = sink.send(WsMessage::Close(None)).await;
                return;
            }

            item = stream.next() => {
                match item {
                    Some(Ok(msg)) => {
                        if let WsMessage::Ping(data) = msg {
                            ping_time = tokio::time::Instant::now();
                            let _ = sink.send(WsMessage::Pong(data)).await;
                            continue;
                        }
                        if msg.is_close() {
                            info!("[FeishuWS:{app_id}] Server closed connection");
                            return;
                        }
                        if let WsMessage::Binary(data) = msg {
                            match PbFrame::decode(&data) {
                                Ok(frame) => {
                                    trace!("[FeishuWS:{app_id}] Frame: method={}, headers={:?}",
                                        frame.method, frame.headers);
                                    match frame.method {
                                        0 => {
                                            // Control frame
                                            if let Some(ty) = frame.header("type") {
                                                if ty == "pong" {
                                                    if let Some(ref payload) = frame.payload {
                                                        if let Ok(cfg) = serde_json::from_slice::<ClientConfig>(payload) {
                                                            ping_tick = tokio::time::interval(
                                                                Duration::from_secs(cfg.ping_interval as u64)
                                                            );
                                                            ping_tick.reset_after(
                                                                Duration::from_secs(cfg.ping_interval as u64)
                                                            );
                                                            debug!(
                                                                "[FeishuWS:{app_id}] Pong received, updated ping_interval={}s",
                                                                cfg.ping_interval
                                                            );
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        1 => {
                                            // Data frame — forward event payload
                                            if let Some(ref payload) = frame.payload {
                                                on_message(payload.clone());
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                                Err(e) => {
                                    warn!("[FeishuWS:{app_id}] Failed to decode frame: {e}");
                                }
                            }
                        }
                    }
                    Some(Err(e)) => {
                        error!("[FeishuWS:{app_id}] WS error: {e}");
                        return;
                    }
                    None => {
                        info!("[FeishuWS:{app_id}] WS stream ended");
                        return;
                    }
                }
            }

            _ = ping_tick.tick() => {
                // Heartbeat check
                if ping_time.elapsed() > HEARTBEAT_TIMEOUT {
                    warn!("[FeishuWS:{app_id}] Heartbeat timeout, closing");
                    let _ = sink.send(WsMessage::Close(None)).await;
                    return;
                }
                let frame = PbFrame::ping(service_id, seq_counter, seq_counter);
                seq_counter = seq_counter.wrapping_add(1);
                trace!("[FeishuWS:{app_id}] Sending ping (seq={seq_counter})");
                if sink.send(WsMessage::Binary(frame.encode())).await.is_err() {
                    error!("[FeishuWS:{app_id}] Failed to send ping");
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pb_frame_roundtrip() {
        let frame = PbFrame {
            seq_id: 1,
            log_id: 2,
            service: 3,
            method: 1,
            headers: vec![
                PbHeader {
                    key: "type".into(),
                    value: "ping".into(),
                },
                PbHeader {
                    key: "k2".into(),
                    value: "v2".into(),
                },
            ],
            payload_encoding: Some("json".into()),
            payload_type: None,
            payload: Some(b"hello".to_vec()),
            log_id_new: None,
        };
        let encoded = frame.encode();
        let decoded = PbFrame::decode(&encoded).expect("decode");
        assert_eq!(decoded.seq_id, 1);
        assert_eq!(decoded.log_id, 2);
        assert_eq!(decoded.service, 3);
        assert_eq!(decoded.method, 1);
        assert_eq!(decoded.headers.len(), 2);
        assert_eq!(decoded.headers[0].key, "type");
        assert_eq!(decoded.headers[0].value, "ping");
        assert_eq!(decoded.payload_encoding.as_deref(), Some("json"));
        assert_eq!(decoded.payload.as_deref(), Some(b"hello".as_slice()));
    }

    #[test]
    fn test_pb_frame_no_optional_fields() {
        let frame = PbFrame {
            seq_id: 42,
            log_id: 99,
            service: 1,
            method: 0,
            headers: vec![],
            payload_encoding: None,
            payload_type: None,
            payload: None,
            log_id_new: None,
        };
        let encoded = frame.encode();
        let decoded = PbFrame::decode(&encoded).expect("decode");
        assert_eq!(decoded.seq_id, 42);
        assert_eq!(decoded.log_id, 99);
        assert_eq!(decoded.service, 1);
        assert_eq!(decoded.method, 0);
        assert!(decoded.headers.is_empty());
        assert!(decoded.payload.is_none());
    }

    #[test]
    fn test_pb_frame_ping() {
        let ping = PbFrame::ping(5, 1, 1);
        assert_eq!(ping.service, 5);
        assert_eq!(ping.method, 0);
        assert_eq!(ping.header("type"), Some("ping"));
        let encoded = ping.encode();
        let decoded = PbFrame::decode(&encoded).expect("decode ping");
        assert_eq!(decoded.service, 5);
        assert_eq!(decoded.method, 0);
    }
}
