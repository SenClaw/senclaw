//! OS-level sandbox engine, integrated from `apps/sandbox`.
//!
//! Runs shell commands and code snippets isolated from the real machine:
//! `direct` (macOS Seatbelt / Linux bubblewrap / Windows AppContainer — write
//! access only inside the sandbox directory, read isolation selectable, network
//! off by default and openable per port) or `docker` (container with resource
//! limits). The engine is shared by:
//!
//!   - the `senclaw-sandbox` MCP server (`sbx_*` tools, `src/mcp/sandbox_server.rs`)
//!   - the daemon REST API (`/api/sandbox/*`, nested from the UI server)
//!   - the enforcement hooks ([`policy`]): agent `Bash` tool, the
//!     `/api/code/run` REPL (python / nodejs), and scheduler `script` tasks
//!
//! The Space App under `apps/sandbox` still exists as a standalone bundle; this
//! module is the same engine compiled into the daemon, with its own data root
//! (`~/.senclaw/sandbox`) so neither steps on the other.

pub mod api;
pub mod app_launch;
pub mod app_policy;
pub mod backend;
pub mod caps;
pub mod code;
pub mod config;
pub mod db;
pub mod files;
pub mod fsmode;
pub mod monitor;
pub mod mounts;
pub mod policy;
pub mod ports;
pub mod proxy;
pub mod pty;
pub mod runner;
pub mod settings;
pub mod state;
pub mod trace;

use std::sync::OnceLock;

use db::Db;

/// The daemon-wide engine handle: one sqlite connection for the UI server, the
/// scheduler and the in-process tools. The MCP subprocess opens its own
/// connection to the same file (WAL makes that safe).
///
/// `None` when the data directory cannot be created or the DB cannot open —
/// callers treat that as "sandbox unavailable" rather than panicking, because
/// the daemon must keep serving chats even if this feature is broken.
pub fn shared_db() -> Option<Db> {
    static DB: OnceLock<Option<Db>> = OnceLock::new();
    DB.get_or_init(|| {
        let dir = config::data_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::error!("[sandbox] cannot create data dir {}: {e}", dir.display());
            return None;
        }
        if let Err(e) = std::fs::create_dir_all(config::workspaces_dir()) {
            tracing::error!("[sandbox] cannot create workspaces dir: {e}");
            return None;
        }
        match Db::open(&config::db_path()) {
            Ok(db) => Some(db),
            Err(e) => {
                tracing::error!("[sandbox] cannot open {}: {e}", config::db_path());
                None
            }
        }
    })
    .clone()
}
