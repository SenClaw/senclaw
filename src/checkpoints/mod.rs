//! Checkpoints — a commit in a shadow git repo after every tool that writes
//! into the chat's working directory, so a person can see and undo what an
//! agent did step by step.
//!
//! Model (from Cline / Roo Code): the project's own `.git` is never touched;
//! history lives in `~/.senclaw/checkpoints/<chat>/` with the working
//! directory as work tree ([`shadow_repo::ShadowRepo`]). Rows in
//! `chat_checkpoints` map each commit to the tool call that produced it.
//!
//! Policy, in one place ([`CheckpointService::on_tool_event`]):
//!
//! - Only chats with a working directory that is a git repository, or chats
//!   of `group_type = "code"`, are checkpointed by default. A chat whose
//!   working directory is `$HOME` must never be crawled by `git add -A`.
//! - A person can switch it off per chat (`router_state`
//!   `checkpoints:off:<jid>`); the service is otherwise on.
//! - `Edit` / `Write` / `NotebookEdit` always checkpoint. `Bash` checkpoints
//!   unless the command is classified read-only; a checkpoint with no
//!   changes is skipped at the git level, so a wrong guess costs a `git
//!   status`, never a bogus row.
//! - Any tool event on an eligible chat initializes the shadow repo, so the
//!   baseline is taken at the first `Read`/`Grep` — before the first edit.

pub mod shadow_repo;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};

use crate::db::Db;
use crate::types::ChatCheckpoint;
pub use shadow_repo::{ChangedFile, RestoreReport, ShadowRepo};

/// Largest unified diff returned over the API. Larger ones are truncated and
/// flagged; the caller can still ask per file.
pub const MAX_DIFF_BYTES: usize = 200 * 1024;

/// Tools whose completion always produces a checkpoint.
const WRITE_TOOLS: &[&str] = &["Edit", "Write", "NotebookEdit"];

/// Decide from a completed tool call whether it may have written files.
/// Errors never checkpoint (nothing changed). `Bash` is judged by the
/// read-only classifier on the command; a command long enough to have been
/// truncated in `title` is treated as writing — better one no-op `git status`
/// than a missed checkpoint.
pub fn tool_may_write(tool_name: &str, content: &serde_json::Value, ok: bool) -> bool {
    if !ok {
        return false;
    }
    if content.get("error").and_then(|v| v.as_bool()) == Some(true) {
        return false;
    }
    if WRITE_TOOLS.contains(&tool_name) {
        return true;
    }
    if tool_name == "Bash" {
        let cmd = content
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if cmd.ends_with("...") {
            return true;
        }
        return !crate::util::shell_safety::is_readonly_safe_command(cmd);
    }
    false
}

/// Directories that must never become a shadow work tree, whatever the chat
/// says: crawling them is either enormous or meaningless.
fn is_forbidden_root(dir: &Path) -> bool {
    if dir == Path::new("/") {
        return true;
    }
    if let Some(home) = dirs::home_dir() {
        if dir == home {
            return true;
        }
    }
    false
}

/// Short, filesystem-safe name for a chat's shadow directory.
fn shadow_dir_name(jid: &str) -> String {
    let safe: String = jid
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    // Two jids can collide after sanitizing (`a:b` and `a_b`); a short hash
    // keeps them apart without making the directory unreadable.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in jid.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{safe}-{:08x}", (h & 0xffff_ffff) as u32)
}

pub struct CheckpointService {
    db: Arc<Db>,
    /// `~/.senclaw/checkpoints`
    root: PathBuf,
    /// Chats whose shadow repo has been initialized in this process — saves a
    /// filesystem probe per tool event.
    ready: Mutex<HashSet<String>>,
    /// One async lock per chat so two tool events never run git concurrently
    /// on the same shadow repo.
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl CheckpointService {
    pub fn new(db: Arc<Db>, senclaw_home: &Path) -> Self {
        Self {
            db,
            root: senclaw_home.join("checkpoints"),
            ready: Mutex::new(HashSet::new()),
            locks: Mutex::new(HashMap::new()),
        }
    }

    fn lock_for(&self, jid: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(jid.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn repo_for(&self, jid: &str, work_tree: &Path) -> ShadowRepo {
        ShadowRepo::new(self.root.join(shadow_dir_name(jid)), work_tree.to_path_buf())
    }

    // ----- policy -------------------------------------------------------

    fn off_key(jid: &str) -> String {
        format!("checkpoints:off:{jid}")
    }

    pub fn is_enabled(&self, jid: &str) -> bool {
        !matches!(
            self.db.get_router_state(&Self::off_key(jid)),
            Ok(Some(v)) if v == "1"
        )
    }

    pub fn set_enabled(&self, jid: &str, enabled: bool) -> Result<()> {
        if enabled {
            self.db.delete_router_state(&Self::off_key(jid))
        } else {
            self.db.set_router_state(&Self::off_key(jid), "1")
        }
    }

    /// Is this working directory one we will checkpoint for this chat?
    /// `group_type == "code"` opts a non-git folder in; everything else needs
    /// a `.git` to prove it is a project and not somebody's home folder.
    pub fn eligible_dir(&self, jid: &str, working_dir: &str, group_type: &str) -> Option<PathBuf> {
        if working_dir.is_empty() || !self.is_enabled(jid) {
            return None;
        }
        let dir = crate::util::paths::expand_tilde(working_dir);
        if !dir.is_absolute() || !dir.is_dir() || is_forbidden_root(&dir) {
            return None;
        }
        if group_type == "code" || dir.join(".git").exists() {
            Some(dir)
        } else {
            None
        }
    }

    // ----- recording ----------------------------------------------------

    /// Take the baseline for a chat **before** its agent runs anything.
    ///
    /// The baseline has to be the tree as it was before the first edit, and
    /// the tool hook cannot produce that: it fires *after* the tool. On a
    /// repository that already has files the difference is usually invisible,
    /// because a Read or a Grep precedes the first Edit and the baseline
    /// lands on an untouched tree. On a **new, empty project** there is
    /// nothing to read first — the agent's very first tool is a Write, the
    /// baseline is taken after it, and the file it just created is committed
    /// *as* the baseline. Nothing is left to diff, so the first edits of
    /// every new project were silently unrecoverable.
    ///
    /// Called when a chat's working directory becomes known, which is before
    /// the turn starts. Cheap and idempotent: an initialized repo returns at
    /// the `is_initialized` check.
    pub async fn prepare(&self, jid: &str, working_dir: &str, group_type: &str) {
        let Some(dir) = self.eligible_dir(jid, working_dir, group_type) else {
            return;
        };
        if self.ready.lock().unwrap().contains(jid) {
            return;
        }
        let repo = self.repo_for(jid, &dir);
        let guard = self.lock_for(jid);
        let _held = guard.lock().await;
        match repo.ensure_init().await {
            Ok(()) => {
                self.ready.lock().unwrap().insert(jid.to_string());
            }
            // A missing baseline costs history, never correctness — the edit
            // itself is unaffected, and the tool hook will try again.
            Err(e) => tracing::warn!(
                error = %e,
                jid = %jid,
                "[Checkpoints] baseline not taken; the first edits may not be recoverable"
            ),
        }
    }

    /// Called for every completed tool call. Initializes the shadow repo on
    /// the first call for a chat (baseline), and commits when the tool may
    /// have written. Returns the new checkpoint, if one was made.
    pub async fn on_tool_event(
        &self,
        jid: &str,
        working_dir: Option<&str>,
        group_type: &str,
        tool_name: &str,
        content: &serde_json::Value,
        ok: bool,
        summary: &str,
    ) -> Result<Option<ChatCheckpoint>> {
        let Some(dir) = working_dir.and_then(|d| self.eligible_dir(jid, d, group_type)) else {
            return Ok(None);
        };
        let repo = self.repo_for(jid, &dir);
        let guard = self.lock_for(jid);
        let _held = guard.lock().await;

        // Whether this very call already changed the tree decides what a
        // first-time baseline may contain: a tree photographed after a write
        // has the edit in it, and committing that as the baseline is what
        // lost the first turn of every new project.
        let wrote = tool_may_write(tool_name, content, ok);
        let first_time = !self.ready.lock().unwrap().contains(jid);
        if first_time || !repo.is_initialized() {
            repo.ensure_init_with(wrote).await?;
            self.ready.lock().unwrap().insert(jid.to_string());
        }
        if !wrote {
            return Ok(None);
        }
        let parent = repo.head().await?;
        let message = format!("{tool_name}: {}", one_line(summary, 120));
        let Some(sha) = repo.commit(&message).await? else {
            return Ok(None);
        };
        let files_changed = match &parent {
            Some(p) => repo.count_changed_from(p, &sha).await.unwrap_or(0),
            None => 0,
        };
        let cp = self.db.insert_checkpoint(
            jid,
            &sha,
            parent.as_deref(),
            tool_name,
            &one_line(summary, 200),
            &dir.to_string_lossy(),
            files_changed as i64,
        )?;
        Ok(Some(cp))
    }

    // ----- queries ------------------------------------------------------

    pub fn list(&self, jid: &str) -> Result<Vec<ChatCheckpoint>> {
        self.db.list_checkpoints(jid)
    }

    fn get_owned(&self, jid: &str, id: i64) -> Result<ChatCheckpoint> {
        self.db
            .get_checkpoint(id)?
            .filter(|c| c.chat_jid == jid)
            .ok_or_else(|| anyhow!("checkpoint {id} not found for this chat"))
    }

    /// Diff of checkpoint `id` against `from` (another checkpoint id) or,
    /// by default, its parent commit.
    pub async fn diff(
        &self,
        jid: &str,
        id: i64,
        from: Option<i64>,
    ) -> Result<(ChatCheckpoint, String, Vec<ChangedFile>, String, bool)> {
        let cp = self.get_owned(jid, id)?;
        let from_sha = match from {
            Some(f) => self.get_owned(jid, f)?.sha,
            None => cp
                .parent_sha
                .clone()
                .ok_or_else(|| anyhow!("checkpoint {id} has no parent (it is the baseline)"))?,
        };
        let repo = self.repo_for(jid, Path::new(&cp.workspace));
        let files = repo.changed_files(&from_sha, &cp.sha).await?;
        let (diff, truncated) = repo.diff(&from_sha, &cp.sha, MAX_DIFF_BYTES).await?;
        Ok((cp, from_sha, files, diff, truncated))
    }

    /// Restore the working tree to checkpoint `id` (whole tree, or only
    /// `files`). The restored state is itself recorded as a new checkpoint,
    /// so a restore can be undone like any other step.
    pub async fn restore(
        &self,
        jid: &str,
        id: i64,
        files: &[String],
    ) -> Result<(RestoreReport, Option<ChatCheckpoint>)> {
        let cp = self.get_owned(jid, id)?;
        let dir = PathBuf::from(&cp.workspace);
        if !dir.is_dir() {
            return Err(anyhow!("workspace {} no longer exists", dir.display()));
        }
        let repo = self.repo_for(jid, &dir);
        let guard = self.lock_for(jid);
        let _held = guard.lock().await;
        // Whatever is in the tree right now — an agent edit that never got
        // its checkpoint, or the person's own hand edits — is committed first,
        // so a restore can never destroy state that no checkpoint recorded.
        let before = repo.head().await?;
        if let Some(sha) = repo.commit("snapshot before restore").await? {
            let n = match &before {
                Some(b) => repo.count_changed_from(b, &sha).await.unwrap_or(0),
                None => 0,
            };
            self.db.insert_checkpoint(
                jid,
                &sha,
                before.as_deref(),
                "snapshot",
                "snapshot before restore",
                &cp.workspace,
                n as i64,
            )?;
        }
        let report = repo.restore(&cp.sha, files).await?;
        let parent = repo.head().await?;
        let what = if files.is_empty() {
            "whole tree".to_string()
        } else {
            format!("{} file(s)", files.len())
        };
        let message = format!("restore: {what} to checkpoint #{id}");
        let new_cp = match repo.commit(&message).await? {
            Some(sha) => {
                let n = report.restored.len() + report.removed.len();
                Some(self.db.insert_checkpoint(
                    jid,
                    &sha,
                    parent.as_deref(),
                    "restore",
                    &message,
                    &cp.workspace,
                    n as i64,
                )?)
            }
            None => None,
        };
        Ok((report, new_cp))
    }
}

impl ShadowRepo {
    /// Number of files that differ between two commits (helper kept here so
    /// the shadow module stays a plain git wrapper).
    pub async fn count_changed_from(&self, from: &str, to: &str) -> Result<usize> {
        Ok(self.changed_files(from, to).await?.len())
    }
}

fn one_line(s: &str, max: usize) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    if first.chars().count() > max {
        format!("{}…", first.chars().take(max).collect::<String>())
    } else {
        first.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_tools_always_checkpoint_and_errors_never_do() {
        let j = serde_json::json!({});
        assert!(tool_may_write("Edit", &j, true));
        assert!(tool_may_write("Write", &j, true));
        assert!(tool_may_write("NotebookEdit", &j, true));
        assert!(!tool_may_write("Edit", &j, false));
        assert!(!tool_may_write("Edit", &serde_json::json!({"error": true}), true));
        assert!(!tool_may_write("Read", &j, true));
        assert!(!tool_may_write("Grep", &j, true));
    }

    #[test]
    fn bash_is_judged_by_its_command() {
        let ro = serde_json::json!({"title": "git status"});
        assert!(!tool_may_write("Bash", &ro, true));
        let rw = serde_json::json!({"title": "cargo fmt"});
        assert!(tool_may_write("Bash", &rw, true));
        // Truncated title → cannot classify → assume it wrote.
        let long = serde_json::json!({"title": "ls -la some/very/long/path...".to_string()});
        assert!(tool_may_write("Bash", &long, true));
    }

    #[test]
    fn shadow_dir_names_are_safe_and_distinct() {
        let a = shadow_dir_name("tg:123:user:9");
        let b = shadow_dir_name("tg_123_user_9");
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(a, b);
    }

    fn test_service() -> (CheckpointService, tempfile::TempDir, tempfile::TempDir) {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::from_env();
        cfg.paths.db_path = home.path().join("t.db");
        cfg.paths.cognitive_db_path = home.path().join("t_cog.db");
        let db = Arc::new(crate::db::Db::open(&cfg).unwrap());
        (CheckpointService::new(db, home.path()), home, work)
    }

    /// The failure this guards was found by running a real session, not by a
    /// unit test: on a brand-new project the agent's first tool is a Write —
    /// there is no file to Read first — so a baseline taken by the tool hook
    /// commits that very file and leaves nothing to diff or restore.
    #[tokio::test]
    async fn a_new_projects_first_write_is_still_recoverable() {
        let (svc, _home, work) = test_service();
        let jid = "code:new-project";
        let dir = work.path().to_string_lossy().to_string();

        // What `set_working_dir` does, before the agent runs anything.
        svc.prepare(jid, &dir, "code").await;

        // The agent's first tool call creates the project's first file.
        std::fs::write(work.path().join("task.py"), "print('hi')\n").unwrap();
        let cp = svc
            .on_tool_event(
                jid,
                Some(&dir),
                "code",
                "Write",
                &serde_json::json!({}),
                true,
                "Write task.py",
            )
            .await
            .unwrap();

        let cp = cp.expect("the first write of a new project must produce a checkpoint");
        assert_eq!(cp.tool_name, "Write");
        assert_eq!(svc.list(jid).unwrap().len(), 1);
        // And it must be a real change against the baseline, not an empty one.
        let (_, _from_sha, files, diff, _) = svc.diff(jid, cp.id, None).await.unwrap();
        assert!(diff.contains("task.py"), "diff was: {diff}");
        assert_eq!(files.len(), 1);
    }

    /// The same loss, reached the other way: an ordinary chat whose working
    /// directory is empty never goes through `prepare`, so the tool hook is
    /// what initializes the repo — and it runs after the write. The baseline
    /// must then be empty, or the edit disappears into it.
    #[tokio::test]
    async fn a_write_that_creates_the_repo_is_not_swallowed_by_its_own_baseline() {
        let (svc, _home, work) = test_service();
        let jid = "code:lazy-init";
        let dir = work.path().to_string_lossy().to_string();

        // No `prepare` — straight to the first tool, which writes.
        std::fs::write(work.path().join("first.txt"), "hello\n").unwrap();
        let cp = svc
            .on_tool_event(jid, Some(&dir), "code", "Write", &serde_json::json!({}), true, "w")
            .await
            .unwrap()
            .expect("a write that initializes the repo must still checkpoint");

        let (_, _from, files, diff, _) = svc.diff(jid, cp.id, None).await.unwrap();
        assert_eq!(files.len(), 1, "the created file must be in the diff");
        assert!(diff.contains("first.txt"), "diff was: {diff}");
    }

    #[tokio::test]
    async fn preparing_twice_keeps_the_original_baseline() {
        // `set_working_dir` fires on session creation and again on later
        // turns; a second baseline would orphan the first edits.
        let (svc, _home, work) = test_service();
        let jid = "code:repeat";
        let dir = work.path().to_string_lossy().to_string();
        svc.prepare(jid, &dir, "code").await;
        std::fs::write(work.path().join("a.txt"), "one\n").unwrap();
        svc.prepare(jid, &dir, "code").await;
        let cp = svc
            .on_tool_event(jid, Some(&dir), "code", "Write", &serde_json::json!({}), true, "w")
            .await
            .unwrap();
        assert!(cp.is_some(), "the second prepare must not swallow the edit");
    }

    #[test]
    fn home_and_root_are_never_eligible() {
        assert!(is_forbidden_root(Path::new("/")));
        if let Some(h) = dirs::home_dir() {
            assert!(is_forbidden_root(&h));
        }
    }
}
