//! Skills loader and registry. Port targets: src-old/skills/*.ts

pub mod bundled;
pub mod config;
pub mod disabled;
pub mod expand;
pub mod matching;
pub mod metadata;
pub mod scan;

use std::collections::HashMap;
use std::sync::RwLock;

pub use metadata::{SkillMetadata, SkillParam, SkillUseMode};
use scan::SkillEntry;

/// A loaded skill with metadata, content, and source info.
#[derive(Debug, Clone)]
pub struct Skill {
    pub metadata: SkillMetadata,
    /// The markdown body (instructions).
    pub content: String,
    pub file_path: String,
    pub base_dir: String,
    /// Source: user / project / managed / workspace
    pub locate: String,
}

/// Thread-safe skill registry keyed by skill name.
#[derive(Debug)]
pub struct SkillRegistry {
    inner: RwLock<HashMap<String, Skill>>,
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }
}

impl SkillRegistry {
    pub fn find(&self, name: &str) -> Option<Skill> {
        self.inner.read().unwrap().get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.inner.read().unwrap().keys().cloned().collect();
        names.sort();
        names
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.read().unwrap().len()
    }

    /// Load skills from scanned entries into this registry.
    pub fn load_entries(&self, entries: &[SkillEntry]) {
        let map = build_skill_map(entries);
        *self.inner.write().unwrap() = map;
    }

    /// Build a registry from scanned `SkillEntry` values.
    pub fn from_entries(entries: &[SkillEntry]) -> Self {
        Self {
            inner: RwLock::new(build_skill_map(entries)),
        }
    }
}

fn build_skill_map(entries: &[SkillEntry]) -> HashMap<String, Skill> {
    entries
        .iter()
        .filter_map(|e| {
            let content = std::fs::read_to_string(&e.file_path).ok()?;
            let body = metadata::extract_body(&content);
            // Reuse the metadata parsed during scanning rather than re-parsing.
            let meta = e.metadata.clone();
            Some((
                meta.name.clone(),
                Skill {
                    metadata: meta,
                    content: body,
                    file_path: e.file_path.to_string_lossy().to_string(),
                    base_dir: e.dir.to_string_lossy().to_string(),
                    locate: e.source.clone(),
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_find() {
        let reg = SkillRegistry::from_entries(&[]);
        assert!(reg.find("nonexistent").is_none());
        assert!(reg.is_empty());
    }
}
