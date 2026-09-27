//! The local model library: what GGUF/MLX checkpoints are on disk, their
//! stable keys and capabilities, HuggingFace downloads, and the REST surface
//! at `/api/local-models/*` (`docs/runtime-protocol.md` §5.3/§6).
//!
//! No inference lives here — a model is loaded by handing its path to
//! whichever runtime fills its format's slot ([`crate::runtime`]).

pub mod download;
pub mod gguf;
pub mod hf_files;
pub mod keys;
pub mod rest;
pub mod scan;
pub mod settings;

pub use scan::{find_by_key, scan_all, LocalModel};

/// Prefix marking an LLM config id as a local model (`local:<key>`), the same
/// pattern `crate::apps::llm_provider::ID_PREFIX` uses for Space Apps.
pub const ID_PREFIX: &str = "local:";

pub fn config_id(key: &str) -> String {
    format!("{ID_PREFIX}{key}")
}

/// The model key inside a `local:<key>` config id, or `None` when `id` is not
/// one.
pub fn parse_config_id(id: &str) -> Option<&str> {
    id.strip_prefix(ID_PREFIX).filter(|k| !k.is_empty())
}

/// Model roots the daemon was configured with, keyed by the config file they
/// belong to. `load_llm_configs` receives only a config path (it has dozens of
/// callers) while `SENCLAW_LOCAL_MODELS_DIR` may point anywhere, so the daemon
/// records its real root at boot. Keyed by config file rather than a single
/// global so a test or tool reading a different config never inherits it.
static ROOTS: std::sync::LazyLock<std::sync::RwLock<std::collections::HashMap<std::path::PathBuf, std::path::PathBuf>>> =
    std::sync::LazyLock::new(Default::default);

/// Record `root` as the local-models directory for the daemon whose global
/// config lives at `config_path`.
pub fn register_root(config_path: &std::path::Path, root: std::path::PathBuf) {
    if let Ok(mut roots) = ROOTS.write() {
        roots.insert(config_path.to_path_buf(), root);
    }
}

/// The local-models directory for `config_path`: the one registered at boot,
/// else `local-models/` beside the config file (the default layout, where both
/// live in `~/.senclaw`).
pub fn root_for(config_path: &std::path::Path) -> Option<std::path::PathBuf> {
    ROOTS
        .read()
        .ok()
        .and_then(|roots| roots.get(config_path).cloned())
        .or_else(|| config_path.parent().map(|p| p.join("local-models")))
}

#[cfg(test)]
mod root_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn a_registered_root_wins_only_for_its_own_config_file() {
        let config = Path::new("/tmp/senclaw-root-test-a/config.json");
        assert_eq!(root_for(config), Some(PathBuf::from("/tmp/senclaw-root-test-a/local-models")));
        register_root(config, PathBuf::from("/Volumes/Models/senclaw"));
        assert_eq!(root_for(config), Some(PathBuf::from("/Volumes/Models/senclaw")));
        let other = Path::new("/tmp/senclaw-root-test-b/config.json");
        assert_eq!(root_for(other), Some(PathBuf::from("/tmp/senclaw-root-test-b/local-models")));
    }
}

pub fn is_local_config(id: &str) -> bool {
    parse_config_id(id).is_some()
}
