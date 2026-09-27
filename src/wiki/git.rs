//! Git operations for the wiki repository.

use std::process::Command;

use anyhow::{bail, Context, Result};

use super::manager::WikiManager;
use super::types::GitCommit;

impl WikiManager {
    /// Run a raw git command in the wiki directory.
    pub(crate) async fn git(&self, args: &[&str]) -> Result<()> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.wiki_dir)
            .output()
            .context("git command failed")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git {} failed: {}", args.join(" "), stderr);
        }
        Ok(())
    }

    /// Stage and commit changes.
    pub(crate) async fn git_commit(&self, message: &str, files: Option<&[&str]>) -> Result<()> {
        if let Some(files) = files {
            for f in files {
                let _ = Command::new("git")
                    .args(["add", "--", f])
                    .current_dir(&self.wiki_dir)
                    .output();
            }
        } else {
            let _ = Command::new("git")
                .args(["add", "-A"])
                .current_dir(&self.wiki_dir)
                .output();
        }

        let output = Command::new("git")
            .args(["commit", "-m", message])
            .current_dir(&self.wiki_dir)
            .output()?;

        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            let msg = stderr.to_string();
            if !msg.contains("nothing to commit") && !msg.contains("nothing added") {
                tracing::warn!(
                    "[WikiManager] git commit warning: {}",
                    &msg[..msg.len().min(200)]
                );
            }
        }
        Ok(())
    }

    /// Git log for a specific file.
    pub fn get_history(&self, rel_path: &str, limit: Option<usize>) -> Result<Vec<GitCommit>> {
        let limit = limit.unwrap_or(10);
        let safe_rel = rel_path.replace('"', "\\\"");
        let output = Command::new("git")
            .args([
                "log",
                "--pretty=format:%H|%ai|%s",
                &format!("-n{limit}"),
                "--",
                &safe_rel,
            ])
            .current_dir(&self.wiki_dir)
            .output();

        match output {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let commits: Vec<GitCommit> = stdout
                    .lines()
                    .filter(|l| !l.is_empty())
                    .filter_map(|line| {
                        let mut parts = line.splitn(3, '|');
                        Some(GitCommit {
                            hash: parts.next()?.to_string(),
                            date: parts.next()?.to_string(),
                            message: parts.next()?.to_string(),
                        })
                    })
                    .collect();
                Ok(commits)
            }
            _ => Ok(Vec::new()),
        }
    }
}
