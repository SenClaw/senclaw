//! Built-in Kanban task board — folded into the SenClaw core (no longer a
//! separate Space App). The REST API is mounted on the daemon UI server under
//! `/api/kanban/*`, the MCP is a native `kanban-server` stdio server, and the
//! in-process `MCPDispatcher` drives it directly over [`db::Db`]. The UI is a
//! native Flutter screen in `desktop_app` that talks to `/api/kanban/*`.
//!
//! Data lives in its own SQLite file (`~/.senclaw/space-apps/kanban/kanban.db`)
//! to preserve existing boards; only the *hosting* changed.

pub mod api;
pub mod db;
pub mod dispatch;
pub mod llm;
pub mod mcp;
pub mod templates;

pub use api::{make_state, AppState};
pub use db::Db;
