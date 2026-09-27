//! Autonomous work dispatch — shared surface for the core `MCPDispatcher` engine
//! and any dispatchable source.
//!
//! - **In-process source:** implement [`DispatchSource`] over your data (or wrap a
//!   [`DispatchProvider`] with [`LocalDispatchSource`]).
//! - **Remote Space App:** implement [`DispatchProvider`] and mount
//!   [`dispatch_router`]; the engine drives it via [`HttpDispatchSource`].
//! - **Worker tools:** declare [`McpServerSpec`]s on a [`WorkItem`] — prefer native
//!   `Stdio` servers (the built-in Kanban uses `senclaw kanban-server`).

mod provider;
mod source;
mod types;

pub use provider::{DispatchProvider, dispatch_router};
pub use source::{DispatchSource, HttpDispatchSource, LocalDispatchSource};
pub use types::{
    Capacity, FinalizeRequest, ItemIdRequest, McpServerSpec, Outcome, PollRequest, WorkItem,
    Workspace,
};
