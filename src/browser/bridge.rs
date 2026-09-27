//! WebSocket bridge — listens for Chrome extension connections and MCP client
//! connections, routing messages between them.
//!
//! Two listeners:
//! - Extension port (browser_ws_port): Chrome extension connects here.
//!   Receives ExtensionMessage (tab events, crawl progress, responses).
//!   Sends DaemonMessage (commands to execute in the browser).
//! - Internal port (browser_ws_port + 1): MCP subprocess connects here.
//!   Receives DaemonMessage (tool requests from MCP server).
//!   Sends back ExtensionMessage::Response with results.
//!
//! The bridge runs permanently in the daemon so the extension stays connected
//! even when no agent is actively calling browser tools.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock, Semaphore};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use super::crawl_engine::CrawlEngine;
use super::protocol::{DaemonMessage, ExtensionMessage};
use super::tab_registry::TabRegistry;
use super::types::*;

/// Response channel for a pending request.
type PendingRequest = oneshot::Sender<ActionResult>;

/// Shared state between the bridge, MCP server, and extension.
#[derive(Clone)]
pub struct BrowserBridge {
    /// WebSocket sender to the connected extension.
    ext_tx: Arc<RwLock<Option<mpsc::UnboundedSender<Message>>>>,
    /// Tab state registry.
    pub tabs: Arc<TabRegistry>,
    /// Pending requests awaiting response.
    pending: Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
    /// Crawl engine.
    pub crawl_engine: Arc<CrawlEngine>,
    /// Port the extension WS server listens on.
    pub ext_port: u16,
    /// Port the MCP internal WS server listens on.
    pub int_port: u16,
    /// Backpressure on requests in flight to the extension.
    inflight: Arc<Semaphore>,
}

/// Cap on requests concurrently forwarded to the extension (see gateway
/// BrowserRelay for rationale).
const MAX_INFLIGHT_TO_EXTENSION: usize = 12;

impl BrowserBridge {
    /// Create a new bridge. Does not start listening yet.
    /// `port` is the extension-facing port; internal port is `port + 1`.
    pub fn new(port: u16) -> Self {
        Self {
            ext_tx: Arc::new(RwLock::new(None)),
            tabs: Arc::new(TabRegistry::new()),
            pending: Arc::new(Mutex::new(HashMap::new())),
            crawl_engine: Arc::new(CrawlEngine::new()),
            ext_port: port,
            int_port: port + 1,
            inflight: Arc::new(Semaphore::new(MAX_INFLIGHT_TO_EXTENSION)),
        }
    }

    /// Start both WebSocket listeners:
    /// - Extension listener on `ext_port`
    /// - Internal MCP listener on `int_port`
    pub async fn start(&self) -> Result<()> {
        // Start extension listener
        let ext_addr = format!("127.0.0.1:{}", self.ext_port);
        let ext_listener = tokio::net::TcpListener::bind(&ext_addr)
            .await
            .context(format!(
                "[BrowserBridge] Failed to bind extension port {ext_addr}"
            ))?;
        tracing::info!("[BrowserBridge] Extension listener on ws://{ext_addr}");

        // Start internal MCP listener
        let int_addr = format!("127.0.0.1:{}", self.int_port);
        let int_listener = tokio::net::TcpListener::bind(&int_addr)
            .await
            .context(format!(
                "[BrowserBridge] Failed to bind internal port {int_addr}"
            ))?;
        tracing::info!("[BrowserBridge] Internal MCP listener on ws://{int_addr}");

        // Spawn extension accept loop
        {
            let ext_tx = self.ext_tx.clone();
            let pending = self.pending.clone();
            let tabs = self.tabs.clone();
            let crawl_engine = self.crawl_engine.clone();

            tokio::spawn(async move {
                loop {
                    match ext_listener.accept().await {
                        Ok((stream, peer_addr)) => {
                            tracing::info!("[BrowserBridge] Extension connected from {peer_addr}");
                            let ws = match tokio_tungstenite::accept_async(stream).await {
                                Ok(ws) => ws,
                                Err(e) => {
                                    tracing::error!(
                                        "[BrowserBridge] Extension WS upgrade failed: {e}"
                                    );
                                    continue;
                                }
                            };

                            let (mut ws_sink, mut ws_stream) = ws.split();
                            let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

                            // Store sender for outgoing messages to extension
                            *ext_tx.write().await = Some(tx.clone());

                            // Forward task: mpsc → ws sink
                            tokio::spawn(async move {
                                while let Some(msg) = rx.recv().await {
                                    if ws_sink.send(msg).await.is_err() {
                                        break;
                                    }
                                }
                            });

                            // Read loop: extension → ExtensionMessage
                            let pending_clone = pending.clone();
                            let tabs_clone = tabs.clone();
                            let crawl_clone = crawl_engine.clone();
                            let ext_tx_clone = ext_tx.clone();

                            loop {
                                match ws_stream.next().await {
                                    Some(Ok(Message::Text(text))) => {
                                        let text_str = text.to_string();
                                        match serde_json::from_str::<ExtensionMessage>(&text_str) {
                                            Ok(msg) => {
                                                handle_extension_message(
                                                    &pending_clone,
                                                    &tabs_clone,
                                                    &crawl_clone,
                                                    msg,
                                                )
                                                .await;
                                            }
                                            Err(e) => {
                                                tracing::warn!(
                                                    "[BrowserBridge] Failed to parse extension message: {e}"
                                                );
                                            }
                                        }
                                    }
                                    Some(Ok(Message::Close(_))) | None => {
                                        tracing::info!("[BrowserBridge] Extension disconnected");
                                        *ext_tx_clone.write().await = None;
                                        break;
                                    }
                                    Some(Ok(_)) => {} // Ignore binary, ping, pong
                                    Some(Err(e)) => {
                                        tracing::error!("[BrowserBridge] Extension WS error: {e}");
                                        *ext_tx_clone.write().await = None;
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("[BrowserBridge] Extension accept error: {e}");
                        }
                    }
                }
            });
        }

        // Spawn internal MCP accept loop
        {
            let ext_tx = self.ext_tx.clone();
            let pending = self.pending.clone();
            let tabs = self.tabs.clone();
            let crawl_engine = self.crawl_engine.clone();
            let inflight = self.inflight.clone();

            tokio::spawn(async move {
                loop {
                    match int_listener.accept().await {
                        Ok((stream, peer_addr)) => {
                            tracing::info!("[BrowserBridge] MCP client connected from {peer_addr}");
                            let ws = match tokio_tungstenite::accept_async(stream).await {
                                Ok(ws) => ws,
                                Err(e) => {
                                    tracing::error!("[BrowserBridge] MCP WS upgrade failed: {e}");
                                    continue;
                                }
                            };

                            let (mut ws_sink, mut ws_stream) = ws.split();

                            // Per-connection state; the read loop runs in its
                            // own task so concurrent MCP clients don't block
                            // each other at the accept loop.
                            let ext_tx_clone = ext_tx.clone();
                            let pending_clone = pending.clone();
                            let tabs_clone = tabs.clone();
                            let crawl_clone = crawl_engine.clone();
                            let inflight_clone = inflight.clone();

                            tokio::spawn(async move {
                                loop {
                                    match ws_stream.next().await {
                                        Some(Ok(Message::Text(text))) => {
                                            let text_str = text.to_string();

                                            // Try DaemonMessage (MCP client request)
                                            match serde_json::from_str::<DaemonMessage>(&text_str) {
                                                Ok(dm) => {
                                                    let response = relay_mcp_request(
                                                        &ext_tx_clone,
                                                        &pending_clone,
                                                        &tabs_clone,
                                                        &crawl_clone,
                                                        &inflight_clone,
                                                        dm,
                                                    )
                                                    .await;

                                                    let resp_json = serde_json::to_string(&response)
                                                    .unwrap_or_else(|e| {
                                                        format!(
                                                            r#"{{"type":"Response","status":"error","message":"serialize failed: {}"}}"#,
                                                            e
                                                        )
                                                    });
                                                    if ws_sink
                                                        .send(Message::Text(resp_json.into()))
                                                        .await
                                                        .is_err()
                                                    {
                                                        break;
                                                    }
                                                }
                                                Err(e) => {
                                                    tracing::warn!(
                                                    "[BrowserBridge] Failed to parse MCP message: {e}"
                                                );
                                                    let err_resp = serde_json::json!({
                                                        "type": "Response",
                                                        "status": "error",
                                                        "message": format!("parse error: {e}"),
                                                    });
                                                    let _ = ws_sink
                                                        .send(Message::Text(
                                                            err_resp.to_string().into(),
                                                        ))
                                                        .await;
                                                }
                                            }
                                        }
                                        Some(Ok(Message::Close(_))) | None => {
                                            tracing::info!(
                                                "[BrowserBridge] MCP client disconnected"
                                            );
                                            break;
                                        }
                                        Some(Ok(_)) => {} // Ignore binary, ping, pong
                                        Some(Err(e)) => {
                                            tracing::error!("[BrowserBridge] MCP WS error: {e}");
                                            break;
                                        }
                                    }
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("[BrowserBridge] MCP accept error: {e}");
                        }
                    }
                }
            });
        }

        Ok(())
    }

    /// Send a message to the extension and wait for a response.
    /// Used by in-process MCP server (when running in daemon).
    pub async fn request(&self, msg: &DaemonMessage, timeout_ms: u64) -> Result<ActionResult> {
        let request_id = extract_request_id(msg);

        // Set up response channel
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id.clone(), tx);

        // Send to extension
        let payload =
            serde_json::to_string(msg).context("[BrowserBridge] Failed to serialize message")?;

        let sent = {
            let tx_guard = self.ext_tx.read().await;
            if let Some(tx) = tx_guard.as_ref() {
                tx.send(Message::Text(payload.into())).is_ok()
            } else {
                false
            }
        };

        if !sent {
            self.pending.lock().await.remove(&request_id);
            return Err(anyhow::anyhow!("[BrowserBridge] Extension not connected"));
        }

        // Wait for response with timeout
        match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), rx).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(anyhow::anyhow!("[BrowserBridge] Response channel closed")),
            Err(_) => {
                self.pending.lock().await.remove(&request_id);
                Err(anyhow::anyhow!(
                    "[BrowserBridge] Request timed out after {timeout_ms}ms"
                ))
            }
        }
    }

    /// Send a fire-and-forget message to the extension.
    pub async fn send(&self, msg: &DaemonMessage) -> Result<()> {
        let payload =
            serde_json::to_string(msg).context("[BrowserBridge] Failed to serialize message")?;

        let tx_guard = self.ext_tx.read().await;
        if let Some(tx) = tx_guard.as_ref() {
            tx.send(Message::Text(payload.into()))
                .map_err(|e| anyhow::anyhow!("[BrowserBridge] Send error: {e}"))
        } else {
            Err(anyhow::anyhow!("[BrowserBridge] Extension not connected"))
        }
    }

    /// Check if extension is connected.
    pub async fn is_connected(&self) -> bool {
        self.ext_tx.read().await.is_some()
    }
}

/// Relay an MCP client request to the extension and return the response.
async fn relay_mcp_request(
    ext_tx: &Arc<RwLock<Option<mpsc::UnboundedSender<Message>>>>,
    pending: &Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
    tabs: &Arc<TabRegistry>,
    crawl_engine: &Arc<CrawlEngine>,
    inflight: &Arc<Semaphore>,
    msg: DaemonMessage,
) -> ExtensionMessage {
    let request_id = extract_request_id(&msg);

    // Check for messages we can handle locally (no extension needed)
    match &msg {
        DaemonMessage::ListTabs { request_id: rid } => {
            let tabs_list = tabs.list().await;
            return ExtensionMessage::Response {
                request_id: rid.clone(),
                result: ActionResult::Ok {
                    data: serde_json::to_value(tabs_list).unwrap_or_default(),
                },
            };
        }
        DaemonMessage::GetStatus { request_id: rid } => {
            let ext_connected = ext_tx.read().await.is_some();
            let alive = tabs.is_alive().await;
            let tab_count = tabs.count().await;
            let active_tab = tabs.get_active().await;
            return ExtensionMessage::Response {
                request_id: rid.clone(),
                result: ActionResult::Ok {
                    data: serde_json::json!({
                        "connected": ext_connected,
                        "extension_alive": alive,
                        "tab_count": tab_count,
                        "active_tab": active_tab,
                        "agent_tabs": tabs.agent_tab_map().await,
                        "active_crawl_jobs": crawl_engine.list_jobs().await,
                    }),
                },
            };
        }
        DaemonMessage::CrawlStart {
            job_id,
            start_url,
            depth,
            max_pages,
            link_patterns,
            exclude_patterns,
            same_domain,
            ..
        } => {
            let config = CrawlConfig {
                job_id: job_id.clone(),
                start_url: start_url.clone(),
                depth: *depth,
                max_pages: *max_pages,
                link_patterns: link_patterns.clone(),
                exclude_patterns: exclude_patterns.clone(),
                same_domain: *same_domain,
                per_page_timeout_ms: 10000,
                wait_between_pages_ms: 1000,
            };
            crawl_engine.create_job(config).await;
        }
        _ => {}
    }

    // Backpressure: hold slow bursts here rather than piling them all onto
    // the browser at once. Permit is held until the response (or timeout).
    let _permit = match inflight.acquire().await {
        Ok(p) => p,
        Err(_) => {
            return ExtensionMessage::Response {
                request_id,
                result: ActionResult::Error {
                    message: "Bridge shutting down".into(),
                    code: None,
                },
            };
        }
    };

    // Set up response channel
    let (tx, rx) = oneshot::channel();
    pending.lock().await.insert(request_id.clone(), tx);

    // Send to extension
    let payload = match serde_json::to_string(&msg) {
        Ok(p) => p,
        Err(e) => {
            return ExtensionMessage::Response {
                request_id,
                result: ActionResult::Error {
                    message: format!("serialize failed: {e}"),
                    code: None,
                },
            };
        }
    };

    let sent = {
        let tx_guard = ext_tx.read().await;
        if let Some(tx) = tx_guard.as_ref() {
            tx.send(Message::Text(payload.into())).is_ok()
        } else {
            false
        }
    };

    if !sent {
        pending.lock().await.remove(&request_id);
        return ExtensionMessage::Response {
            request_id,
            result: ActionResult::Error {
                message: "Extension not connected".into(),
                code: None,
            },
        };
    }

    // Wait for response with timeout
    match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
        Ok(Ok(result)) => ExtensionMessage::Response { request_id, result },
        Ok(Err(_)) => ExtensionMessage::Response {
            request_id,
            result: ActionResult::Error {
                message: "Response channel closed".into(),
                code: None,
            },
        },
        Err(_) => {
            pending.lock().await.remove(&request_id);
            ExtensionMessage::Response {
                request_id,
                result: ActionResult::Error {
                    message: "Request timed out after 30s".into(),
                    code: None,
                },
            }
        }
    }
}

fn extract_request_id(msg: &DaemonMessage) -> String {
    match msg {
        DaemonMessage::Navigate { request_id, .. } => request_id.clone(),
        DaemonMessage::NewTab { request_id, .. } => request_id.clone(),
        DaemonMessage::CloseTab { request_id, .. } => request_id.clone(),
        DaemonMessage::SwitchTab { request_id, .. } => request_id.clone(),
        DaemonMessage::GoBack { request_id, .. } => request_id.clone(),
        DaemonMessage::GoForward { request_id, .. } => request_id.clone(),
        DaemonMessage::Reload { request_id, .. } => request_id.clone(),
        DaemonMessage::Click { request_id, .. } => request_id.clone(),
        DaemonMessage::Type { request_id, .. } => request_id.clone(),
        DaemonMessage::SelectOption { request_id, .. } => request_id.clone(),
        DaemonMessage::Scroll { request_id, .. } => request_id.clone(),
        DaemonMessage::Hover { request_id, .. } => request_id.clone(),
        DaemonMessage::PressKey { request_id, .. } => request_id.clone(),
        DaemonMessage::UploadFile { request_id, .. } => request_id.clone(),
        DaemonMessage::ExecuteJs { request_id, .. } => request_id.clone(),
        DaemonMessage::WaitFor { request_id, .. } => request_id.clone(),
        DaemonMessage::GetSnapshot { request_id, .. } => request_id.clone(),
        DaemonMessage::GetScreenshot { request_id, .. } => request_id.clone(),
        DaemonMessage::ExtractText { request_id, .. } => request_id.clone(),
        DaemonMessage::ExtractLinks { request_id, .. } => request_id.clone(),
        DaemonMessage::ExtractTable { request_id, .. } => request_id.clone(),
        DaemonMessage::Search { request_id, .. } => request_id.clone(),
        DaemonMessage::FillForm { request_id, .. } => request_id.clone(),
        DaemonMessage::ListTabs { request_id, .. } => request_id.clone(),
        DaemonMessage::GetStatus { request_id, .. } => request_id.clone(),
        DaemonMessage::CrawlStart { job_id, .. } => job_id.clone(),
        DaemonMessage::CrawlStop { .. }
        | DaemonMessage::CrawlPause { .. }
        | DaemonMessage::CrawlResume { .. } => Uuid::new_v4().to_string(),
    }
}

/// Handle an incoming message from the chrome extension.
async fn handle_extension_message(
    pending: &Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
    tabs: &Arc<TabRegistry>,
    crawl_engine: &Arc<CrawlEngine>,
    msg: ExtensionMessage,
) {
    match msg {
        ExtensionMessage::Response { request_id, result } => {
            if let Some(tx) = pending.lock().await.remove(&request_id) {
                let _ = tx.send(result);
            }
        }
        ExtensionMessage::TabCreated {
            tab_id,
            url,
            window_id: _,
            agent_id,
        } => {
            tabs.register(tab_id.clone(), url.clone(), agent_id).await;
            tabs.set_active(&tab_id).await;
            tracing::info!("[BrowserBridge] Tab created: {tab_id} -> {url}");
        }
        ExtensionMessage::TabUpdated {
            tab_id,
            url,
            title,
            status,
            agent_id: _,
        } => {
            tabs.update(&tab_id, url, title, status).await;
        }
        ExtensionMessage::TabClosed { tab_id, .. } => {
            tabs.remove(&tab_id).await;
            tracing::info!("[BrowserBridge] Tab closed: {tab_id}");
        }
        ExtensionMessage::CrawlProgress {
            job_id,
            pages_crawled,
            pages_total,
            current_url,
        } => {
            crawl_engine
                .update_progress(&job_id, pages_crawled, pages_total, &current_url)
                .await;
        }
        ExtensionMessage::CrawlResult {
            job_id,
            page_result,
        } => {
            crawl_engine.add_result(&job_id, page_result).await;
        }
        ExtensionMessage::CrawlComplete {
            job_id,
            total_pages,
            duration_ms: _,
        } => {
            crawl_engine.mark_complete(&job_id).await;
            tracing::info!("[BrowserBridge] Crawl complete: {job_id} ({total_pages} pages)");
        }
        ExtensionMessage::ScreenshotFrame {
            tab_id: _,
            data: _,
            format: _,
        } => {
            // Screenshot frames are currently logged but not forwarded
        }
        ExtensionMessage::Heartbeat {
            tab_count,
            active_tab_id: _,
            agent_tabs,
        } => {
            tabs.heartbeat(tab_count).await;
            if let Some(map) = agent_tabs {
                tabs.sync_agent_tabs(map).await;
            }
        }
    }
}
