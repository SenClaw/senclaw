//! Admin MCP server. Port target: src-old/mcp/admin-server.ts
//!
//! Tools that were here (group register/unregister/update, list_groups,
//! list_all_tasks, manage_task) have been moved to CLI and WebSocket Gateway
//! commands. The server exists as a placeholder but registers no tools.

use anyhow::Result;
use rmcp::ServiceExt;

/// Admin MCP server — intentionally has no tools registered.
/// Group management moved to CLI (`channel group add/remove`),
/// task queries moved to WebSocket Gateway direct-query commands.
#[derive(Clone)]
pub struct AdminServer;

impl AdminServer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AdminServer {
    fn default() -> Self {
        Self::new()
    }
}

// ===== MCP stdio server =====

#[rmcp::tool_router(server_handler)]
impl AdminServer {}

/// Start the admin MCP server over stdio.
pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server = AdminServer::new();
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
