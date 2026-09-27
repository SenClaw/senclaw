//! Skills directory scanner. Mirrors `src-old/skills/scan.ts`.
//!
//! Scans all skill sources (bundled, global-compat, global-sema, clawhub-managed, marketplace)
//! and returns deduplicated skill entries sorted by name.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::skills::metadata::{parse_skill_metadata, SkillMetadata};

#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    /// Source label: bundled / global-compat / global-sema / clawhub-managed / marketplace
    pub source: String,
    /// Absolute path to the skill directory
    pub dir: PathBuf,
    /// Absolute path to SKILL.md
    pub file_path: PathBuf,
    /// Full parsed frontmatter (triggers, gating requirements, params, …).
    pub metadata: SkillMetadata,
    /// Whether the skill passes its load-time `os`/`requires` gates.
    pub eligible: bool,
    /// Human-readable reason the skill is ineligible (if any).
    pub ineligible_reason: Option<String>,
}

pub struct SourceDef {
    pub dir: PathBuf,
    pub source: String,
}

/// Marketplace source definition
pub struct MarketplaceSourceDef {
    pub dir: PathBuf,
    pub source_id: String,
    pub source_name: String,
}

/// Where the skills that ship with this build actually live.
///
/// [`Config::paths::bundled_skills_dir`] is a *compile-time* path — an env
/// override, or else `CARGO_MANIFEST_DIR/skills`. The second resolves on the
/// machine that built the binary and nowhere else, so a downloaded release
/// found no directory, pushed no `bundled` source, and served zero builtin
/// skills without logging a thing. Fall back to unpacking the copy compiled
/// into the binary, which cannot go missing.
///
/// The compile-time path still wins when it exists: a developer running out of
/// the checkout edits `skills/` and expects the next scan to see it, not a
/// snapshot taken when the binary was built.
fn bundled_skills_dir(config: &Config) -> Option<PathBuf> {
    match config.paths.bundled_skills_dir {
        Some(ref dir) if !dir.as_os_str().is_empty() && dir.is_dir() => Some(dir.clone()),
        // Beside the clawhub-managed skills rather than inside them: the
        // scanner reads both, and mixing binary-owned files into a directory it
        // rewrites would make `senclaw skills remove` look broken.
        _ => super::bundled::ensure(
            &config.paths.managed_skills_dir.with_file_name("bundled-skills"),
        ),
    }
}

/// Build the set of source directories to scan (excluding marketplace).
pub fn get_source_defs(config: &Config) -> Vec<SourceDef> {
    let mut defs: Vec<SourceDef> = Vec::new();

    if let Some(dir) = bundled_skills_dir(config) {
        defs.push(SourceDef {
            dir,
            source: "bundled".to_string(),
        });
    }

    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    defs.push(SourceDef {
        dir: home.join(".claude").join("skills"),
        source: "global-compat".to_string(),
    });
    defs.push(SourceDef {
        dir: home.join(".sema").join("skills"),
        source: "global-sema".to_string(),
    });
    defs.push(SourceDef {
        dir: config.paths.managed_skills_dir.clone(),
        source: "clawhub-managed".to_string(),
    });

    defs
}

/// Build the set of source directories to scan including marketplace sources.
pub fn get_source_defs_with_marketplace(
    config: &Config,
    marketplace_sources: Vec<MarketplaceSourceDef>,
) -> Vec<SourceDef> {
    let mut defs = get_source_defs(config);

    // Add marketplace sources
    for ms in marketplace_sources {
        defs.push(SourceDef {
            dir: ms.dir,
            source: format!("marketplace:{}", ms.source_name),
        });
    }

    defs
}

fn find_skill_md(dir: &Path) -> Option<PathBuf> {
    for name in &["SKILL.md", "skill.md", "Skill.md"] {
        let p = dir.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// If a skill directory was installed by a Space App it carries a
/// `.senclaw-app.json` marker; return `app:<app_id>` so the source label marks
/// it read-only and tied to that app.
fn app_marker_source(dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(dir.join(".senclaw-app.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let app_id = v.get("app_id").and_then(|x| x.as_str())?;
    Some(format!("app:{app_id}"))
}

/// Scan a single source directory for skills.
///
/// Each entry carries its fully parsed [`SkillMetadata`] plus an `eligible`
/// flag derived from the `os` / `requires` gates. Ineligible skills are still
/// returned here (so the CLI can list them with a reason); the runtime
/// loaders filter them out.
pub fn scan_source(def: &SourceDef) -> Vec<SkillEntry> {
    if !def.dir.exists() {
        return Vec::new();
    }

    let mut entries: Vec<SkillEntry> = Vec::new();
    let items = match fs::read_dir(&def.dir) {
        Ok(i) => i,
        Err(_) => return entries,
    };

    for item in items.flatten() {
        let name = item.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let full_path = item.path();
        if !full_path.is_dir() {
            continue;
        }

        let skill_md = match find_skill_md(&full_path) {
            Some(p) => p,
            None => continue,
        };

        let content = match fs::read_to_string(&skill_md) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let dir_name = name.to_string_lossy().to_string();
        let meta = parse_skill_metadata(&content, &dir_name, "");
        if meta.name.is_empty() && meta.description.is_empty() {
            continue;
        }

        let ineligible_reason = meta.ineligible_reason();
        let source = app_marker_source(&full_path).unwrap_or_else(|| def.source.clone());
        entries.push(SkillEntry {
            name: meta.name.clone(),
            description: meta.description.clone(),
            version: meta.version.clone(),
            source,
            dir: full_path,
            file_path: skill_md,
            eligible: ineligible_reason.is_none(),
            ineligible_reason,
            metadata: meta,
        });
    }

    entries
}

/// Scan all sources, deduplicate by name (later sources override earlier ones),
/// and return sorted results.
pub fn load_all_local_skills(config: &Config) -> Vec<SkillEntry> {
    let sources = get_source_defs(config);
    let mut map: HashMap<String, SkillEntry> = HashMap::new();
    for def in &sources {
        for entry in scan_source(def) {
            map.insert(entry.name.clone(), entry);
        }
    }
    let mut entries: Vec<SkillEntry> = map.into_values().filter(gate).collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Load-time gate (OpenClaw-style): drop skills that fail their `os` /
/// `requires` checks, logging the reason so the omission isn't silent.
fn gate(entry: &SkillEntry) -> bool {
    if let Some(reason) = &entry.ineligible_reason {
        tracing::info!(
            "[skills] skipping ineligible skill '{}': {}",
            entry.name,
            reason
        );
        return false;
    }
    true
}

/// Scan all sources including marketplace, deduplicate by name (later sources override earlier ones),
/// and return sorted results.
pub fn load_all_skills_with_marketplace(
    config: &Config,
    marketplace_sources: Vec<MarketplaceSourceDef>,
) -> Vec<SkillEntry> {
    let sources = get_source_defs_with_marketplace(config, marketplace_sources);
    let mut map: HashMap<String, SkillEntry> = HashMap::new();
    for def in &sources {
        for entry in scan_source(def) {
            map.insert(entry.name.clone(), entry);
        }
    }
    let mut entries: Vec<SkillEntry> = map.into_values().filter(gate).collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_parses_triggers_and_gating() {
        let tmp = std::env::temp_dir().join(format!("test-skills-trig-{}", uuid::Uuid::new_v4()));
        let skill_dir = tmp.join("weather");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: weather\ndescription: d\ntriggers: [weather, forecast]\nmetadata:\n  openclaw:\n    requires:\n      bins: [this-bin-does-not-exist-xyz]\n---\n\n# Weather\n",
        )
        .unwrap();
        let def = SourceDef {
            dir: tmp.clone(),
            source: "test".to_string(),
        };
        let entries = scan_source(&def);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].metadata.triggers, vec!["weather", "forecast"]);
        // Missing required binary → ineligible.
        assert!(!entries[0].eligible);
        assert!(entries[0].ineligible_reason.is_some());
        fs::remove_dir_all(&tmp).ok();
    }

    /// The repo's `<project>/skills/` directory is the runtime fallback for
    /// `bundled_skills_dir` (see `config.rs`). This test scans it via the same
    /// code path the daemon uses and asserts every bundled skill is parsed
    /// successfully — frontmatter present, no parse errors, eligible to run.
    /// Anchored on the new `web-research` skill so adding it doesn't silently
    /// regress (e.g. broken YAML, missing `when-to-use`).
    #[test]
    fn bundled_skills_dir_contains_web_research_and_all_parse() {
        let bundled = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("skills");
        assert!(
            bundled.exists(),
            "project skills dir missing: {}",
            bundled.display()
        );
        let def = SourceDef {
            dir: bundled,
            source: "bundled".to_string(),
        };
        let entries = scan_source(&def);

        // Every shipped skill must have a non-empty name + description (catches
        // accidental commits of broken YAML frontmatter).
        for e in &entries {
            assert!(
                !e.name.is_empty(),
                "bundled skill with empty name at {}",
                e.file_path.display()
            );
            assert!(
                !e.description.is_empty(),
                "bundled skill '{}' missing description",
                e.name
            );
        }

        // The new web-research skill must be discovered.
        let web_research = entries
            .iter()
            .find(|e| e.name == "web-research")
            .unwrap_or_else(|| {
                panic!(
                    "web-research skill not found among {} bundled entries: {:?}",
                    entries.len(),
                    entries.iter().map(|e| &e.name).collect::<Vec<_>>()
                )
            });
        assert!(
            web_research.description.contains("research"),
            "web-research description should mention research: {:?}",
            web_research.description
        );
        assert!(
            web_research.eligible,
            "web-research should be eligible by default (reason: {:?})",
            web_research.ineligible_reason
        );

        // Sanity: agent-browser still present (web-research composes it).
        assert!(
            entries.iter().any(|e| e.name == "agent-browser"),
            "agent-browser missing — web-research composes it; missing this would break docs"
        );
    }

    #[test]
    fn test_scan_empty_dir() {
        let tmp = std::env::temp_dir().join(format!("empty-skills-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&tmp).unwrap();
        let def = SourceDef {
            dir: tmp.clone(),
            source: "test".to_string(),
        };
        let entries = scan_source(&def);
        assert!(entries.is_empty());
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_scan_source_with_skill() {
        let tmp = std::env::temp_dir().join(format!("test-skills-scan-{}", uuid::Uuid::new_v4()));
        let skill_dir = tmp.join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: Does things\n---\n\n# My Skill\n",
        )
        .unwrap();

        let def = SourceDef {
            dir: tmp.clone(),
            source: "test".to_string(),
        };
        let entries = scan_source(&def);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "my-skill");
        assert_eq!(entries[0].description, "Does things");
        assert_eq!(entries[0].source, "test");

        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_scan_skips_hidden() {
        let tmp = std::env::temp_dir().join(format!("test-skills-hidden-{}", uuid::Uuid::new_v4()));
        let hidden_dir = tmp.join(".hidden-skill");
        fs::create_dir_all(&hidden_dir).unwrap();
        fs::write(
            hidden_dir.join("SKILL.md"),
            "---\nname: hidden\ndescription: Hidden\n---\n\n# Hidden\n",
        )
        .unwrap();

        let def = SourceDef {
            dir: tmp.clone(),
            source: "test".to_string(),
        };
        let entries = scan_source(&def);
        assert!(entries.is_empty());

        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn test_scan_skips_files() {
        let tmp = std::env::temp_dir().join(format!("test-skills-files-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("not-a-skill.md"), "just a file").unwrap();

        let def = SourceDef {
            dir: tmp.clone(),
            source: "test".to_string(),
        };
        let entries = scan_source(&def);
        assert!(entries.is_empty());

        fs::remove_dir_all(&tmp).ok();
    }
}
