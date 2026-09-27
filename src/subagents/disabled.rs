//! Subagent (virtual persona) enable/disable persistence.
//! Mirrors `src-old/subagents/disabled.ts`.
//!
//! Storage: `~/.senclaw/disabled-subagents.json`
//! Format: `{ "disabled": ["persona-name", ...] }`
//!
//! Disabled by persona name (frontmatter `name` field).
//! Missing file = no personas disabled (all enabled).

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

static DISABLED_FILE: Mutex<PathBuf> = Mutex::new(PathBuf::new());
static CACHE: Mutex<Option<Vec<String>>> = Mutex::new(None);

#[derive(Serialize, Deserialize)]
struct DisabledSubagentsStore {
    disabled: Vec<String>,
}

fn default_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".senclaw")
        .join("disabled-subagents.json")
}

fn get_path() -> PathBuf {
    let path = DISABLED_FILE.lock().unwrap();
    if path.as_os_str().is_empty() {
        drop(path);
        let p = default_path();
        *DISABLED_FILE.lock().unwrap() = p.clone();
        p
    } else {
        path.clone()
    }
}

fn read_store() -> Vec<String> {
    let mut cache = CACHE.lock().unwrap();
    if let Some(ref cached) = *cache {
        return cached.clone();
    }
    let store = match fs::read_to_string(get_path()) {
        Ok(raw) => {
            let parsed: Option<DisabledSubagentsStore> = serde_json::from_str(&raw).ok();
            parsed.map(|s| s.disabled).unwrap_or_default()
        }
        Err(_) => Vec::new(),
    };
    *cache = Some(store.clone());
    store
}

fn write_store(list: &[String]) {
    let path = get_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let store = DisabledSubagentsStore {
        disabled: list.to_vec(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&store) {
        let _ = fs::write(&path, json + "\n");
    }
}

pub fn read_disabled_subagents() -> Vec<String> {
    read_store()
}

pub fn disable_subagent(name: &str) {
    let mut current = read_store();
    if !current.iter().any(|n| n == name) {
        current.push(name.to_string());
        current.sort();
        write_store(&current);
        *CACHE.lock().unwrap() = Some(current);
    }
}

pub fn enable_subagent(name: &str) {
    let mut current = read_store();
    if current.iter().any(|n| n == name) {
        current.retain(|n| n != name);
        write_store(&current);
        *CACHE.lock().unwrap() = Some(current);
    }
}

pub fn is_subagent_disabled(name: &str) -> bool {
    read_store().iter().any(|n| n == name)
}

pub fn invalidate_disabled_subagents_cache() {
    *CACHE.lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disabled_subagents_lifecycle() {
        let tmp = std::env::temp_dir().join(format!(
            "test-disabled-subagents-{}.json",
            uuid::Uuid::new_v4()
        ));
        *DISABLED_FILE.lock().unwrap() = tmp.clone();
        invalidate_disabled_subagents_cache();

        assert!(!is_subagent_disabled("coder"));
        assert!(read_disabled_subagents().is_empty());

        disable_subagent("coder");
        assert!(is_subagent_disabled("coder"));

        disable_subagent("coder"); // idempotent
        assert_eq!(read_disabled_subagents().len(), 1);

        enable_subagent("coder");
        assert!(!is_subagent_disabled("coder"));
        assert!(read_disabled_subagents().is_empty());

        let _ = std::fs::remove_file(&tmp);
    }
}
