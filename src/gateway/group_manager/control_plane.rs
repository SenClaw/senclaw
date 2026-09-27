//! Control-plane settings (`controlPlane` block). Fully daemon-owned — see
//! `crate::control_plane::ControlPlaneSettings` and, for contrast, the
//! `decisionConfig` raw-passthrough pattern in `llm.rs` (needed only because
//! that key is shared with the `sen-sysone` runtime; this one is not).

use std::path::Path;

use anyhow::Result;

use super::config::{load_global_config, save_global_config};
use crate::control_plane::ControlPlaneSettings;

pub fn load_control_plane_settings(config_path: &Path) -> ControlPlaneSettings {
    load_global_config(config_path).control_plane.unwrap_or_default()
}

pub fn save_control_plane_settings(config_path: &Path, settings: &ControlPlaneSettings) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.control_plane = Some(settings.clone());
    save_global_config(config_path, &cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_block_loads_as_default_and_a_save_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        assert_eq!(load_control_plane_settings(&path), ControlPlaneSettings::default());

        let mut s = ControlPlaneSettings::default();
        s.shadow = true;
        s.jev_off = true;
        save_control_plane_settings(&path, &s).unwrap();
        assert_eq!(load_control_plane_settings(&path), s);
    }

    #[test]
    fn saving_control_plane_settings_leaves_other_config_sections_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, r#"{"decisionConfig": {"backend": "online"}}"#).unwrap();
        save_control_plane_settings(&path, &ControlPlaneSettings::default()).unwrap();
        let raw: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["decisionConfig"]["backend"], "online");
        assert_eq!(raw["controlPlane"]["shadow"], false);
    }
}
