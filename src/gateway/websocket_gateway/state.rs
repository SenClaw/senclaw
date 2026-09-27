// ===== WsClient =====

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::ws::Message;

use super::browser::BrowserRelay;
use crate::types::AgentApi;

pub(crate) struct WsClient {
    pub(crate) sender: tokio::sync::mpsc::UnboundedSender<Message>,
    pub(crate) authenticated: bool,
    pub(crate) is_admin: bool,
    pub(crate) subscriptions: HashSet<String>,
    /// Tombstone flag. Connections are identified by their positional index in
    /// the `Vec<WsClient>` (`client_idx`), captured once at connect. `Vec::remove`
    /// would shift every later element down and invalidate those cached indices —
    /// so a disconnecting client would misroute the events of everyone after it
    /// (its `subscribe` recorded on the wrong client, its `history:load` sent to
    /// the wrong socket). Instead we mark the slot `dead` (indices stay stable)
    /// and reuse it for the next connection. Dead slots are `authenticated=false`
    /// with empty `subscriptions`, so every broadcast already skips them.
    pub(crate) dead: bool,
}

// ===== Shared state passed through to handlers =====

pub struct WsState {
    pub config: Arc<crate::config::Config>,
    pub db: Arc<crate::db::Db>,
    pub group_manager: Arc<crate::gateway::group_manager::GroupManager>,
    pub agent_manager: Arc<crate::gateway::agent_manager::AgentManager>,
    pub binding_manager: Arc<crate::gateway::binding_manager::BindingManager>,
    pub channel_manager: Arc<crate::gateway::channel_manager::ChannelManager>,
    pub api: Arc<dyn super::gateway::WsGatewayApi>,
    pub agent_api: Option<Arc<dyn AgentApi>>,
    pub browser_relay: Arc<BrowserRelay>,
    /// Shared with `UiState` and `MessageRouter` so `/plugin` chat commands and
    /// the marketplace panel operate on one manager. `None` disables the
    /// commands (they fall through to normal chat).
    pub marketplace_manager:
        Option<Arc<std::sync::Mutex<crate::marketplace::manager::MarketplaceManager>>>,
}
