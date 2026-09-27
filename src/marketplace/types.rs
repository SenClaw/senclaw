//! Marketplace type definitions. Mirrors `src-old/marketplace/types.ts`.

use serde::{Deserialize, Serialize};

/// A marketplace source (hub catalog, git repository, or local directory)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceSource {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub source_type: SourceType,
    /// Git remote for `git` sources; catalog URL for `hub` sources.
    pub url: Option<String>,
    pub branch: Option<String>,
    #[serde(rename = "localPath")]
    pub local_path: String,
    pub priority: i32,
    pub enabled: bool,
    #[serde(rename = "lastSynced")]
    pub last_synced: Option<String>,
    #[serde(rename = "syncError")]
    pub sync_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceType {
    Git,
    Local,
    /// A remote `marketplace.json` catalog; plugins are installed one by one.
    Hub,
}

/// Plugin-level toggle state: plugins[name] = true → enabled; absent = disabled (default-off)
/// mcpUseTools key: `${pluginName}/${serverName}` → string[] to allowlist tools, null to clear override
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MarketplaceSourceItemState {
    pub plugins: std::collections::HashMap<String, bool>,
    #[serde(rename = "mcpUseTools")]
    pub mcp_use_tools: Option<std::collections::HashMap<String, Option<Vec<String>>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceConfig {
    pub sources: Vec<MarketplaceSource>,
}

impl Default for MarketplaceConfig {
    fn default() -> Self {
        Self {
            sources: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MarketplaceStateFile {
    #[serde(flatten)]
    pub sources: std::collections::HashMap<String, MarketplaceSourceItemState>,
}

// ===== API response types =====

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplacePluginSkill {
    pub name: String,
    pub description: String,
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplacePluginSubagent {
    pub name: String,
    pub description: String,
    pub disabled: bool,
}

/// One MCP server a plugin declares in its `.mcp.json`.
///
/// Carries everything needed to actually launch or dial it — the earlier
/// shape named a server it had no way to start, which is why plugin tools
/// never reached a model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MarketplacePluginMCPServer {
    pub name: String,
    pub transport: String,
    pub description: Option<String>,
    #[serde(rename = "useTools")]
    pub use_tools: Option<Vec<String>>,
    // stdio
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub env: std::collections::HashMap<String, String>,
    // sse / http
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub headers: std::collections::HashMap<String, String>,
    /// Directory the plugin was discovered in — the server's working directory
    /// and the value `${CLAUDE_PLUGIN_ROOT}` expands to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_root: Option<String>,
    /// Set when the install scan found the command dangerous beyond the
    /// baseline "this plugin runs code" finding. Such a server is listed but
    /// never auto-registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplacePlugin {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub author: Option<String>,
    pub keywords: Option<Vec<String>>,
    pub dir: String,
    #[serde(rename = "sourceId")]
    pub source_id: String,
    #[serde(rename = "sourceName")]
    pub source_name: String,
    pub priority: i32,
    pub enabled: bool,
    /// Hub plugins start out listed-but-absent; git/local plugins are always
    /// installed by virtue of being on disk.
    pub installed: bool,
    /// `plugin` (git clone) or `app`/`skill`/`workflow` (registry artifact).
    /// Absent means plugin. The two install routes are different endpoints, so
    /// a client that ignores this will try to git-clone an app and fail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `scope/name`, present only for registry entries — the coordinate
    /// `POST /api/marketplace/hub/install` takes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// 30-day downloads, when the hub reports them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downloads: Option<u64>,
    /// For a registry `app` already present as a Space App: the version on
    /// disk. `None` while the app was installed before origins were stamped —
    /// which is why `update_available` is a separate field and not something a
    /// client can derive by comparing this to `version`.
    #[serde(rename = "installedVersion", skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    /// The catalog's version is worth offering over what is installed. Computed
    /// here so both UIs agree, and so an unstamped install still offers the
    /// update rather than looking current.
    #[serde(rename = "updateAvailable", default)]
    pub update_available: bool,
    /// Catalog metadata, only ever set for hub sources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(rename = "skillCount")]
    pub skill_count: usize,
    #[serde(rename = "subagentCount")]
    pub subagent_count: usize,
    #[serde(rename = "hasHooks")]
    pub has_hooks: bool,
    #[serde(rename = "mcpServerCount")]
    pub mcp_server_count: usize,
    pub skills: Vec<MarketplacePluginSkill>,
    pub subagents: Vec<MarketplacePluginSubagent>,
    #[serde(rename = "mcpServers")]
    pub mcp_servers: Vec<MarketplacePluginMCPServer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceSourceInfo {
    #[serde(flatten)]
    pub source: MarketplaceSource,
    pub plugins: Vec<MarketplacePlugin>,
}
