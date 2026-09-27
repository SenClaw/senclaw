//! Agent pool — core agent lifecycle management.
//!
//! Re-exports all public types previously accessible as `crate::agent::agent_pool::*`.

// Sub-modules
pub(crate) mod agent_api;
pub mod engine;
pub mod pool;
pub(crate) mod reflection;
pub(crate) mod state;
// Gated: the file's helpers (fakes, tempdir fixtures) reach for dev-dependencies
// that a plain `cargo check --lib` doesn't link.
#[cfg(test)]
mod tests;
pub mod traits;
pub mod types;
pub(crate) mod workspace;

// Re-export constants
pub use types::AGENT_TIMEOUT_MS;
pub(crate) use types::MAIN_AGENT_ID;

// Re-export public payload types
pub use types::{CachedTodos, PermissionsConfig, TodoSnapshot};

// Re-export event data types
pub use types::{
    AskQuestionRequestData, CompactExecData, CompactStartData, MessageCompleteData,
    SessionErrorData, StateUpdateData, TodosUpdateItem, ToolPermissionRequestData,
};

// Re-export traits
pub use traits::{AgentEventSink, AgentToolInfo, CachedTools, CoreApi};

// Re-export engine
pub use engine::ZenCoreApi;

// Re-export pool
pub use pool::AgentPool;

// Re-export callback types
pub use types::{ReplyFn, SendReplyFn, TypingFn};

// Re-export workspace types
pub use workspace::FeishuCredentials;
