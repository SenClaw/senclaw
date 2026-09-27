//! Skill enable/disable persistence. Mirrors `src-old/skills/disabled.ts`.
//!
//! Storage: `~/.senclaw/disabled-skills.json`
//! Format: `{ "disabled": ["docx", "pdf", ...] }`
//!
//! Disabled by skill name (SKILL.md frontmatter `name` field).
//! Applies to all sources (bundled / clawhub-managed / global).
//! Missing file = no skills disabled (all enabled).

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

static DISABLED_FILE: Mutex<PathBuf> = Mutex::new(PathBuf::new());
static CACHE: Mutex<Option<Vec<String>>> = Mutex::new(None);

#[derive(Serialize, Deserialize)]
struct DisabledSkillsStore {
    disabled: Vec<String>,
}

fn default_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".senclaw")
        .join("disabled-skills.json")
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
            let parsed: Option<DisabledSkillsStore> = serde_json::from_str(&raw).ok();
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
    let store = DisabledSkillsStore {
        disabled: list.to_vec(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&store) {
        let _ = fs::write(&path, json + "\n");
    }
}

/// Return the set of disabled skill names (cached).
pub fn read_disabled_skills() -> Vec<String> {
    read_store()
}

/// Disable a skill by name (idempotent).
pub fn disable_skill(name: &str) {
    let mut current = read_store();
    if !current.iter().any(|n| n == name) {
        current.push(name.to_string());
        current.sort();
        write_store(&current);
        *CACHE.lock().unwrap() = Some(current);
    }
}

/// Enable a skill by name (idempotent).
pub fn enable_skill(name: &str) {
    let mut current = read_store();
    if current.iter().any(|n| n == name) {
        current.retain(|n| n != name);
        write_store(&current);
        *CACHE.lock().unwrap() = Some(current);
    }
}

/// Check if a skill is disabled.
pub fn is_skill_disabled(name: &str) -> bool {
    read_store().iter().any(|n| n == name)
}

/// Invalidate cache so next read reloads from disk.
pub fn invalidate_disabled_skills_cache() {
    *CACHE.lock().unwrap() = None;
}

/// Override the storage file path (for testing).
pub fn set_disabled_skills_file(p: PathBuf) {
    *DISABLED_FILE.lock().unwrap() = p;
    invalidate_disabled_skills_cache();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialise access to global statics so parallel `cargo test` doesn't race.
    static TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_reset() {
        *DISABLED_FILE.lock().unwrap() = PathBuf::new();
        *CACHE.lock().unwrap() = None;
    }

    #[test]
    fn test_disabled_skills_lifecycle() {
        let _guard = TEST_MUTEX.lock().unwrap();
        test_reset();
        let tmp = std::env::temp_dir().join(format!(
            "test-disabled-skills-{}.json",
            uuid::Uuid::new_v4()
        ));
        set_disabled_skills_file(tmp.clone());

        // Start empty
        assert!(!is_skill_disabled("pdf"));
        assert!(read_disabled_skills().is_empty());

        // Disable
        disable_skill("pdf");
        assert!(is_skill_disabled("pdf"));
        assert_eq!(read_disabled_skills(), vec!["pdf"]);

        // Idempotent disable
        disable_skill("pdf");
        assert_eq!(read_disabled_skills().len(), 1);

        // Enable
        enable_skill("pdf");
        assert!(!is_skill_disabled("pdf"));
        assert!(read_disabled_skills().is_empty());

        // Enable unknown is no-op
        enable_skill("nonexistent");
        assert!(read_disabled_skills().is_empty());

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_read_nonexistent_file() {
        let _guard = TEST_MUTEX.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("nonexistent-{}.json", uuid::Uuid::new_v4()));
        set_disabled_skills_file(tmp);
        assert!(read_disabled_skills().is_empty());
    }

    #[test]
    fn test_multiple_disabled() {
        let _guard = TEST_MUTEX.lock().unwrap();
        test_reset();
        let tmp =
            std::env::temp_dir().join(format!("test-disabled-multi-{}.json", uuid::Uuid::new_v4()));
        set_disabled_skills_file(tmp.clone());

        disable_skill("pdf");
        disable_skill("docx");
        disable_skill("xlsx");

        let list = read_disabled_skills();
        assert_eq!(list.len(), 3);
        assert!(list.contains(&"pdf".to_string()));
        assert!(list.contains(&"docx".to_string()));
        assert!(list.contains(&"xlsx".to_string()));

        // Sorted
        assert!(list.windows(2).all(|w| w[0] <= w[1]));

        // Invalidate and reload from disk
        invalidate_disabled_skills_cache();
        let list2 = read_disabled_skills();
        assert_eq!(list2, list);

        let _ = std::fs::remove_file(&tmp);
    }
}
