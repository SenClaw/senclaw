//! Directory management: agent dirs, SOUL.md, MEMORY.md.

use std::fs;

use crate::config::Config;

use super::soul::default_soul_md;

/// Move a pre-0.1.3 workspace onto the current layout: `~/senclaw/agents/`
/// becomes `~/senclaw/profiles/`, and each profile's `.sema/` becomes `.sen/`.
/// Runs on every boot; a no-op once migrated. Never overwrites: a target that
/// already exists is left alone and the legacy copy stays where it was.
pub fn migrate_legacy_layout(config: &Config) {
    let profiles = &config.paths.profiles_dir;
    if !profiles.exists() && profiles.file_name().is_some_and(|n| n == "profiles") {
        if let Some(legacy) = profiles.parent().map(|p| p.join("agents")) {
            if legacy.is_dir() {
                match fs::rename(&legacy, profiles) {
                    Ok(()) => tracing::info!(
                        "[GroupManager] moved {} → {}",
                        legacy.display(),
                        profiles.display()
                    ),
                    Err(e) => tracing::warn!(
                        "[GroupManager] could not move {} → {}: {e}",
                        legacy.display(),
                        profiles.display()
                    ),
                }
            }
        }
    }

    let Ok(entries) = fs::read_dir(profiles) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let old = dir.join(".sema");
        let new = dir.join(crate::config::AGENT_STATE_DIR);
        if old.is_dir() && !new.exists() {
            if let Err(e) = fs::rename(&old, &new) {
                tracing::warn!("[GroupManager] could not move {}: {e}", old.display());
            }
        }
    }
}

pub fn ensure_agent_dirs(config: &Config, folder: &str, name: &str) -> (String, String) {
    let agent_data_dir = config.paths.profiles_dir.join(folder);
    let workspace_dir = config.paths.workspace_dir.join(folder);

    fs::create_dir_all(agent_data_dir.join("memory")).ok();
    fs::create_dir_all(
        agent_data_dir
            .join(crate::config::AGENT_STATE_DIR)
            .join("sessions"),
    )
    .ok();

    let soul_md = agent_data_dir.join("SOUL.md");
    if !soul_md.exists() {
        fs::write(&soul_md, default_soul_md(folder, name)).ok();
    }

    let memory_md = agent_data_dir.join("MEMORY.md");
    if !memory_md.exists() {
        fs::write(&memory_md, "# Memory\n\n").ok();
    }

    fs::create_dir_all(&workspace_dir).ok();

    (
        agent_data_dir.to_string_lossy().into_owned(),
        workspace_dir.to_string_lossy().into_owned(),
    )
}

/// Write (or overwrite) SOUL.md with the given core_prompt.
/// If core_prompt is empty, writes the default template.
pub fn write_soul_md(config: &Config, folder: &str, name: &str, core_prompt: &str) {
    let soul_md = config.paths.profiles_dir.join(folder).join("SOUL.md");
    let content = if core_prompt.trim().is_empty() {
        default_soul_md(folder, name)
    } else {
        core_prompt.to_string()
    };
    fs::write(&soul_md, content).ok();
}

/// Read SOUL.md content for an agent folder (empty string if missing).
pub fn read_soul_md(config: &Config, folder: &str) -> String {
    let soul_md = config.paths.profiles_dir.join(folder).join("SOUL.md");
    fs::read_to_string(&soul_md).unwrap_or_default()
}

/// Write (or overwrite) MEMORY.md with the given content.
/// Empty content writes the default header so the file always exists.
pub fn write_memory_md(config: &Config, folder: &str, content: &str) {
    let memory_md = config.paths.profiles_dir.join(folder).join("MEMORY.md");
    let body = if content.trim().is_empty() {
        "# Memory\n\n".to_string()
    } else {
        content.to_string()
    };
    fs::write(&memory_md, body).ok();
}

/// Read MEMORY.md content for an agent folder (empty string if missing).
pub fn read_memory_md(config: &Config, folder: &str) -> String {
    let memory_md = config.paths.profiles_dir.join(folder).join("MEMORY.md");
    fs::read_to_string(&memory_md).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_legacy_layout_renames_agents_and_sema() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("agents").join("main");
        fs::create_dir_all(legacy.join(".sema").join("sessions")).unwrap();
        fs::write(legacy.join("SOUL.md"), "soul").unwrap();

        let mut config = Config::from_env();
        config.paths.profiles_dir = tmp.path().join("profiles");
        migrate_legacy_layout(&config);

        let main = tmp.path().join("profiles").join("main");
        assert!(!tmp.path().join("agents").exists());
        assert_eq!(fs::read_to_string(main.join("SOUL.md")).unwrap(), "soul");
        assert!(main.join(".sen").join("sessions").is_dir());
        assert!(!main.join(".sema").exists());

        // Second run is a no-op.
        migrate_legacy_layout(&config);
        assert!(main.join(".sen").join("sessions").is_dir());
    }

    #[test]
    fn migrate_legacy_layout_keeps_existing_targets() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("agents").join("old")).unwrap();
        let main = tmp.path().join("profiles").join("main");
        fs::create_dir_all(main.join(".sema")).unwrap();
        fs::create_dir_all(main.join(".sen")).unwrap();

        let mut config = Config::from_env();
        config.paths.profiles_dir = tmp.path().join("profiles");
        migrate_legacy_layout(&config);

        assert!(tmp.path().join("agents").join("old").exists());
        assert!(main.join(".sema").exists());
    }
}
