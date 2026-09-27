//! Parse PLUGIN.md frontmatter into `PluginManifest`.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginType {
    ChannelAdapter,
    McpServer,
    HttpRoute,
    Cron,
}

impl PluginType {
    pub fn as_str(&self) -> &'static str {
        match self {
            PluginType::ChannelAdapter => "channel_adapter",
            PluginType::McpServer => "mcp_server",
            PluginType::HttpRoute => "http_route",
            PluginType::Cron => "cron",
        }
    }
}

impl Default for PluginType {
    fn default() -> Self {
        PluginType::McpServer
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(rename = "display_name")]
    pub display_name: Option<String>,
    pub version: String,
    pub description: Option<String>,
    #[serde(default)]
    pub plugin_type: PluginType,
    /// Relative path to binary inside plugin dir (optional)
    pub entry_point: Option<String>,
    /// Required env var names (user must supply values)
    #[serde(default)]
    pub env_vars: Vec<String>,
    /// HTTP routes the plugin exposes (for http_route type)
    #[serde(default)]
    pub routes: Vec<String>,
    /// Declared permission scopes
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub min_senclaw: Option<String>,
}

/// Parse PLUGIN.md — extract YAML frontmatter between `---` delimiters.
/// Uses a minimal line-by-line parser to avoid adding serde_yaml dependency.
pub fn parse_plugin_md(path: &Path) -> Option<PluginManifest> {
    let content = fs::read_to_string(path).ok()?;
    let body = content.trim_start();
    if !body.starts_with("---") {
        return None;
    }
    let rest = &body[3..];
    let end = rest.find("\n---")?;
    let yaml = &rest[..end];
    parse_yaml_manifest(yaml)
}

fn parse_yaml_manifest(yaml: &str) -> Option<PluginManifest> {
    use std::collections::HashMap;

    // Build a flat key→value map from simple `key: value` lines.
    // Multi-line lists (`- item`) are collected under the last key.
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let mut current_key: Option<String> = None;

    for raw in yaml.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line.starts_with("- ") {
            // List item under current_key
            if let Some(ref k) = current_key {
                map.entry(k.clone())
                    .or_default()
                    .push(line[2..].trim().to_string());
            }
        } else if let Some(colon) = line.find(':') {
            let key = line[..colon].trim().to_string();
            let val = line[colon + 1..]
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            current_key = Some(key.clone());
            if !val.is_empty() {
                map.entry(key).or_default().push(val);
            }
        }
    }

    let scalar = |k: &str| map.get(k).and_then(|v| v.first()).cloned();
    let list = |k: &str| map.get(k).cloned().unwrap_or_default();

    Some(PluginManifest {
        name: scalar("name")?,
        display_name: scalar("display_name"),
        version: scalar("version").unwrap_or_else(|| "0.0.0".into()),
        description: scalar("description"),
        plugin_type: match scalar("plugin_type").as_deref() {
            Some("channel_adapter") => PluginType::ChannelAdapter,
            Some("http_route") => PluginType::HttpRoute,
            Some("cron") => PluginType::Cron,
            _ => PluginType::McpServer,
        },
        entry_point: scalar("entry_point"),
        env_vars: list("env_vars"),
        routes: list("routes"),
        permissions: list("permissions"),
        tags: list("tags"),
        min_senclaw: scalar("min_senclaw"),
    })
}
