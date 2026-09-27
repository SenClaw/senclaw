//! Wiki manager — personal knowledge base backed by git.
//! Mirrors `src-old/wiki/WikiManager.ts`.
//!
//! Core struct + lifecycle operations. Search, stats, tree, frontmatter,
//! and git helpers live in sibling modules.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Result};

use super::types::{Frontmatter, WikiDoc};

// ===== WikiManager =====

pub struct WikiManager {
    pub(crate) wiki_dir: PathBuf,
    pub(crate) excluded: HashSet<String>,
}

impl WikiManager {
    pub fn new(wiki_dir: PathBuf) -> Self {
        let mut excluded = HashSet::new();
        excluded.insert(".git".to_string());
        excluded.insert("node_modules".to_string());
        excluded.insert(".DS_Store".to_string());
        Self { wiki_dir, excluded }
    }

    /// Initialize git repo + base directory structure on first use.
    pub async fn ensure_init(&self) -> Result<()> {
        let git_dir = self.wiki_dir.join(".git");
        if git_dir.exists() {
            return Ok(());
        }

        fs::create_dir_all(self.wiki_dir.join("inbox"))?;

        self.git(&["init"]).await?;
        self.git(&["config", "user.name", "senclaw"]).await?;
        self.git(&["config", "user.email", "senclaw@local"]).await?;

        fs::write(self.wiki_dir.join(".gitignore"), ".DS_Store\n*.swp\n")?;

        let readme = concat!(
            "# Wiki\n",
            "\n",
            "Personal knowledge base maintained by SenClaw.\n",
            "\n",
            "## Directory structure\n",
            "\n",
            "- `inbox/` — Agent staging area; put files here when category is unclear\n",
            "\n",
            "## Remote backup (optional)\n",
            "\n",
            "```bash\n",
            "cd ~/senclaw/wiki\n",
            "git remote add origin git@github.com:user/my-wiki.git\n",
            "git push -u origin main\n",
            "```\n",
        );
        fs::write(self.wiki_dir.join("README.md"), readme)?;

        self.git(&["add", "-A"]).await?;
        self.git(&["commit", "-m", "wiki: initial commit"]).await?;

        tracing::info!("[WikiManager] Initialized wiki at {:?}", self.wiki_dir);
        Ok(())
    }

    /// Read document: content + frontmatter + git history.
    pub fn read_file(&self, rel_path: &str) -> Result<WikiDoc> {
        let abs_path = self.safe_path(rel_path)?;
        let content = fs::read_to_string(&abs_path)?;
        let fm = Self::parse_frontmatter(&content).0;
        let git_log = self.get_history(rel_path, Some(10))?;
        Ok(WikiDoc {
            path: rel_path.to_string(),
            content,
            frontmatter: fm,
            git_log,
        })
    }

    /// Write document (create or update).
    /// Auto-injects/updates frontmatter and auto git-commits.
    pub async fn write_file(
        &self,
        rel_path: &str,
        content: &str,
        source: Option<&str>,
        tags: Option<&[String]>,
        commit_msg: Option<&str>,
    ) -> Result<()> {
        let abs_path = self.safe_path(rel_path)?;
        let is_new = !abs_path.exists();
        let now = chrono::Utc::now().to_rfc3339();

        let (existing_fm, _) = Self::parse_frontmatter(content);
        let use_existing = !existing_fm.created.is_empty();
        let fm = Frontmatter {
            created: if is_new {
                now.clone()
            } else if use_existing {
                existing_fm.created.clone()
            } else {
                now.clone()
            },
            updated: now,
            tags: tags
                .map(|t| t.to_vec())
                .or_else(|| {
                    if existing_fm.tags.is_empty() {
                        None
                    } else {
                        Some(existing_fm.tags.clone())
                    }
                })
                .unwrap_or_default(),
            source: source.map(|s| s.to_string()).unwrap_or_else(|| {
                if existing_fm.source.is_empty() {
                    "manual".to_string()
                } else {
                    existing_fm.source
                }
            }),
        };

        let final_content = Self::inject_frontmatter(content, &fm);
        if let Some(parent) = abs_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&abs_path, &final_content)?;

        let action = if is_new { "add" } else { "edit" };
        let default_msg = format!("wiki: {action} {rel_path}");
        let cmsg = commit_msg.unwrap_or(&default_msg);
        self.git_commit(cmsg, Some(&[rel_path])).await?;
        Ok(())
    }

    /// Create a directory (with .gitkeep so git tracks it).
    pub async fn mkdir(&self, rel_path: &str) -> Result<()> {
        let abs_path = self.safe_path(rel_path)?;
        fs::create_dir_all(&abs_path)?;
        let keep_file = abs_path.join(".gitkeep");
        if !keep_file.exists() {
            fs::write(&keep_file, "")?;
            self.git_commit(
                &format!("wiki: mkdir {rel_path}"),
                Some(&[&format!("{rel_path}/.gitkeep")]),
            )
            .await?;
        }
        Ok(())
    }

    /// Delete an empty directory (fails if not empty).
    pub async fn delete_empty_dir(&self, rel_path: &str) -> Result<()> {
        let abs_path = self.safe_path(rel_path)?;
        let entries: Vec<_> = fs::read_dir(&abs_path)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != ".gitkeep")
            .collect();
        if !entries.is_empty() {
            bail!("Directory not empty: {rel_path}");
        }
        fs::remove_dir_all(&abs_path)?;
        self.git_commit(&format!("wiki: rmdir {rel_path}"), None)
            .await?;
        Ok(())
    }

    /// Delete a single wiki file and commit the removal.
    pub async fn delete_file(&self, rel_path: &str) -> Result<()> {
        let abs_path = self.safe_path(rel_path)?;
        if !abs_path.is_file() {
            bail!("Not a file: {rel_path}");
        }
        fs::remove_file(&abs_path)?;
        self.git_commit(&format!("wiki: delete {rel_path}"), None)
            .await?;
        Ok(())
    }

    /// Resolve a relative path, rejecting traversal attempts.
    fn safe_path(&self, rel_path: &str) -> Result<PathBuf> {
        if rel_path.contains("..") {
            bail!("Path traversal detected: {rel_path}");
        }
        let abs = self.wiki_dir.join(rel_path);
        if abs.exists() || self.wiki_dir.exists() {
            if let (Ok(wiki_canon), Ok(abs_canon)) =
                (self.wiki_dir.canonicalize(), abs.canonicalize())
            {
                if !abs_canon.starts_with(&wiki_canon) {
                    bail!("Path traversal detected: {rel_path}");
                }
            }
        }
        Ok(abs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn make_manager(tmp: &Path) -> WikiManager {
        WikiManager::new(tmp.join("wiki"))
    }

    #[test]
    fn test_safe_path_traversal_blocked() {
        let tmp = std::env::temp_dir().join(format!("wiki-test-{}", uuid::Uuid::new_v4()));
        let mgr = make_manager(&tmp);
        fs::create_dir_all(&mgr.wiki_dir).unwrap();

        assert!(mgr.safe_path("subdir/file.md").is_ok());
        assert!(mgr.safe_path("../outside.md").is_err());

        fs::remove_dir_all(&tmp).ok();
    }
}
