//! MCP external server configuration persistence.
//!
//! Mirrors sema-core `MCPManager` config loading/saving pattern.
//! Config is stored as JSON on disk:
//!   - `~/.senclaw/mcp.json` — user scope
//!   - `{working_dir}/.senclaw/mcp.json` — project scope
//!
//! File format per sema-core convention:
//! ```json
//! {
//!   "mcpServers": {
//!     "server-name": { ... }
//!   }
//! }
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Transport & Scope enums
// ---------------------------------------------------------------------------

/// MCP transport protocol. Mirrors `MCPTransportType` in sema-core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransportType {
    Stdio,
    Sse,
    Http,
}

/// Scope where a server configuration is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpScopeType {
    User,
    Project,
}

// ---------------------------------------------------------------------------
// Server config
// ---------------------------------------------------------------------------

/// Full external MCP server configuration.
/// Mirrors `MCPServerConfig` from sema-core types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalMcpServerConfig {
    /// Unique server name.
    pub name: String,

    /// Transport type (stdio / sse / http).
    pub transport: McpTransportType,

    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Whether the server is enabled. Defaults to `true`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Allowlist of tool names exposed from this server.
    /// `null` or missing means all tools are exposed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_tools: Option<Vec<String>>,

    // -- stdio fields --
    /// Executable path (required for stdio transport).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,

    /// Arguments passed to the command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,

    /// Environment variables injected into the subprocess.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env: HashMap<String, String>,

    // -- sse / http fields --
    /// Server URL (required for sse/http transports).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// HTTP headers sent to the server.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
}

fn default_enabled() -> bool {
    true
}

impl ExternalMcpServerConfig {
    /// Validate the configuration. Returns an error message string on failure.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("server name is required".into());
        }
        match self.transport {
            McpTransportType::Stdio => match &self.command {
                None => Err("command is required for stdio transport".into()),
                Some(c) if c.trim().is_empty() => {
                    Err("command must not be empty for stdio transport".into())
                }
                _ => Ok(()),
            },
            McpTransportType::Sse | McpTransportType::Http => match &self.url {
                None => Err("url is required for sse/http transports".into()),
                Some(u) if u.trim().is_empty() => {
                    Err("url must not be empty for sse/http transports".into())
                }
                _ => Ok(()),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin `.mcp.json`
// ---------------------------------------------------------------------------

/// The entries of a plugin's `.mcp.json`, in either shape it is written in.
///
/// SenClaw's own files nest servers under `mcpServers`. A Claude Code plugin's
/// `.mcp.json` is **flat** — the top level *is* the server map — and plugins
/// published for both ship one or the other. Reading only one shape makes the
/// other file look empty: no servers to launch, and nothing to scan either.
///
/// A flat file is recognised by its values looking like server configs
/// (carrying `command`, `url` or `type`), so a nested file is never mistaken
/// for a server named `mcpServers`.
pub fn parse_plugin_mcp_json(text: &str) -> Vec<(String, serde_json::Value)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    if let Some(map) = v.get("mcpServers").and_then(|m| m.as_object()) {
        return map.iter().map(|(k, c)| (k.clone(), c.clone())).collect();
    }
    let Some(map) = v.as_object() else {
        return Vec::new();
    };
    map.iter()
        .filter(|(_, c)| {
            c.get("command").is_some() || c.get("url").is_some() || c.get("type").is_some()
        })
        .map(|(k, c)| (k.clone(), c.clone()))
        .collect()
}

/// Expand `${CLAUDE_PLUGIN_ROOT}` (and the SenClaw spelling) in a value taken
/// from a plugin's `.mcp.json`. Claude Code plugins use the former to point at
/// their own directory; leaving it verbatim spawns a command under a literal
/// `${CLAUDE_PLUGIN_ROOT}` path that does not exist.
pub fn expand_plugin_root(value: &str, plugin_root: &str) -> String {
    value
        .replace("${CLAUDE_PLUGIN_ROOT}", plugin_root)
        .replace("${SENCLAW_PLUGIN_ROOT}", plugin_root)
}

// ---------------------------------------------------------------------------
// Server runtime status (not persisted)
// ---------------------------------------------------------------------------

/// Runtime connection status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpServerStatus {
    Disconnected,
    Connecting,
    Connected,
    Error,
}

/// A lightweight tool definition obtained from the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "inputSchema"
    )]
    pub input_schema: Option<serde_json::Value>,
}

/// Combined server config + runtime info sent to the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct McpServerInfo {
    #[serde(flatten)]
    pub config: ExternalMcpServerConfig,
    pub scope: McpScopeType,
    pub status: McpServerStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<McpToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Config file format
// ---------------------------------------------------------------------------

/// Top-level JSON structure stored on disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfigFile {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub mcp_servers: HashMap<String, ExternalMcpServerConfig>,
}

// ---------------------------------------------------------------------------
// Config manager
// ---------------------------------------------------------------------------

/// Manages loading and saving MCP config JSON files for user and project scopes.
#[derive(Debug, Clone)]
pub struct McpConfigManager {
    /// Path to the user-level config file: `~/.senclaw/mcp.json`
    user_config_path: PathBuf,
}

impl McpConfigManager {
    /// Create a new manager.
    pub fn new(user_config_path: PathBuf) -> Self {
        Self { user_config_path }
    }

    /// Build a project-scope config path from a working directory.
    pub fn project_config_path(working_dir: &Path) -> PathBuf {
        working_dir.join(".senclaw").join("mcp.json")
    }

    // ---- load ----

    /// Load the config file at the given path, returning an empty default when
    /// the file is missing or unreadable.
    pub fn load_config(&self, path: &Path) -> McpConfigFile {
        match std::fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str::<McpConfigFile>(&raw) {
                Ok(cfg) => cfg,
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to parse MCP config file, using empty defaults"
                    );
                    McpConfigFile::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => McpConfigFile::default(),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to read MCP config file, using empty defaults"
                );
                McpConfigFile::default()
            }
        }
    }

    /// Load user-scope config.
    pub fn load_user_config(&self) -> McpConfigFile {
        self.load_config(&self.user_config_path)
    }

    /// Load project-scope config.
    pub fn load_project_config(&self, working_dir: &Path) -> McpConfigFile {
        self.load_config(&Self::project_config_path(working_dir))
    }

    /// Load both scopes and merge, with project taking precedence over user.
    pub fn load_merged(&self, working_dir: &Path) -> McpConfigFile {
        let mut merged = self.load_user_config();
        let project = self.load_project_config(working_dir);
        for (name, cfg) in project.mcp_servers {
            merged.mcp_servers.insert(name, cfg);
        }
        merged
    }

    // ---- save ----

    /// Save a config file atomically (write to temp, then rename).
    pub fn save_config(&self, path: &Path, config: &McpConfigFile) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(config)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        // Write to temp file in the same directory, then rename atomically.
        let tmp = path.with_extension("tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(raw.as_bytes())?;
            f.flush()?;
        }
        std::fs::rename(&tmp, path)?;
        // MCP config carries per-server `env` blocks, which is where API keys
        // and tokens for external servers live.
        crate::util::file_perms::restrict_best_effort(path);
        Ok(())
    }

    /// Save user-scope config.
    pub fn save_user_config(&self, config: &McpConfigFile) -> std::io::Result<()> {
        self.save_config(&self.user_config_path, config)
    }

    /// Save project-scope config.
    pub fn save_project_config(
        &self,
        working_dir: &Path,
        config: &McpConfigFile,
    ) -> std::io::Result<()> {
        self.save_config(&Self::project_config_path(working_dir), config)
    }

    /// Add or update a server in the given scope and persist.
    pub fn add_or_update_server(
        &self,
        working_dir: &Path,
        scope: McpScopeType,
        server: &ExternalMcpServerConfig,
    ) -> std::io::Result<()> {
        let path = match scope {
            McpScopeType::User => self.user_config_path.clone(),
            McpScopeType::Project => Self::project_config_path(working_dir),
        };
        let mut cfg = self.load_config(&path);
        cfg.mcp_servers.insert(server.name.clone(), server.clone());
        self.save_config(&path, &cfg)
    }

    /// Remove a server from the given scope and persist.
    pub fn remove_server(
        &self,
        working_dir: &Path,
        scope: McpScopeType,
        name: &str,
    ) -> std::io::Result<bool> {
        let path = match scope {
            McpScopeType::User => self.user_config_path.clone(),
            McpScopeType::Project => Self::project_config_path(working_dir),
        };
        let mut cfg = self.load_config(&path);
        let existed = cfg.mcp_servers.remove(name).is_some();
        if existed {
            self.save_config(&path, &cfg)?;
        }
        Ok(existed)
    }

    /// Update the `use_tools` allowlist for a server and persist.
    pub fn update_use_tools(
        &self,
        working_dir: &Path,
        scope: McpScopeType,
        name: &str,
        tool_names: &Option<Vec<String>>,
    ) -> std::io::Result<bool> {
        let path = match scope {
            McpScopeType::User => self.user_config_path.clone(),
            McpScopeType::Project => Self::project_config_path(working_dir),
        };
        let mut cfg = self.load_config(&path);
        let found = match cfg.mcp_servers.get_mut(name) {
            Some(s) => {
                s.use_tools = tool_names.clone();
                true
            }
            None => false,
        };
        if found {
            self.save_config(&path, &cfg)?;
        }
        Ok(found)
    }

    /// Update the `enabled` flag for a server and persist.
    pub fn update_enabled(
        &self,
        working_dir: &Path,
        scope: McpScopeType,
        name: &str,
        enabled: bool,
    ) -> std::io::Result<bool> {
        let path = match scope {
            McpScopeType::User => self.user_config_path.clone(),
            McpScopeType::Project => Self::project_config_path(working_dir),
        };
        let mut cfg = self.load_config(&path);
        let found = match cfg.mcp_servers.get_mut(name) {
            Some(s) => {
                s.enabled = enabled;
                true
            }
            None => false,
        };
        if found {
            self.save_config(&path, &cfg)?;
        }
        Ok(found)
    }

    /// Remove the project config file entirely.  Useful when cleaning up a
    /// working directory.
    pub fn delete_project_config(working_dir: &Path) -> std::io::Result<bool> {
        let path = Self::project_config_path(working_dir);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_empty_name() {
        let cfg = ExternalMcpServerConfig {
            name: "  ".into(),
            transport: McpTransportType::Stdio,
            description: None,
            enabled: true,
            use_tools: None,
            command: Some("node".into()),
            args: vec![],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_stdio_requires_command() {
        let cfg = ExternalMcpServerConfig {
            name: "test".into(),
            transport: McpTransportType::Stdio,
            description: None,
            enabled: true,
            use_tools: None,
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_stdio_accepts_command() {
        let cfg = ExternalMcpServerConfig {
            name: "test".into(),
            transport: McpTransportType::Stdio,
            description: None,
            enabled: true,
            use_tools: None,
            command: Some("node".into()),
            args: vec![],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_http_requires_url() {
        let cfg = ExternalMcpServerConfig {
            name: "test".into(),
            transport: McpTransportType::Http,
            description: None,
            enabled: true,
            use_tools: None,
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_sse_accepts_url() {
        let cfg = ExternalMcpServerConfig {
            name: "test".into(),
            transport: McpTransportType::Sse,
            description: None,
            enabled: true,
            use_tools: None,
            command: None,
            args: vec![],
            env: HashMap::new(),
            url: Some("http://localhost:8080/sse".into()),
            headers: HashMap::new(),
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn roundtrip_json() {
        let mut env = HashMap::new();
        env.insert("KEY".into(), "value".into());

        let cfg = ExternalMcpServerConfig {
            name: "my-server".into(),
            transport: McpTransportType::Stdio,
            description: Some("A test server".into()),
            enabled: true,
            use_tools: Some(vec!["tool1".into(), "tool2".into()]),
            command: Some("node".into()),
            args: vec!["server.js".into()],
            env,
            url: None,
            headers: HashMap::new(),
        };

        let file = McpConfigFile {
            mcp_servers: {
                let mut m = HashMap::new();
                m.insert("my-server".into(), cfg.clone());
                m
            },
        };

        let json = serde_json::to_string_pretty(&file).unwrap();
        let parsed: McpConfigFile = serde_json::from_str(&json).unwrap();
        let roundtripped = parsed.mcp_servers.get("my-server").unwrap();

        assert_eq!(roundtripped.name, cfg.name);
        assert_eq!(roundtripped.transport, cfg.transport);
        assert_eq!(roundtripped.command, cfg.command);
        assert_eq!(
            roundtripped.use_tools.as_ref().unwrap(),
            &vec!["tool1", "tool2"]
        );
    }

    #[test]
    fn merge_project_overrides_user() {
        let dir = tempfile::TempDir::new().unwrap();

        let user_mgr = McpConfigManager::new(dir.path().join("mcp.json"));

        // Write user config
        let mut user_cfg = McpConfigFile::default();
        user_cfg.mcp_servers.insert(
            "shared".into(),
            ExternalMcpServerConfig {
                name: "shared".into(),
                transport: McpTransportType::Stdio,
                description: Some("user version".into()),
                enabled: true,
                use_tools: None,
                command: Some("user_cmd".into()),
                args: vec![],
                env: HashMap::new(),
                url: None,
                headers: HashMap::new(),
            },
        );
        user_mgr.save_user_config(&user_cfg).unwrap();

        // Write project config in a subdir simulating a working directory
        let work_dir = dir.path().join("project");
        let mut proj_cfg = McpConfigFile::default();
        proj_cfg.mcp_servers.insert(
            "shared".into(),
            ExternalMcpServerConfig {
                name: "shared".into(),
                transport: McpTransportType::Stdio,
                description: Some("project version".into()),
                enabled: true,
                use_tools: None,
                command: Some("proj_cmd".into()),
                args: vec![],
                env: HashMap::new(),
                url: None,
                headers: HashMap::new(),
            },
        );
        user_mgr.save_project_config(&work_dir, &proj_cfg).unwrap();

        // Project should win
        let merged = user_mgr.load_merged(&work_dir);
        let shared = merged.mcp_servers.get("shared").unwrap();
        assert_eq!(shared.description.as_deref(), Some("project version"));
        assert_eq!(shared.command.as_deref(), Some("proj_cmd"));
    }
}
