//! External MCP client with multi-transport support.
//!
//! Supports connecting to user-configured MCP servers via:
//!   - **stdio** — spawn a subprocess, JSON-RPC over stdin/stdout
//!   - **sse**  — Server-Sent Events (GET for events, POST for requests)
//!   - **http** — Streamable HTTP (POST with optional streaming response)
//!
//! Mirrors sema-core `MCPClient` which wraps `@modelcontextprotocol/sdk`.

use std::collections::HashMap;
use std::process::Stdio;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};
use uuid::Uuid;

// Re-use the existing McpToolInfo from the parent module
use super::client::McpToolInfo;
use crate::util::text::truncate_on_char_boundary;

// ---------------------------------------------------------------------------
// JSON-RPC types (same protocol as client.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    #[allow(dead_code)]
    id: Option<u64>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    #[allow(dead_code)]
    code: i64,
    message: String,
}

// ---------------------------------------------------------------------------
// Transport trait
// ---------------------------------------------------------------------------

/// Generic asynchronous MCP transport layer.
#[async_trait::async_trait]
trait McpTransport: Send + Sync {
    /// Send a JSON-RPC request and return the result.
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value>;

    /// Check if the transport is still alive.
    fn is_alive(&mut self) -> bool;

    /// Disconnect / clean up.
    async fn disconnect(&mut self) -> Result<()>;
}

// ---------------------------------------------------------------------------
// stdio transport
// ---------------------------------------------------------------------------

struct StdioTransport {
    name: String,
    child: Child,
    reader: BufReader<tokio::process::ChildStdout>,
    writer: tokio::process::ChildStdin,
    next_id: u64,
}

impl StdioTransport {
    async fn spawn(
        name: &str,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> Result<Self> {
        info!(
            "MCP-external stdio spawn: {name} ({command} {})",
            args.join(" ")
        );

        let mut cmd = Command::new(command);
        cmd.args(args);
        cmd.envs(env);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit());
        cmd.kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn MCP stdio server: {name}"))?;

        let stdout = child.stdout.take().context("MCP stdout pipe")?;
        let stdin = child.stdin.take().context("MCP stdin pipe")?;

        Ok(Self {
            name: name.to_string(),
            child,
            reader: BufReader::new(stdout),
            writer: stdin,
            next_id: 1,
        })
    }
}

#[async_trait::async_trait]
impl McpTransport for StdioTransport {
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };

        let mut body = serde_json::to_vec(&req)?;
        body.push(b'\n');
        self.writer.write_all(&body).await?;

        let mut line = String::new();
        self.reader.read_line(&mut line).await?;

        let resp: JsonRpcResponse = serde_json::from_str(&line)
            .with_context(|| format!("MCP JSON-RPC parse error ({}) : {line}", self.name))?;

        if let Some(err) = resp.error {
            bail!("MCP error ({} / {}): {}", self.name, method, err.message);
        }

        Ok(resp.result.unwrap_or(Value::Null))
    }

    fn is_alive(&mut self) -> bool {
        // Try a non-blocking wait — if the process has exited, it's dead.
        match self.child.try_wait() {
            Ok(Some(status)) => {
                warn!(
                    "MCP stdio subprocess {} exited with {:?}",
                    self.name, status
                );
                false
            }
            Ok(None) => true,
            Err(_) => false,
        }
    }

    async fn disconnect(&mut self) -> Result<()> {
        info!("MCP stdio {} — disconnecting", self.name);
        // kill_on_drop will handle cleanup.
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SSE transport
// ---------------------------------------------------------------------------

/// Finds the blank line that ends the first frame in `buf`: returns the frame's
/// length and where the next frame starts. SSE lets a line end in LF or CRLF,
/// so the blank line after a `\n` is `\n` or `\r\n`. A CRLF frame keeps the `\r`
/// of its last line, which the `trim` of each `data:` line removes.
fn frame_end(buf: &[u8]) -> Option<(usize, usize)> {
    (0..buf.len()).filter(|&i| buf[i] == b'\n').find_map(|i| {
        let rest = &buf[i + 1..];
        if rest.starts_with(b"\n") {
            Some((i, i + 2))
        } else if rest.starts_with(b"\r\n") {
            Some((i, i + 3))
        } else {
            None
        }
    })
}

struct SseTransport {
    name: String,
    base_url: String, // e.g. http://localhost:8080
    client: reqwest::Client,
    event_rx: tokio::sync::mpsc::UnboundedReceiver<JsonRpcResponse>,
    next_id: u64,
    _task: tokio::task::JoinHandle<()>,
    session_id: Option<String>,
}

impl SseTransport {
    async fn connect(name: &str, url: &str, headers: &HashMap<String, String>) -> Result<Self> {
        let base_url = url.trim_end_matches('/').to_string();
        info!("MCP-external SSE connect: {name} -> {base_url}");

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3600)) // long-lived
            .build()
            .context("build reqwest client for SSE")?;

        // Build a request to the SSE endpoint
        let sse_url = format!("{}/sse", base_url);
        let mut req_builder = client.get(&sse_url);
        for (k, v) in headers {
            req_builder = req_builder.header(k, v);
        }

        let response = req_builder
            .send()
            .await
            .with_context(|| format!("SSE connect failed: {sse_url}"))?;

        if !response.status().is_success() {
            bail!("SSE connect HTTP {} for {}", response.status(), sse_url);
        }

        // Extract session ID from response headers (Mcp-Session-Id)
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .or_else(|| response.headers().get("Mcp-Session-Id"))
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let stream = response.bytes_stream();
        let task_name = name.to_string();

        let handle = tokio::spawn(async move {
            use futures::StreamExt;
            tokio::pin!(stream);

            // Bytes, not text: a chunk can end inside a multi-byte character,
            // so nothing is decoded until its frame is complete. `\n` is never
            // part of one, which makes the byte search below safe.
            let mut buf: Vec<u8> = Vec::new();
            let _current_event: Option<String> = None;

            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        buf.extend_from_slice(&bytes);

                        // Parse SSE frames from the buffer
                        while let Some((len, next)) = frame_end(&buf) {
                            let frame = String::from_utf8_lossy(&buf[..len]).into_owned();
                            buf.drain(..next);

                            // Parse SSE frame lines
                            let mut data = String::new();

                            for line in frame.lines() {
                                if let Some(rest) = line.strip_prefix("data:") {
                                    if !data.is_empty() {
                                        data.push('\n');
                                    }
                                    data.push_str(rest.trim());
                                }
                                // "event:" prefix intentionally ignored — we only need data lines
                            }

                            if !data.is_empty() {
                                // Try to parse as JSON-RPC response
                                match serde_json::from_str::<JsonRpcResponse>(&data) {
                                    Ok(resp) => {
                                        debug!(
                                            "MCP SSE {} — received message id={:?}",
                                            task_name, resp.id
                                        );
                                        if tx.send(resp).is_err() {
                                            // Receiver dropped, stop
                                            return;
                                        }
                                    }
                                    Err(_) => {
                                        // Not JSON-RPC — could be a notification or error
                                        debug!(
                                            "MCP SSE {} — non-JSON-RPC event: {}",
                                            task_name,
                                            truncate_on_char_boundary(&data, 200)
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("MCP SSE {} — stream error: {e}", task_name);
                        break;
                    }
                }
            }
            info!("MCP SSE {} — event stream ended", task_name);
        });

        Ok(Self {
            name: name.to_string(),
            base_url,
            client,
            event_rx: rx,
            next_id: 1,
            _task: handle,
            session_id,
        })
    }

    /// POST a JSON-RPC request to the message endpoint.
    async fn post_message(
        client: &reqwest::Client,
        base_url: &str,
        session_id: Option<&str>,
        body: &[u8],
    ) -> Result<reqwest::Response> {
        let msg_url = format!("{}/message", base_url);
        let mut req = client.post(&msg_url).body(body.to_vec());

        req = req.header("Content-Type", "application/json");
        if let Some(sid) = session_id {
            req = req.header("Mcp-Session-Id", sid);
        }

        let resp = req
            .send()
            .await
            .with_context(|| format!("SSE POST failed: {msg_url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("SSE POST HTTP {status} for {msg_url}: {body}");
        }

        Ok(resp)
    }
}

#[async_trait::async_trait]
impl McpTransport for SseTransport {
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };

        let body = serde_json::to_vec(&req)?;

        // POST the request
        let response = Self::post_message(
            &self.client,
            &self.base_url,
            self.session_id.as_deref(),
            &body,
        )
        .await?;

        // The response might be:
        //   1. A direct JSON-RPC response (HTTP 200 with JSON body)
        //   2. An empty 202 Accepted (response will come via SSE)
        //   3. An event stream

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        if content_type.contains("text/event-stream") || response.status().as_u16() == 202 {
            // Response will arrive via SSE event channel
            use tokio::time::{timeout, Duration};
            match timeout(Duration::from_secs(30), self.event_rx.recv()).await {
                Ok(Some(resp)) => {
                    if let Some(err) = resp.error {
                        bail!(
                            "MCP SSE error ({} / {}): {}",
                            self.name,
                            method,
                            err.message
                        );
                    }
                    return Ok(resp.result.unwrap_or(Value::Null));
                }
                Ok(None) => bail!("MCP SSE {} — event channel closed", self.name),
                Err(_) => bail!("MCP SSE {} — timeout waiting for response", self.name),
            }
        }

        // Direct JSON response
        let text = response.text().await.context("read SSE POST response")?;
        let resp: JsonRpcResponse =
            serde_json::from_str(&text).with_context(|| format!("SSE response parse: {text}"))?;

        if let Some(err) = resp.error {
            bail!(
                "MCP SSE error ({} / {}): {}",
                self.name,
                method,
                err.message
            );
        }

        Ok(resp.result.unwrap_or(Value::Null))
    }

    fn is_alive(&mut self) -> bool {
        !self._task.is_finished()
    }

    async fn disconnect(&mut self) -> Result<()> {
        info!("MCP SSE {} — disconnecting", self.name);
        self._task.abort();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// HTTP (Streamable) transport
// ---------------------------------------------------------------------------

struct HttpTransport {
    name: String,
    url: String,
    client: reqwest::Client,
    headers: HashMap<String, String>,
    next_id: u64,
    session_id: Option<String>,
}

impl HttpTransport {
    async fn connect(name: &str, url: &str, headers: &HashMap<String, String>) -> Result<Self> {
        let url = url.trim_end_matches('/').to_string();
        info!("MCP-external HTTP connect: {name} -> {url}");

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3600))
            .build()
            .context("build reqwest client for HTTP MCP")?;

        Ok(Self {
            name: name.to_string(),
            url,
            client,
            headers: headers.clone(),
            next_id: 1,
            session_id: None,
        })
    }

}

#[async_trait::async_trait]
impl McpTransport for HttpTransport {
    async fn request(&mut self, method: &str, params: Option<Value>) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };

        let body = serde_json::to_vec(&req)?;

        let mut req_builder = self
            .client
            .post(&self.url)
            .body(body)
            // Streamable HTTP spec requires the client to advertise both media
            // types; strict servers (e.g. the MCP TS SDK) reject the request
            // with HTTP 406 otherwise.
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");

        if let Some(sid) = &self.session_id {
            req_builder = req_builder.header("Mcp-Session-Id", sid);
        }
        for (k, v) in &self.headers {
            req_builder = req_builder.header(k, v);
        }

        let response = req_builder
            .send()
            .await
            .with_context(|| format!("HTTP MCP POST failed: {}", self.url))?;

        // Capture session ID from response if not already set
        if self.session_id.is_none() {
            self.session_id = response
                .headers()
                .get("mcp-session-id")
                .or_else(|| response.headers().get("Mcp-Session-Id"))
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
        }

        if !response.status().is_success() {
            let status = response.status();
            let err_body = response.text().await.unwrap_or_default();
            bail!(
                "HTTP MCP error ({} / {}): HTTP {status} — {err_body}",
                self.name,
                method
            );
        }

        let text = response
            .text()
            .await
            .with_context(|| "read HTTP MCP response")?;

        let resp: JsonRpcResponse = serde_json::from_str(&text)
            .with_context(|| format!("HTTP MCP response parse: {text}"))?;

        if let Some(err) = resp.error {
            bail!(
                "HTTP MCP error ({} / {}): {}",
                self.name,
                method,
                err.message
            );
        }

        Ok(resp.result.unwrap_or(Value::Null))
    }

    fn is_alive(&mut self) -> bool {
        // HTTP is stateless; always considered alive
        true
    }

    async fn disconnect(&mut self) -> Result<()> {
        info!("HTTP MCP {} — disconnecting", self.name);
        // No persistent connection to close
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// External MCP Client
// ---------------------------------------------------------------------------

/// Connected external MCP server client.
/// Wraps one of the three transport types behind a common trait.
pub struct ExternalMcpClient {
    name: String,
    transport: Box<dyn McpTransport>,
}

impl ExternalMcpClient {
    /// Connect to an external MCP server.  The transport type is inferred from
    /// the config.
    pub async fn connect(
        name: &str,
        transport: &super::config::McpTransportType,
        command: Option<&str>,
        args: &[String],
        env: &HashMap<String, String>,
        url: Option<&str>,
        headers: &HashMap<String, String>,
    ) -> Result<Self> {
        let transport: Box<dyn McpTransport> = match transport {
            super::config::McpTransportType::Stdio => {
                let cmd = command.context("command is required for stdio transport")?;
                let t = StdioTransport::spawn(name, cmd, args, env).await?;
                Box::new(t)
            }
            super::config::McpTransportType::Sse => {
                let sse_url = url.context("url is required for SSE transport")?;
                let t = SseTransport::connect(name, sse_url, headers).await?;
                Box::new(t)
            }
            super::config::McpTransportType::Http => {
                let http_url = url.context("url is required for HTTP transport")?;
                let t = HttpTransport::connect(name, http_url, headers).await?;
                Box::new(t)
            }
        };

        Ok(Self {
            name: name.to_string(),
            transport,
        })
    }

    /// Perform the MCP initialize handshake.  Must be called once after
    /// connect.
    pub async fn initialize(&mut self) -> Result<()> {
        let _session_id = Uuid::new_v4().to_string();
        self.transport
            .request(
                "initialize",
                Some(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "senclaw",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                })),
            )
            .await?;

        // Send initialized notification
        let _ = self
            .transport
            .request("notifications/initialized", None)
            .await;

        info!("MCP external {} — initialized", self.name);
        Ok(())
    }

    /// List tools from this MCP server.
    pub async fn list_tools(&mut self) -> Result<Vec<McpToolInfo>> {
        let response = self.transport.request("tools/list", None).await?;
        let tools: Vec<McpToolInfo> = response
            .get("tools")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        debug!(
            "MCP external {} — {} tool(s) available",
            self.name,
            tools.len()
        );
        Ok(tools)
    }

    /// Call a tool on this MCP server.
    pub async fn call_tool(&mut self, tool_name: &str, arguments: Value) -> Result<Value> {
        self.transport
            .request(
                "tools/call",
                Some(serde_json::json!({
                    "name": tool_name,
                    "arguments": arguments,
                })),
            )
            .await
    }

    /// Check whether the underlying transport is still alive.
    pub fn is_alive(&mut self) -> bool {
        self.transport.is_alive()
    }

    /// Disconnect and clean up.
    pub async fn disconnect(&mut self) -> Result<()> {
        info!("MCP external {} — disconnecting", self.name);
        self.transport.disconnect().await
    }
}

impl Drop for ExternalMcpClient {
    fn drop(&mut self) {
        info!("MCP external {} — dropped", self.name);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_rpc_request_serializes() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "tools/list".into(),
            params: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"method\":\"tools/list\""));
        assert!(json.contains("\"id\":1"));
    }

    #[test]
    fn json_rpc_response_deserializes_result() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.error.is_none());
        assert!(resp.result.is_some());
    }

    #[test]
    fn json_rpc_response_deserializes_error() {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Invalid request"}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().message, "Invalid request");
    }

    #[test]
    fn json_rpc_response_deserializes_notification() {
        // Notifications have no id
        let json = r#"{"jsonrpc":"2.0","result":"ok"}"#;
        let resp: JsonRpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.id.is_none());
        assert_eq!(resp.result.unwrap(), Value::String("ok".into()));
    }

    /// An MCP server whose event stream is exactly `packets` and then ends.
    /// Each packet is one HTTP chunk, so it reaches `bytes_stream()` on its own
    /// however the kernel merges the writes. A POST is accepted and left for
    /// the stream to answer.
    async fn sse_server(packets: Vec<Vec<u8>>) -> String {
        let events = move || {
            let packets = packets.clone();
            async move {
                axum::body::Body::from_stream(futures::stream::iter(
                    packets
                        .into_iter()
                        .map(|p| Ok::<_, std::io::Error>(axum::body::Bytes::from(p))),
                ))
            }
        };
        let app = axum::Router::new()
            .route("/sse", axum::routing::get(events))
            .route(
                "/message",
                axum::routing::post(|| async { axum::http::StatusCode::ACCEPTED }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn sse_client(packets: Vec<Vec<u8>>) -> SseTransport {
        let url = sse_server(packets).await;
        SseTransport::connect("mock", &url, &HashMap::new())
            .await
            .unwrap()
    }

    fn rpc_result(id: u64, result: Value) -> String {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
    }

    /// A packet ends wherever the network cut it, which can be inside a
    /// character. Decoded per packet, the "ờ" below reached the agent's tool
    /// result as U+FFFD.
    #[tokio::test]
    async fn a_character_cut_between_two_packets_reaches_the_result_whole() {
        let text = "Kết quả: thời tiết hôm nay đẹp";
        let event = format!("data: {}\n\n", rpc_result(1, text.into()));
        let cut = event.find('ờ').unwrap() + 1;
        let (first, second) = event.as_bytes().split_at(cut);

        let mut sse = sse_client(vec![first.to_vec(), second.to_vec()]).await;

        assert_eq!(sse.request("tools/call", None).await.unwrap(), text);
    }

    /// One event may span several `data:` lines, and the blank line that ends
    /// it may be cut in two like any other byte.
    #[tokio::test]
    async fn a_frame_of_several_data_lines_is_one_event() {
        let mut sse = sse_client(vec![
            b"event: message\ndata: {\"jsonrpc\":\"2.0\",\n".to_vec(),
            b"data:\"id\":1,\n".to_vec(),
            "data:   \"result\":\"chào bạn\"}\n".as_bytes().to_vec(),
            b"\n".to_vec(),
        ])
        .await;

        assert_eq!(sse.request("tools/call", None).await.unwrap(), "chào bạn");
    }

    /// Lines joined with `\n` keep two digits apart, so this is no number but
    /// broken JSON. It is skipped, and the next event in the packet still
    /// arrives.
    #[tokio::test]
    async fn data_lines_are_joined_with_a_newline_and_a_bad_event_is_skipped() {
        let broken = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":1\ndata: 2}\n\n";
        let good = format!("data: {}\n\n", rpc_result(2, "ok".into()));

        let mut sse = sse_client(vec![format!("{broken}{good}").into_bytes()]).await;

        assert_eq!(sse.request("tools/call", None).await.unwrap(), "ok");
    }

    /// SSE lets a line end in CRLF. A blank line there is `\r\n\r\n`, which
    /// holds no `\n\n`, so no frame of such a server was ever cut.
    #[tokio::test]
    async fn frames_that_end_in_crlf_are_cut() {
        let events = format!(
            ": ping\r\n\r\nevent: message\r\ndata: {}\r\n\r\ndata: {}\r\n\r\n",
            rpc_result(1, "một".into()),
            rpc_result(2, "hai".into()),
        );

        let mut sse = sse_client(vec![events.into_bytes()]).await;

        assert_eq!(sse.request("tools/call", None).await.unwrap(), "một");
        assert_eq!(sse.request("tools/call", None).await.unwrap(), "hai");
    }

    /// `\r\n\r` is not a blank line yet: the frame waits for the last `\n`,
    /// here in the next packet, and its `data:` lines still join.
    #[tokio::test]
    async fn a_crlf_blank_line_cut_between_two_packets_is_still_one_frame() {
        let mut sse = sse_client(vec![
            b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\r\n".to_vec(),
            "data: \"result\":\"chào bạn\"}\r\n\r".as_bytes().to_vec(),
            b"\n".to_vec(),
        ])
        .await;

        assert_eq!(sse.request("tools/call", None).await.unwrap(), "chào bạn");
    }

    /// With debug logging on, the log line for an event that is not JSON-RPC
    /// cut the text at byte 200. Inside a character that panicked and took the
    /// reader task with it, so every later answer was lost.
    #[tokio::test]
    async fn a_long_event_that_is_not_json_rpc_cannot_kill_the_reader() {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .finish();
        let _logging = tracing::subscriber::set_default(subscriber);

        let not_rpc = format!("{}ờ", "a".repeat(199));
        assert!(!not_rpc.is_char_boundary(200));
        let good = rpc_result(1, "ok".into());

        let mut sse =
            sse_client(vec![format!("data: {not_rpc}\n\ndata: {good}\n\n").into_bytes()]).await;

        assert_eq!(sse.request("tools/call", None).await.unwrap(), "ok");
    }

    /// When the stream ends, every finished frame has been delivered, an
    /// unfinished one is dropped rather than parsed, the next request fails
    /// instead of waiting, and the transport reports itself dead.
    #[tokio::test]
    async fn the_end_of_the_stream_closes_the_transport_and_drops_an_unfinished_frame() {
        let finished = format!(
            "data: {}\n\ndata: {}\n\n",
            rpc_result(1, "một".into()),
            rpc_result(2, "hai".into()),
        );
        let unfinished = format!("data: {}\n", rpc_result(3, "ba".into()));

        let mut sse = sse_client(vec![finished.into_bytes(), unfinished.into_bytes()]).await;

        assert_eq!(sse.request("tools/call", None).await.unwrap(), "một");
        assert_eq!(sse.request("tools/call", None).await.unwrap(), "hai");
        let err = sse.request("tools/call", None).await.unwrap_err();
        assert!(err.to_string().contains("event channel closed"), "{err}");

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while sse.is_alive() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the reader task must end with the stream");
    }
}
