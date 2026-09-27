//! The environment the daemon sets on every runtime it launches.
//!
//! The daemon side builds these with [`launch_vars`]; a runtime reads them back
//! with [`LaunchEnv::from_env`]. Every variable has a standalone default so a
//! runtime can also be started by hand (`sen-ocr serve --port 4999`) for
//! development — no token (no auth), no parent (no watchdog), and the usual
//! `~/.senclaw` folders.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Runtime id (`sen-ocr`).
pub const RUNTIME_ID: &str = "SENCLAW_RUNTIME_ID";
/// Package version being run.
pub const RUNTIME_VERSION: &str = "SENCLAW_RUNTIME_VERSION";
/// Bind host — always loopback when the daemon launches.
pub const HOST: &str = "SENCLAW_RUNTIME_HOST";
/// Bind port, chosen by the daemon per launch.
pub const PORT: &str = "SENCLAW_RUNTIME_PORT";
/// Per-launch bearer token. Every route but `/health` must demand it.
pub const TOKEN: &str = "SENCLAW_RUNTIME_TOKEN";
/// Persistent per-runtime folder (`~/.senclaw/runtime-data/<id>/`): settings
/// and anything the runtime keeps between versions.
pub const DATA_DIR: &str = "SENCLAW_RUNTIME_DATA_DIR";
/// Shared model root (`~/.senclaw/local-models/`). Runtimes keep the
/// sub-folders the in-daemon engines used so nothing is re-downloaded.
pub const LOCAL_MODELS_DIR: &str = "SENCLAW_LOCAL_MODELS_DIR";
/// The daemon's pid. The runtime exits when it disappears.
pub const PARENT_PID: &str = "SENCLAW_PARENT_PID";
/// The daemon's `config.json`, read-only, for first-start settings import.
pub const CONFIG_PATH: &str = "SENCLAW_CONFIG_PATH";
/// `~/.senclaw`.
pub const HOME: &str = "SENCLAW_HOME";
/// Model mode: the model file or directory this process serves.
pub const MODEL_PATH: &str = "SENCLAW_MODEL_PATH";
/// Model mode: the daemon's key for that model.
pub const MODEL_ID: &str = "SENCLAW_MODEL_ID";
/// Opt-in to a non-loopback bind. Never set by the daemon.
pub const ALLOW_REMOTE: &str = "SENCLAW_RUNTIME_ALLOW_REMOTE";

/// Values for one launch, as the daemon knows them.
#[derive(Debug, Clone)]
pub struct LaunchVars {
    pub id: String,
    pub version: String,
    pub port: u16,
    pub token: String,
    pub data_dir: PathBuf,
    pub models_dir: PathBuf,
    pub package_dir: PathBuf,
    pub parent_pid: u32,
    pub config_path: PathBuf,
    pub home: PathBuf,
    /// Model mode only.
    pub model: Option<ModelLaunch>,
}

/// The model a model-mode process is launched for.
#[derive(Debug, Clone)]
pub struct ModelLaunch {
    pub id: String,
    pub path: PathBuf,
    pub mmproj_path: Option<PathBuf>,
    pub context_length: u32,
}

/// Placeholder values for [`crate::manifest::render`], keyed by placeholder
/// name. Model placeholders are present only for a model launch, so a service
/// manifest that names one fails validation long before it gets here.
pub fn placeholder_values(v: &LaunchVars) -> BTreeMap<&'static str, String> {
    let mut m = BTreeMap::new();
    m.insert("host", "127.0.0.1".to_string());
    m.insert("port", v.port.to_string());
    m.insert("token", v.token.clone());
    m.insert("data_dir", v.data_dir.to_string_lossy().into_owned());
    m.insert("models_dir", v.models_dir.to_string_lossy().into_owned());
    m.insert("package_dir", v.package_dir.to_string_lossy().into_owned());
    m.insert("parent_pid", v.parent_pid.to_string());
    if let Some(model) = &v.model {
        m.insert("model_id", model.id.clone());
        m.insert("model_path", model.path.to_string_lossy().into_owned());
        m.insert("context_length", model.context_length.to_string());
        if let Some(p) = &model.mmproj_path {
            m.insert("mmproj_path", p.to_string_lossy().into_owned());
        }
    }
    m
}

/// The environment variables for one launch. Applied after the manifest's own
/// `entry.env`, so these always win.
pub fn launch_vars(v: &LaunchVars) -> BTreeMap<&'static str, String> {
    let mut m = BTreeMap::new();
    m.insert(RUNTIME_ID, v.id.clone());
    m.insert(RUNTIME_VERSION, v.version.clone());
    m.insert(HOST, "127.0.0.1".to_string());
    m.insert(PORT, v.port.to_string());
    m.insert(TOKEN, v.token.clone());
    m.insert(DATA_DIR, v.data_dir.to_string_lossy().into_owned());
    m.insert(LOCAL_MODELS_DIR, v.models_dir.to_string_lossy().into_owned());
    m.insert(PARENT_PID, v.parent_pid.to_string());
    m.insert(CONFIG_PATH, v.config_path.to_string_lossy().into_owned());
    m.insert(HOME, v.home.to_string_lossy().into_owned());
    if let Some(model) = &v.model {
        m.insert(MODEL_PATH, model.path.to_string_lossy().into_owned());
        m.insert(MODEL_ID, model.id.clone());
    }
    m
}

/// What a runtime process was launched with.
#[derive(Debug, Clone)]
pub struct LaunchEnv {
    pub id: String,
    pub version: String,
    pub host: String,
    pub port: Option<u16>,
    /// `None` when started by hand: no auth.
    pub token: Option<String>,
    pub data_dir: PathBuf,
    pub models_dir: PathBuf,
    /// `None` when started by hand: no watchdog.
    pub parent_pid: Option<u32>,
    pub config_path: PathBuf,
    pub home: PathBuf,
    pub model_path: Option<PathBuf>,
    pub model_id: Option<String>,
    pub allow_remote: bool,
}

impl LaunchEnv {
    /// Read the launch environment, defaulting what is missing.
    /// `default_id` / `default_version` are the runtime's own
    /// (`env!("CARGO_PKG_NAME")`), used when started by hand.
    pub fn from_env(default_id: &str, default_version: &str) -> LaunchEnv {
        Self::from_lookup(default_id, default_version, |k| std::env::var(k).ok())
    }

    /// [`LaunchEnv::from_env`] over an arbitrary lookup (for tests).
    pub fn from_lookup(
        default_id: &str,
        default_version: &str,
        get: impl Fn(&str) -> Option<String>,
    ) -> LaunchEnv {
        let non_empty = |k: &str| get(k).filter(|v| !v.trim().is_empty());
        let home = non_empty(HOME)
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".senclaw"));
        let id = non_empty(RUNTIME_ID).unwrap_or_else(|| default_id.to_string());
        LaunchEnv {
            version: non_empty(RUNTIME_VERSION).unwrap_or_else(|| default_version.to_string()),
            host: non_empty(HOST).unwrap_or_else(|| "127.0.0.1".to_string()),
            port: non_empty(PORT).and_then(|p| p.parse().ok()),
            token: non_empty(TOKEN),
            data_dir: non_empty(DATA_DIR)
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("runtime-data").join(&id)),
            models_dir: non_empty(LOCAL_MODELS_DIR)
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("local-models")),
            parent_pid: non_empty(PARENT_PID).and_then(|p| p.parse().ok()),
            config_path: non_empty(CONFIG_PATH)
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join("config.json")),
            model_path: non_empty(MODEL_PATH).map(PathBuf::from),
            model_id: non_empty(MODEL_ID),
            allow_remote: non_empty(ALLOW_REMOTE).is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true")),
            home,
            id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn standalone_defaults_have_no_token_and_no_parent() {
        let env = LaunchEnv::from_lookup("sen-tts", "0.1.0", |_| None);
        assert_eq!(env.id, "sen-tts");
        assert_eq!(env.host, "127.0.0.1");
        assert!(env.token.is_none());
        assert!(env.parent_pid.is_none());
        assert!(env.data_dir.ends_with("runtime-data/sen-tts"));
        assert!(env.models_dir.ends_with(".senclaw/local-models"));
    }

    #[test]
    fn launch_vars_round_trip_through_from_env() {
        let vars = LaunchVars {
            id: "sen-mlx".into(),
            version: "0.2.0".into(),
            port: 40001,
            token: "abc".into(),
            data_dir: "/h/runtime-data/sen-mlx".into(),
            models_dir: "/h/local-models".into(),
            package_dir: "/h/runtimes/sen-mlx/0.2.0".into(),
            parent_pid: 42,
            config_path: "/h/config.json".into(),
            home: "/h".into(),
            model: Some(ModelLaunch {
                id: "mlx:qwen".into(),
                path: "/h/local-models/mlx/qwen".into(),
                mmproj_path: None,
                context_length: 8192,
            }),
        };
        let map: HashMap<String, String> =
            launch_vars(&vars).into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        let env = LaunchEnv::from_lookup("x", "y", |k| map.get(k).cloned());
        assert_eq!(env.id, "sen-mlx");
        assert_eq!(env.port, Some(40001));
        assert_eq!(env.token.as_deref(), Some("abc"));
        assert_eq!(env.parent_pid, Some(42));
        assert_eq!(env.model_id.as_deref(), Some("mlx:qwen"));
        assert_eq!(env.model_path, Some(PathBuf::from("/h/local-models/mlx/qwen")));
        let placeholders = placeholder_values(&vars);
        assert_eq!(placeholders["context_length"], "8192");
        assert!(!placeholders.contains_key("mmproj_path"));
    }
}
