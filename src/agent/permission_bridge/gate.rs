//! A gate that may approve a permission request before a person sees it — the
//! seam the tool-call gate (`crate::decision::gate`) plugs into, and the same
//! place a Claude Code `PermissionRequest` hook runs: after the engine's own
//! safe list, saved grants and the auto-accept rules have all said "ask".
//!
//! A gate can only turn a prompt into a **one-time** allow. It never denies:
//! a gate that cannot decide, fails, panics or is slow leaves the prompt in
//! place, so the permission path stays fail-closed toward the person.

use async_trait::async_trait;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GateCall {
    /// Skip the prompt and approve this one call.
    pub allow: bool,
    /// The audit row, so the person's answer to the prompt can join it.
    pub log_id: Option<i64>,
}

#[async_trait]
pub trait PermissionGate: Send + Sync {
    /// Whether this request is one the gate judges at all. Everything else
    /// goes straight to the prompt.
    fn wants(&self, tool_name: &str, content: &serde_json::Value) -> bool;

    /// Judge one request that is about to prompt.
    async fn judge(&self, tool_name: &str, content: &serde_json::Value, chat_jid: &str) -> GateCall;

    /// What the person answered a prompt the gate had judged.
    fn record_answer(&self, log_id: i64, option_key: &str);
}
