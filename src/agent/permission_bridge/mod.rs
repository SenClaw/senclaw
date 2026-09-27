//! Permission bridge — relay sema-core permission requests to inline keyboards / Web UI.
//! Port target: src-old/agent/PermissionBridge.ts
//!
//! Stores pending permission and ask-question requests keyed by request ID (8-char hex).
//! Routes inline-button callback data back to sema-core via the [`PermissionBridgeApi`] trait.
//! Web UI can also resolve requests via `resolve_permission` / `resolve_ask_question_batch`.
//!
//! Channel-agnostic: callbacks and channel sends go through the API trait so the daemon
//! wiring connects them to real channel adapters (Telegram, Feishu, etc.).

pub mod api;
pub mod bridge;
pub mod gate;
pub mod types;
pub(crate) mod utils;

// Re-export public items from submodules
pub use api::PermissionBridgeApi;
pub use bridge::PermissionBridge;
pub use gate::{GateCall, PermissionGate};
pub use types::{
    AskQuestionData, AskQuestionOption, AskQuestionPayload, FormPayload, PermissionOption,
    PermissionPayload,
};

#[cfg(test)]
mod tests;
