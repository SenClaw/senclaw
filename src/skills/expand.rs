//! Expand a skills parent directory into individual enabled skill directories.
//! Mirrors `src-old/skills/expand.ts`.
//!
//! Skills disabled by name (matching SKILL.md frontmatter `name` field) are filtered out.
//! When no skills are disabled, returns the raw dir entry without reading individual files.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillLocate {
    Managed,
    User,
    Workspace,
    Project,
}

#[derive(Debug, Clone)]
pub struct ExpandedSkillDir {
    pub dir: PathBuf,
    pub locate: SkillLocate,
}

/// Expand a skills parent directory into enabled skill subdirectories.
/// If `disabled` is empty, returns the parent dir as a single entry.
pub fn expand_skills_dir(
    dir: &Path,
    locate: SkillLocate,
    disabled: &HashSet<String>,
) -> Vec<ExpandedSkillDir> {
    if disabled.is_empty() || !dir.exists() {
        return vec![ExpandedSkillDir {
            dir: dir.to_path_buf(),
            locate,
        }];
    }

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => {
            return vec![ExpandedSkillDir {
                dir: dir.to_path_buf(),
                locate,
            }];
        }
    };

    let mut result: Vec<ExpandedSkillDir> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        let full_path = entry.path();
        if !full_path.is_dir() {
            continue;
        }

        let skill_name = read_skill_name(&full_path).unwrap_or_else(|| name_str.to_string());
        if !disabled.contains(&skill_name) {
            result.push(ExpandedSkillDir {
                dir: full_path,
                locate: locate.clone(),
            });
        }
    }
    result
}

fn read_skill_name(dir: &Path) -> Option<String> {
    for fname in &["SKILL.md", "skill.md", "Skill.md"] {
        let md_path = dir.join(fname);
        if !md_path.exists() {
            continue;
        }
        let content = fs::read_to_string(&md_path).ok()?;
        if !content.starts_with("---") {
            return None;
        }
        let end = content[3..].find("\n---")?;
        let fm = &content[4..3 + end];
        for line in fm.lines() {
            if let Some(col) = line.find(':') {
                let key = line[..col].trim();
                if key == "name" {
                    let val = line[col + 1..]
                        .trim()
                        .trim_matches(|c| c == '"' || c == '\'');
                    return Some(val.to_string());
                }
            }
        }
        return None;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_disabled_returns_parent() {
        let tmp = std::env::temp_dir();
        let disabled = HashSet::new();
        let result = expand_skills_dir(&tmp, SkillLocate::Managed, &disabled);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].dir, tmp);
        assert_eq!(result[0].locate, SkillLocate::Managed);
    }

    #[test]
    fn test_nonexistent_dir_returns_parent() {
        let nonexistent = Path::new("/nonexistent/skills/dir");
        let mut disabled = HashSet::new();
        disabled.insert("test".to_string());
        let result = expand_skills_dir(nonexistent, SkillLocate::User, &disabled);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].dir, nonexistent);
    }

    #[test]
    fn test_read_skill_name() {
        let dir = std::env::temp_dir().join(format!("test-skill-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let md = dir.join("SKILL.md");
        fs::write(
            &md,
            "---\nname: test-skill\ndescription: A test\n---\n\n# Body\n",
        )
        .unwrap();

        let name = read_skill_name(&dir);
        assert_eq!(name, Some("test-skill".to_string()));

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_read_skill_name_no_frontmatter() {
        let dir = std::env::temp_dir().join(format!("test-skill-nofm-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "# No frontmatter\n").unwrap();

        let name = read_skill_name(&dir);
        assert!(name.is_none());

        fs::remove_dir_all(&dir).ok();
    }
}
