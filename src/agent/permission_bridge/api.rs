//! PermissionBridgeApi trait — abstracts channel + sema-core dependencies.

use std::collections::HashMap;

use anyhow::Result;

use crate::types::InlineButton;

// ===== Callback data prefix constants =====

pub(crate) const PREFIX_PERM: &str = "P";
pub(crate) const PREFIX_ASK: &str = "Q";

// ===== API trait (abstracts sema-core + channel dependencies) =====

/// Abstracts the external dependencies of PermissionBridge:
/// channel message sending, sema-core responding, and web-chat detection.
///
/// Default no-op implementations are provided so partial wiring compiles;
/// the daemon replaces them with real implementations at startup.
#[allow(unused_variables)]
pub trait PermissionBridgeApi: Send + Sync {
    /// Send an inline-keyboard message to a chat.
    fn send_with_buttons(
        &self,
        chat_jid: &str,
        text: &str,
        buttons: &[InlineButton],
        bot_token: Option<&str>,
    ) -> Result<()> {
        Ok(())
    }

    /// Send a plain text message (fallback when buttons aren't supported).
    fn send_message(&self, chat_jid: &str, text: &str, bot_token: Option<&str>) -> Result<()> {
        Ok(())
    }

    /// Whether the channel for this JID supports inline buttons.
    fn supports_buttons(&self, _chat_jid: &str) -> bool {
        false
    }

    /// Whether this JID is a web-only chat (no backing channel adapter).
    fn is_web_jid(&self, _chat_jid: &str) -> bool {
        false
    }

    /// Route `respondToToolPermission` to the correct sema-core instance.
    fn respond_to_tool_permission(&self, _group_jid: &str, _tool_name: &str, _selected: &str) {}

    /// Route `respondToAskQuestion` to the correct sema-core instance.
    fn respond_to_ask_question(
        &self,
        _group_jid: &str,
        _agent_id: &str,
        _answers: HashMap<String, String>,
    ) {
    }

    /// Persist a globally-scoped auto-accept rule created by an "allow /
    /// never ask again" answer, so it survives a daemon restart and shows up
    /// in the Tool Rules UI where the user can revoke it. The bridge keeps the
    /// in-memory copy itself; this is only the durable half.
    fn persist_tool_rule(&self, _rule: &super::types::ToolAutoAcceptRule) {}

    /// Route a FormUI response to the correct sema-core instance.
    fn respond_to_form(
        &self,
        _group_jid: &str,
        _agent_id: &str,
        _values: HashMap<String, serde_json::Value>,
        _submitted: bool,
    ) {
    }
}
