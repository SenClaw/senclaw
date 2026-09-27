//! Browser extension relay within the WebSocket gateway.
//!
//! Chrome extension connects on the `/browser` path of the existing gateway.
//! Messages from extension (ExtensionMessage) update tab/crawl state and
//! resolve pending requests. Messages to extension (DaemonMessage) are
//! sent via the stored sender.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::Message;
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock, Semaphore};

use crate::browser::crawl_engine::CrawlEngine;
use crate::browser::protocol::{DaemonMessage, ExtensionMessage};
use crate::browser::tab_registry::TabRegistry;
use crate::browser::types::*;

/// Response channel for a pending request.
pub(crate) type PendingRequest = oneshot::Sender<ActionResult>;

/// Cap on requests concurrently forwarded to the extension. The extension is
/// a single browser: unbounded fan-in just makes every page load slower until
/// everything trips the 30s timeout. Excess requests queue here instead.
const MAX_INFLIGHT_TO_EXTENSION: usize = 12;

/// Shared browser relay state — lives in WsState.
pub struct BrowserRelay {
    /// Sender to the connected extension (if any).
    ext_tx: RwLock<Option<mpsc::UnboundedSender<Message>>>,
    /// Tab state registry.
    pub tabs: Arc<TabRegistry>,
    /// Pending requests awaiting response.
    pub pending: Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
    /// Crawl engine.
    pub crawl_engine: Arc<CrawlEngine>,
    /// Backpressure on requests in flight to the extension.
    pub inflight: Arc<Semaphore>,
}

impl BrowserRelay {
    pub fn new() -> Self {
        Self {
            ext_tx: RwLock::new(None),
            tabs: Arc::new(TabRegistry::new()),
            pending: Arc::new(Mutex::new(HashMap::new())),
            crawl_engine: Arc::new(CrawlEngine::new()),
            inflight: Arc::new(Semaphore::new(MAX_INFLIGHT_TO_EXTENSION)),
        }
    }

    pub(crate) fn ext_tx(&self) -> &RwLock<Option<mpsc::UnboundedSender<Message>>> {
        &self.ext_tx
    }
}

/// Handle an extension WebSocket connection on the `/browser` path.
pub(crate) async fn handle_browser_connection(
    ws: axum::extract::ws::WebSocket,
    relay: Arc<BrowserRelay>,
) {
    let (mut ws_sink, mut ws_stream) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    // Store sender for outgoing messages to extension
    *relay.ext_tx.write().await = Some(tx.clone());
    tracing::info!("[BrowserGateway] Extension connected");

    // Forward task: mpsc → ws sink
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Read loop: extension → ExtensionMessage
    let tabs = relay.tabs.clone();
    let pending = relay.pending.clone();
    let crawl_engine = relay.crawl_engine.clone();

    loop {
        match ws_stream.next().await {
            Some(Ok(Message::Text(text))) => {
                let text_str = text.to_string();
                match serde_json::from_str::<ExtensionMessage>(&text_str) {
                    Ok(msg) => {
                        handle_extension_message(&pending, &tabs, &crawl_engine, msg).await;
                    }
                    Err(e) => {
                        tracing::warn!("[BrowserGateway] Failed to parse extension message: {e}");
                    }
                }
            }
            Some(Ok(Message::Close(_))) | None => {
                tracing::info!("[BrowserGateway] Extension disconnected");
                *relay.ext_tx.write().await = None;
                break;
            }
            Some(Ok(_)) => {} // Ignore binary, ping, pong
            Some(Err(e)) => {
                tracing::error!("[BrowserGateway] Extension WS error: {e}");
                *relay.ext_tx.write().await = None;
                break;
            }
        }
    }
}

/// Handle an MCP client WebSocket connection on the `/browser-mcp` path.
pub(crate) async fn handle_browser_mcp_connection(
    ws: axum::extract::ws::WebSocket,
    relay: Arc<BrowserRelay>,
) {
    let (mut ws_sink, mut ws_stream) = ws.split();
    tracing::info!("[BrowserGateway] MCP client connected");

    let pending = relay.pending.clone();
    let tabs = relay.tabs.clone();
    let crawl_engine = relay.crawl_engine.clone();
    let inflight = relay.inflight.clone();

    loop {
        match ws_stream.next().await {
            Some(Ok(Message::Text(text))) => {
                let text_str = text.to_string();
                match serde_json::from_str::<DaemonMessage>(&text_str) {
                    Ok(dm) => {
                        let response = relay_mcp_request(
                            relay.ext_tx(),
                            &pending,
                            &tabs,
                            &crawl_engine,
                            &inflight,
                            dm,
                        )
                        .await;

                        let resp_json =
                            serde_json::to_string(&response).unwrap_or_else(|e| {
                                format!(
                                    r#"{{"type":"Response","status":"error","message":"serialize failed: {}"}}"#,
                                    e
                                )
                            });
                        if ws_sink.send(Message::Text(resp_json.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("[BrowserGateway] Failed to parse MCP message: {e}");
                        let err = serde_json::json!({
                            "type": "Response",
                            "status": "error",
                            "message": format!("parse error: {e}"),
                        });
                        let _ = ws_sink.send(Message::Text(err.to_string().into())).await;
                    }
                }
            }
            Some(Ok(Message::Close(_))) | None => {
                tracing::info!("[BrowserGateway] MCP client disconnected");
                break;
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => {
                tracing::error!("[BrowserGateway] MCP WS error: {e}");
                break;
            }
        }
    }
}

// ===== Message handlers =====

/// Relay an MCP client request to the extension and return the response.
async fn relay_mcp_request(
    ext_tx: &RwLock<Option<mpsc::UnboundedSender<Message>>>,
    pending: &Arc<Mutex<HashMap<RequestId, PendingRequest>>>,
    tabs: &Arc<TabRegistry>,
    crawl_engine: &Arc<CrawlEngine>,
    inflight: &Arc<Semaphore>,
    msg: DaemonMessage,
) -> ExtensionMessage {
    let request_id = extract_request_id(&msg);

    // Handle locally-resolvable messages
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
        Err(_) => return error_response(request_id, "Relay shutting down"),
    };

    // Forward to extension
    let (tx, rx) = oneshot::channel();
    pending.lock().await.insert(request_id.clone(), tx);

    let payload = match serde_json::to_string(&msg) {
        Ok(p) => p,
        Err(e) => {
            return error_response(request_id, &format!("serialize: {e}"));
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
        return error_response(request_id, "Extension not connected");
    }

    match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
        Ok(Ok(result)) => ExtensionMessage::Response { request_id, result },
        Ok(Err(_)) => error_response(request_id, "Response channel closed"),
        Err(_) => {
            pending.lock().await.remove(&request_id);
            error_response(request_id, "Request timed out after 30s")
        }
    }
}

fn error_response(request_id: String, message: &str) -> ExtensionMessage {
    ExtensionMessage::Response {
        request_id,
        result: ActionResult::Error {
            message: message.into(),
            code: None,
        },
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
        | DaemonMessage::CrawlResume { .. } => uuid::Uuid::new_v4().to_string(),
    }
}

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
            tracing::info!("[BrowserGateway] Tab created: {tab_id} -> {url}");
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
            tracing::info!("[BrowserGateway] Tab closed: {tab_id}");
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
            tracing::info!("[BrowserGateway] Crawl complete: {job_id} ({total_pages} pages)");
        }
        ExtensionMessage::ScreenshotFrame { .. } => {}
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
