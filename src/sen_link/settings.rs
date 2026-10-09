//! `config.json` → `senLink`. Off by default: the A2A endpoints answer 404
//! until a person turns sen-link on.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SenLinkSettings {
    pub enabled: bool,
    /// Name this daemon shows peers (Agent Card `name`).
    pub name: String,
    pub description: String,
    /// How peers reach this daemon, e.g. `http://100.64.0.2:18788` (Tailscale)
    /// — the daemon must be bound beyond loopback for that to work. `None`
    /// means peers can receive tasks from us but cannot send us any.
    pub public_url: Option<String>,
    /// Kanban board incoming tasks land on (created on first use).
    pub board_title: String,
    /// Where dispatched workers for incoming tasks run (the board's workspace).
    pub workspace_dir: Option<String>,
    /// Assignee for incoming tasks when the peer has none of its own.
    pub default_assignee: Option<String>,
    /// SenClaw hub (`senclaw-hub-connector`) for peers that cannot reach each
    /// other directly (NAT). When set, every invite mints its own hub channel.
    pub hub_url: Option<String>,
}

impl Default for SenLinkSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            name: "SenClaw".into(),
            description: "A SenClaw personal agent".into(),
            public_url: None,
            board_title: "sen-link".into(),
            workspace_dir: None,
            default_assignee: None,
            hub_url: None,
        }
    }
}

impl SenLinkSettings {
    pub fn validated(self) -> Result<Self, String> {
        if self.name.trim().is_empty() {
            return Err("name: must not be empty".into());
        }
        if self.board_title.trim().is_empty() {
            return Err("boardTitle: must not be empty".into());
        }
        for (field, u) in [("publicUrl", &self.public_url), ("hubUrl", &self.hub_url)] {
            if let Some(u) = u {
                if !(u.starts_with("http://") || u.starts_with("https://")) {
                    return Err(format!("{field}: must start with http:// or https://"));
                }
            }
        }
        Ok(self)
    }
}

pub fn load(config_path: &Path) -> SenLinkSettings {
    std::fs::read_to_string(config_path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("senLink").cloned())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_by_default_and_urls_are_checked() {
        assert!(!SenLinkSettings::default().enabled);
        let mut s = SenLinkSettings::default();
        s.public_url = Some("100.64.0.2:18788".into());
        assert!(s.validated().is_err());
    }
}
