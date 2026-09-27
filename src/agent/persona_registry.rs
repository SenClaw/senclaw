//! Virtual agent persona registry. Mirrors `src-old/agent/PersonaRegistry.ts`.
//!
//! Scans markdown files with YAML-like frontmatter from the virtual agents directory.
//! Includes a polling-based background watcher for hot-reload (1.5s interval, matching
//! the TS `fs.watch` behavior).
//!
//! Enhanced with priority system (project > user > builtin) and file caching.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use tokio::task::JoinHandle;

use super::builtin_agents;

/// Location where a persona configuration was loaded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonaLocation {
    /// Built-in agent (from builtin_agents module)
    Builtin,
    /// User-level agent (from user directory)
    User,
    /// Project-level agent (from project directory)
    Project,
}

impl PersonaLocation {
    /// Get priority for location (higher = more important).
    /// Priority: Project > User > Builtin
    pub fn priority(&self) -> u8 {
        match self {
            PersonaLocation::Project => 3,
            PersonaLocation::User => 2,
            PersonaLocation::Builtin => 1,
        }
    }
}

/// Parsed persona configuration from a `.md` file or built-in definition.
#[derive(Debug, Clone)]
pub struct PersonaConfig {
    pub name: String,
    pub description: String,
    /// `None` = use default toolset.
    pub tools: Option<Vec<String>>,
    /// Reserved, not currently active.
    pub model: Option<String>,
    pub max_concurrent: u32,
    /// Body text after the frontmatter block.
    pub system_prompt: String,
    pub file_path: PathBuf,
    /// Where this persona was loaded from.
    pub location: PersonaLocation,
}

const MAX_SYSTEM_PROMPT_LENGTH: usize = 8000;

/// File cache entry with modification time for cache invalidation.
#[derive(Debug, Clone)]
struct FileCacheEntry {
    mtime: SystemTime,
    config: PersonaConfig,
}

pub struct PersonaRegistry {
    dir: PathBuf,
    personas: HashMap<String, PersonaConfig>,
    /// File cache: maps file path to (mtime, config) for cache invalidation.
    file_cache: HashMap<PathBuf, FileCacheEntry>,
    /// User-level personas directory (higher priority than builtin).
    user_dir: Option<PathBuf>,
    /// Project-level personas directory (highest priority).
    project_dir: Option<PathBuf>,
}

impl PersonaRegistry {
    /// Create a new PersonaRegistry with a single directory (legacy behavior).
    pub fn new(dir: PathBuf) -> Self {
        Self::with_dirs(dir, None, None)
    }

    /// Create a new PersonaRegistry with multiple directories supporting priority system.
    ///
    /// # Arguments
    /// * `dir` - Primary directory (usually virtual agents dir)
    /// * `user_dir` - Optional user-level personas directory
    /// * `project_dir` - Optional project-level personas directory
    ///
    /// Priority order: project_dir > user_dir > dir > built-in
    pub fn with_dirs(
        dir: PathBuf,
        user_dir: Option<PathBuf>,
        project_dir: Option<PathBuf>,
    ) -> Self {
        let mut reg = Self {
            dir,
            personas: HashMap::new(),
            file_cache: HashMap::new(),
            user_dir,
            project_dir,
        };
        reg.load_all();
        tracing::info!(
            "[PersonaRegistry] Initialized with {} persona(s)",
            reg.personas.len()
        );
        reg
    }

    pub fn get(&self, name: &str) -> Option<&PersonaConfig> {
        self.personas.get(name)
    }

    pub fn list(&self) -> Vec<&PersonaConfig> {
        self.personas.values().collect()
    }

    /// Rescan the directory and refresh in-memory cache.
    pub fn reload(&mut self) {
        self.load_all();
    }

    /// Spawn a background task that watches the virtual agents directory for
    /// `.md` file changes and auto-reloads. Uses polling at the same 1.5 s
    /// interval as [`crate::memory::manager::MemoryManager`].
    pub fn spawn_watcher(this: Arc<Mutex<Self>>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let dir = {
                let guard = this.lock().unwrap();
                guard.dir.clone()
            };
            if !dir.is_dir() {
                tracing::warn!(
                    "[PersonaRegistry] Watcher: directory not found, skipping: {}",
                    dir.display()
                );
                return;
            }

            // Seed initial mtimes
            let mut last_mtimes: HashMap<PathBuf, SystemTime> = HashMap::new();
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().map_or(true, |e| e != "md") {
                        continue;
                    }
                    if let Ok(meta) = fs::metadata(&path) {
                        if let Ok(mtime) = meta.modified() {
                            last_mtimes.insert(path, mtime);
                        }
                    }
                }
            }

            let mut interval = tokio::time::interval(std::time::Duration::from_millis(1500));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;

                let mut changed = false;
                // Check for new/modified files
                if let Ok(entries) = fs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().map_or(true, |e| e != "md") {
                            continue;
                        }
                        if let Ok(meta) = fs::metadata(&path) {
                            if let Ok(mtime) = meta.modified() {
                                let prev = last_mtimes.get(&path).copied();
                                if prev != Some(mtime) {
                                    changed = true;
                                    last_mtimes.insert(path, mtime);
                                }
                            }
                        }
                    }
                }
                // Check for deleted files
                let mut deleted = Vec::new();
                for path in last_mtimes.keys() {
                    if !path.exists() {
                        deleted.push(path.clone());
                        changed = true;
                    }
                }
                for path in &deleted {
                    last_mtimes.remove(path);
                }

                if changed {
                    tracing::debug!("[PersonaRegistry] File change detected, reloading...");
                    if let Ok(mut guard) = this.lock() {
                        guard.load_all();
                    }
                }
            }
        })
    }

    // ===== Internal =====

    fn load_all(&mut self) {
        // Clear existing personas and cache
        self.personas.clear();
        self.file_cache.clear();

        // 1. Load built-in agents (lowest priority)
        self.load_builtin_agents();

        // Clone paths before borrowing mutably
        let dir = self.dir.clone();
        let user_dir = self.user_dir.clone();
        let project_dir = self.project_dir.clone();

        // 2. Load from primary directory
        if dir.is_dir() {
            self.load_from_dir(&dir, PersonaLocation::Project);
        } else {
            tracing::warn!(
                "[PersonaRegistry] Primary directory not found, skipping: {}",
                dir.display()
            );
        }

        // 3. Load from user directory (medium priority)
        if let Some(user_dir) = user_dir {
            if user_dir.is_dir() {
                self.load_from_dir(&user_dir, PersonaLocation::User);
            } else {
                tracing::debug!(
                    "[PersonaRegistry] User directory not found: {}",
                    user_dir.display()
                );
            }
        }

        // 4. Load from project directory (highest priority)
        if let Some(project_dir) = project_dir {
            if project_dir.is_dir() {
                self.load_from_dir(&project_dir, PersonaLocation::Project);
            } else {
                tracing::debug!(
                    "[PersonaRegistry] Project directory not found: {}",
                    project_dir.display()
                );
            }
        }

        let keys: Vec<&str> = self.personas.keys().map(|s| s.as_str()).collect();
        tracing::debug!(
            "[PersonaRegistry] Loaded {} persona(s): {}",
            self.personas.len(),
            keys.join(", ")
        );
    }

    /// Load built-in agents from the builtin_agents module.
    fn load_builtin_agents(&mut self) {
        for builtin in builtin_agents::BUILTIN_AGENTS {
            let config = PersonaConfig {
                name: builtin.name.to_string(),
                description: builtin.description.to_string(),
                tools: Some(builtin.tools.iter().map(|s| s.to_string()).collect()),
                model: None,
                max_concurrent: 5,
                system_prompt: builtin.prompt.to_string(),
                file_path: PathBuf::from(format!("builtin://{}", builtin.name)),
                location: PersonaLocation::Builtin,
            };

            // Only add if not already overridden by higher priority
            if !self.personas.contains_key(&config.name) {
                self.personas.insert(config.name.clone(), config.clone());
                tracing::debug!("[PersonaRegistry] Loaded built-in agent: {}", config.name);
            }
        }
    }

    /// Load personas from a directory with specified location.
    fn load_from_dir(&mut self, dir: &Path, location: PersonaLocation) {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(
                    "[PersonaRegistry] Failed to read dir {}: {e}",
                    dir.display()
                );
                return;
            }
        };

        for entry in entries.flatten() {
            let file_path = entry.path();
            if file_path.extension().map_or(true, |e| e != "md") {
                continue;
            }

            // Check cache first
            let cached = self.check_file_cache(&file_path);
            if let Some(config) = cached {
                // Use cached config if file hasn't changed
                let current_priority = location.priority();
                let existing_priority = self
                    .personas
                    .get(&config.name)
                    .map(|p| p.location.priority())
                    .unwrap_or(0);

                // Only update if new location has higher or equal priority
                if current_priority >= existing_priority {
                    if current_priority > existing_priority {
                        tracing::debug!(
                            "[PersonaRegistry] Agent [{}] overridden by {}-level config",
                            config.name,
                            location_str(location)
                        );
                    }
                    self.personas.insert(config.name.clone(), config);
                }
                continue;
            }

            // Parse file
            match parse_file(&file_path, location) {
                Ok(Some(mut persona)) => {
                    let key = persona.name.clone();
                    let current_priority = location.priority();
                    let existing_priority = self
                        .personas
                        .get(&key)
                        .map(|p| p.location.priority())
                        .unwrap_or(0);

                    // Only add/update if new location has higher or equal priority
                    if current_priority >= existing_priority {
                        if current_priority > existing_priority {
                            tracing::debug!(
                                "[PersonaRegistry] Agent [{}] overridden by {}-level config",
                                key,
                                location_str(location)
                            );
                        }

                        // Handle name conflicts
                        if self.personas.contains_key(&key) && current_priority == existing_priority
                        {
                            let file_base =
                                file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                            if self.personas.contains_key(file_base) {
                                tracing::warn!(
                                    "[PersonaRegistry] Duplicate name \"{}\" and filename \"{}\" both taken, skipping {}",
                                    persona.name,
                                    file_base,
                                    file_path.display()
                                );
                                continue;
                            }
                            tracing::warn!(
                                "[PersonaRegistry] Duplicate name \"{}\", falling back to filename \"{}\"",
                                persona.name,
                                file_base
                            );
                            persona.name = file_base.to_string();
                        }

                        // Update cache
                        if let Ok(meta) = fs::metadata(&file_path) {
                            if let Ok(mtime) = meta.modified() {
                                self.file_cache.insert(
                                    file_path.clone(),
                                    FileCacheEntry {
                                        mtime,
                                        config: persona.clone(),
                                    },
                                );
                            }
                        }

                        self.personas.insert(persona.name.clone(), persona);
                    }
                }
                Ok(None) => { /* parse failure already logged */ }
                Err(e) => {
                    tracing::warn!(
                        "[PersonaRegistry] Failed to parse {}: {e}",
                        file_path.display()
                    );
                }
            }
        }
    }

    /// Check file cache and return cached config if file hasn't changed.
    fn check_file_cache(&self, file_path: &Path) -> Option<PersonaConfig> {
        if let Some(cached) = self.file_cache.get(file_path) {
            if let Ok(meta) = fs::metadata(file_path) {
                if let Ok(mtime) = meta.modified() {
                    if mtime == cached.mtime {
                        return Some(cached.config.clone());
                    }
                }
            }
        }
        None
    }
}

// ===== File parsing =====

fn parse_file(
    file_path: &Path,
    location: PersonaLocation,
) -> Result<Option<PersonaConfig>, anyhow::Error> {
    let raw = fs::read_to_string(file_path)?;
    let lines: Vec<&str> = raw.lines().collect();

    // Find frontmatter boundaries
    let mut fm_start: Option<usize> = None;
    let mut fm_end: Option<usize> = None;

    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "---" {
            if fm_start.is_none() {
                fm_start = Some(i);
            } else {
                fm_end = Some(i);
                break;
            }
        }
    }

    let (fm_start, fm_end) = match (fm_start, fm_end) {
        (Some(s), Some(e)) => (s, e),
        _ => {
            tracing::warn!(
                "[PersonaRegistry] No frontmatter found in {}",
                file_path.file_name().unwrap_or_default().to_string_lossy()
            );
            return Ok(None);
        }
    };

    // Parse frontmatter key: value pairs
    let mut fm: HashMap<String, String> = HashMap::new();
    for i in (fm_start + 1)..fm_end {
        let line = lines[i].trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(colon_idx) = line.find(':') {
            let key = line[..colon_idx].trim().to_string();
            let value = line[colon_idx + 1..].trim().to_string();
            fm.insert(key, value);
        }
    }

    let file_name = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

    let name = fm.remove("name").unwrap_or_else(|| file_name.to_string());

    let description = match fm.remove("description") {
        Some(d) if !d.is_empty() => d,
        _ => {
            tracing::warn!(
                "[PersonaRegistry] Missing required field \"description\" in {}",
                file_path.file_name().unwrap_or_default().to_string_lossy()
            );
            return Ok(None);
        }
    };

    let tools = fm.remove("tools").map(|t| {
        t.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });

    let model = fm.remove("model").filter(|m| !m.is_empty());

    let max_concurrent = fm
        .remove("max_concurrent")
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    // Everything after second --- is the system prompt
    let mut system_prompt = lines[fm_end + 1..].join("\n").trim().to_string();
    if system_prompt.len() > MAX_SYSTEM_PROMPT_LENGTH {
        tracing::warn!(
            "[PersonaRegistry] \"{name}\" systemPrompt too long ({} chars), truncating to {MAX_SYSTEM_PROMPT_LENGTH}",
            system_prompt.len()
        );
        system_prompt = system_prompt
            .chars()
            .take(MAX_SYSTEM_PROMPT_LENGTH)
            .collect();
    }

    Ok(Some(PersonaConfig {
        name,
        description,
        tools,
        model,
        max_concurrent,
        system_prompt,
        file_path: file_path.to_path_buf(),
        location,
    }))
}

/// Convert PersonaLocation to string for logging.
fn location_str(loc: PersonaLocation) -> &'static str {
    match loc {
        PersonaLocation::Builtin => "builtin",
        PersonaLocation::User => "user",
        PersonaLocation::Project => "project",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_persona() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("coder.md");
        let content = "\
---
name: Coder
description: Writes and reviews code
tools: Read,Write,Edit,Bash
max_concurrent: 3
---

You are an expert programmer. Write clean, idiomatic code.
";
        fs::write(&file_path, content).unwrap();

        let persona = parse_file(&file_path, PersonaLocation::Project)
            .unwrap()
            .unwrap();
        assert_eq!(persona.name, "Coder");
        assert_eq!(persona.description, "Writes and reviews code");
        assert_eq!(
            persona.tools.unwrap(),
            vec!["Read", "Write", "Edit", "Bash"]
        );
        assert_eq!(persona.max_concurrent, 3);
        assert!(persona.system_prompt.contains("expert programmer"));
        assert_eq!(persona.location, PersonaLocation::Project);
    }

    #[test]
    fn name_fallback_to_filename() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("helper.md");
        fs::write(
            &file_path,
            "\
---
description: A helpful assistant
tools: Read
---

Be helpful.
",
        )
        .unwrap();

        let persona = parse_file(&file_path, PersonaLocation::User)
            .unwrap()
            .unwrap();
        assert_eq!(persona.name, "helper");
        assert_eq!(persona.description, "A helpful assistant");
        assert_eq!(persona.tools.unwrap(), vec!["Read"]);
        assert_eq!(persona.location, PersonaLocation::User);
    }

    #[test]
    fn missing_description_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("bad.md");
        fs::write(
            &file_path,
            "\
---
name: Bad
tools: Bash
---

No description here.
",
        )
        .unwrap();

        assert!(parse_file(&file_path, PersonaLocation::Project)
            .unwrap()
            .is_none());
    }

    #[test]
    fn no_frontmatter_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("plain.md");
        fs::write(&file_path, "Just plain text, no frontmatter.").unwrap();

        assert!(parse_file(&file_path, PersonaLocation::Project)
            .unwrap()
            .is_none());
    }

    #[test]
    fn defaults_when_fields_missing() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("minimal.md");
        fs::write(
            &file_path,
            "\
---
description: Minimal persona
---

Hello world.
",
        )
        .unwrap();

        let persona = parse_file(&file_path, PersonaLocation::Project)
            .unwrap()
            .unwrap();
        assert_eq!(persona.name, "minimal");
        assert_eq!(persona.max_concurrent, 5);
        assert!(persona.tools.is_none());
        assert!(persona.model.is_none());
        assert_eq!(persona.location, PersonaLocation::Project);
    }

    #[test]
    fn truncates_long_system_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("verbose.md");
        let long_body = "x".repeat(9000);
        fs::write(
            &file_path,
            format!(
                "\
---
description: Verbose
---

{long_body}
"
            ),
        )
        .unwrap();

        let persona = parse_file(&file_path, PersonaLocation::Project)
            .unwrap()
            .unwrap();
        assert_eq!(
            persona.system_prompt.chars().count(),
            MAX_SYSTEM_PROMPT_LENGTH
        );
        assert_eq!(persona.location, PersonaLocation::Project);
    }

    #[test]
    fn registry_loads_and_lists() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("alpha.md"),
            "\
---
description: First persona
---

Body A.
",
        )
        .unwrap();
        fs::write(
            dir.path().join("beta.md"),
            "\
---
name: Beta
description: Second persona
tools: Bash
max_concurrent: 2
---

Body B.
",
        )
        .unwrap();

        // The registry also seeds built-in agents; count only the personas
        // loaded from our temp dir (non-builtin).
        let user_count = |reg: &PersonaRegistry| {
            reg.list()
                .iter()
                .filter(|p| p.location != PersonaLocation::Builtin)
                .count()
        };

        let reg = PersonaRegistry::new(dir.path().to_path_buf());
        assert_eq!(user_count(&reg), 2);
        assert!(reg.get("alpha").is_some());
        assert_eq!(reg.get("Beta").unwrap().max_concurrent, 2);

        // Reload after adding a new file
        fs::write(
            dir.path().join("gamma.md"),
            "\
---
description: Third
---

Body C.
",
        )
        .unwrap();
        // Create a new registry pointing to same dir
        let mut reg2 = PersonaRegistry::new(dir.path().to_path_buf());
        assert_eq!(user_count(&reg2), 3);
        // Or use reload
        reg2.reload();
        assert_eq!(user_count(&reg2), 3);
    }

    #[test]
    fn test_priority_system() {
        let dir = tempfile::tempdir().unwrap();
        let user_dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();

        // Create a persona in each directory with the same name
        let content = "\
---
name: test
description: Test persona
---

Test body.
";

        fs::write(dir.path().join("test.md"), content).unwrap();
        fs::write(user_dir.path().join("test.md"), content).unwrap();
        fs::write(project_dir.path().join("test.md"), content).unwrap();

        // Create registry with all three directories
        let reg = PersonaRegistry::with_dirs(
            dir.path().to_path_buf(),
            Some(user_dir.path().to_path_buf()),
            Some(project_dir.path().to_path_buf()),
        );

        // Project-level should win
        let persona = reg.get("test").unwrap();
        assert_eq!(persona.location, PersonaLocation::Project);
    }

    #[test]
    fn test_builtin_agents_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let reg = PersonaRegistry::new(dir.path().to_path_buf());

        // Built-in agents should be loaded
        assert!(reg.get("researcher").is_some());
        assert!(reg.get("creator").is_some());
        assert!(reg.get("architect").is_some());

        // They should have builtin location
        assert_eq!(
            reg.get("researcher").unwrap().location,
            PersonaLocation::Builtin
        );
    }

    #[test]
    fn test_builtin_agents_can_be_overridden() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = tempfile::tempdir().unwrap();

        // Create a custom researcher in project dir
        let content = "\
---
name: researcher
description: Custom researcher
---

Custom prompt.
";
        fs::write(project_dir.path().join("researcher.md"), content).unwrap();

        let reg = PersonaRegistry::with_dirs(
            dir.path().to_path_buf(),
            None,
            Some(project_dir.path().to_path_buf()),
        );

        // Project-level should override builtin
        let persona = reg.get("researcher").unwrap();
        assert_eq!(persona.location, PersonaLocation::Project);
        assert_eq!(persona.description, "Custom researcher");
        assert!(persona.system_prompt.contains("Custom prompt"));
    }
}
