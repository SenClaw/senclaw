//! Install / remove personas (sub-agents) bundled with a Space App.
//!
//! A manifest may declare a `personas` array, each `{ name, path }` pointing at
//! a persona `.md` file (YAML frontmatter + system prompt) inside the app. On
//! install we copy each into the virtual-agents dir as `<app_id>__<name>.md` so
//! the `PersonaRegistry` (and its file watcher) picks it up and `run_persona` /
//! the dispatch DAG can use it. The persona's registered name comes from its
//! frontmatter `name:`, so the filename prefix only tracks ownership for clean
//! removal. On app uninstall we delete every file carrying that prefix.

use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::config::Config;

fn personas_dir(config: &Config) -> &Path {
    &config.paths.virtual_agents_dir
}

/// Install (or refresh) all personas declared in the manifest. Idempotent.
pub fn install_app_personas(config: &Config, app_id: &str, app_dir: &Path, manifest: &Value) {
    let Some(personas) = manifest.get("personas").and_then(Value::as_array) else {
        return;
    };
    let dir = personas_dir(config);
    let _ = fs::create_dir_all(dir);

    for p in personas {
        let Some(name) = p.get("name").and_then(Value::as_str) else {
            continue;
        };
        let rel = p
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("personas/{name}.md"));
        let src = app_dir.join(&rel);
        if !src.is_file() {
            tracing::warn!("[space-personas] app '{app_id}' persona '{name}': no file at {src:?}");
            continue;
        }
        let dest = dir.join(format!("{app_id}__{name}.md"));
        if let Err(e) = fs::copy(&src, &dest) {
            tracing::warn!("[space-personas] copy persona '{name}' for '{app_id}' failed: {e}");
            continue;
        }
        tracing::info!("[space-personas] installed persona '{name}' for app '{app_id}'");
    }
}

/// Remove every persona file tagged with this app's `<app_id>__` prefix.
pub fn remove_app_personas(config: &Config, app_id: &str) {
    let dir = personas_dir(config);
    let Ok(items) = fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{app_id}__");
    for item in items.flatten() {
        let path = item.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let is_owned = path
            .file_name()
            .and_then(|f| f.to_str())
            .map(|f| f.starts_with(&prefix))
            .unwrap_or(false);
        if is_owned {
            let _ = fs::remove_file(&path);
            tracing::info!("[space-personas] removed persona file {path:?} for app '{app_id}'");
        }
    }
}
