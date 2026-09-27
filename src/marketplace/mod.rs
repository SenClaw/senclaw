//! Marketplace plugin management. Mirrors `src-old/marketplace/*.ts`.
//!
//! Manages git/local/hub sources for plugins (skills, subagents, hooks, MCP
//! servers) with enable/disable state and priority-based loading. A *hub* is a
//! remote catalog (`marketplace.json`) browsed and installed per plugin — see
//! [`hub`].

pub mod app_update;
pub mod git_sync;
pub mod hub;
pub mod manager;
pub mod mcp_sync;
pub mod publish;
pub mod registry;
pub mod types;

pub use hub::DEFAULT_HUB_URL;

use types::SourceType;

/// What kind of source a bare URL/path describes: a `marketplace.json` catalog
/// (or bare host) is a hub, anything git-ish is a git clone, and a filesystem
/// path is local. Passing `explicit` short-circuits the guess.
///
/// Shared by the REST `add source` handler and the `/plugin marketplace add`
/// chat command so both classify URLs identically.
pub fn infer_source_type(url: Option<&str>, explicit: Option<SourceType>) -> SourceType {
    if let Some(t) = explicit {
        return t;
    }
    let Some(url) = url.map(str::trim).filter(|u| !u.is_empty()) else {
        return SourceType::Local;
    };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        // git@host:owner/repo, file paths, everything else
        return if url.starts_with("git@") {
            SourceType::Git
        } else {
            SourceType::Local
        };
    }
    if url.ends_with(".json") {
        SourceType::Hub
    } else if url.ends_with(".git") || url.trim_end_matches('/').matches('/').count() > 2 {
        // https://host/owner/repo → git; https://host → hub
        SourceType::Git
    } else {
        SourceType::Hub
    }
}

/// A readable default name for a source when the caller only supplied a URL.
/// An explicit non-empty `name` wins; otherwise derive one from the URL/path.
pub fn default_source_name(
    name: Option<&str>,
    url: Option<&str>,
    local_path: Option<&str>,
    source_type: SourceType,
) -> String {
    if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    let raw = url.or(local_path).unwrap_or("").trim();
    let label = match source_type {
        SourceType::Hub => hub::catalog_home(raw),
        _ => raw.trim_end_matches('/').to_string(),
    };
    let label = label
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches(".git")
        .to_string();
    if label.is_empty() {
        "Untitled source".to_string()
    } else {
        label
    }
}
