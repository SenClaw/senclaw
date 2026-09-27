//! Bridge between MCP external tools and zen-core Tool trait.
//!
//! Each external MCP tool (from a user-configured MCP server) is wrapped in
//! an `McpBridgeTool` so it can participate in the agent's tool roster.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolResultMessage};

use super::manager::McpManager;

/// A single external MCP tool, adapted to the zen-core `Tool` trait.
pub struct McpBridgeTool {
    /// Full `mcp__<server>__<tool>` name.
    pub full_name: String,
    /// Short tool name (without mcp prefix).
    pub display_name: String,
    /// Human-readable description.
    pub desc: String,
    /// JSON Schema for input parameters (may be Null).
    pub schema: Value,
    /// Reference to the manager for call dispatch.
    pub manager: Arc<McpManager>,
}

impl McpBridgeTool {
    /// Create bridge tools for all currently-connected external MCP servers.
    pub async fn from_manager(manager: &Arc<McpManager>) -> Vec<Arc<dyn Tool>> {
        let infos = manager.get_external_tools_full().await;
        infos
            .into_iter()
            .map(|info| {
                let display_name = info.name.clone();
                Arc::new(Self {
                    full_name: info.name,
                    display_name,
                    desc: info.description,
                    schema: info.input_schema,
                    manager: Arc::clone(manager),
                }) as Arc<dyn Tool>
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl Tool for McpBridgeTool {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn description(&self) -> &str {
        &self.desc
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn call(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let result = self
            .manager
            .call_external_tool(&self.full_name, input)
            .await?;
        let summary = match &result {
            Value::String(s) => s.clone(),
            other => serde_json::to_string_pretty(other).unwrap_or_else(|_| format!("{other:?}")),
        };
        Ok(vec![ToolOutput::Result {
            data: result,
            result_for_assistant: summary,
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, input: &Value) -> ToolResultMessage {
        let summary = match data {
            Value::String(s) => s.clone(),
            other => serde_json::to_string_pretty(other).unwrap_or_else(|_| format!("{other:?}")),
        };
        // The display name alone is not an account of a step: "Browser Search"
        // says nothing about what was searched. Prefer what the caller asked
        // for; when the input says nothing useful — `browser_extract_text`
        // takes a tab id and a selector — fall back to what came back.
        let hint = crate::zen_core::run_tools::argument_hint(input)
            .or_else(|| crate::zen_core::run_tools::result_excerpt(data));
        ToolResultMessage {
            title: match hint {
                Some(h) => format!("{}: {h}", self.display_name),
                None => self.display_name.clone(),
            },
            summary,
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        // No result yet here (this also titles the permission prompt), so the
        // input is all there is — and a prompt that names the URL it is about
        // to open is the point.
        match crate::zen_core::run_tools::argument_hint(input) {
            Some(h) => format!("{}: {h}", self.display_name),
            None => self.display_name.clone(),
        }
    }

    // ===== Lazy-load policy =====
    //
    // 100+ MCP tools across 11 servers blow the prompt to ~17k tokens. Default
    // to **deferred** so they're only included after the LLM finds them via
    // `ToolSearch`. A small whitelist of high-frequency tools stays loaded.
    fn should_defer(&self) -> bool {
        !ALWAYS_LOADED_MCP_TOOLS.contains(&self.full_name.as_str())
    }

    fn search_hint(&self) -> String {
        // First sentence of description, prefixed with display_name so name
        // tokens contribute to scoring.
        let first_sentence = self.desc.split('.').next().unwrap_or("").trim().to_string();
        if first_sentence.is_empty() {
            self.display_name.clone()
        } else {
            format!("{} — {first_sentence}", self.display_name)
        }
    }
}

/// MCP tools that **always** stay in the initial prompt — never deferred.
///
/// Pick these for capabilities the model needs to invoke without prior
/// discovery (memory retrieval, schedule listing, etc.). Override-able by
/// the per-group `allowed_tools` whitelist if admins want a different set.
pub const ALWAYS_LOADED_MCP_TOOLS: &[&str] = &[
    // Both naming schemes covered (engine.rs strips "senclaw-" for the
    // per-engine McpRegistryBridgeTool path; the manager-driven McpBridgeTool
    // keeps the full prefix). Listing both lets either bridge match.
    "mcp__memory__search",
    "mcp__senclaw-memory__search",
    "mcp__memory__memory_search",
    "mcp__senclaw-memory__memory_search",
    "mcp__schedule__list_schedules",
    "mcp__senclaw-schedule__list_schedules",
    // `schedule_watch` is how an agent keeps a promise to report back on a long
    // job. Deferring it means the agent must *discover* it mid-turn, and that
    // discovery failed in the field: told to load
    // `mcp__senclaw-schedule__schedule_watch`, ToolSearch answered "no
    // registered tool" — under the default bundled core server the real name is
    // `mcp__core__schedule_watch`. The agent then promised a notification it
    // had no way to deliver. Always-loading it removes the discovery step, and
    // both spellings are listed because either bridge may be the live one.
    "mcp__core__schedule_watch",
    "mcp__senclaw-schedule__schedule_watch",
    "mcp__workspace__workspace_info",
    "mcp__senclaw-workspace__workspace_info",
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn mk(full_name: &str, desc: &str) -> McpBridgeTool {
        // Use a real McpManager constructed with throwaway paths — none of these
        // tests exercise `call()` so the manager never spawns any subprocess.
        let mgr = Arc::new(McpManager::new(
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp"),
        ));
        McpBridgeTool {
            full_name: full_name.to_string(),
            display_name: full_name
                .rsplit("__")
                .next()
                .unwrap_or(full_name)
                .to_string(),
            desc: desc.to_string(),
            schema: Value::Null,
            manager: mgr,
        }
    }

    #[test]
    fn defers_unknown_mcp_tools() {
        let t = mk(
            "mcp__senclaw-browser__screenshot",
            "Capture a screenshot of the current page.",
        );
        assert!(t.should_defer());
    }

    #[test]
    fn always_loaded_set_overrides_defer() {
        let t = mk("mcp__senclaw-memory__search", "Search the memory store.");
        assert!(!t.should_defer());
    }

    #[test]
    fn search_hint_uses_first_sentence() {
        let t = mk(
            "mcp__senclaw-browser__screenshot",
            "Capture a screenshot of the current page. Returns PNG bytes.",
        );
        let hint = t.search_hint();
        assert!(hint.contains("screenshot"));
        assert!(hint.contains("Capture a screenshot of the current page"));
        // No second sentence
        assert!(!hint.contains("PNG bytes"));
    }

    #[test]
    fn search_hint_falls_back_to_display_name_when_desc_empty() {
        let t = mk("mcp__senclaw-x__foo", "");
        assert_eq!(t.search_hint(), "foo");
    }
}

#[cfg(test)]
mod always_loaded_tests {
    use super::ALWAYS_LOADED_MCP_TOOLS;

    #[test]
    fn schedule_watch_is_never_deferred_under_either_naming_scheme() {
        // How this broke in the field: the agent was told to arm a watch,
        // searched for `mcp__senclaw-schedule__schedule_watch`, and ToolSearch
        // answered "no registered tool" — under the default bundled core server
        // the live name is `mcp__core__schedule_watch`. Having promised to
        // report back, it then had no way to. Always-loading the tool removes
        // the discovery step entirely; listing both spellings covers bundled
        // and per-server modes, since either bridge may be the live one.
        assert!(ALWAYS_LOADED_MCP_TOOLS.contains(&"mcp__core__schedule_watch"));
        assert!(ALWAYS_LOADED_MCP_TOOLS.contains(&"mcp__senclaw-schedule__schedule_watch"));
    }
}
