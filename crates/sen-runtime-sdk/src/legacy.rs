//! First-start import of settings the daemon used to own.
//!
//! Before the split, OCR, TTS, Whisper and the decision engine kept their
//! settings in the daemon's `config.json` (`ocrConfig`, `ttsConfig`,
//! `whisperConfig`, `decisionConfig`). A runtime owns its settings now, in
//! `<data_dir>/settings.json`; when that file does not exist yet it seeds it
//! from the old key so an upgraded machine keeps the user's choices. The daemon
//! stops writing those keys, and nothing ever writes back to `config.json`
//! from here — the file is read, never modified.

use std::path::Path;

/// The value under `key` in the daemon's `config.json`, if the file parses and
/// has it. Any failure is `None`: a missing or broken legacy file means
/// "start from defaults", never a runtime that refuses to start.
pub fn config_value(config_path: &Path, key: &str) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(config_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let found = value.get(key)?.clone();
    if found.is_null() {
        None
    } else {
        Some(found)
    }
}

/// Load `<data_dir>/settings.json`, seeding it from `config.json[key]` the
/// first time. Returns the raw JSON so each runtime keeps its own typed,
/// lenient parse (the in-daemon parsers already tolerate old shapes).
pub fn load_or_import(data_dir: &Path, config_path: &Path, key: &str) -> Option<serde_json::Value> {
    let own = data_dir.join("settings.json");
    if let Ok(text) = std::fs::read_to_string(&own) {
        return serde_json::from_str(&text).ok();
    }
    let imported = config_value(config_path, key)?;
    if std::fs::create_dir_all(data_dir).is_ok() {
        if let Ok(text) = serde_json::to_string_pretty(&imported) {
            let tmp = data_dir.join("settings.json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &own);
            }
        }
    }
    tracing::info!("imported settings from {} [{key}]", config_path.display());
    Some(imported)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sen-runtime-sdk-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn imports_once_then_reads_its_own_file() {
        let dir = temp_dir("import");
        let config = dir.join("config.json");
        std::fs::write(&config, r#"{"ocrConfig": {"lang": "vi"}, "other": 1}"#).unwrap();
        let data = dir.join("data");

        let first = load_or_import(&data, &config, "ocrConfig").unwrap();
        assert_eq!(first["lang"], "vi");
        assert!(data.join("settings.json").is_file());

        // The legacy file changing afterwards must not leak in.
        std::fs::write(&config, r#"{"ocrConfig": {"lang": "en"}}"#).unwrap();
        let second = load_or_import(&data, &config, "ocrConfig").unwrap();
        assert_eq!(second["lang"], "vi");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_broken_legacy_config_is_none() {
        let dir = temp_dir("missing");
        assert!(load_or_import(&dir.join("d"), &dir.join("nope.json"), "ttsConfig").is_none());
        std::fs::write(dir.join("bad.json"), "{not json").unwrap();
        assert!(config_value(&dir.join("bad.json"), "ttsConfig").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
