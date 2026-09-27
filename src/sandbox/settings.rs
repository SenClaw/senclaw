//! App-level settings: the defaults a new sandbox inherits.
//!
//! Stored in the DB rather than in env vars because the user changes them from
//! the UI, and a setting that needs an app restart to take effect is a setting
//! people stop trusting.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::sandbox::db::Db;
use crate::sandbox::fsmode::FsMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Read-isolation a new sandbox starts with.
    pub default_fs_mode: FsMode,
    /// Directories readable in `allowlist` mode. Absolute paths.
    pub allowlist: Vec<String>,
    pub default_network: bool,
    pub default_memory_mb: i64,
    pub default_cpus: f64,
    pub default_timeout_ms: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            // "Chỉ map" — a new sandbox sees only what it is given. Changing
            // this default is the single biggest lever in the app, which is why
            // it is a setting rather than a constant.
            default_fs_mode: FsMode::Strict,
            allowlist: Vec::new(),
            default_network: false,
            default_memory_mb: 512,
            default_cpus: 1.0,
            default_timeout_ms: 30_000,
        }
    }
}

const KEY: &str = "app";

pub fn load(db: &Db) -> Settings {
    // A settings row that fails to parse must not take the app down — the
    // defaults are always a valid answer, and the UI can rewrite the row.
    db.setting(KEY)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Settings>(&s).ok())
        .unwrap_or_default()
}

pub fn save(db: &Db, s: &Settings) -> Result<Settings> {
    let mut s = s.clone();
    s.allowlist = normalise_allowlist(&s.allowlist);
    s.default_memory_mb = s.default_memory_mb.clamp(64, 65_536);
    s.default_cpus = s.default_cpus.clamp(0.1, 32.0);
    s.default_timeout_ms = s.default_timeout_ms.clamp(1_000, 10 * 60 * 1000);
    db.set_setting(KEY, &serde_json::to_string(&s).unwrap_or_else(|_| json!({}).to_string()))?;
    Ok(s)
}

/// Trim, drop blanks and duplicates, and keep only absolute paths.
///
/// A relative entry here would be resolved against whatever directory the app
/// happened to start in — silently allowing a different folder than the user
/// typed. Refusing them at the door is clearer than guessing.
pub fn normalise_allowlist(paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in paths {
        let p = p.trim().trim_end_matches('/');
        if p.is_empty() || !p.starts_with('/') {
            continue;
        }
        let s = p.to_string();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_default_is_map_only() {
        let s = Settings::default();
        assert_eq!(s.default_fs_mode, FsMode::Strict);
        assert!(s.allowlist.is_empty());
        assert!(!s.default_network, "a new sandbox must start offline");
    }

    #[test]
    fn allowlist_keeps_only_absolute_deduped_paths() {
        let got = normalise_allowlist(&[
            "/Users/u/a".into(),
            "  /Users/u/b/  ".into(),
            "relative/path".into(),
            "".into(),
            "/Users/u/a".into(),
        ]);
        assert_eq!(got, vec!["/Users/u/a", "/Users/u/b"]);
    }

    #[test]
    fn save_round_trips_and_clamps() {
        let db = Db::open_memory().unwrap();
        let saved = save(
            &db,
            &Settings {
                default_fs_mode: FsMode::Allowlist,
                allowlist: vec!["/tmp/x".into(), "nope".into()],
                default_network: true,
                default_memory_mb: 999_999,
                default_cpus: 0.0,
                default_timeout_ms: 1,
            },
        )
        .unwrap();
        assert_eq!(saved.default_memory_mb, 65_536);
        assert_eq!(saved.default_cpus, 0.1);
        assert_eq!(saved.default_timeout_ms, 1_000);
        assert_eq!(saved.allowlist, vec!["/tmp/x"]);

        let back = load(&db);
        assert_eq!(back.default_fs_mode, FsMode::Allowlist);
        assert!(back.default_network);
        assert_eq!(back.allowlist, vec!["/tmp/x"]);
    }

    #[test]
    fn a_corrupt_row_falls_back_to_defaults_instead_of_failing() {
        let db = Db::open_memory().unwrap();
        db.set_setting(KEY, "{not json").unwrap();
        assert_eq!(load(&db).default_fs_mode, FsMode::Strict);
    }

    #[test]
    fn an_empty_db_reads_as_defaults() {
        let db = Db::open_memory().unwrap();
        assert_eq!(load(&db).default_fs_mode, FsMode::Strict);
    }
}
