//! Browser engine settings (`browserAgent` block). The typed view and its
//! defaults are `crate::browser_agent::settings`; this module only writes the
//! block back, leaving every other section of `config.json` as it was.

use std::path::Path;

use anyhow::Result;

use super::config::{load_global_config, save_global_config};
use crate::browser_agent::settings::BrowserSettings;

pub fn save_browser_agent_settings(config_path: &Path, settings: &BrowserSettings) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    cfg.browser_agent = Some(serde_json::to_value(settings)?);
    save_global_config(config_path, &cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser_agent::settings::{load, Driver};

    #[test]
    fn a_save_of_another_section_keeps_the_browser_block() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        let mut s = BrowserSettings::default();
        s.default_driver = Driver::Extension;
        s.hosted_domains = vec!["example.com".into()];
        save_browser_agent_settings(&path, &s).unwrap();

        super::super::save_control_plane_settings(&path, &Default::default()).unwrap();
        let back = load(&path);
        assert_eq!(back.default_driver, Driver::Extension);
        assert_eq!(back.hosted_domains, vec!["example.com".to_string()]);
    }
}
