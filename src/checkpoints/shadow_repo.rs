//! A *shadow* git repository: history of the chat's working directory kept
//! outside that directory.
//!
//! The user's own `.git` is never touched. The shadow repo lives under
//! `~/.senclaw/checkpoints/<chat>/` and uses the working directory only as
//! its work tree (`git --git-dir=<shadow> --work-tree=<dir>`), which is the
//! Cline/Roo "checkpoints" model: every tool that writes files leaves a commit
//! here, so a person can diff and restore per step without the agent's edits
//! ever appearing in the project's real history.
//!
//! Shelling out to `git` rather than linking libgit2 is deliberate: the same
//! binary is what phase 5 (worktrees) and `gh` need anyway, and `git add -A`
//! honours the project's `.gitignore` exactly the way the user expects.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use tokio::process::Command;

/// Files above this size are dropped from a checkpoint and excluded from
/// later ones. A checkpoint exists to undo an agent's edit; a 200 MB model
/// weight in the tree would make every step cost seconds and disk.
pub const MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// Patterns excluded in addition to the project's own `.gitignore`. These are
/// the directories that make `git add -A` crawl for minutes and that no one
/// wants restored by a checkpoint.
const DEFAULT_EXCLUDES: &str = "\
# Written by SenClaw checkpoints — build output and dependency trees.
node_modules/
target/
.venv/
venv/
__pycache__/
.dart_tool/
build/
dist/
.next/
.cache/
*.log
.DS_Store
";

#[derive(Debug, Clone)]
pub struct ShadowRepo {
    git_dir: PathBuf,
    work_tree: PathBuf,
}

/// One entry of `git diff --name-status`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChangedFile {
    /// `A` added, `M` modified, `D` deleted, `R` renamed.
    pub status: String,
    pub path: String,
}

/// What a restore actually did, so the UI can say more than "done".
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct RestoreReport {
    pub restored: Vec<String>,
    pub removed: Vec<String>,
}

impl ShadowRepo {
    pub fn new(git_dir: PathBuf, work_tree: PathBuf) -> Self {
        Self { git_dir, work_tree }
    }

    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    pub fn work_tree(&self) -> &Path {
        &self.work_tree
    }

    async fn git(&self, args: &[&str]) -> Result<std::process::Output> {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.work_tree)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.work_tree)
            .output()
            .await
            .with_context(|| format!("spawning git {}", args.join(" ")))?;
        Ok(out)
    }

    async fn git_ok(&self, args: &[&str]) -> Result<String> {
        let out = self.git(args).await?;
        if !out.status.success() {
            return Err(anyhow!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    pub fn is_initialized(&self) -> bool {
        self.git_dir.join("HEAD").exists()
    }

    /// Create the shadow repo if needed and make sure it has a baseline
    /// commit. Idempotent and cheap once initialized.
    pub async fn ensure_init(&self) -> Result<()> {
        self.ensure_init_with(false).await
    }

    /// As [`Self::ensure_init`], but `after_write` says the tree has *already*
    /// been modified by the call that triggered this.
    ///
    /// The baseline must predate the agent's edits. When it is taken after a
    /// write there is no such tree left to photograph, so the baseline is an
    /// **empty commit** and the edit becomes a diff against nothing — large,
    /// but complete. Committing the modified tree as the baseline instead is
    /// what silently lost the first turn of every new project.
    pub async fn ensure_init_with(&self, after_write: bool) -> Result<()> {
        if !self.is_initialized() {
            std::fs::create_dir_all(&self.git_dir)
                .with_context(|| format!("creating {}", self.git_dir.display()))?;
            self.git_ok(&["init", "-q"]).await?;
            self.git_ok(&["config", "user.name", "SenClaw checkpoints"])
                .await?;
            self.git_ok(&["config", "user.email", "checkpoints@senclaw.local"])
                .await?;
            self.git_ok(&["config", "core.autocrlf", "false"]).await?;
            // Never sign: a signing key prompt would hang the tool loop.
            self.git_ok(&["config", "commit.gpgsign", "false"]).await?;
            let info = self.git_dir.join("info");
            std::fs::create_dir_all(&info)?;
            std::fs::write(info.join("exclude"), DEFAULT_EXCLUDES)?;
        }
        if self.head().await?.is_none() {
            // The baseline is the tree as it stood before the agent touched
            // it. `CheckpointService::prepare` calls this when the working
            // directory becomes known, which is before the turn runs. When
            // that did not happen and the triggering tool already wrote, an
            // empty baseline is the only honest one — see `after_write`.
            if after_write {
                self.commit_empty("baseline").await?;
            } else {
                self.commit_inner("baseline", true).await?;
            }
        }
        Ok(())
    }

    pub async fn head(&self) -> Result<Option<String>> {
        let out = self.git(&["rev-parse", "--verify", "-q", "HEAD"]).await?;
        if out.status.success() {
            Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_string()))
        } else {
            Ok(None)
        }
    }

    /// Stage everything, drop oversized files, commit. `None` when nothing
    /// changed since the previous checkpoint.
    pub async fn commit(&self, message: &str) -> Result<Option<String>> {
        self.commit_inner(message, false).await
    }

    /// An empty root commit: no files staged at all.
    ///
    /// Distinct from `commit_inner(msg, true)`, which stages the tree first
    /// and would therefore record the very edit the baseline must predate.
    async fn commit_empty(&self, message: &str) -> Result<Option<String>> {
        self.git_ok(&["commit", "-q", "--no-verify", "--allow-empty", "-m", message])
            .await?;
        Ok(self.head().await?)
    }

    async fn commit_inner(&self, message: &str, allow_empty: bool) -> Result<Option<String>> {
        self.git_ok(&["add", "-A", "--"]).await?;
        self.drop_oversized().await?;
        let staged = self.git(&["diff", "--cached", "--quiet"]).await?;
        let has_changes = !staged.status.success();
        if !has_changes && !allow_empty {
            return Ok(None);
        }
        let mut args = vec!["commit", "-q", "--no-verify", "-m", message];
        if allow_empty {
            args.push("--allow-empty");
        }
        self.git_ok(&args).await?;
        Ok(self.head().await?)
    }

    /// Unstage files larger than [`MAX_FILE_BYTES`] and exclude them from
    /// future checkpoints.
    async fn drop_oversized(&self) -> Result<()> {
        let listed = self.git_ok(&["diff", "--cached", "--name-only", "-z"]).await?;
        let mut excluded = Vec::new();
        for rel in listed.split('\0').filter(|s| !s.is_empty()) {
            let abs = self.work_tree.join(rel);
            let Ok(meta) = std::fs::metadata(&abs) else { continue };
            if meta.is_file() && meta.len() > MAX_FILE_BYTES {
                self.git_ok(&["rm", "--cached", "-q", "--", rel]).await?;
                excluded.push(rel.to_string());
            }
        }
        if !excluded.is_empty() {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(self.git_dir.join("info").join("exclude"))?;
            for rel in &excluded {
                writeln!(f, "/{rel}")?;
            }
            tracing::info!(
                count = excluded.len(),
                "[Checkpoints] excluded oversized file(s) from the shadow repo"
            );
        }
        Ok(())
    }

    /// `git diff --name-status from..to`.
    pub async fn changed_files(&self, from: &str, to: &str) -> Result<Vec<ChangedFile>> {
        let out = self
            .git_ok(&["diff", "--name-status", "-M", from, to, "--"])
            .await?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let mut parts = l.split('\t');
                let status = parts.next()?.trim();
                // Renames come as `R100\told\tnew`; report the new path.
                let path = parts.last()?.trim();
                Some(ChangedFile {
                    status: status.chars().next()?.to_string(),
                    path: path.to_string(),
                })
            })
            .collect())
    }

    /// Unified diff between two checkpoints, truncated to `max_bytes`.
    /// The bool says whether truncation happened.
    pub async fn diff(&self, from: &str, to: &str, max_bytes: usize) -> Result<(String, bool)> {
        let out = self.git_ok(&["diff", "-M", from, to, "--"]).await?;
        if out.len() > max_bytes {
            let cut = crate::util::text::truncate_on_char_boundary(&out, max_bytes).to_string();
            Ok((cut, true))
        } else {
            Ok((out, false))
        }
    }

    /// Every path recorded at `sha`.
    pub async fn files_at(&self, sha: &str) -> Result<Vec<String>> {
        let known = self.git_ok(&["ls-tree", "-r", "--name-only", sha]).await?;
        Ok(known
            .lines()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect())
    }

    /// Put the work tree back to `sha`. With `files` empty the whole tree is
    /// restored: every path known at `sha` is checked out, and files that were
    /// *added* between `sha` and the latest checkpoint are removed. Files the
    /// agent created after the latest checkpoint are untracked and left alone —
    /// a restore must never delete something no checkpoint ever recorded.
    pub async fn restore(&self, sha: &str, files: &[String]) -> Result<RestoreReport> {
        let mut report = RestoreReport::default();
        let head = self
            .head()
            .await?
            .ok_or_else(|| anyhow!("shadow repo has no checkpoints"))?;
        if files.is_empty() {
            self.git_ok(&["checkout", "-q", sha, "--", "."]).await?;
            let added = self
                .git_ok(&["diff", "--name-only", "--diff-filter=A", sha, &head, "--"])
                .await?;
            for rel in added.lines().map(str::trim).filter(|s| !s.is_empty()) {
                let abs = self.work_tree.join(rel);
                if abs.is_file() {
                    std::fs::remove_file(&abs)
                        .with_context(|| format!("removing {}", abs.display()))?;
                    report.removed.push(rel.to_string());
                }
            }
            report.restored = self.files_at(sha).await?;
            return Ok(report);
        }
        for rel in files {
            let rel = rel.trim_start_matches("./");
            if rel.is_empty() || rel.starts_with('/') || rel.split('/').any(|c| c == "..") {
                return Err(anyhow!("invalid path {rel:?}"));
            }
            // Did the file exist at `sha`? If not, restoring it means removing it.
            let exists = self
                .git(&["cat-file", "-e", &format!("{sha}:{rel}")])
                .await?
                .status
                .success();
            if exists {
                self.git_ok(&["checkout", "-q", sha, "--", rel]).await?;
                report.restored.push(rel.to_string());
            } else {
                let abs = self.work_tree.join(rel);
                if abs.is_file() {
                    std::fs::remove_file(&abs)
                        .with_context(|| format!("removing {}", abs.display()))?;
                    report.removed.push(rel.to_string());
                }
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn commit_restore_round_trip() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("a.txt"), "one\n").unwrap();
        let repo = ShadowRepo::new(tmp.path().join("shadow"), work.clone());
        repo.ensure_init().await.unwrap();
        let base = repo.head().await.unwrap().unwrap();

        // Edit + create → one checkpoint with two changed files.
        std::fs::write(work.join("a.txt"), "two\n").unwrap();
        std::fs::write(work.join("b.txt"), "new\n").unwrap();
        let sha = repo.commit("Edit a.txt").await.unwrap().unwrap();
        assert_ne!(sha, base);
        let changed = repo.changed_files(&base, &sha).await.unwrap();
        assert_eq!(changed.len(), 2);
        // Nothing changed → no new commit.
        assert!(repo.commit("noop").await.unwrap().is_none());

        // Whole-tree restore to baseline: a.txt back, b.txt gone.
        let rep = repo.restore(&base, &[]).await.unwrap();
        assert_eq!(std::fs::read_to_string(work.join("a.txt")).unwrap(), "one\n");
        assert!(!work.join("b.txt").exists());
        assert_eq!(rep.removed, vec!["b.txt".to_string()]);

        // Single-file restore forward to `sha`.
        let rep = repo.restore(&sha, &["b.txt".into()]).await.unwrap();
        assert_eq!(std::fs::read_to_string(work.join("b.txt")).unwrap(), "new\n");
        assert_eq!(rep.restored, vec!["b.txt".to_string()]);
        assert_eq!(std::fs::read_to_string(work.join("a.txt")).unwrap(), "one\n");
    }

    #[tokio::test]
    async fn oversized_files_are_excluded_and_gitignore_honoured() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(work.join("node_modules/x")).unwrap();
        std::fs::write(work.join("node_modules/x/i.js"), "x").unwrap();
        std::fs::write(work.join(".gitignore"), "secret.env\n").unwrap();
        std::fs::write(work.join("secret.env"), "k=v").unwrap();
        std::fs::write(work.join("big.bin"), vec![0u8; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        std::fs::write(work.join("ok.txt"), "ok").unwrap();
        let repo = ShadowRepo::new(tmp.path().join("shadow"), work.clone());
        repo.ensure_init().await.unwrap();
        let base = repo.head().await.unwrap().unwrap();
        let known = repo.files_at(&base).await.unwrap();
        assert!(known.iter().any(|f| f == "ok.txt"));
        assert!(known.iter().any(|f| f == ".gitignore"));
        assert!(!known.iter().any(|f| f == "secret.env"), "user .gitignore must be honoured");
        assert!(!known.iter().any(|f| f.starts_with("node_modules")), "default excludes must apply");
        assert!(!known.iter().any(|f| f == "big.bin"), "oversized file must be dropped");
        let excl = std::fs::read_to_string(repo.git_dir().join("info/exclude")).unwrap();
        assert!(excl.contains("/big.bin"));
    }

    #[tokio::test]
    async fn restore_rejects_escaping_paths() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let repo = ShadowRepo::new(tmp.path().join("s"), work);
        repo.ensure_init().await.unwrap();
        let head = repo.head().await.unwrap().unwrap();
        assert!(repo.restore(&head, &["../x".into()]).await.is_err());
        assert!(repo.restore(&head, &["/etc/passwd".into()]).await.is_err());
    }
}
