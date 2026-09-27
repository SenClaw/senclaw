//! ClawHub local token management. Mirrors `src-old/clawhub/auth.ts`.
//!
//! Token storage (compatible with clawhub web convention):
//!   macOS:  ~/Library/Application Support/clawhub/config.json
//!   Linux:  ~/.config/clawhub/config.json

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default)]
struct ClawhubLocalConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
}

pub fn get_config_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    if cfg!(target_os = "macos") {
        home.join("Library")
            .join("Application Support")
            .join("clawhub")
            .join("config.json")
    } else {
        home.join(".config").join("clawhub").join("config.json")
    }
}

pub fn read_stored_token() -> Option<String> {
    let content = fs::read_to_string(get_config_path()).ok()?;
    let cfg: ClawhubLocalConfig = serde_json::from_str(&content).ok()?;
    cfg.token.filter(|t| !t.trim().is_empty())
}

pub fn write_stored_token(token: &str) -> Result<(), anyhow::Error> {
    let config_path = get_config_path();
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut existing: ClawhubLocalConfig = fs::read_to_string(&config_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    existing.token = Some(token.to_string());
    let json = serde_json::to_string_pretty(&existing)?;
    fs::write(&config_path, json + "\n")?;
    Ok(())
}

pub fn clear_stored_token() -> Result<(), anyhow::Error> {
    let config_path = get_config_path();
    if let Ok(content) = fs::read_to_string(&config_path) {
        let mut existing: ClawhubLocalConfig = serde_json::from_str(&content)?;
        existing.token = None;
        let json = serde_json::to_string_pretty(&existing)?;
        fs::write(&config_path, json + "\n")?;
    }
    Ok(())
}
