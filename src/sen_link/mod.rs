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
//! on the LAN ([`lan`]) that a person typed, that mDNS announced
//! ([`discovery`]) or that a scan found ([`scan`]).

pub mod a2a;
pub mod api;
pub mod client;
pub mod discovery;
pub mod knowledge;
pub mod lan;
pub mod listener;
pub mod pairing;
pub mod probe;
pub mod scan;
pub mod seal;
pub mod server;
pub mod settings;
pub mod store;
pub mod tasks;

#[cfg(test)]
pub(crate) mod testkit;

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
    /// Other SLinks seen on the network (mDNS and scans).
    pub discovery: Arc<discovery::Discovery>,
    /// Wakes the mDNS supervisor when what it announces may have changed.
    pub discovery_wake: Arc<Notify>,
    /// The network scan, one at a time.
    pub scans: scan::Scans,
    /// Pairing handshakes this daemon has answered and not yet seen finish.
    pub sessions: pairing::Sessions,
    /// Request ids served lately, so a captured request cannot run twice.
    pub replay: seal::ReplayGuard,
    /// What a peer may ask this daemon about what it knows; absent where the daemon has no
    /// memory to answer from (tests, tools).
    pub knowledge: Option<Arc<knowledge::Knowledge>>,
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
        let discovery = Arc::new(discovery::Discovery::new(store.daemon_id().to_string()));
        Self {
            config_path,
            kanban,
            store,
            http: lan::client(),
            migrated,
            wake: Arc::new(Notify::new()),
            discovery,
            discovery_wake: Arc::new(Notify::new()),
            scans: scan::Scans::default(),
            sessions: pairing::Sessions::default(),
            replay: seal::ReplayGuard::default(),
            knowledge: None,
            listen: Mutex::new(listener::ListenStatus::default()),
        }
    }

    /// Let paired peers ask this daemon what it knows (within each peer's policy).
    pub fn with_knowledge(mut self, db: Arc<crate::db::Db>, profiles_dir: PathBuf) -> Self {
        self.knowledge = Some(Arc::new(knowledge::Knowledge::new(db, profiles_dir)));
        self
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
