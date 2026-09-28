//! WebSocket gateway core: WsGatewayApi trait, WebSocketGateway struct, route, broadcast.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::extract::ws::Message;
use tokio::sync::Mutex;

use crate::types::GroupBinding;

// ===== Trait for AgentPool / channel dependencies =====

#[async_trait]
pub trait WsGatewayApi: Send + Sync {
    /// Enqueue a message to the group queue for agent processing.
    fn enqueue_and_process(
        &self,
        _group_jid: &str,
        _group: &GroupBinding,
        _text: &str,
        _attachments: &[crate::types::MessageAttachment],
    ) {
    }
    /// Resolve a pending permission request.
    fn resolve_permission(&self, _request_id: &str, _option_key: &str) {}
    /// Add/replace a tool auto-accept rule.
    fn add_tool_rule(&self, _rule: crate::agent::permission_bridge::types::ToolAutoAcceptRule) {}
    /// Remove a tool auto-accept rule by ID.
    fn remove_tool_rule(&self, _rule_id: &str) {}
    /// Update an existing rule (replaces by ID).
    fn update_tool_rule(&self, _rule: crate::agent::permission_bridge::types::ToolAutoAcceptRule) {}
    /// Set the global "accept all" flag.
    fn set_accept_all(&self, _enabled: bool) {}
    /// Get all current rules (for sending to newly connected client).
    fn get_tool_rules(&self) -> Vec<crate::agent::permission_bridge::types::ToolAutoAcceptRule> {
        vec![]
    }
    /// Resolve a pending ask-question batch.
    fn resolve_ask_question(
        &self,
        _request_id: &str,
        _answers: &serde_json::Value,
        _other_texts: Option<&serde_json::Value>,
    ) {
    }
    /// Resolve a pending FormUI form. `submitted = false` = user skipped.
    fn resolve_form(&self, _request_id: &str, _values: &serde_json::Value, _submitted: bool) {}
    /// Pause the agent for a group.
    fn pause_agent(&self, _group_jid: &str) {}
    /// Resume the agent for a group, with optional follow-up query.
    fn resume_agent(&self, _group_jid: &str, _query: Option<&str>) {}
    /// Stop the agent for a group.
    async fn stop_agent(&self, _group_jid: &str) {}
    /// Switch agent mode (`"Agent" | "Plan"`) for a group's engine.
    fn set_agent_mode(&self, _group_jid: &str, _mode: &str) {}
    /// Read the current agent mode for a group. Returns `None` if no engine.
    fn get_agent_mode(&self, _group_jid: &str) -> Option<String> {
        None
    }
    /// Resolve a pending plan-exit approval. `selected` is one of
    /// `startEditing` / `clearContextAndStart` / `cancelled`. Delivers the
    /// choice to the waiting `ExitPlanMode` tool and (on approval) flips the
    /// engine back to Agent mode.
    fn resolve_plan_exit(&self, _group_jid: &str, _agent_id: &str, _selected: &str) {}

    /// Get current dispatch parents (for admin clients on subscribe).
    fn get_dispatch_parents(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    /// Get current agent todos (for admin clients on subscribe).
    fn get_agent_todos(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    /// Dismiss one agent's todo list from the console: drop the in-memory
    /// cache entry so subscribe snapshots stop replaying it. The DB row is
    /// deleted separately by the WS handler. Default no-op.
    fn dismiss_agent_todos(&self, _agent_jid: &str) {}
    /// Get current per-agent tool rosters (for admin clients on subscribe).
    /// Returned shape: `{ "<agentJid>": { "agentName": ..., "tools": [...] } }`.
    fn get_agent_tools(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    // Channel management (default no-ops — real impl provided by daemon wiring).
    async fn add_telegram_bot(&self, _token: &str) -> Result<()> {
        Ok(())
    }
    fn get_telegram_bot_user_id(&self, _token: &str) -> Option<String> {
        None
    }
    async fn ensure_feishu_channel(&self) -> Result<()> {
        Ok(())
    }
    async fn add_feishu_app_runtime(
        &self,
        _app_id: &str,
        _app_secret: &str,
        _domain: Option<&str>,
    ) -> Result<bool> {
        Ok(true)
    }
}

// ===== WebSocketGateway =====

/// Sink invoked for every `broadcast` targeting an `app:*` JID, so chat events
/// (tool executions, agent state, …) can be forwarded to mobile app channels
/// over the relay. Args: `(chat_jid, message)`.
pub type AppEventSink = Arc<dyn Fn(String, serde_json::Value) + Send + Sync>;

pub struct WebSocketGateway {
    pub port: u16,
    pub(crate) token: Option<String>,
    pub(crate) clients: Arc<Mutex<Vec<super::state::WsClient>>>,
    pub(crate) last_known_states: Arc<Mutex<HashMap<String, String>>>,
    /// Pending permission:request and question:request messages keyed by requestId.
    /// Stored so they can be replayed as a snapshot when an admin client (re)connects.
    pub(crate) pending_interactions: Arc<Mutex<HashMap<String, serde_json::Value>>>,
    /// Optional forwarder for `app:*` chat events → mobile relay clients.
    pub(crate) app_event_sink: std::sync::RwLock<Option<AppEventSink>>,
    /// Optional DB handle for the cowork runtime hooks. Filled by
    /// `set_db_for_cowork`; when absent the hooks are no-ops.
    pub(crate) db: std::sync::RwLock<Option<Arc<crate::db::Db>>>,
}

impl WebSocketGateway {
    pub fn new(port: u16, token: Option<String>) -> Self {
        Self {
            port,
            token,
            clients: Arc::new(Mutex::new(Vec::new())),
            last_known_states: Arc::new(Mutex::new(HashMap::new())),
            pending_interactions: Arc::new(Mutex::new(HashMap::new())),
            app_event_sink: std::sync::RwLock::new(None),
            db: std::sync::RwLock::new(None),
        }
    }

    /// Give the gateway a DB handle so cowork notify hooks can update
    /// task rows when agent state transitions. Optional — without it the
    /// hooks short-circuit cleanly.
    pub fn set_db_for_cowork(&self, db: Arc<crate::db::Db>) {
        *self.db.write().unwrap() = Some(db);
    }

    /// Install the forwarder that mirrors `app:*` chat events to mobile relay
    /// clients. Called once by daemon wiring after app channels are created.
    pub fn set_app_event_sink(&self, sink: AppEventSink) {
        *self.app_event_sink.write().unwrap() = Some(sink);
    }

    /// Build the axum route for WebSocket upgrade at `/ws`.
    /// Returns the router and handles for external event injection.
    pub fn route(&self, state: Arc<super::state::WsState>) -> axum::Router {
        let clients = self.clients.clone();
        let states = self.last_known_states.clone();
        let pending_interactions = self.pending_interactions.clone();
        let token = self.token.clone();

        let main_route = {
            let clients = clients.clone();
            let states = states.clone();
            let pending_interactions = pending_interactions.clone();
            let token = token.clone();
            let state = state.clone();
            axum::routing::get(move |ws: axum::extract::WebSocketUpgrade| {
                let clients = clients.clone();
                let states = states.clone();
                let pending_interactions = pending_interactions.clone();
                let token = token.clone();
                let state = state.clone();
                async move {
                    ws.on_upgrade(move |socket| {
                        super::connection::handle_connection(
                            socket,
                            clients,
                            states,
                            pending_interactions,
                            token,
                            state,
                        )
                    })
                }
            })
        };

        let browser_relay = state.browser_relay.clone();
        let browser_route = axum::routing::get(move |ws: axum::extract::WebSocketUpgrade| {
            let relay = browser_relay.clone();
            async move {
                ws.on_upgrade(move |socket| {
                    super::browser::handle_browser_connection(socket, relay)
                })
            }
        });

        let browser_relay2 = state.browser_relay.clone();
        let browser_mcp_route = axum::routing::get(move |ws: axum::extract::WebSocketUpgrade| {
            let relay = browser_relay2.clone();
            async move {
                ws.on_upgrade(move |socket| {
                    super::browser::handle_browser_mcp_connection(socket, relay)
                })
            }
        });

        // The browser engine v2 extension channel. Refused before the upgrade
        // unless Chrome stamped an extension Origin (a web page cannot forge
        // one); the extension then pairs or presents its token in-band.
        let browser_ext_route = axum::routing::get(
            |headers: axum::http::HeaderMap, ws: axum::extract::WebSocketUpgrade| async move {
                use axum::response::IntoResponse;
                let origin = headers
                    .get(axum::http::header::ORIGIN)
                    .and_then(|v| v.to_str().ok());
                match crate::browser_agent::extension::origin_extension_id(origin) {
                    Ok(ext_id) => ws
                        .on_upgrade(move |socket| {
                            crate::browser_agent::extension::handle_socket(socket, ext_id)
                        })
                        .into_response(),
                    Err(reason) => {
                        tracing::warn!("[browser] refused /browser/ext: {reason}");
                        (axum::http::StatusCode::FORBIDDEN, reason).into_response()
                    }
                }
            },
        );

        axum::Router::new()
            .route("/", main_route)
            .route("/browser", browser_route)
            .route("/browser-mcp", browser_mcp_route)
            .route("/browser/ext", browser_ext_route)
    }

    // ===== Broadcast helpers =====

    pub(crate) async fn broadcast_to_admins(&self, msg: &serde_json::Value) {
        let raw = msg.to_string();
        let clients = self.clients.lock().await;
        let total = clients.len();
        let mut sent = 0usize;
        for client in clients.iter() {
            if client.authenticated && client.is_admin {
                let _ = client.sender.send(Message::Text(raw.clone().into()));
                sent += 1;
            }
        }
        let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("?");
        tracing::info!(
            "[WsGateway] broadcast_to_admins type={msg_type} sent={sent}/{total} client(s)"
        );
        if sent == 0
            && (msg_type == "dispatch:update"
                || msg_type == "agent:todos"
                || msg_type == "task:backlog")
        {
            tracing::warn!(
                "[WsGateway] {msg_type} fired but NO admin clients connected — \
                 web client must subscribe to a group first"
            );
        }
    }

    /// Broadcast to admin clients that are NOT already subscribed to `skip_jid`.
    /// Used for permission events on non-admin groups: group subscribers get it via
    /// `broadcast(jid, ...)`, and admins watching the Agent Console get it here
    /// without receiving a duplicate if they happen to also subscribe to that group.
    pub(crate) async fn broadcast_to_admins_excluding(
        &self,
        skip_jid: &str,
        msg: &serde_json::Value,
    ) {
        let raw = msg.to_string();
        let clients = self.clients.lock().await;
        for client in clients.iter() {
            if client.authenticated && client.is_admin && !client.subscriptions.contains(skip_jid) {
                let _ = client.sender.send(Message::Text(raw.clone().into()));
            }
        }
    }

    pub(crate) async fn broadcast_to_all(&self, msg: &serde_json::Value) {
        let raw = msg.to_string();
        let clients = self.clients.lock().await;
        for client in clients.iter() {
            if client.authenticated {
                let _ = client.sender.send(Message::Text(raw.clone().into()));
            }
        }
    }

    pub(crate) async fn broadcast(&self, group_jid: &str, msg: &serde_json::Value) {
        let raw = msg.to_string();
        {
            let clients = self.clients.lock().await;
            for client in clients.iter() {
                if client.authenticated && client.subscriptions.contains(group_jid) {
                    let _ = client.sender.send(Message::Text(raw.clone().into()));
                }
            }
        }
        // Mirror to mobile relay clients for app-channel chats.
        if group_jid.starts_with("app:") {
            let sink = self
                .app_event_sink
                .read()
                .ok()
                .and_then(|g| g.as_ref().cloned());
            if let Some(sink) = sink {
                sink(group_jid.to_string(), msg.clone());
            }
        }
    }
}
