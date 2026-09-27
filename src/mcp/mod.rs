//! MCP servers and helpers. Port targets: src-old/mcp/*.ts

pub mod admin_server;
pub mod background_server;
pub mod bridge;
pub mod browser_server;
pub mod client;
pub mod cognitive_server;
pub mod config;
pub mod dispatch_server;
pub mod external_client;
pub mod helper;
pub mod js_server;
pub mod litho_server;
pub mod manager;
pub mod memory_server;
pub mod ocr_server;
pub mod patterns_server;
pub mod sandbox_server;
pub mod schedule_server;
pub mod send_server;
pub mod space_apps;
pub mod space_server;
pub mod space_sync;
pub mod ts_transpile;
pub mod usage_server;
pub mod user_profile_server;
pub mod virtual_server;
pub mod docs_server;
pub mod wiki_server;
pub mod workspace_server;
pub mod core_server;

pub use client::{McpToolInfo, SharedMcpRegistry};
pub use config::{
    ExternalMcpServerConfig, McpConfigFile, McpConfigManager, McpScopeType, McpServerInfo,
    McpServerStatus, McpToolDef, McpTransportType,
};
pub use manager::{is_mcp_tool, parse_mcp_tool_name, BuiltInServerInfo, McpManager};
pub use core_server::CoreServer;
