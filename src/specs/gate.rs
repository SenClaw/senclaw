//! The tool gate of the spec flow, run in `zen_core::run_tools` after the
//! PreToolUse hook and before the permission layer, so no autonomy level, skip
//! flag or permission hook can step around it.
//!
//! It answers one question for a call that would change files: may this change
//! land now? Inside a folder with an open spec the answer is no until a person
//! approved the current design, and then no until this chat has read it in
//! full. A refusal starts with [`REFUSAL_PREFIX`] so `finish_check` reads it as a
//! failure and the edit tally does not nudge the agent into retrying it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use super::{normalize_path, stage_of, SpecFlow, SpecStore, Stage, DESIGNS_DIR};

pub const REFUSAL_PREFIX: &str = "Error: [spec]";

pub const STORE: &str = "Error: [spec] The spec flow's own records cannot be changed by a tool.";
pub const NO_SPEC: &str = "Error: [spec] No design spec for this folder yet. Changes here need one: ask the person to \
                           run /brainstorm <idea>, or /spec skip for a small fix.";
pub const NO_SPEC_SUBAGENT: &str = "Error: [spec] No design spec for this folder yet; a subagent cannot change files \
                                    here. Report back to the main agent.";

pub fn stage_refusal(path: &str, stage: Stage) -> String {
    match stage {
        Stage::Changed => format!(
            "{REFUSAL_PREFIX} {path} changed after it was approved. Code is locked until the person runs /review and \
             /approve again."
        ),
        Stage::Missing => format!(
            "{REFUSAL_PREFIX} {path} has not been written yet. Only files in {DESIGNS_DIR}/ may change until the person \
             runs /approve."
        ),
        other => format!(
            "{REFUSAL_PREFIX} {path} is {}, not approved. Only files in {DESIGNS_DIR}/ may change until the person runs \
             /approve.",
            other.as_str()
        ),
    }
}

pub fn read_first(path: &str) -> String {
    format!("{REFUSAL_PREFIX} Read {path} in full first (no offset/limit): it is the approved design for this folder.")
}

/// A spec this long is handed over in the refusal itself; a longer one is not
/// (the refusal asks for a `Read`), so one refusal never floods the context.
pub const INLINE_SPEC_MAX_CHARS: usize = 24_000;

/// The refusal for a chat that has not read the approved spec, with the spec
/// in it. Asked to `Read` it, gemma-4-e4b never did (7 refusals in a row, no
/// code written); with the text in the result it has seen it, so the gate
/// records the read and the same change goes through when it is made again.
pub fn read_first_inline(path: &str, text: &str) -> String {
    format!(
        "{REFUSAL_PREFIX} {path} is the approved design for this folder and this chat had not read it. Here it is in full; \
         make your change again so that it follows this design.\n<spec_file path=\"{path}\">\n{}\n</spec_file>",
        text.trim_end().replace("</spec_file", "<\\/spec_file")
    )
}

/// One tool call as the gate sees it. `tool` is the permission name (the tool
/// that will actually run, so an alias of `Edit` is an `Edit`) and `input` has
/// its paths already resolved against the working folder.
pub struct GateInput<'a> {
    pub tool: &'a str,
    pub input: &'a serde_json::Value,
    pub working_dir: &'a str,
    pub chat_jid: &'a str,
    pub is_subagent: bool,
    pub flow: SpecFlow,
}

enum Change {
    File(PathBuf),
    Shell,
}

fn change_of(tool: &str, input: &serde_json::Value, working_dir: &str) -> Option<Change> {
    let field = match tool {
        "Edit" | "Write" | "MultiEdit" => "file_path",
        "NotebookEdit" => "notebook_path",
        "Bash" => {
            let command = input.get("command").and_then(|v| v.as_str()).unwrap_or_default();
            return (!crate::util::shell_safety::is_readonly_safe_command(command)).then_some(Change::Shell);
        }
        _ => return None,
    };
    let raw = input.get(field).and_then(|v| v.as_str()).filter(|s| !s.is_empty())?;
    let path = Path::new(raw);
    let abs = if path.is_absolute() { path.to_path_buf() } else { Path::new(working_dir).join(path) };
    Some(Change::File(normalize_path(&abs)))
}

/// A file the design phase may write: a regular `.md` file under the root's
/// `docs/designs/`, judged on the real path (so neither `..` nor a link inside
/// that folder leads out of it).
fn is_design_file(root: &Path, target: &Path) -> bool {
    target.starts_with(root.join(DESIGNS_DIR))
        && target.extension().is_some_and(|e| e == "md")
        && std::fs::symlink_metadata(target).map(|m| !m.file_type().is_symlink()).unwrap_or(true)
}

/// `None` = the call may go on; `Some(text)` = the tool result to return instead.
pub fn check(g: &GateInput, store: &SpecStore) -> Option<String> {
    if g.flow == SpecFlow::Off {
        return None;
    }
    let change = change_of(g.tool, g.input, g.working_dir)?;
    let target = match &change {
        Change::File(p) => Some(p.clone()),
        Change::Shell => None,
    };
    if let Some(t) = &target {
        if t.starts_with(normalize_path(store.base())) {
            return Some(STORE.to_string());
        }
    }
    let start = match &target {
        Some(t) => t.parent().map(Path::to_path_buf).unwrap_or_else(|| t.clone()),
        None if g.working_dir.is_empty() => return None,
        None => normalize_path(Path::new(g.working_dir)),
    };
    let Some((root, state)) = store.find_root(&start) else {
        if g.flow == SpecFlow::Required && !g.working_dir.is_empty() {
            let wd = normalize_path(Path::new(g.working_dir));
            // A `Task` subagent with worktree isolation works in its own checkout
            // under SENCLAW_HOME (`worktrees/`): what it changes lands on a branch a
            // person merges, so `required` does not hold it there.
            let in_senclaw_home = store.base().parent().is_some_and(|home| wd.starts_with(normalize_path(home)));
            if !in_senclaw_home && target.as_deref().is_none_or(|t| t.starts_with(&wd)) {
                return Some(if g.is_subagent { NO_SPEC_SUBAGENT } else { NO_SPEC }.to_string());
            }
        }
        return None;
    };
    if target.as_deref().is_some_and(|t| is_design_file(&root, t)) {
        return None;
    }
    let rel = state.active.as_str();
    let spec_path = root.join(rel);
    let text = std::fs::read_to_string(&spec_path).ok();
    let (stage, hash) = stage_of(text.as_deref(), state.specs.get(rel));
    if stage != Stage::Approved {
        return Some(stage_refusal(rel, stage));
    }
    if !g.is_subagent && !g.chat_jid.is_empty() {
        let read = receipts().lock().unwrap_or_else(|p| p.into_inner()).get(g.chat_jid).cloned();
        let current = read.is_some_and(|(p, h)| p == spec_path && Some(h.as_str()) == hash.as_deref());
        if !current {
            return Some(match (text.as_deref(), hash) {
                (Some(text), Some(hash)) if text.chars().count() <= INLINE_SPEC_MAX_CHARS => {
                    receipts().lock().unwrap_or_else(|p| p.into_inner()).insert(g.chat_jid.to_string(), (spec_path, hash));
                    read_first_inline(rel, text)
                }
                _ => read_first(rel),
            });
        }
    }
    None
}

fn receipts() -> &'static Mutex<HashMap<String, (PathBuf, String)>> {
    static R: OnceLock<Mutex<HashMap<String, (PathBuf, String)>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

fn skips() -> &'static Mutex<HashSet<String>> {
    static S: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// After a successful `Read`: if it read the whole open spec of its folder,
/// remember that this chat has seen this version. A slice (`offset` past the
/// first line, or a `limit` short of the file) is not a read of the spec.
pub fn record_read(chat_jid: &str, input: &serde_json::Value, working_dir: &str, store: &SpecStore) {
    if chat_jid.is_empty() {
        return;
    }
    let Some(raw) = input.get("file_path").and_then(|v| v.as_str()) else { return };
    let path = Path::new(raw);
    let abs = if path.is_absolute() { path.to_path_buf() } else { Path::new(working_dir).join(path) };
    let file = normalize_path(&abs);
    let Some((root, state)) = file.parent().and_then(|dir| store.find_root(dir)) else { return };
    if root.join(&state.active) != file {
        return;
    }
    let Ok(text) = std::fs::read_to_string(&file) else { return };
    let offset = input.get("offset").and_then(|v| v.as_u64()).unwrap_or(1);
    let limit = input.get("limit").and_then(|v| v.as_u64());
    if offset > 1 || limit.is_some_and(|l| (l as usize) < text.lines().count()) {
        return;
    }
    receipts()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(chat_jid.to_string(), (file, super::design_hash(&text)));
}

/// The chat's history was cleared (reset, "clear context", compaction, a
/// deleted session): what it read is gone from its context, and so is a skip.
pub fn forget(chat_jid: &str) {
    receipts().lock().unwrap_or_else(|p| p.into_inner()).remove(chat_jid);
    skips().lock().unwrap_or_else(|p| p.into_inner()).remove(chat_jid);
}

/// `/spec skip`: this chat runs as `on` instead of `required` until [`forget`].
pub fn skip(chat_jid: &str) {
    skips().lock().unwrap_or_else(|p| p.into_inner()).insert(chat_jid.to_string());
}

pub fn is_skipped(chat_jid: &str) -> bool {
    skips().lock().unwrap_or_else(|p| p.into_inner()).contains(chat_jid)
}

#[cfg(test)]
mod tests {
    use super::super::{design_hash, Approval, SpecEntry, DETAILED_HEADING};
    use super::*;
    use serde_json::json;

    struct Fixture {
        _store_dir: tempfile::TempDir,
        _repo: tempfile::TempDir,
        store: SpecStore,
        root: PathBuf,
        wd: String,
    }

    const REL: &str = "docs/designs/s.md";

    fn fixture() -> Fixture {
        let store_dir = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let root = normalize_path(repo.path());
        std::fs::create_dir_all(root.join("docs/designs")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let wd = root.to_string_lossy().into_owned();
        // `<home>/specs`, as in the daemon: the gate treats the store's parent as SENCLAW_HOME.
        Fixture { store: SpecStore::at(store_dir.path().join("specs")), _store_dir: store_dir, _repo: repo, root, wd }
    }

    impl Fixture {
        fn open(&self, text: &str) {
            std::fs::write(self.root.join(REL), text).unwrap();
            self.store.update(&self.root, |s| s.active = REL.into()).unwrap();
        }
        fn approve(&self) {
            let hash = design_hash(&std::fs::read_to_string(self.root.join(REL)).unwrap());
            self.store
                .update(&self.root, |s| {
                    s.specs.insert(
                        REL.into(),
                        SpecEntry { approval: Some(Approval { hash, ..Default::default() }), ..Default::default() },
                    );
                })
                .unwrap();
        }
        fn call(&self, tool: &str, input: serde_json::Value, jid: &str, subagent: bool, flow: SpecFlow) -> Option<String> {
            check(
                &GateInput { tool, input: &input, working_dir: &self.wd, chat_jid: jid, is_subagent: subagent, flow },
                &self.store,
            )
        }
        fn edit(&self, path: &str, jid: &str) -> Option<String> {
            let p = self.root.join(path);
            self.call("Edit", json!({"file_path": p.to_string_lossy()}), jid, false, SpecFlow::On)
        }
    }

    fn full_spec() -> String {
        format!("# S\n## Goals\n* g\n{DETAILED_HEADING}\n- [ ] T1\n")
    }

    #[test]
    fn off_lets_everything_through() {
        let f = fixture();
        f.open(&full_spec());
        let input = json!({"file_path": f.root.join("src/a.rs").to_string_lossy()});
        assert!(f.call("Edit", input, "c1", false, SpecFlow::Off).is_none());
    }

    #[test]
    fn no_spec_under_on_changes_nothing() {
        let f = fixture();
        assert!(f.edit("src/a.rs", "c1").is_none());
    }

    #[test]
    fn design_phase_refuses_code_allows_designs_md() {
        let f = fixture();
        f.open(&full_spec());
        let refusal = f.edit("src/a.rs", "c1").unwrap();
        assert!(refusal.starts_with(REFUSAL_PREFIX) && refusal.contains("draft, not approved"), "{refusal}");
        assert!(f.edit(REL, "c1").is_none());
        assert!(f.edit("docs/designs/other.md", "c1").is_none());
    }

    #[test]
    fn dotdot_and_symlink_into_designs_are_refused() {
        let f = fixture();
        f.open(&full_spec());
        assert!(f.edit("docs/designs/../../src/a.rs", "c1").is_some());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(f.root.join("src/real.rs"), f.root.join("docs/designs/link.md")).unwrap();
            std::fs::write(f.root.join("src/real.rs"), "x").unwrap();
            assert!(f.edit("docs/designs/link.md", "c1").is_some());
            std::os::unix::fs::symlink(f.root.join("src"), f.root.join("docs/designs/code")).unwrap();
            assert!(f.edit("docs/designs/code/x.md", "c1").is_some());
        }
    }

    #[test]
    fn non_md_in_designs_is_refused() {
        let f = fixture();
        f.open(&full_spec());
        assert!(f.edit("docs/designs/x.rs", "c1").is_some());
    }

    #[test]
    fn store_writes_are_always_refused() {
        let f = fixture();
        let target = f.store.dir_for(&f.root).join("state.json");
        let out = f.call("Write", json!({"file_path": target.to_string_lossy()}), "c1", false, SpecFlow::On);
        assert_eq!(out.as_deref(), Some(STORE));
    }

    #[test]
    fn readonly_bash_passes_writing_bash_is_refused() {
        let f = fixture();
        f.open(&full_spec());
        assert!(f.call("Bash", json!({"command": "ls -la"}), "c1", false, SpecFlow::On).is_none());
        assert!(f.call("Bash", json!({"command": "echo x > src/a.rs"}), "c1", false, SpecFlow::On).is_some());
        assert!(f.call("Read", json!({"file_path": "src/a.rs"}), "c1", false, SpecFlow::On).is_none());
    }

    #[test]
    fn approved_hands_the_spec_over_once_then_lets_the_change_through() {
        let f = fixture();
        f.open(&full_spec());
        f.approve();
        let jid = "gate-test:inline";
        let refusal = f.edit("src/a.rs", jid).unwrap();
        assert!(refusal.starts_with(REFUSAL_PREFIX), "{refusal}");
        assert!(refusal.contains("<spec_file path=\"docs/designs/s.md\">") && refusal.contains("- [ ] T1"), "{refusal}");
        // It has seen it now: the same change goes through, no Read needed.
        assert!(f.edit("src/a.rs", jid).is_none());
        // A new version of the design is handed over again.
        std::fs::write(f.root.join(REL), full_spec().replace("T1", "T1 and T2")).unwrap();
        f.approve();
        assert!(f.edit("src/a.rs", jid).unwrap().contains("T1 and T2"));
        assert!(f.edit("src/a.rs", jid).is_none());
        forget(jid);
    }

    #[test]
    fn a_spec_too_long_to_hand_over_asks_for_a_read() {
        let f = fixture();
        f.open(&format!("{}{}", full_spec(), "x\n".repeat(INLINE_SPEC_MAX_CHARS)));
        f.approve();
        let jid = "gate-test:long";
        let refusal = f.edit("src/a.rs", jid).unwrap();
        assert!(refusal.contains("Read docs/designs/s.md in full"), "{refusal}");
        assert!(f.edit("src/a.rs", jid).is_some());
        record_read(jid, &json!({"file_path": REL}), &f.wd, &f.store);
        assert!(f.edit("src/a.rs", jid).is_none());
        forget(jid);
    }

    #[test]
    fn approved_needs_a_read_receipt_per_chat() {
        let f = fixture();
        f.open(&full_spec());
        f.approve();
        let jid = "gate-test:approved";
        record_read(jid, &json!({"file_path": REL}), &f.wd, &f.store);
        assert!(f.edit("src/a.rs", jid).is_none());
        // Another chat has not read it.
        assert!(f.edit("src/a.rs", "gate-test:other").is_some());
        // A subagent needs approval only.
        let input = json!({"file_path": f.root.join("src/a.rs").to_string_lossy()});
        assert!(f.call("Edit", input, "", true, SpecFlow::On).is_none());
        forget(jid);
        assert!(f.edit("src/a.rs", jid).is_some());
    }

    #[test]
    fn partial_read_gives_no_receipt() {
        let f = fixture();
        f.open(&full_spec());
        f.approve();
        let read = |input: serde_json::Value, jid: &str| {
            record_read(jid, &input, &f.wd, &f.store);
            receipts().lock().unwrap().contains_key(jid)
        };
        assert!(!read(json!({"file_path": REL, "offset": 3}), "gate-test:partial-a"));
        assert!(!read(json!({"file_path": REL, "limit": 2}), "gate-test:partial-b"));
        assert!(read(json!({"file_path": REL, "offset": 1, "limit": 2000}), "gate-test:partial-c"));
        for j in ["gate-test:partial-a", "gate-test:partial-b", "gate-test:partial-c"] {
            forget(j);
        }
    }

    #[test]
    fn changed_after_approval_is_refused_and_ticking_is_not_a_change() {
        let f = fixture();
        f.open(&full_spec());
        f.approve();
        let jid = "gate-test:changed";
        std::fs::write(f.root.join(REL), full_spec().replace("- [ ] T1", "- [x] T1")).unwrap();
        record_read(jid, &json!({"file_path": REL}), &f.wd, &f.store);
        assert!(f.edit("src/a.rs", jid).is_none());
        std::fs::write(f.root.join(REL), full_spec().replace("T1", "T1 and drop a table")).unwrap();
        let refusal = f.edit("src/a.rs", jid).unwrap();
        assert!(!refusal.contains("<spec_file"), "a changed spec is not handed over as approved: {refusal}");
        assert!(refusal.contains("changed after it was approved"), "{refusal}");
        forget(jid);
    }

    #[test]
    fn required_without_spec_refuses_main_and_subagent_inside_the_folder_only() {
        let f = fixture();
        let inside = json!({"file_path": f.root.join("src/a.rs").to_string_lossy()});
        assert_eq!(f.call("Edit", inside.clone(), "c1", false, SpecFlow::Required).as_deref(), Some(NO_SPEC));
        assert_eq!(f.call("Edit", inside, "", true, SpecFlow::Required).as_deref(), Some(NO_SPEC_SUBAGENT));
        let elsewhere = tempfile::tempdir().unwrap();
        let outside = json!({"file_path": elsewhere.path().join("x.txt").to_string_lossy()});
        assert!(f.call("Write", outside, "c1", false, SpecFlow::Required).is_none());
    }

    #[test]
    fn required_does_not_hold_a_worktree_under_senclaw_home() {
        let home = tempfile::tempdir().unwrap();
        let store = SpecStore::at(home.path().join("specs"));
        let wt = home.path().join("worktrees/task-1");
        std::fs::create_dir_all(&wt).unwrap();
        let input = json!({"file_path": wt.join("src/a.rs").to_string_lossy()});
        let g = GateInput {
            tool: "Write",
            input: &input,
            working_dir: &wt.to_string_lossy(),
            chat_jid: "",
            is_subagent: true,
            flow: SpecFlow::Required,
        };
        assert!(check(&g, &store).is_none());
    }

    #[test]
    fn skip_is_per_chat_and_forget_clears_it() {
        let jid = "gate-test:skip";
        assert!(!is_skipped(jid));
        skip(jid);
        assert!(is_skipped(jid));
        assert!(!is_skipped("gate-test:skip-other"));
        forget(jid);
        assert!(!is_skipped(jid));
    }

    #[test]
    fn target_in_another_spec_root_is_gated_whatever_the_working_dir() {
        let f = fixture();
        f.open(&full_spec());
        // The chat now works somewhere else (a workspace switch), and edits the
        // first folder by absolute path.
        let other = tempfile::tempdir().unwrap();
        let input = json!({"file_path": f.root.join("src/a.rs").to_string_lossy()});
        let out = check(
            &GateInput {
                tool: "Write",
                input: &input,
                working_dir: &other.path().to_string_lossy(),
                chat_jid: "c1",
                is_subagent: false,
                flow: SpecFlow::On,
            },
            &f.store,
        );
        assert!(out.is_some());
    }

    #[test]
    fn notebook_edit_reads_notebook_path() {
        let f = fixture();
        f.open(&full_spec());
        let input = json!({"notebook_path": f.root.join("src/n.ipynb").to_string_lossy()});
        assert!(f.call("NotebookEdit", input, "c1", false, SpecFlow::On).is_some());
    }
}
