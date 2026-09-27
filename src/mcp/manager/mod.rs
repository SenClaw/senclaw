//! Unified MCP server registry manager.
//!
//! Sits above both:
//!   - `SharedMcpRegistry` (built-in senclaw subprocess servers)
//!   - `McpConfigManager` + `ExternalMcpClient` (user-configured external servers)
//!
//! Provides a single API for listing, adding, removing, connecting, and
//! querying tools across all MCP servers regardless of origin.

pub mod service;
#[cfg(test)]
mod tests;
pub mod types;
pub mod utils;

// Re-exports for external consumers
pub use service::McpManager;
pub use types::BuiltInServerInfo;
pub use utils::{is_mcp_tool, parse_mcp_tool_name};
