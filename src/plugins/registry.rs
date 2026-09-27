//! Scan managed plugins directory and return installed plugin entries.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::manifest::{parse_plugin_md, PluginManifest};
use crate::config::Config;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntry {
    pub slug: String,
    pub display_name: String,
    pub description: Option<String>,
    pub version: String,
    pub plugin_type: String,
    pub dir: PathBuf,
    pub has_binary: bool,
    pub env_vars: Vec<String>,
    pub tags: Vec<String>,
}

pub fn scan_installed_plugins(config: &Config) -> Vec<PluginEntry> {
    let base = &config.paths.managed_plugins_dir;
    let Ok(entries) = std::fs::read_dir(base) else {
        return vec![];
    };

    let mut plugins: Vec<PluginEntry> = entries
        .flatten()
        .filter_map(|e| {
            let dir = e.path();
            if !dir.is_dir() {
                return None;
            }
            let slug = dir.file_name()?.to_string_lossy().into_owned();
            if slug.starts_with('.') {
                return None;
            }
            let plugin_md = dir.join("PLUGIN.md");
            let manifest: Option<PluginManifest> = if plugin_md.exists() {
                parse_plugin_md(&plugin_md)
            } else {
                None
            };
            let has_binary = manifest
                .as_ref()
                .and_then(|m| m.entry_point.as_deref())
                .map(|ep| dir.join(ep).exists())
                .unwrap_or(false);

            Some(PluginEntry {
                slug: slug.clone(),
                display_name: manifest
                    .as_ref()
                    .and_then(|m| m.display_name.clone())
                    .unwrap_or_else(|| slug.clone()),
                description: manifest.as_ref().and_then(|m| m.description.clone()),
                version: manifest
                    .as_ref()
                    .map(|m| m.version.clone())
                    .unwrap_or_else(|| "0.0.0".to_string()),
                plugin_type: manifest
                    .as_ref()
                    .map(|m| m.plugin_type.as_str().to_string())
                    .unwrap_or_else(|| "mcp_server".to_string()),
                dir,
                has_binary,
                env_vars: manifest
                    .as_ref()
                    .map(|m| m.env_vars.clone())
                    .unwrap_or_default(),
                tags: manifest
                    .as_ref()
                    .map(|m| m.tags.clone())
                    .unwrap_or_default(),
            })
        })
        .collect();

    plugins.sort_by(|a, b| a.slug.cmp(&b.slug));
    plugins
}
