//! Claude Code worker (`claudeCode`) and sen-link (`senLink`) blocks. The typed
//! views and their defaults live in `crate::claude_code::settings` and
//! `crate::sen_link::settings`; this module only writes the blocks back,
//! leaving every other section of `config.json` as it was.

use std::path::Path;

use anyhow::Result;

use super::config::{load_global_config, save_global_config};

pub fn save_claude_code_settings(
    config_path: &Path,
    settings: &crate::claude_code::settings::ClaudeCodeSettings,
) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.claude_code = Some(serde_json::to_value(settings)?);
    save_global_config(config_path, &cfg)
}

pub fn save_sen_link_settings(
    config_path: &Path,
    settings: &crate::sen_link::settings::SenLinkSettings,
) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.sen_link = Some(serde_json::to_value(settings)?);
    save_global_config(config_path, &cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_save_of_another_section_keeps_both_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        let mut cc = crate::claude_code::settings::ClaudeCodeSettings::default();
        cc.verify_command = Some("cargo test".into());
        save_claude_code_settings(&path, &cc).unwrap();
        let mut sl = crate::sen_link::settings::SenLinkSettings::default();
        sl.enabled = true;
        save_sen_link_settings(&path, &sl).unwrap();

        super::super::save_control_plane_settings(&path, &Default::default()).unwrap();
        assert_eq!(
            crate::claude_code::settings::load(&path).verify_command.as_deref(),
            Some("cargo test")
        );
        assert!(crate::sen_link::settings::load(&path).enabled);
    }
}
