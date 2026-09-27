//! WorkbenchBridge — relay engine workbench events to WebSocket + IM channels.
//!
//! Port of `code-old/SenClaw/src/agent/WorkbenchBridge.ts`.
//!
//! Responsibilities:
//!   1. Subscribe to each engine's [`EventBus`] and fan out
//!      `workbench:new` / `workbench:service_ready` / `workbench:service_crashed`
//!      / `workbench:service_stopped` to:
//!        - injected callbacks (WebSocket gateway broadcasts to WebUI)
//!        - IM channel text fallback (Telegram/Feishu/WeChat — non-web JIDs)
//!   2. Reverse ops: when the user clicks close/markViewed/read-file/fetch-logs
//!      in the WebUI, the bridge dispatches to the per-group engine's
//!      [`WorkbenchService`].
//!
//! No pending-state machine (events are fire-and-forget). One bridge instance
//! serves all groups; each `bind_engine` subscribes a per-group listener task.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::zen_core::{
    EngineEvent, WorkbenchArtifact, WorkbenchMode, WorkbenchNewData, WorkbenchServiceCrashedData,
    WorkbenchServiceReadyData, WorkbenchServiceStoppedData, ZenEngine,
};

/// Closure used to push a text notification to a chat channel (IM fallback for
/// non-web JIDs). Caller resolves the right channel via `owns_jid` internally.
/// Signature mirrors `AgentPool::set_send_reply`: `(chat_jid, text, bot_token)`.
pub type SendChannelNoticeFn = Arc<dyn Fn(&str, &str, Option<&str>) + Send + Sync>;

// ===== UI-facing payloads (forwarded to WebSocket gateway) =====

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkbenchNewPayload {
    pub artifact: WorkbenchArtifact,
    #[serde(rename = "replacesId", skip_serializing_if = "Option::is_none")]
    pub replaces_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkbenchServiceReadyPayload {
    #[serde(rename = "artifactId")]
    pub artifact_id: String,
    pub ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkbenchServiceCrashedPayload {
    #[serde(rename = "artifactId")]
    pub artifact_id: String,
    #[serde(rename = "lastLogLines")]
    pub last_log_lines: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkbenchServiceStoppedPayload {
    #[serde(rename = "artifactId")]
    pub artifact_id: String,
    pub reason: String,
}

// ===== Callback types =====

pub type OnNewCb = Box<dyn Fn(&str, WorkbenchNewPayload) + Send + Sync>;
pub type OnServiceReadyCb = Box<dyn Fn(&str, WorkbenchServiceReadyPayload) + Send + Sync>;
pub type OnServiceCrashedCb = Box<dyn Fn(&str, WorkbenchServiceCrashedPayload) + Send + Sync>;
pub type OnServiceStoppedCb = Box<dyn Fn(&str, WorkbenchServiceStoppedPayload) + Send + Sync>;

// ===== Bridge =====

/// Per-engine binding state used to clean up.
struct BindHandle {
    cancel: CancellationToken,
    join: JoinHandle<()>,
}

pub struct WorkbenchBridge {
    /// chat_jid → engine (for reverse ops).
    engines: Mutex<HashMap<String, Arc<ZenEngine>>>,
    /// chat_jid → cached bot_token used for IM-fallback notifications.
    bot_tokens: Mutex<HashMap<String, Option<String>>>,
    /// chat_jid → background listener task handle.
    handles: Mutex<HashMap<String, BindHandle>>,
    /// IM text-notification sink for non-web JIDs. Set via [`Self::set_send_channel_notice`].
    send_channel_notice: Mutex<Option<SendChannelNoticeFn>>,

    on_new: Mutex<Option<OnNewCb>>,
    on_service_ready: Mutex<Option<OnServiceReadyCb>>,
    on_service_crashed: Mutex<Option<OnServiceCrashedCb>>,
    on_service_stopped: Mutex<Option<OnServiceStoppedCb>>,
}

impl WorkbenchBridge {
    pub fn new() -> Self {
        Self {
            engines: Mutex::new(HashMap::new()),
            bot_tokens: Mutex::new(HashMap::new()),
            handles: Mutex::new(HashMap::new()),
            send_channel_notice: Mutex::new(None),
            on_new: Mutex::new(None),
            on_service_ready: Mutex::new(None),
            on_service_crashed: Mutex::new(None),
            on_service_stopped: Mutex::new(None),
        }
    }

    /// Wire the IM text-notification sink (fire-and-forget).
    pub fn set_send_channel_notice(&self, cb: SendChannelNoticeFn) {
        *self.send_channel_notice.lock().unwrap() = Some(cb);
    }

    // ===== Callback setters =====

    pub fn set_on_new(&self, cb: OnNewCb) {
        *self.on_new.lock().unwrap() = Some(cb);
    }

    pub fn set_on_service_ready(&self, cb: OnServiceReadyCb) {
        *self.on_service_ready.lock().unwrap() = Some(cb);
    }

    pub fn set_on_service_crashed(&self, cb: OnServiceCrashedCb) {
        *self.on_service_crashed.lock().unwrap() = Some(cb);
    }

    pub fn set_on_service_stopped(&self, cb: OnServiceStoppedCb) {
        *self.on_service_stopped.lock().unwrap() = Some(cb);
    }

    // ===== Bind / unbind =====

    /// Bind an engine's event stream to this bridge for a specific group.
    /// Should be called by `ZenCoreApi` once per engine creation. Returns
    /// gracefully if already bound.
    ///
    /// `bot_token` is cached for IM-fallback notifications on non-web JIDs.
    pub fn bind_engine(
        self: &Arc<Self>,
        engine: Arc<ZenEngine>,
        chat_jid: &str,
        bot_token: Option<String>,
    ) {
        let chat_jid_owned = chat_jid.to_string();
        if self.handles.lock().unwrap().contains_key(&chat_jid_owned) {
            return;
        }
        self.engines
            .lock()
            .unwrap()
            .insert(chat_jid_owned.clone(), engine.clone());
        self.bot_tokens
            .lock()
            .unwrap()
            .insert(chat_jid_owned.clone(), bot_token);

        let rx = engine.event_bus.subscribe();
        let cancel = CancellationToken::new();
        let bridge = Arc::clone(self);
        let cancel_for_task = cancel.clone();
        let jid_for_task = chat_jid_owned.clone();

        let join = tokio::spawn(async move {
            bridge.run_listener(rx, jid_for_task, cancel_for_task).await;
        });

        self.handles
            .lock()
            .unwrap()
            .insert(chat_jid_owned, BindHandle { cancel, join });
    }

    /// Stop listening for an engine. Idempotent.
    pub fn unbind(&self, chat_jid: &str) {
        self.engines.lock().unwrap().remove(chat_jid);
        self.bot_tokens.lock().unwrap().remove(chat_jid);
        if let Some(handle) = self.handles.lock().unwrap().remove(chat_jid) {
            handle.cancel.cancel();
            handle.join.abort();
        }
    }

    // ===== Reverse ops (called by WebSocket gateway → UI server handlers) =====

    /// User in WebUI brought an artifact to foreground. Returns `false` when
    /// the chat_jid / artifact is unknown.
    pub fn mark_viewed(&self, chat_jid: &str, artifact_id: &str) -> bool {
        let Some(engine) = self.engines.lock().unwrap().get(chat_jid).cloned() else {
            return false;
        };
        engine.workbench_service.mark_viewed(artifact_id)
    }

    /// User closed an artifact panel.
    pub fn close(&self, chat_jid: &str, artifact_id: &str) -> bool {
        let Some(engine) = self.engines.lock().unwrap().get(chat_jid).cloned() else {
            return false;
        };
        engine.workbench_service.close(artifact_id)
    }

    /// Read a file from a static artifact (for HTML iframe / MD viewer).
    pub fn read_file(
        &self,
        chat_jid: &str,
        artifact_id: &str,
        file_path: &str,
    ) -> Result<String, String> {
        let Some(engine) = self.engines.lock().unwrap().get(chat_jid).cloned() else {
            return Err("engine_not_found".to_string());
        };
        engine.workbench_service.read_file(artifact_id, file_path)
    }

    /// Tail logs for a service artifact.
    pub fn fetch_logs(&self, chat_jid: &str, artifact_id: &str, tail_lines: usize) -> String {
        let Some(engine) = self.engines.lock().unwrap().get(chat_jid).cloned() else {
            return String::new();
        };
        engine.workbench_service.fetch_logs(artifact_id, tail_lines)
    }

    // ===== Internal listener =====

    async fn run_listener(
        self: Arc<Self>,
        mut rx: broadcast::Receiver<EngineEvent>,
        chat_jid: String,
        cancel: CancellationToken,
    ) {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                msg = rx.recv() => match msg {
                    Err(broadcast::error::RecvError::Closed) => return,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("[WorkbenchBridge] dropped {n} events on chat={}", chat_jid);
                    }
                    Ok(EngineEvent::WorkbenchNew(data)) => {
                        self.handle_new(data, &chat_jid).await;
                    }
                    Ok(EngineEvent::WorkbenchServiceReady(data)) => {
                        self.handle_ready(data, &chat_jid);
                    }
                    Ok(EngineEvent::WorkbenchServiceCrashed(data)) => {
                        self.handle_crashed(data, &chat_jid);
                    }
                    Ok(EngineEvent::WorkbenchServiceStopped(data)) => {
                        self.handle_stopped(data, &chat_jid);
                    }
                    Ok(_) => {} // ignore non-workbench events
                }
            }
        }
    }

    async fn handle_new(&self, data: WorkbenchNewData, chat_jid: &str) {
        // 1) WebSocket fanout
        if let Some(cb) = self.on_new.lock().unwrap().as_ref() {
            cb(
                chat_jid,
                WorkbenchNewPayload {
                    artifact: data.artifact.clone(),
                    replaces_id: data.replaces_id.clone(),
                },
            );
        }

        // 2) IM channel fallback for non-web JIDs
        if !is_web_jid(chat_jid) {
            let cb = self.send_channel_notice.lock().unwrap().clone();
            if let Some(cb) = cb {
                let bot_token = self
                    .bot_tokens
                    .lock()
                    .unwrap()
                    .get(chat_jid)
                    .cloned()
                    .flatten();
                let notice = format_channel_notice(&data.artifact);
                cb(chat_jid, &notice, bot_token.as_deref());
            }
        }
    }

    fn handle_ready(&self, data: WorkbenchServiceReadyData, chat_jid: &str) {
        if let Some(cb) = self.on_service_ready.lock().unwrap().as_ref() {
            cb(
                chat_jid,
                WorkbenchServiceReadyPayload {
                    artifact_id: data.artifact_id,
                    ready: data.ready,
                },
            );
        }
    }

    fn handle_crashed(&self, data: WorkbenchServiceCrashedData, chat_jid: &str) {
        if let Some(cb) = self.on_service_crashed.lock().unwrap().as_ref() {
            cb(
                chat_jid,
                WorkbenchServiceCrashedPayload {
                    artifact_id: data.artifact_id,
                    last_log_lines: data.last_log_lines,
                },
            );
        }
    }

    fn handle_stopped(&self, data: WorkbenchServiceStoppedData, chat_jid: &str) {
        if let Some(cb) = self.on_service_stopped.lock().unwrap().as_ref() {
            cb(
                chat_jid,
                WorkbenchServiceStoppedPayload {
                    artifact_id: data.artifact_id,
                    reason: data.reason.as_str().to_string(),
                },
            );
        }
    }
}

// ===== helpers =====

fn is_web_jid(jid: &str) -> bool {
    jid.starts_with("web:")
}

fn format_channel_notice(artifact: &WorkbenchArtifact) -> String {
    let mut lines: Vec<String> = vec![format!("📁 Workbench: {}", artifact.title)];
    match artifact.mode {
        WorkbenchMode::Static if !artifact.files.is_empty() => {
            if artifact.files.len() == 1 {
                lines.push(format!("File: {}", artifact.files[0].path));
            } else {
                lines.push(format!("{} files:", artifact.files.len()));
                for f in &artifact.files {
                    lines.push(format!("• {}", f.path));
                }
            }
        }
        _ => {
            if let Some(ref url) = artifact.url {
                lines.push(format!("URL: {}", url));
            }
        }
    }
    lines.push("(See full view in WebUI)".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zen_core::{WorkbenchFile, WorkbenchMode};

    #[test]
    fn web_jid_detected() {
        assert!(is_web_jid("web:1"));
        assert!(!is_web_jid("telegram:123"));
        assert!(!is_web_jid("feishu:abc"));
    }

    #[test]
    fn channel_notice_single_static_file() {
        let a = WorkbenchArtifact {
            id: "x".into(),
            title: "Report".into(),
            mode: WorkbenchMode::Static,
            files: vec![WorkbenchFile {
                path: "out.html".into(),
                content: None,
                mime_type: None,
                hash: None,
                extension: None,
            }],
            url: None,
            process: None,
            usage: None,
            agent_id: None,
            instance_id: None,
            created_at: 0,
        };
        let notice = format_channel_notice(&a);
        assert!(notice.contains("Workbench: Report"));
        assert!(notice.contains("File: out.html"));
    }

    #[test]
    fn channel_notice_web_artifact_uses_url() {
        let a = WorkbenchArtifact {
            id: "x".into(),
            title: "Dashboard".into(),
            mode: WorkbenchMode::Web,
            files: vec![],
            url: Some("http://localhost:5173".into()),
            process: None,
            usage: None,
            agent_id: None,
            instance_id: None,
            created_at: 0,
        };
        let notice = format_channel_notice(&a);
        assert!(notice.contains("URL: http://localhost:5173"));
    }

    #[test]
    fn bridge_reverse_ops_return_false_for_unknown_jid() {
        let bridge = WorkbenchBridge::new();
        assert!(!bridge.mark_viewed("missing", "a"));
        assert!(!bridge.close("missing", "a"));
        assert!(bridge.read_file("missing", "a", "x").is_err());
        assert_eq!(bridge.fetch_logs("missing", "a", 10), "");
    }
}

impl Default for WorkbenchBridge {
    fn default() -> Self {
        Self::new()
    }
}
