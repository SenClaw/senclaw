//! sen-link — SenClaw ↔ SenClaw peering over A2A v1.0 (JSON-RPC binding).
//! Two daemons pair once with a one-time invite code; after that each can
//! delegate tasks to the other. Incoming tasks become kanban cards (Triage by
//! default, Ready for peers trusted to auto-run), so the existing dispatcher
//! — and Claude Code via assignee `claude-code` — does the work. See
//! `docs/sen-link.md`.

pub mod a2a;
pub mod api;
pub mod client;
pub mod server;
pub mod settings;
pub mod store;
pub mod tasks;

use std::path::PathBuf;
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

pub struct LinkState {
    config_path: PathBuf,
    pub kanban: Arc<crate::kanban::db::Db>,
    pub store: store::PeerStore,
    pub http: reqwest::Client,
}

impl LinkState {
    pub fn new(config: &crate::config::Config, kanban: Arc<crate::kanban::db::Db>) -> Self {
        Self::new_at(config.paths.global_config_path.clone(), kanban, store::PeerStore::new(store::PeerStore::default_path()))
    }

    pub fn new_at(config_path: PathBuf, kanban: Arc<crate::kanban::db::Db>, store: store::PeerStore) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self { config_path, kanban, store, http }
    }

    pub fn settings(&self) -> settings::SenLinkSettings {
        settings::load(&self.config_path)
    }

    pub fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }
}

/// Every sen-link route: the public A2A surface and the `/api/sen-link/*`
/// management API (the latter sits under the daemon's API token gate).
pub fn router(state: Arc<LinkState>) -> Router {
    Router::new()
        .route("/.well-known/agent-card.json", get(server::agent_card))
        .route("/a2a", post(server::rpc))
        .route("/a2a/pair", post(server::pair))
        .merge(api::routes())
        .with_state(state)
}
