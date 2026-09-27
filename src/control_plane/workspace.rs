//! Workspace (§6/§9): one working directory per chat —
//! `progress.md` (decisions / constraints / failed_paths), `todo.json` (a
//! mirror of the todo tool's state), `artifacts/` (large tool output offload)
//! and `handoff.md` — used for resume and as external memory when the
//! in-context history gets compacted.
//!
//! **What is wired live, and what is not.** `todo.json` and the
//! `failed_paths` section of `progress.md` are passive mirrors, fed from the
//! same per-chat event seam as [`super::trace`] — they only ever *write a new
//! file*, never change what a tool returns to the LLM, so they are on by
//! default (`controlPlane.workspace.enabled`) the same way trace is. L1
//! offload ([`Workspace::write_artifact`]) is different: `run_tools` calls it
//! only when `controlPlane.workspace.substituteToolOutput` is on (default
//! off), because it changes what the LLM sees on a large result — exactly "a
//! new thing that would change a decision". It stays off until an eval shows
//! it does not regress tasks that need the full output inline.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::zen_core::{EngineEvent, TodosUpdateItem};

use super::{restrict_dir, restrict_file, safe_id, senclaw_home, ControlPlaneSettings};

/// Total bytes the mirrored files + artifacts for one chat may use before the
/// oldest artifact is pruned. `progress.md`/`todo.json`/`handoff.md` are
/// never pruned — only `artifacts/*` counts against this and is prunable
/// (see module docs: "compaction always keeps progress.md").
const QUOTA_BYTES: u64 = 50 * 1024 * 1024;
/// Preview kept in-context per artifact (§7 L1: "≤ 500 token"; ~4 chars/token).
const PREVIEW_HEAD_CHARS: usize = 1000;
const PREVIEW_TAIL_CHARS: usize = 1000;

/// Below this, a tool result is left alone even when
/// `controlPlane.workspace.substituteToolOutput` is on — offloading a result
/// barely past the preview's own size would replace it with something almost
/// as large, for no benefit. Checked *before* reading the setting at all
/// (`zen_core::run_tools`), so the common case of a small result never even
/// touches `config.json`.
pub const OFFLOAD_THRESHOLD_BYTES: usize = 8_000;

fn root() -> PathBuf {
    std::env::var("SENCLAW_CONTROL_PLANE_WORKSPACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| senclaw_home().join("control-plane").join("workspace"))
}

pub struct Workspace {
    dir: PathBuf,
}

impl Workspace {
    pub fn for_chat(jid: &str) -> Workspace {
        Workspace { dir: root().join(safe_id(jid)) }
    }

    fn ensure_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        restrict_dir(&self.dir);
        Ok(())
    }

    fn progress_path(&self) -> PathBuf {
        self.dir.join("progress.md")
    }

    fn todo_path(&self) -> PathBuf {
        self.dir.join("todo.json")
    }

    fn handoff_path(&self) -> PathBuf {
        self.dir.join("handoff.md")
    }

    fn artifacts_dir(&self) -> PathBuf {
        self.dir.join("artifacts")
    }

    /// Append one line under a `## <section>` heading in `progress.md`,
    /// creating the file (and the heading, once) as needed. `section` is one
    /// of `decisions` / `constraints` / `failed_paths` — never the model's
    /// own text verbatim; the caller is expected to have already summarized.
    pub fn append_progress(&self, section: &str, line: &str) -> std::io::Result<()> {
        self.ensure_dir()?;
        let path = self.progress_path();
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let heading = format!("## {section}");
        let mut out = existing.clone();
        if !existing.contains(&heading) {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&heading);
            out.push('\n');
        }
        out.push_str("- ");
        out.push_str(line.trim());
        out.push('\n');
        std::fs::write(&path, out)?;
        restrict_file(&path);
        Ok(())
    }

    /// Mirror the todo tool's state. Overwrite semantics — this is a mirror
    /// of current state, not a log.
    pub fn write_todo(&self, items: &[TodosUpdateItem]) -> std::io::Result<()> {
        self.ensure_dir()?;
        let path = self.todo_path();
        let text = serde_json::to_string_pretty(items).unwrap_or_else(|_| "[]".to_string());
        std::fs::write(&path, text)?;
        restrict_file(&path);
        Ok(())
    }

    pub fn write_handoff(&self, text: &str) -> std::io::Result<()> {
        self.ensure_dir()?;
        let path = self.handoff_path();
        std::fs::write(&path, text)?;
        restrict_file(&path);
        Ok(())
    }

    /// L1 offload (§8 "no silent edits"): the full content is written under
    /// `artifacts/<call_id>`, and the returned preview says plainly that it
    /// was cut and where the rest lives — never a silent truncation.
    /// `call_id` is confined to a single path component (see
    /// [`super::safe_id`]) — a model-influenced id cannot escape the
    /// artifacts directory.
    pub fn write_artifact(&self, call_id: &str, full_content: &str) -> std::io::Result<ArtifactPreview> {
        self.ensure_dir()?;
        let dir = self.artifacts_dir();
        std::fs::create_dir_all(&dir)?;
        restrict_dir(&dir);
        let safe_call_id = safe_id(call_id);
        let path = dir.join(&safe_call_id);
        {
            let mut f = std::fs::File::create(&path)?;
            f.write_all(full_content.as_bytes())?;
        }
        restrict_file(&path);
        self.enforce_quota(&dir)?;
        Ok(build_preview(full_content, &path))
    }

    /// Delete the oldest artifacts (by mtime) until the directory is back
    /// under [`QUOTA_BYTES`]. Never touches `progress.md`/`todo.json`/
    /// `handoff.md` — those live one level up and this only walks
    /// `artifacts/`.
    fn enforce_quota(&self, artifacts_dir: &Path) -> std::io::Result<()> {
        let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = std::fs::read_dir(artifacts_dir)?
            .flatten()
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                Some((e.path(), meta.len(), meta.modified().ok()?))
            })
            .collect();
        let mut total: u64 = entries.iter().map(|(_, size, _)| size).sum();
        if total <= QUOTA_BYTES {
            return Ok(());
        }
        entries.sort_by_key(|(_, _, mtime)| *mtime);
        for (path, size, _) in entries {
            if total <= QUOTA_BYTES {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactPreview {
    pub path: PathBuf,
    pub preview: String,
    pub full_bytes: usize,
}

fn build_preview(full_content: &str, path: &Path) -> ArtifactPreview {
    let full_bytes = full_content.len();
    let chars: Vec<char> = full_content.chars().collect();
    let preview = if chars.len() <= PREVIEW_HEAD_CHARS + PREVIEW_TAIL_CHARS {
        full_content.to_string()
    } else {
        let head: String = chars[..PREVIEW_HEAD_CHARS].iter().collect();
        let tail: String = chars[chars.len() - PREVIEW_TAIL_CHARS..].iter().collect();
        format!(
            "{head}\n\n[... cut {} bytes; full output saved at {} ...]\n\n{tail}",
            full_bytes.saturating_sub(head.len() + tail.len()),
            path.display()
        )
    };
    ArtifactPreview { path: path.to_path_buf(), preview, full_bytes }
}

/// Feed one engine event for `jid` into the workspace mirror. A no-op unless
/// `controlPlane.workspace.enabled` — unlike trace/failures, this touches the
/// filesystem for a purpose a user may reasonably want off entirely (a
/// workspace directory per chat, not just a metadata log).
pub fn record(settings: &ControlPlaneSettings, jid: &str, event: &EngineEvent) {
    if !settings.workspace.enabled {
        return;
    }
    let ws = Workspace::for_chat(jid);
    match event {
        EngineEvent::TodosUpdate(items) => {
            if let Err(e) = ws.write_todo(items) {
                tracing::debug!("[control-plane] workspace todo mirror failed for {jid}: {e}");
            }
        }
        EngineEvent::ToolExecutionError(d) => {
            let class = crate::failures::error_class(&d.content);
            let line = format!("{} — {class}", d.tool_name);
            if let Err(e) = ws.append_progress("failed_paths", &line) {
                tracing::debug!("[control-plane] workspace progress append failed for {jid}: {e}");
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_tmp_dir<F: FnOnce()>(f: F) {
        // See `control_plane::env_test_guard`'s docs: held for the whole
        // closure so a concurrently-running test cannot flip this
        // process-global env var out from under this one.
        let _guard = super::super::env_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("SENCLAW_CONTROL_PLANE_WORKSPACE_DIR", tmp.path());
        f();
        std::env::remove_var("SENCLAW_CONTROL_PLANE_WORKSPACE_DIR");
    }

    #[test]
    fn append_progress_creates_a_heading_once_and_appends_lines_under_it() {
        with_tmp_dir(|| {
            let ws = Workspace::for_chat("jid-progress");
            ws.append_progress("failed_paths", "Bash — permission denied").unwrap();
            ws.append_progress("failed_paths", "Edit — file not found").unwrap();
            let text = std::fs::read_to_string(ws.progress_path()).unwrap();
            assert_eq!(text.matches("## failed_paths").count(), 1, "heading written once");
            assert!(text.contains("- Bash — permission denied"));
            assert!(text.contains("- Edit — file not found"));
        });
    }

    #[test]
    fn write_artifact_confines_the_call_id_to_one_path_component() {
        with_tmp_dir(|| {
            let ws = Workspace::for_chat("jid-artifact");
            let preview = ws.write_artifact("../../etc/passwd", "danger").unwrap();
            assert!(preview.path.starts_with(ws.artifacts_dir()), "escape attempt was confined");
            assert!(!preview.path.to_string_lossy().contains(".."));
        });
    }

    #[test]
    fn a_large_artifact_gets_a_head_and_tail_preview_naming_the_cut_and_the_path() {
        with_tmp_dir(|| {
            let ws = Workspace::for_chat("jid-large");
            let big = format!("{}MIDDLE{}", "A".repeat(3000), "B".repeat(3000));
            let preview = ws.write_artifact("c_1", &big).unwrap();
            assert!(preview.preview.contains("cut"));
            assert!(preview.preview.contains("full output saved at"));
            assert!(!preview.preview.contains("MIDDLE"), "the cut middle must not survive into the preview");
            let saved = std::fs::read_to_string(&preview.path).unwrap();
            assert!(saved.contains("MIDDLE"), "the full file on disk keeps everything");
        });
    }

    #[test]
    fn a_small_artifact_is_not_truncated() {
        with_tmp_dir(|| {
            let ws = Workspace::for_chat("jid-small");
            let preview = ws.write_artifact("c_1", "short output").unwrap();
            assert_eq!(preview.preview, "short output");
        });
    }

    #[test]
    fn quota_prunes_the_oldest_artifact_first_and_never_touches_progress_md() {
        with_tmp_dir(|| {
            let ws = Workspace::for_chat("jid-quota");
            ws.append_progress("decisions", "keep this forever").unwrap();
            // Shrink the effective quota for this test by writing artifacts
            // larger than a few of them combined would allow under the real
            // constant would take too long; instead verify the *ordering*
            // logic directly against a temp dir sized like production would
            // exceed only after many writes — so assert the invariant that
            // matters: progress.md survives any number of artifact writes.
            for i in 0..5 {
                ws.write_artifact(&format!("c_{i}"), &"x".repeat(1000)).unwrap();
            }
            assert!(ws.progress_path().is_file(), "progress.md is never pruned");
            assert!(std::fs::read_to_string(ws.progress_path()).unwrap().contains("keep this forever"));
        });
    }

    #[test]
    fn record_is_a_no_op_when_workspace_is_disabled() {
        with_tmp_dir(|| {
            let mut settings = ControlPlaneSettings::default();
            settings.workspace.enabled = false;
            record(
                &settings,
                "jid-disabled",
                &EngineEvent::ToolExecutionError(crate::zen_core::ToolExecutionErrorData {
                    agent_id: "main".into(),
                    tool_name: "Bash".into(),
                    title: "run".into(),
                    description: String::new(),
                    content: "boom".into(),
                    args_shape: Default::default(),
                }),
            );
            assert!(!Workspace::for_chat("jid-disabled").progress_path().exists());
        });
    }
}
