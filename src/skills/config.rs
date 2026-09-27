//! Per-skill runtime configuration (env injection), OpenClaw-compatible.
//!
//! Mirrors OpenClaw's `skills.entries.<name>` block in `openclaw.json`. In
//! SenClaw this lives under the `skills` key of the global `config.json`
//! (path = [`crate::config::PathsConfig::global_config_path`]):
//!
//! ```json
//! {
//!   "skills": {
//!     "entries": {
//!       "image-lab": {
//!         "enabled": true,
//!         "apiKey": "sk-...",
//!         "env": { "GEMINI_API_KEY": "..." }
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! `env` / `apiKey` are injected into the host process for the skill turn
//! when the [`crate::tools::skill::SkillTool`] activates a skill — and only
//! when the var is not already set, matching OpenClaw semantics. The
//! credential named by the skill's `primaryEnv` (declared in frontmatter) is
//! filled from `apiKey` if present.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SkillEntryConfig {
    /// When `Some(false)`, the skill is disabled via config (in addition to
    /// the `disabled-skills.json` mechanism).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Plaintext credential, injected into the skill's `primaryEnv`.
    #[serde(default, rename = "apiKey", alias = "api_key")]
    pub api_key: Option<String>,
    /// Environment variables injected for the agent run (only if not set).
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SkillsRuntimeConfig {
    #[serde(default)]
    pub entries: HashMap<String, SkillEntryConfig>,
}

#[derive(Debug, Deserialize)]
struct GlobalConfigShape {
    #[serde(default)]
    skills: SkillsRuntimeConfig,
}

impl SkillsRuntimeConfig {
    /// Load the `skills` section from the global `config.json`. Returns an
    /// empty config when the file is missing or malformed (best-effort).
    pub fn load(global_config_path: &Path) -> Self {
        let raw = match std::fs::read_to_string(global_config_path) {
            Ok(s) => s,
            Err(_) => return Self::default(),
        };
        serde_json::from_str::<GlobalConfigShape>(&raw)
            .map(|g| g.skills)
            .unwrap_or_default()
    }

    pub fn entry(&self, skill_name: &str) -> Option<&SkillEntryConfig> {
        self.entries.get(skill_name)
    }
}

/// Inject a skill's configured env into the current process, *only* for vars
/// that are not already set (OpenClaw "only if not already set" semantics).
/// `primary_env` (from the skill's frontmatter) receives `apiKey` when given.
///
/// Returns the list of variable names that were actually set, for logging.
pub fn inject_env(entry: &SkillEntryConfig, primary_env: Option<&str>) -> Vec<String> {
    let mut injected = Vec::new();

    for (k, v) in &entry.env {
        if std::env::var_os(k).is_none() {
            std::env::set_var(k, v);
            injected.push(k.clone());
        }
    }

    if let (Some(key), Some(name)) = (entry.api_key.as_deref(), primary_env) {
        if !key.is_empty() && std::env::var_os(name).is_none() {
            std::env::set_var(name, key);
            injected.push(name.to_string());
        }
    }

    injected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_entries_from_json() {
        let dir = std::env::temp_dir().join(format!("skills-cfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{ "skills": { "entries": { "image-lab": { "enabled": true, "apiKey": "sk-1", "env": { "FOO": "bar" } } } } }"#,
        )
        .unwrap();

        let cfg = SkillsRuntimeConfig::load(&path);
        let entry = cfg.entry("image-lab").expect("entry present");
        assert_eq!(entry.enabled, Some(true));
        assert_eq!(entry.api_key.as_deref(), Some("sk-1"));
        assert_eq!(entry.env.get("FOO").map(String::as_str), Some("bar"));
        assert!(cfg.entry("missing").is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_empty() {
        let cfg = SkillsRuntimeConfig::load(Path::new("/no/such/config.json"));
        assert!(cfg.entries.is_empty());
    }

    #[test]
    fn malformed_json_is_empty() {
        let dir = std::env::temp_dir().join(format!("skills-cfg-bad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, "{ not json").unwrap();
        let cfg = SkillsRuntimeConfig::load(&path);
        assert!(cfg.entries.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn inject_env_skips_already_set() {
        let entry = SkillEntryConfig {
            enabled: None,
            api_key: Some("secret".into()),
            env: {
                let mut m = HashMap::new();
                m.insert("SKILL_TEST_PREEXISTING".into(), "new".into());
                m
            },
        };
        std::env::set_var("SKILL_TEST_PREEXISTING", "old");
        let injected = inject_env(&entry, Some("SKILL_TEST_PRIMARY"));
        // Pre-existing var is untouched; primary credential gets set.
        assert!(!injected.contains(&"SKILL_TEST_PREEXISTING".to_string()));
        assert_eq!(std::env::var("SKILL_TEST_PREEXISTING").unwrap(), "old");
        assert_eq!(std::env::var("SKILL_TEST_PRIMARY").unwrap(), "secret");
        std::env::remove_var("SKILL_TEST_PREEXISTING");
        std::env::remove_var("SKILL_TEST_PRIMARY");
    }
}
