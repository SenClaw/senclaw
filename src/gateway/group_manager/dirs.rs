//! Directory management: agent dirs, SOUL.md, MEMORY.md.

use std::fs;

use crate::config::Config;

use super::soul::default_soul_md;

pub fn ensure_agent_dirs(config: &Config, folder: &str, name: &str) -> (String, String) {
    let agent_data_dir = config.paths.agents_dir.join(folder);
    let workspace_dir = config.paths.workspace_dir.join(folder);

    fs::create_dir_all(agent_data_dir.join("memory")).ok();
    fs::create_dir_all(agent_data_dir.join(".sema").join("sessions")).ok();

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
    let soul_md = config.paths.agents_dir.join(folder).join("SOUL.md");
    let content = if core_prompt.trim().is_empty() {
        default_soul_md(folder, name)
    } else {
        core_prompt.to_string()
    };
    fs::write(&soul_md, content).ok();
}

/// Read SOUL.md content for an agent folder (empty string if missing).
pub fn read_soul_md(config: &Config, folder: &str) -> String {
    let soul_md = config.paths.agents_dir.join(folder).join("SOUL.md");
    fs::read_to_string(&soul_md).unwrap_or_default()
}

/// Write (or overwrite) MEMORY.md with the given content.
/// Empty content writes the default header so the file always exists.
pub fn write_memory_md(config: &Config, folder: &str, content: &str) {
    let memory_md = config.paths.agents_dir.join(folder).join("MEMORY.md");
    let body = if content.trim().is_empty() {
        "# Memory\n\n".to_string()
    } else {
        content.to_string()
    };
    fs::write(&memory_md, body).ok();
}

/// Read MEMORY.md content for an agent folder (empty string if missing).
pub fn read_memory_md(config: &Config, folder: &str) -> String {
    let memory_md = config.paths.agents_dir.join(folder).join("MEMORY.md");
    fs::read_to_string(&memory_md).unwrap_or_default()
}
