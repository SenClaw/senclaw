//! SLink (code name sen-link) — SenClaw ↔ SenClaw peering over A2A v1.0
//! (JSON-RPC binding), on the local network only. Two daemons pair once with
//! a one-time invite code; after that each can delegate tasks to the other.
//! Incoming tasks become kanban cards (Triage by default, Ready for peers
//! trusted to auto-run), so the existing dispatcher — and Claude Code via
//! assignee `claude-code` — does the work. See `docs/sen-link.md`.
//!
//! The A2A surface is served by its own listener ([`listener`]); the
//! management API (`/api/sen-link/*`) stays on the UI server behind the
//! daemon's token. There is no relay and no public URL: a peer is an address
//! on the LAN that a person typed or a scan found ([`lan`]).

pub mod a2a;
pub mod api;
pub mod client;
pub mod lan;
pub mod listener;
pub mod probe;
pub mod server;
pub mod settings;
pub mod store;
pub mod tasks;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use tokio::sync::Notify;

pub struct LinkState {
    config_path: PathBuf,
    pub kanban: Arc<crate::kanban::db::Db>,
    pub store: store::PeerStore,
    /// The one HTTP client for outbound calls: LAN addresses only (`lan::client`).
    pub http: reqwest::Client,
    /// Peers dropped from `peers.json` at startup because only a hub reached them.
    pub migrated: Vec<String>,
    /// Wakes the listener supervisor after a settings change.
    pub wake: Arc<Notify>,
    listen: Mutex<listener::ListenStatus>,
}

impl LinkState {
    pub fn new(config: &crate::config::Config, kanban: Arc<crate::kanban::db::Db>) -> Self {
        Self::new_at(config.paths.global_config_path.clone(), kanban, store::PeerStore::new(store::PeerStore::default_path()))
    }

    pub fn new_at(config_path: PathBuf, kanban: Arc<crate::kanban::db::Db>, store: store::PeerStore) -> Self {
        let migrated = store.migrate_hub_leftovers().unwrap_or_else(|e| {
            tracing::warn!("[slink] could not clean hub leftovers out of peers.json: {e:#}");
            Vec::new()
        });
        if !migrated.is_empty() {
            tracing::warn!(peers = ?migrated, "[slink] dropped peers that only a hub reached; pair them again over the LAN");
        }
        Self {
            config_path,
            kanban,
            store,
            http: lan::client(),
            migrated,
            wake: Arc::new(Notify::new()),
            listen: Mutex::new(listener::ListenStatus::default()),
        }
    }

    pub fn settings(&self) -> settings::SenLinkSettings {
        settings::load(&self.config_path)
    }

    pub fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }

    pub fn listen_status(&self) -> listener::ListenStatus {
        self.listen.lock().unwrap().clone()
    }

    pub(crate) fn set_listen(&self, addr: Option<SocketAddr>, error: Option<String>) {
        *self.listen.lock().unwrap() = listener::ListenStatus { addr, error };
    }
}

/// The management API (`/api/sen-link/*`), for the UI server; it sits under the
/// daemon's API token gate. The A2A surface is [`listener::router`] instead.
pub fn api_router(state: Arc<LinkState>) -> Router {
    api::routes().with_state(state)
}
