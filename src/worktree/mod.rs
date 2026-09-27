//! Git worktrees for isolated agent runs (vibe-kanban / Claude Code model).
//!
//! A task that runs `isolation: worktree` gets its own checkout of the
//! repository under `~/.senclaw/worktrees/<repo-hash>/<name>/` on a branch
//! `senclaw/<name>`, so two agents can edit the same file at once and a person
//! reviews the result as a branch diff before merging or opening a PR.
//! The user's own checkout is never modified until they choose **merge**.
//!
//! Everything shells out to `git` (and `gh` for pull requests). `gh` absent
//! is a clean error carrying the branch name, never a silent skip.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// Branch prefix for every worktree SenClaw creates.
pub const BRANCH_PREFIX: &str = "senclaw/";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeInfo {
    /// Absolute path of the worktree.
    pub path: String,
    /// `senclaw/<name>`.
    pub branch: String,
    /// The branch (or commit) the worktree was created from.
    pub base: String,
    /// Repository the worktree belongs to.
    pub repo: String,
    pub created_at: String,
    /// Who made it: `dispatch:<task-id>`, `kanban:<card-id>`, `task:<id>`.
    #[serde(default)]
    pub owner: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeDiff {
    pub base: String,
    pub branch: String,
    /// `git diff --stat` against the base, working tree included.
    pub stat: String,
    pub files: Vec<String>,
    pub diff: String,
    pub truncated: bool,
    /// Commits on the branch beyond the base.
    pub commits: usize,
    /// Uncommitted changes present in the worktree.
    pub dirty: bool,
}

const MAX_DIFF_BYTES: usize = 200 * 1024;

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .with_context(|| format!("spawning git {}", args.join(" ")))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

fn hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Where this repository's worktrees live.
pub fn root_for(repo: &Path) -> PathBuf {
    let base = std::env::var("SENCLAW_WORKTREES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".senclaw").join("worktrees"));
    base.join(hash(&repo.to_string_lossy()))
}

fn meta_path(path: &Path) -> PathBuf {
    path.parent()
        .unwrap_or(path)
        .join(format!(".{}.json", path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()))
}

/// A name safe for a branch and a directory.
pub fn sanitize_name(raw: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for c in raw.chars() {
        let ok = c.is_ascii_alphanumeric() || c == '_' || c == '.';
        if ok {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    let out = out.trim_matches(|c| c == '-' || c == '.').to_string();
    let out: String = out.chars().take(60).collect();
    if out.is_empty() {
        "task".into()
    } else {
        out
    }
}

/// Is `dir` inside a git repository (its top level is returned)?
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    let out = git(dir, &["rev-parse", "--show-toplevel"]).ok()?;
    let p = PathBuf::from(out.trim());
    if p.is_dir() {
        Some(p)
    } else {
        None
    }
}

/// The branch a repository's worktrees are based on: the checked-out branch,
/// or `HEAD` when detached.
pub fn base_branch(repo: &Path) -> Result<String> {
    let out = git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let b = out.trim().to_string();
    Ok(if b.is_empty() || b == "HEAD" {
        git(repo, &["rev-parse", "HEAD"])?.trim().to_string()
    } else {
        b
    })
}

/// Create (or reuse) the worktree `name` for `repo`. Reuse happens when the
/// branch already exists — a retried task lands in the same branch.
pub fn create(repo: &Path, name: &str, owner: &str) -> Result<WorktreeInfo> {
    let repo = repo_root(repo).ok_or_else(|| anyhow!("{} is not inside a git repository", repo.display()))?;
    let root = root_for(&repo);
    create_in(&root, &repo, name, owner)
}

/// [`create`] with an explicit worktree root (tests; the daemon uses
/// [`root_for`]). `repo` must already be a repository top level.
pub fn create_in(root: &Path, repo: &Path, name: &str, owner: &str) -> Result<WorktreeInfo> {
    let repo = repo_root(repo).ok_or_else(|| anyhow!("{} is not inside a git repository", repo.display()))?;
    let name = sanitize_name(name);
    let branch = format!("{BRANCH_PREFIX}{name}");
    std::fs::create_dir_all(&root)?;
    let path = root.join(&name);
    if path.join(".git").exists() {
        if let Some(info) = read_meta(&path) {
            return Ok(info);
        }
    }
    let base = base_branch(&repo)?;
    let branch_exists = git(&repo, &["rev-parse", "--verify", "-q", &format!("refs/heads/{branch}")]).is_ok();
    let path_s = path.to_string_lossy().to_string();
    if path.exists() {
        // A stale directory without a registered worktree: clear it so
        // `worktree add` does not refuse.
        let _ = git(&repo, &["worktree", "prune"]);
        if !path.join(".git").exists() {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
    if branch_exists {
        git(&repo, &["worktree", "add", &path_s, &branch])?;
    } else {
        git(&repo, &["worktree", "add", "-b", &branch, &path_s, "HEAD"])?;
    }
    let info = WorktreeInfo {
        path: path_s,
        branch,
        base,
        repo: repo.to_string_lossy().to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        owner: owner.to_string(),
    };
    write_meta(&info);
    Ok(info)
}

fn read_meta(path: &Path) -> Option<WorktreeInfo> {
    let raw = std::fs::read(meta_path(path)).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn write_meta(info: &WorktreeInfo) {
    if let Ok(json) = serde_json::to_vec_pretty(info) {
        let _ = std::fs::write(meta_path(Path::new(&info.path)), json);
    }
}

/// Every worktree SenClaw created for `repo` that still exists.
pub fn list(repo: &Path) -> Result<Vec<WorktreeInfo>> {
    let repo = repo_root(repo).ok_or_else(|| anyhow!("{} is not inside a git repository", repo.display()))?;
    list_in(&root_for(&repo))
}

/// [`list`] under an explicit root.
pub fn list_in(root: &Path) -> Result<Vec<WorktreeInfo>> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(&root) else { return Ok(out) };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() && p.join(".git").exists() {
            if let Some(info) = read_meta(&p) {
                out.push(info);
            }
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

pub fn info_for(path: &Path) -> Option<WorktreeInfo> {
    read_meta(path)
}

/// What the branch changed relative to its base, uncommitted work included.
pub fn diff(path: &Path) -> Result<WorktreeDiff> {
    let info = read_meta(path).ok_or_else(|| anyhow!("{} is not a SenClaw worktree", path.display()))?;
    let merge_base = git(path, &["merge-base", &info.base, "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| info.base.clone());
    let stat = git(path, &["diff", "--stat", &merge_base])?;
    let files: Vec<String> = git(path, &["diff", "--name-only", &merge_base])?
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let untracked: Vec<String> = git(path, &["ls-files", "--others", "--exclude-standard"])?
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let mut all_files = files;
    for u in untracked {
        if !all_files.contains(&u) {
            all_files.push(u);
        }
    }
    let full = git(path, &["diff", &merge_base])?;
    let (diff, truncated) = if full.len() > MAX_DIFF_BYTES {
        (crate::util::text::truncate_on_char_boundary(&full, MAX_DIFF_BYTES).to_string(), true)
    } else {
        (full, false)
    };
    let commits = git(path, &["rev-list", "--count", &format!("{merge_base}..HEAD")])?
        .trim()
        .parse()
        .unwrap_or(0);
    let dirty = !git(path, &["status", "--porcelain"])?.trim().is_empty();
    Ok(WorktreeDiff { base: info.base, branch: info.branch, stat: stat.trim_end().to_string(), files: all_files, diff, truncated, commits, dirty })
}

/// One-line summary for a task result: branch, files, insertions/deletions.
pub fn summary(path: &Path) -> String {
    match diff(path) {
        Ok(d) => {
            let last = d.stat.lines().last().unwrap_or("").trim().to_string();
            format!(
                "branch `{}` (base `{}`), {} file(s) changed{}{}",
                d.branch,
                d.base,
                d.files.len(),
                if last.is_empty() { String::new() } else { format!(": {last}") },
                if d.dirty { " — uncommitted" } else { "" }
            )
        }
        Err(e) => format!("worktree summary unavailable: {e}"),
    }
}

/// Stage and commit everything in the worktree. `None` when clean.
pub fn commit_all(path: &Path, message: &str) -> Result<Option<String>> {
    git(path, &["add", "-A", "--"])?;
    if git(path, &["diff", "--cached", "--quiet"]).is_ok() {
        return Ok(None);
    }
    git(path, &["-c", "user.name=SenClaw", "-c", "user.email=agent@senclaw.local", "commit", "-q", "--no-verify", "-m", message])?;
    Ok(Some(git(path, &["rev-parse", "HEAD"])?.trim().to_string()))
}

/// Merge the worktree's branch into the repository's checked-out branch.
/// Refuses when the user's checkout has uncommitted changes — a merge must
/// never be mixed into work they have not committed.
pub fn merge_into_base(path: &Path, message: Option<&str>) -> Result<String> {
    let info = read_meta(path).ok_or_else(|| anyhow!("{} is not a SenClaw worktree", path.display()))?;
    let repo = PathBuf::from(&info.repo);
    if let Some(sha) = commit_all(path, "senclaw: agent changes")? {
        tracing::info!(sha = %sha, "[Worktree] committed pending changes before merge");
    }
    if !git(&repo, &["status", "--porcelain"])?.trim().is_empty() {
        return Err(anyhow!("the repository checkout has uncommitted changes; commit or stash them before merging"));
    }
    let current = base_branch(&repo)?;
    let msg = message.map(str::to_string).unwrap_or_else(|| format!("Merge {} (SenClaw)", info.branch));
    git(&repo, &["merge", "--no-ff", "-m", &msg, &info.branch])
        .map_err(|e| anyhow!("{e}. Resolve conflicts in {} or discard the worktree.", repo.display()))?;
    Ok(format!("merged {} into {current}", info.branch))
}

/// Rebase the worktree branch onto the current base.
pub fn rebase_onto_base(path: &Path) -> Result<String> {
    let info = read_meta(path).ok_or_else(|| anyhow!("{} is not a SenClaw worktree", path.display()))?;
    commit_all(path, "senclaw: agent changes")?;
    match git(path, &["rebase", &info.base]) {
        Ok(_) => Ok(format!("rebased {} onto {}", info.branch, info.base)),
        Err(e) => {
            let _ = git(path, &["rebase", "--abort"]);
            Err(anyhow!("rebase conflicts; aborted: {e}"))
        }
    }
}

fn on_path(cmd: &str) -> bool {
    std::env::var("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
        .unwrap_or(false)
}

/// Push the branch and open a pull request with `gh`. Without `gh` the
/// error names the branch so a person can open the PR by hand.
pub fn create_pr(path: &Path, title: &str, body: &str) -> Result<String> {
    let info = read_meta(path).ok_or_else(|| anyhow!("{} is not a SenClaw worktree", path.display()))?;
    commit_all(path, "senclaw: agent changes")?;
    if !on_path("gh") {
        return Err(anyhow!(
            "`gh` is not installed; the branch is `{}` — push it and open the PR by hand, or install GitHub CLI",
            info.branch
        ));
    }
    git(path, &["push", "-u", "origin", &info.branch])?;
    let out = std::process::Command::new("gh")
        .args(["pr", "create", "--head", &info.branch, "--title", title, "--body", body])
        .current_dir(path)
        .output()
        .context("spawning gh")?;
    if !out.status.success() {
        return Err(anyhow!("gh pr create failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Remove the worktree; `delete_branch` also drops `senclaw/<name>`.
pub fn remove(path: &Path, delete_branch: bool) -> Result<()> {
    let info = read_meta(path).ok_or_else(|| anyhow!("{} is not a SenClaw worktree", path.display()))?;
    let repo = PathBuf::from(&info.repo);
    git(&repo, &["worktree", "remove", "--force", &info.path])?;
    let _ = std::fs::remove_file(meta_path(path));
    if delete_branch {
        let _ = git(&repo, &["branch", "-D", &info.branch]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]).unwrap();
        git(&repo, &["config", "user.email", "t@t"]).unwrap();
        git(&repo, &["config", "user.name", "t"]).unwrap();
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "."]).unwrap();
        git(&repo, &["commit", "-q", "-m", "init"]).unwrap();
        (tmp, repo)
    }

    #[test]
    fn create_diff_merge_remove() {
        let (tmp, repo) = init_repo();
        let root = tmp.path().join("wt");
        let info = create_in(&root, &repo, "Fix: Login Bug!", "dispatch:d-1").unwrap();
        assert_eq!(info.branch, "senclaw/fix-login-bug");
        assert_eq!(info.base, "main");
        assert!(Path::new(&info.path).join("a.txt").exists());
        // Same name → same worktree, not an error.
        let again = create_in(&root, &repo, "fix-login-bug", "x").unwrap();
        assert_eq!(again.path, info.path);

        let wt = Path::new(&info.path);
        std::fs::write(wt.join("a.txt"), "two\n").unwrap();
        std::fs::write(wt.join("b.txt"), "new\n").unwrap();
        let d = diff(wt).unwrap();
        assert!(d.dirty);
        assert!(d.files.contains(&"a.txt".to_string()));
        assert!(d.files.contains(&"b.txt".to_string()), "untracked files count as changes");
        assert!(d.diff.contains("+two"));
        assert!(summary(wt).contains("senclaw/fix-login-bug"));

        // The user's checkout is untouched until merge.
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "one\n");
        let msg = merge_into_base(wt, None).unwrap();
        assert!(msg.contains("merged"));
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "two\n");
        assert!(repo.join("b.txt").exists());

        assert_eq!(list_in(&root).unwrap().len(), 1);
        remove(wt, true).unwrap();
        assert!(!wt.exists());
        assert!(list_in(&root).unwrap().is_empty());
        assert!(git(&repo, &["rev-parse", "--verify", "-q", "refs/heads/senclaw/fix-login-bug"]).is_err());
    }

    #[test]
    fn merge_refuses_a_dirty_checkout_and_non_repo_is_an_error() {
        let (tmp, repo) = init_repo();
        let info = create_in(&tmp.path().join("wt"), &repo, "t2", "test").unwrap();
        std::fs::write(Path::new(&info.path).join("a.txt"), "x\n").unwrap();
        std::fs::write(repo.join("a.txt"), "user edit\n").unwrap();
        let err = merge_into_base(Path::new(&info.path), None).unwrap_err();
        assert!(err.to_string().contains("uncommitted"));
        assert!(create(tmp.path(), "nope", "test").is_err(), "not a repository");
        assert_eq!(sanitize_name("  Hello  World/Thing"), "hello-world-thing");
        assert_eq!(sanitize_name("///"), "task");
    }
}
