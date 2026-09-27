//! Data types for the MCP manager — runtime server state and built-in server info.

use crate::mcp::config::{
    ExternalMcpServerConfig, McpScopeType, McpServerInfo, McpServerStatus, McpToolDef,
};
use crate::mcp::external_client::ExternalMcpClient;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Runtime state per external server
// ---------------------------------------------------------------------------

/// Runtime state for one external MCP server.
pub(crate) struct ExternalServerState {
    pub(crate) config: ExternalMcpServerConfig,
    pub(crate) scope: McpScopeType,
    pub(crate) client: Option<ExternalMcpClient>,
    pub(crate) tools: Vec<McpToolDef>,
    pub(crate) status: McpServerStatus,
    pub(crate) error: Option<String>,
}

impl ExternalServerState {
    pub(crate) fn new(config: ExternalMcpServerConfig, scope: McpScopeType) -> Self {
        Self {
            config,
            scope,
            client: None,
            tools: Vec::new(),
            status: McpServerStatus::Disconnected,
            error: None,
        }
    }

    pub(crate) fn to_info(&self) -> McpServerInfo {
        McpServerInfo {
            config: self.config.clone(),
            scope: self.scope,
            status: self.status,
            tools: if self.tools.is_empty() {
                None
            } else {
                Some(self.tools.clone())
            },
            error: self.error.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in server info (lightweight — no ExternalMcpClient, uses SharedMcpRegistry)
// ---------------------------------------------------------------------------

/// Describes a built-in MCP server for the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuiltInServerInfo {
    pub name: String,
    pub transport: String, // always "stdio"
    pub description: Option<String>,
    /// Full tool definitions for this built-in server.
    pub tools: Vec<McpToolDef>,
}
