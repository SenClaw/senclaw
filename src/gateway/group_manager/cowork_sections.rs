//! sen-link (`senLink`) block. The typed view and its defaults live in
//! `crate::sen_link::settings`; this module only writes the block back,
//! leaving every other section of `config.json` as it was.

use std::path::Path;

use anyhow::Result;

use super::config::{load_global_config, save_global_config};

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
    fn a_save_of_another_section_keeps_the_sen_link_block() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        let mut sl = crate::sen_link::settings::SenLinkSettings::default();
        sl.enabled = true;
        save_sen_link_settings(&path, &sl).unwrap();

        super::super::save_control_plane_settings(&path, &Default::default()).unwrap();
        assert!(crate::sen_link::settings::load(&path).enabled);
    }
}
