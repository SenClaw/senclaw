//! Registering the MCP servers declared by enabled marketplace plugins.
//!
//! A plugin's `.mcp.json` names servers; this turns them into external MCP
//! servers on the shared [`McpManager`], which is what puts their tools in
//! every agent's roster (`mcp::bridge::McpBridgeTool::from_manager`). One
//! manager means one source of truth: the same servers appear at
//! `GET /api/mcp-servers` and can be inspected there.
//!
//! Registration is **not persisted**. The servers belong to the plugin, so
//! they are rebuilt from the enabled plugins whenever those change; a copy in
//! the user's own `mcp.json` would outlive an uninstall and leave the daemon
//! spawning a command that no longer exists.

use std::sync::{Arc, Mutex};

use crate::mcp::config::{ExternalMcpServerConfig, McpTransportType};
use crate::mcp::manager::McpManager;

use super::manager::MarketplaceManager;
use super::types::MarketplacePluginMCPServer;

/// Turn one plugin-declared server into an external MCP config, or explain why
/// it cannot be registered.
pub fn to_external_config(
    server: &MarketplacePluginMCPServer,
) -> Result<ExternalMcpServerConfig, String> {
    if let Some(reason) = &server.blocked_reason {
        return Err(reason.clone());
    }
    let transport = match server.transport.as_str() {
        "stdio" => McpTransportType::Stdio,
        "sse" => McpTransportType::Sse,
        "http" | "streamable-http" | "streamable_http" => McpTransportType::Http,
        other => return Err(format!("unknown transport `{other}`")),
    };
    let cfg = ExternalMcpServerConfig {
        name: server.name.clone(),
        transport,
        description: server.description.clone(),
        enabled: true,
        use_tools: server.use_tools.clone(),
        command: server.command.clone(),
        args: server.args.clone(),
        env: server.env.clone(),
        url: server.url.clone(),
        headers: server.headers.clone(),
    };
    cfg.validate()?;
    Ok(cfg)
}

/// Rebuild the manager's plugin-contributed servers from what the enabled
/// plugins currently declare. Safe to call whenever a plugin is toggled.
pub async fn sync(marketplace: &Arc<Mutex<MarketplaceManager>>, mcp: &Arc<McpManager>) {
    let declared = {
        let Ok(mgr) = marketplace.lock() else {
            tracing::warn!("[Marketplace] MCP sync skipped: manager lock poisoned");
            return;
        };
        mgr.get_enabled_mcp_servers()
    };

    let mut configs = Vec::new();
    for server in &declared {
        match to_external_config(server) {
            Ok(cfg) => configs.push(cfg),
            Err(reason) => tracing::warn!(
                "[Marketplace] MCP server {} not registered: {reason}",
                server.name
            ),
        }
    }

    let n = configs.len();
    let registered = mcp.sync_plugin_servers(configs).await;
    tracing::info!(
        "[Marketplace] {} plugin MCP server(s) declared, {n} usable, {} registered",
        declared.len(),
        registered.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn stdio(name: &str, command: &str) -> MarketplacePluginMCPServer {
        MarketplacePluginMCPServer {
            name: name.into(),
            transport: "stdio".into(),
            command: Some(command.into()),
            ..Default::default()
        }
    }

    #[test]
    fn stdio_server_becomes_a_launchable_config() {
        let mut s = stdio("mkt__demo__weather", "/usr/bin/node");
        s.args = vec!["server.js".into()];
        s.env = HashMap::from([("KEY".to_string(), "v".to_string())]);
        let cfg = to_external_config(&s).unwrap();
        assert_eq!(cfg.transport, McpTransportType::Stdio);
        assert_eq!(cfg.command.as_deref(), Some("/usr/bin/node"));
        assert_eq!(cfg.args, vec!["server.js".to_string()]);
        assert_eq!(cfg.env.get("KEY").map(String::as_str), Some("v"));
        assert!(cfg.enabled);
    }

    #[test]
    fn http_server_needs_a_url() {
        let mut s = MarketplacePluginMCPServer {
            name: "mkt__demo__remote".into(),
            transport: "http".into(),
            url: Some("https://example.test/mcp".into()),
            ..Default::default()
        };
        assert!(to_external_config(&s).is_ok());
        s.url = None;
        assert!(to_external_config(&s).is_err());
    }

    #[test]
    fn a_blocked_command_is_never_turned_into_a_config() {
        // The plugin toggle is consent to run the plugin, not consent to run
        // anything: a destructive command still needs a person to look at it.
        let mut s = stdio("mkt__demo__evil", "sh -c 'rm -rf /'");
        s.blocked_reason = Some("destructive verb".into());
        assert_eq!(to_external_config(&s).unwrap_err(), "destructive verb");
    }

    #[test]
    fn an_unknown_transport_is_refused_rather_than_guessed() {
        let mut s = stdio("mkt__demo__x", "/bin/true");
        s.transport = "carrier-pigeon".into();
        assert!(to_external_config(&s).unwrap_err().contains("carrier-pigeon"));
    }
}
