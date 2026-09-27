//! Public payload types and internal pending state for permission bridge.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ===== Tool auto-accept rules (mirrors frontend ToolAutoAcceptRule) =====

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    AutoAccept,
    AutoDeny,
    ForceRequest,
    AutoAcceptAndAllow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleMatcherType {
    BashGlob,
    BashRegex,
    ToolExact,
    SkillExact,
    McpServer,
    McpGlob,
    ToolCategory,
    Always,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    FileEdit,
    Bash,
    Skill,
    Agent,
    Mcp,
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleMatcher {
    #[serde(rename = "type")]
    pub matcher_type: RuleMatcherType,
    pub pattern: Option<String>,
    pub tool_name: Option<String>,
    pub skill_name: Option<String>,
    pub server: Option<String>,
    pub tool: Option<String>,
    pub category: Option<ToolCategory>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAutoAcceptRule {
    pub id: String,
    pub matcher: RuleMatcher,
    pub action: RuleAction,
    pub enabled: bool,
    pub description: Option<String>,
}

// ===== Public payload types (mirrors TS PermissionPayload / AskQuestionPayload) =====

#[derive(Debug, Clone, Serialize)]
pub struct PermissionOption {
    pub key: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PermissionPayload {
    #[serde(rename = "toolName")]
    pub tool_name: String,
    pub title: String,
    /// Full untruncated content; frontend decides whether to collapse.
    pub content: String,
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AskQuestionOption {
    pub label: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AskQuestionData {
    pub header: String,
    pub question: String,
    pub options: Vec<AskQuestionOption>,
    #[serde(rename = "multiSelect")]
    pub multi_select: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AskQuestionPayload {
    #[serde(rename = "agentId")]
    pub agent_id: String,
    pub questions: Vec<AskQuestionData>,
}

/// FormUI request payload pushed to the Web UI (mirrors TS `FormPayload`).
#[derive(Debug, Clone, Serialize)]
pub struct FormPayload {
    #[serde(rename = "agentId")]
    pub agent_id: String,
    pub title: String,
    /// `"inline"` (chat card) or `"dock"` (workbench panel).
    pub surface: String,
    #[serde(rename = "submitLabel")]
    pub submit_label: String,
    pub fields: Vec<crate::zen_core::FormField>,
}

// ===== Internal pending state =====

pub(crate) struct PendingPermission {
    pub tool_name: String,
    /// What an "allow" answer gets persisted as. `tool_name` routes the
    /// response back to the waiting engine; this routes the approval into
    /// storage, and for Skill/Bash the two differ.
    pub permission_key: String,
    pub chat_jid: String,
    /// Identifies which group/core to respond to (typically the group JID).
    pub group_jid: String,
    /// The tool gate's audit row for this request, when it judged it; the
    /// person's answer is added to it.
    pub gate_log_id: Option<i64>,
}

pub(crate) struct PendingAskQuestion {
    pub agent_id: String,
    pub chat_jid: String,
    pub group_jid: String,
    pub questions: Vec<AskQuestionData>,
    /// Accumulated answers keyed by question text (Telegram step-by-step path).
    pub answers: HashMap<String, String>,
    /// Remaining unanswered count; triggers respond when zero.
    pub pending_count: usize,
}

pub(crate) struct PendingForm {
    pub agent_id: String,
    pub chat_jid: String,
    pub group_jid: String,
}
