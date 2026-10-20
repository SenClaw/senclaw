//! `config.json` → `senLink`. Off by default: the LAN listener is not even
//! opened until a person turns SLink on.

use std::net::IpAddr;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Board incoming tasks land on, as the Kanban UI shows it.
pub const DEFAULT_BOARD_TITLE: &str = "SLink";
/// What the board was called before the display name changed. A stored value
/// of this is read as [`DEFAULT_BOARD_TITLE`], and a board still carrying it
/// is renamed in place (see `tasks::ensure_board`).
pub const LEGACY_BOARD_TITLE: &str = "sen-link";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SenLinkSettings {
    pub enabled: bool,
    /// Name this daemon shows peers (Agent Card `name`).
    pub name: String,
    pub description: String,
    /// Address the LAN listener binds. `0.0.0.0` is every interface; inbound
    /// connections from outside the LAN are refused either way.
    pub listen_host: String,
    /// Port of the LAN listener (the A2A surface and nothing else).
    pub listen_port: u16,
    /// Answer `GET /.well-known/agent-card.json`. Off hides this daemon from
    /// probes and scans; a peer can still pair by typing the address.
    pub discoverable: bool,
    /// Kanban board incoming tasks land on (created on first use).
    pub board_title: String,
    /// Where dispatched workers for incoming tasks run (the board's workspace).
    pub workspace_dir: Option<String>,
    /// Assignee for incoming tasks when the peer has none of its own.
    pub default_assignee: Option<String>,
}

impl Default for SenLinkSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            name: "SenClaw".into(),
            description: "A SenClaw personal agent".into(),
            listen_host: "0.0.0.0".into(),
            listen_port: super::lan::DEFAULT_PORT,
            discoverable: true,
            board_title: DEFAULT_BOARD_TITLE.into(),
            workspace_dir: None,
            default_assignee: None,
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
        if self.listen_host.trim().parse::<IpAddr>().is_err() {
            return Err("listenHost: must be an IP address (0.0.0.0 for every interface)".into());
        }
        if self.listen_port == 0 {
            return Err("listenPort: must be 1-65535".into());
        }
        Ok(self)
    }

    /// The board title in effect: an empty or legacy stored value means the
    /// current default name.
    pub fn effective_board_title(&self) -> &str {
        match self.board_title.trim() {
            "" | LEGACY_BOARD_TITLE => DEFAULT_BOARD_TITLE,
            t => t,
        }
    }
}

/// Settings from `config.json`. Keys an older build wrote and this one no
/// longer has (`publicUrl`, `hubUrl`) are ignored, and dropped on the next save.
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
    fn off_by_default_and_listener_fields_are_checked() {
        let d = SenLinkSettings::default();
        assert!(!d.enabled && d.discoverable);
        assert_eq!((d.listen_host.as_str(), d.listen_port), ("0.0.0.0", 18791));
        assert!(d.clone().validated().is_ok());

        let mut s = d.clone();
        s.listen_host = "mac-mini.local".into();
        assert!(s.validated().unwrap_err().contains("listenHost"));
        let mut s = d.clone();
        s.listen_port = 0;
        assert!(s.validated().unwrap_err().contains("listenPort"));
        let mut s = d;
        s.listen_host = "::".into();
        assert!(s.validated().is_ok());
    }

    #[test]
    fn keys_from_the_hub_era_are_ignored_not_fatal() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"senLink": {"enabled": true, "name": "Mac", "publicUrl": "http://100.64.0.2:18788", "hubUrl": "https://hub", "boardTitle": "sen-link"}}"#,
        )
        .unwrap();
        let s = load(&path);
        assert!(s.enabled);
        assert_eq!(s.name, "Mac");
        assert_eq!(s.listen_port, 18791, "new keys take their defaults");
        assert_eq!(s.board_title, "sen-link");
        assert_eq!(s.effective_board_title(), "SLink", "the old default name is read as the new one");
    }

    #[test]
    fn only_the_legacy_default_board_name_is_rewritten() {
        let mut s = SenLinkSettings::default();
        assert_eq!(s.effective_board_title(), "SLink");
        s.board_title = "  ".into();
        assert_eq!(s.effective_board_title(), "SLink");
        s.board_title = "Việc từ máy kia".into();
        assert_eq!(s.effective_board_title(), "Việc từ máy kia");
    }
}
