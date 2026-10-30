//! Spec flow: design before code.
//!
//! A chat brainstorms a feature (`/brainstorm`), drafts a spec into the repo
//! (`/draft`, `docs/designs/<slug>.md`), has it read by four reviewers that never
//! saw the conversation (`/review`: Cảnh, Kiểm, Hiểu, Phản biện), and a person
//! approves it (`/approve`). While a folder has an open spec that is not approved,
//! [`gate::check`] refuses every change to that folder except the design files,
//! and once it is approved every chat must read the current text before its
//! first change. Design record: `plans/261027-1505-code-spec-flow/design.md`.
//!
//! The spec text lives in the repo (the agent writes it, people read and commit
//! it). What a tool must never be able to change — which spec is open, the last
//! review, the approval and the hash it was given for — lives in
//! `<SENCLAW_HOME>/specs/<key>/state.json`, written only by the daemon.

pub mod commands;
pub mod gate;
pub mod review;

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::types::GroupBinding;

/// Where specs are written, relative to the root.
pub const DESIGNS_DIR: &str = "docs/designs";
/// The heading that separates the design from the plan to build it.
pub const DETAILED_HEADING: &str = "## Detailed Implementation Plan";
/// The decision log the review step appends to; left out of the hash only when
/// it is the last section of the file.
pub const REVIEW_LOG_HEADING: &str = "## Review Log";
/// The `<spec>` block is cut here; it rides on every model call of a turn.
pub const CAPSULE_MAX_CHARS: usize = 1200;

/// `specFlow` as one chat sees it this turn (see [`effective_flow`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpecFlow {
    /// No `<spec>` block, no gate, the commands answer that the flow is off.
    Off,
    /// The gate closes a folder only once it has an open spec.
    #[default]
    On,
    /// As `On`, and a change in a folder with no spec at all is refused too.
    Required,
}

impl SpecFlow {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "on" => Some(Self::On),
            "required" => Some(Self::Required),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Required => "required",
        }
    }
}

/// What a chat runs under: `required` binds only Code sessions and project
/// chats (a chat that merely sits in a git repository keeps `on`), and a chat
/// whose person typed `/spec skip` drops back to `on` until its history is cleared.
pub fn effective_flow(configured: SpecFlow, binding: &GroupBinding) -> SpecFlow {
    match configured {
        SpecFlow::Required if !is_code_or_project(binding) || gate::is_skipped(&binding.jid) => SpecFlow::On,
        other => other,
    }
}

pub(crate) fn is_code_or_project(binding: &GroupBinding) -> bool {
    binding.group_type == "code" || binding.project_id.is_some()
}

/// One folder's record. Written only by the daemon through [`SpecStore::update`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct State {
    pub root: String,
    /// The open spec, relative to the root; empty once closed.
    #[serde(default)]
    pub active: String,
    #[serde(default)]
    pub specs: BTreeMap<String, SpecEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SpecEntry {
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<Approval>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ReviewSummary {
    pub hash: String,
    pub at: String,
    /// `design` or `brainstorm`.
    pub mode: String,
    pub complete: bool,
    pub blocker: usize,
    pub major: usize,
    pub nit: usize,
    /// Lens id → `ok` / `cut` / `unparsed` / `failed: …`.
    #[serde(default)]
    pub lenses: BTreeMap<String, String>,
    /// Report file, relative to the folder's store directory.
    #[serde(default)]
    pub report: String,
}

impl ReviewSummary {
    /// `/approve` takes this review as long as the design stays the version it read.
    pub fn clears_approval(&self) -> bool {
        self.complete && self.mode == "design" && self.blocker == 0
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Approval {
    pub hash: String,
    pub at: String,
    /// `web`, `rest`, or the channel a person typed `/approve` in.
    pub via: String,
    #[serde(default)]
    pub forced: bool,
}

/// One lock for every write of every folder's state: writes come only from a
/// person's commands and the end of a review, so contention is nil, and a
/// closure that re-reads the file under it cannot lose another writer's field.
static STATE_LOCK: Mutex<()> = Mutex::new(());

/// The daemon-side half of the spec flow.
#[derive(Debug, Clone)]
pub struct SpecStore {
    base: PathBuf,
}

impl SpecStore {
    /// `<SENCLAW_HOME>/specs`.
    pub fn default_store() -> Self {
        Self::at(crate::util::paths::senclaw_home().join("specs"))
    }

    pub fn at(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    /// The store directory of one root (`state.json`, `reviews/`).
    pub fn dir_for(&self, root: &Path) -> PathBuf {
        self.base.join(key_for(root))
    }

    pub fn load(&self, root: &Path) -> Option<State> {
        let raw = std::fs::read_to_string(self.dir_for(root).join("state.json")).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Read the state again under the lock, change it, write it back atomically.
    pub fn update<R>(&self, root: &Path, f: impl FnOnce(&mut State) -> R) -> std::io::Result<R> {
        let _guard = STATE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = self.dir_for(root);
        std::fs::create_dir_all(&dir)?;
        restrict_dir(&self.base);
        restrict_dir(&dir);
        let mut state = self.load(root).unwrap_or_default();
        state.root = root.to_string_lossy().into_owned();
        let out = f(&mut state);
        let json = serde_json::to_vec_pretty(&state).map_err(std::io::Error::other)?;
        crate::tools::file_io::write_atomic(&dir.join("state.json"), &json)?;
        Ok(out)
    }

    /// The nearest folder at or above `start` with an open spec, stopping
    /// before `$HOME` and `/` — so `cd`, a workspace switch or an absolute path
    /// in another folder cannot step around a folder's spec.
    pub fn find_root(&self, start: &Path) -> Option<(PathBuf, State)> {
        let start = normalize_path(start);
        let home = dirs::home_dir().map(|h| normalize_path(&h));
        for dir in start.ancestors() {
            if dir.parent().is_none() || home.as_deref() == Some(dir) {
                break;
            }
            if let Some(state) = self.load(dir) {
                if !state.active.is_empty() {
                    return Some((dir.to_path_buf(), state));
                }
            }
        }
        None
    }
}

#[cfg(unix)]
fn restrict_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_dir(_dir: &Path) {}

/// 16 hex characters of the sha256 of the root's canonical path.
pub fn key_for(root: &Path) -> String {
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    hex::encode(digest)[..16].to_string()
}

/// The real path a write would land on, also for a file that does not exist
/// yet (the usual case for a `Write`): each existing prefix is resolved on disk
/// (links, `/tmp` → `/private/tmp`) before the next component is applied, so a
/// `..` after a link climbs out of the link's target, as the kernel does — a
/// textual `..` first would let `/x/link/../state.json` pass for `/x/state.json`.
/// What does not exist yet is kept as written.
pub fn normalize_path(p: &Path) -> PathBuf {
    let mut cur = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if let Ok(real) = std::fs::canonicalize(&cur) {
                    cur = real;
                }
                cur.pop();
            }
            other => {
                cur.push(other.as_os_str());
                if let Ok(real) = std::fs::canonicalize(&cur) {
                    cur = real;
                }
            }
        }
    }
    cur
}

/// The hash an approval is given for: what the design says, not how the file
/// looks. Unicode NFC, LF line ends, no trailing spaces, ticked boxes read as
/// unticked, the `**Status:**` / `**Date:**` lines of the header (before the
/// first `## `) left out, and the review log left out only when it is the
/// file's last section (a `## Review Log` heading in the middle hides nothing,
/// nor does a task line that happens to say `**Status:**`).
pub fn design_hash(text: &str) -> String {
    let nfc: String = text.nfc().collect();
    let unified = nfc.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = unified.lines().collect();
    let end = lines
        .iter()
        .rposition(|l| l.starts_with("## "))
        .filter(|&i| lines[i].trim_end() == REVIEW_LOG_HEADING)
        .unwrap_or(lines.len());
    let header_end = lines.iter().position(|l| l.starts_with("## ")).unwrap_or(lines.len());
    let kept: Vec<String> = lines[..end]
        .iter()
        .enumerate()
        .filter(|(i, l)| *i >= header_end || !(l.contains("**Status:**") || l.contains("**Date:**")))
        .map(|(_, l)| untick(l.trim_end()))
        .collect();
    hex::encode(Sha256::digest(kept.join("\n").trim_end().as_bytes()))
}

fn untick(line: &str) -> String {
    let body = line.trim_start();
    let indent = &line[..line.len() - body.len()];
    for bullet in ["- ", "* "] {
        if let Some(rest) = body.strip_prefix(bullet) {
            if let Some(after) = rest.strip_prefix("[x]").or_else(|| rest.strip_prefix("[X]")) {
                return format!("{indent}{bullet}[ ]{after}");
            }
        }
    }
    line.to_string()
}

/// Where a spec stands. Computed from the file and the state every time, never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Opened with `/brainstorm`, file not written yet.
    Missing,
    /// No `## Detailed Implementation Plan` yet.
    Brief,
    Draft,
    /// `/approve` takes it: the last review read this very version, as a full
    /// spec, finished and found no blocker.
    Reviewed,
    Approved,
    /// Approved once, the design text changed since.
    Changed,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Brief => "brief",
            Self::Draft => "draft",
            Self::Reviewed => "reviewed",
            Self::Approved => "approved",
            Self::Changed => "changed",
        }
    }

    /// What a person still has to run before code unlocks. `/approve` alone is
    /// the wrong advice until the last step: it refuses a spec with no plan, no
    /// finished review of this version, or blockers in that review.
    pub fn steps_to_unlock(self) -> &'static str {
        match self {
            Self::Missing => "/brainstorm, /draft, /review and /approve",
            Self::Brief => "/draft, /review and /approve",
            Self::Draft | Self::Changed => "/review and /approve",
            Self::Reviewed | Self::Approved => "/approve",
        }
    }
}

pub fn has_detailed_plan(text: &str) -> bool {
    text.lines().any(|l| l.trim_end() == DETAILED_HEADING)
}

/// The stage and, when the file exists, its design hash.
pub fn stage_of(text: Option<&str>, entry: Option<&SpecEntry>) -> (Stage, Option<String>) {
    let Some(text) = text else { return (Stage::Missing, None) };
    let hash = design_hash(text);
    let approval = entry.and_then(|e| e.approval.as_ref());
    let stage = if !has_detailed_plan(text) {
        Stage::Brief
    } else if approval.is_some_and(|a| a.hash == hash) {
        Stage::Approved
    } else if entry.and_then(|e| e.review.as_ref()).is_some_and(|r| r.hash == hash && r.clears_approval()) {
        Stage::Reviewed
    } else if approval.is_some() {
        Stage::Changed
    } else {
        Stage::Draft
    };
    (stage, Some(hash))
}

/// `docs/designs/<YYMMDD>-<first six words, folded>.md`, cut to 40 characters
/// at a dash and numbered when taken. The daemon names the file; the agent never does.
pub fn slug_for(idea: &str, today: &str, root: &Path) -> String {
    let words: Vec<&str> = idea.split_whitespace().take(6).collect();
    let mut slug = crate::docs::slugify(&words.join(" "));
    if slug.len() > 40 {
        slug.truncate(40);
        if let Some(i) = slug.rfind('-') {
            slug.truncate(i);
        }
    }
    let base = format!("{today}-{slug}");
    let mut name = base.clone();
    let mut n = 2;
    while root.join(DESIGNS_DIR).join(format!("{name}.md")).exists() {
        name = format!("{base}-{n}");
        n += 1;
    }
    format!("{DESIGNS_DIR}/{name}.md")
}

/// The folder a chat works in, if the spec flow can use it: a Code session's
/// own folder (`allowed_work_dirs[0]` — outside a project its workspace-state
/// file is the profile's, shared by every session of that profile, so another
/// session opened later would answer for it); a project chat's own state file
/// (keyed by its jid, so its own); any other chat's current folder from the
/// profile's state file, else its first allowed folder. It must be a Code
/// session's or project's folder or a git repository, never `/` or `$HOME`.
pub fn session_dir(binding: &GroupBinding) -> Option<PathBuf> {
    session_dir_in(&crate::util::paths::senclaw_home(), binding)
}

pub fn session_dir_in(senclaw_home: &Path, binding: &GroupBinding) -> Option<PathBuf> {
    let first_allowed = || binding.allowed_work_dirs.as_ref()?.first().cloned();
    let from_state = || {
        let key = crate::projects::workspace_state_key(binding);
        let state_file = senclaw_home.join(crate::projects::workspace_state_file_name(&key));
        std::fs::read_to_string(state_file)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| v.get("currentDir").and_then(|d| d.as_str()).map(str::to_string))
            .filter(|d| !d.is_empty())
    };
    let dir = if binding.group_type == "code" && binding.project_id.is_none() {
        first_allowed()
    } else {
        from_state().or_else(first_allowed)
    }?;
    let dir = crate::checkpoints::checkpointable_dir(&dir, is_code_or_project(binding))?;
    Some(normalize_path(&dir))
}

/// The lines under `## <name>` up to the next `## ` heading.
pub fn section<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let heading = format!("## {name}");
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with("## ") {
            inside = line.trim_end() == heading;
            continue;
        }
        if inside {
            out.push(line);
        }
    }
    out
}

fn is_bullet(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("- ") || t.starts_with("* ")
}

/// Bullets that say something: "None" / "không có" placeholders are not questions.
pub fn open_questions(text: &str) -> usize {
    section(text, "Open Questions")
        .into_iter()
        .filter(|l| is_bullet(l))
        .filter(|l| {
            let body = l.trim_start()[2..].trim().trim_matches('*').trim().to_lowercase();
            !matches!(body.as_str(), "" | "none" | "none." | "không" | "không có" | "chưa có")
        })
        .count()
}

/// (done, total) over every `- [ ]` / `- [x]` line of the file.
pub fn task_counts(text: &str) -> (usize, usize) {
    let mut done = 0;
    let mut total = 0;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("- [ ]") || t.starts_with("* [ ]") {
            total += 1;
        } else if t.starts_with("- [x]") || t.starts_with("- [X]") || t.starts_with("* [x]") || t.starts_with("* [X]") {
            done += 1;
            total += 1;
        }
    }
    (done, total)
}

fn title_of(text: &str) -> Option<&str> {
    text.lines().find_map(|l| l.strip_prefix("# ")).map(str::trim)
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// The rule the `<spec>` block states for a stage.
pub fn stage_rule(stage: Stage) -> String {
    let steps = stage.steps_to_unlock();
    match stage {
        Stage::Approved => "Before your first change in this chat, Read the whole spec. Build what it says, tick \"- [x]\" \
                            as tasks finish; if the code must differ from the design, stop and tell the person."
            .to_string(),
        Stage::Changed => format!("The spec changed after approval; code is locked until the person runs {steps}."),
        _ => format!("Design phase: only files in docs/designs/ may change; code unlocks when the person runs {steps}."),
    }
}

/// The `<spec>` block a chat working in `root` carries on every model call of
/// its turn. Plain text from the repo, so `</spec` is defused like `</project`.
pub fn capsule(root: &Path, state: &State) -> Option<String> {
    if state.active.is_empty() {
        return None;
    }
    let rel = &state.active;
    let text = std::fs::read_to_string(root.join(rel)).ok();
    let entry = state.specs.get(rel);
    let (stage, hash) = stage_of(text.as_deref(), entry);
    let title = text
        .as_deref()
        .and_then(title_of)
        .map(str::to_string)
        .or_else(|| entry.map(|e| e.title.clone()))
        .unwrap_or_default();
    let mut lines = vec![format!("Design spec for this folder: {rel} — \"{title}\"")];
    let mut status = format!("Stage: {}", stage.as_str());
    if let Some(h) = &hash {
        status.push_str(&format!(" · version {}", &h[..8]));
    }
    if let Some(r) = entry.and_then(|e| e.review.as_ref()) {
        status.push_str(&format!(" · review: {} blocker, {} major, {} nit", r.blocker, r.major, r.nit));
    }
    lines.push(status);
    if let Some(text) = text.as_deref() {
        let goals: Vec<String> = section(text, "Goals")
            .into_iter()
            .filter(|l| is_bullet(l))
            .take(3)
            .map(|l| clip(l.trim(), 120))
            .collect();
        if !goals.is_empty() {
            lines.push(format!("Goals: {}", goals.join(" ")));
        }
        let next: Vec<String> = text
            .lines()
            .map(str::trim_start)
            .filter(|l| l.starts_with("- [ ]") || l.starts_with("* [ ]"))
            .take(3)
            .map(|l| clip(l, 120))
            .collect();
        if !next.is_empty() {
            lines.push(format!("Next tasks: {}", next.join(" ")));
        }
        lines.push(format!("Open questions: {}", open_questions(text)));
    }
    lines.push(format!("Rule: {}", stage_rule(stage)));
    let body = clip(&lines.join("\n"), CAPSULE_MAX_CHARS).replace("</spec", "<\\/spec");
    Some(format!("<spec>\n{body}\n</spec>\n"))
}

/// A chat binding for tests across the spec modules.
#[cfg(test)]
pub(crate) fn test_binding(jid: &str, group_type: &str, work_dir: Option<&Path>) -> GroupBinding {
    GroupBinding {
        jid: jid.into(),
        folder: "main".into(),
        name: "t".into(),
        channel: String::new(),
        group_type: group_type.into(),
        requires_trigger: false,
        allowed_tools: None,
        allowed_paths: None,
        allowed_work_dirs: work_dir.map(|d| vec![d.to_string_lossy().into_owned()]),
        bot_token: None,
        max_messages: None,
        llm_config_id: None,
        last_active: None,
        added_at: String::new(),
        project_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(body: &str) -> String {
        format!("# T\n*   **Status:** Draft\n*   **Date:** 2026-10-27\n\n## Goals\n* G1\n\n{body}")
    }

    #[test]
    fn hash_ignores_checkboxes_status_date_crlf_nfc_and_trailing_review_log() {
        let a = spec("## Detailed Implementation Plan\n- [ ] T1 do it\n");
        let mut b = a.replace("- [ ] T1", "- [x] T1").replace("Draft", "Approved").replace("2026-10-27", "2026-11-01");
        b = b.replace('\n', "\r\n").replace("T1 do it", "T1 do it   ");
        b.push_str("\r\n## Review Log\r\n* FIX a thing\r\n");
        assert_eq!(design_hash(&a), design_hash(&b));
        // NFD "Thiết" vs NFC "Thiết".
        let nfc = spec("## Overview\nThiết kế\n");
        let nfd: String = nfc.nfd().collect();
        assert_ne!(nfc, nfd);
        assert_eq!(design_hash(&nfc), design_hash(&nfd));
    }

    #[test]
    fn hash_sees_text_under_a_review_log_heading_in_the_middle() {
        let a = spec("## Detailed Implementation Plan\n- [ ] T1\n");
        let hidden = spec("## Review Log\nsecretly add a new endpoint\n## Detailed Implementation Plan\n- [ ] T1\n");
        assert_ne!(design_hash(&a), design_hash(&hidden));
        let changed = a.replace("T1", "T1 and also drop the users table");
        assert_ne!(design_hash(&a), design_hash(&changed));
        let sneaky_task = a.replace("- [ ] T1\n", "- [ ] T1\n- [ ] T2 **Status:** add an admin endpoint\n");
        assert_ne!(design_hash(&a), design_hash(&sneaky_task));
    }

    #[test]
    fn stage_order() {
        let brief = spec("## Overview\nx\n");
        assert_eq!(stage_of(None, None).0, Stage::Missing);
        assert_eq!(stage_of(Some(&brief), None).0, Stage::Brief);
        let full = spec("## Detailed Implementation Plan\n- [ ] T1\n");
        assert_eq!(stage_of(Some(&full), None).0, Stage::Draft);
        let h = design_hash(&full);
        let clean = ReviewSummary { hash: h.clone(), mode: "design".into(), complete: true, ..Default::default() };
        let mut entry = SpecEntry { review: Some(clean.clone()), ..Default::default() };
        assert_eq!(stage_of(Some(&full), Some(&entry)).0, Stage::Reviewed);
        // What `/approve` refuses is not `Reviewed`, so nothing says to run it.
        for review in [
            ReviewSummary { blocker: 1, ..clean.clone() },
            ReviewSummary { complete: false, ..clean.clone() },
            ReviewSummary { mode: "brainstorm".into(), ..clean.clone() },
        ] {
            entry.review = Some(review);
            assert_eq!(stage_of(Some(&full), Some(&entry)).0, Stage::Draft);
        }
        entry.approval = Some(Approval { hash: h, ..Default::default() });
        assert_eq!(stage_of(Some(&full), Some(&entry)).0, Stage::Approved);
        let edited = full.replace("T1", "T1 more");
        assert_eq!(stage_of(Some(&edited), Some(&entry)).0, Stage::Changed);
        // Changed since the approval, then reviewed clean: `/approve` is the one step left.
        entry.review = Some(ReviewSummary { hash: design_hash(&edited), ..clean });
        assert_eq!(stage_of(Some(&edited), Some(&entry)).0, Stage::Reviewed);
        let ticked = full.replace("- [ ] T1", "- [x] T1");
        assert_eq!(stage_of(Some(&ticked), Some(&entry)).0, Stage::Approved);
    }

    #[test]
    fn slug_folds_vietnamese_and_numbers_duplicates() {
        let root = tempfile::tempdir().unwrap();
        let first = slug_for("Thêm webhook khi đơn hàng đổi trạng thái", "261027", root.path());
        assert_eq!(first, "docs/designs/261027-them-webhook-khi-don-hang-doi.md");
        std::fs::create_dir_all(root.path().join(DESIGNS_DIR)).unwrap();
        std::fs::write(root.path().join(&first), "x").unwrap();
        let second = slug_for("Thêm webhook khi đơn hàng đổi trạng thái", "261027", root.path());
        assert_eq!(second, "docs/designs/261027-them-webhook-khi-don-hang-doi-2.md");
        let long = slug_for("supercalifragilisticexpialidocious antidisestablishmentarianism x", "261027", root.path());
        assert!(long.len() <= "docs/designs/261027-".len() + 40 + ".md".len(), "{long}");
    }

    #[test]
    fn find_root_walks_up_from_a_subfolder_and_a_missing_file() {
        let store_dir = tempfile::tempdir().unwrap();
        let store = SpecStore::at(store_dir.path());
        let repo = tempfile::tempdir().unwrap();
        let root = normalize_path(repo.path());
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        assert!(store.find_root(&root.join("src/deep")).is_none());
        store.update(&root, |s| s.active = "docs/designs/x.md".into()).unwrap();
        let (found, state) = store.find_root(&root.join("src/deep/not-yet.rs")).unwrap();
        assert_eq!(found, root);
        assert_eq!(state.active, "docs/designs/x.md");
        // A closed spec does not count.
        store.update(&root, |s| s.active.clear()).unwrap();
        assert!(store.find_root(&root.join("src")).is_none());
    }

    #[test]
    fn normalize_resolves_dotdot_and_links_as_the_kernel_does() {
        let repo = tempfile::tempdir().unwrap();
        let root = normalize_path(repo.path());
        std::fs::create_dir_all(root.join("docs/designs")).unwrap();
        let sneaky = root.join("docs/designs/../../src/main.rs");
        assert_eq!(normalize_path(&sneaky), root.join("src/main.rs"));
        #[cfg(unix)]
        {
            // `link/..` is the parent of the link's target, not the folder holding the link.
            let store = tempfile::tempdir().unwrap();
            let store = normalize_path(store.path());
            std::fs::create_dir_all(store.join("k/reviews")).unwrap();
            std::os::unix::fs::symlink(store.join("k/reviews"), root.join("lnk")).unwrap();
            assert_eq!(normalize_path(&root.join("lnk/../state.json")), store.join("k/state.json"));
        }
    }

    #[test]
    fn update_keeps_fields_written_by_another_writer() {
        let store_dir = tempfile::tempdir().unwrap();
        let store = SpecStore::at(store_dir.path());
        let root = PathBuf::from("/nowhere/repo");
        store.update(&root, |s| s.specs.entry("a.md".into()).or_default().title = "A".into()).unwrap();
        store
            .update(&root, |s| {
                s.specs.entry("a.md".into()).or_default().approval = Some(Approval { hash: "h".into(), ..Default::default() })
            })
            .unwrap();
        let state = store.load(&root).unwrap();
        assert_eq!(state.specs["a.md"].title, "A");
        assert_eq!(state.specs["a.md"].approval.as_ref().unwrap().hash, "h");
        assert_eq!(state.root, "/nowhere/repo");
    }

    #[test]
    fn capsule_names_the_spec_stage_and_rule_and_stays_short() {
        let store_dir = tempfile::tempdir().unwrap();
        let store = SpecStore::at(store_dir.path());
        let repo = tempfile::tempdir().unwrap();
        let root = normalize_path(repo.path());
        std::fs::create_dir_all(root.join(DESIGNS_DIR)).unwrap();
        let text = format!(
            "# Webhook\n## Goals\n* Send it\n## Open Questions\n* Which retry?\n* None\n{DETAILED_HEADING}\n- [x] T1 a\n- [ ] T2 b\n{}",
            "x\n".repeat(2000)
        );
        std::fs::write(root.join("docs/designs/w.md"), &text).unwrap();
        store.update(&root, |s| s.active = "docs/designs/w.md".into()).unwrap();
        let state = store.load(&root).unwrap();
        let block = capsule(&root, &state).unwrap();
        assert!(block.contains("docs/designs/w.md — \"Webhook\""), "{block}");
        assert!(block.contains("Stage: draft"), "{block}");
        assert!(block.contains("Next tasks: - [ ] T2 b"), "{block}");
        assert!(block.contains("Open questions: 1"), "{block}");
        assert!(block.contains("Design phase"), "{block}");
        assert!(block.chars().count() <= CAPSULE_MAX_CHARS + 20);
    }

    #[test]
    fn a_code_session_uses_its_own_folder_not_the_profiles_state_file() {
        let home = tempfile::tempdir().unwrap();
        let mine = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        // The profile's shared state file points at another session's folder.
        let b = test_binding("code:a", "code", Some(mine.path()));
        let key = crate::projects::workspace_state_key(&b);
        std::fs::write(
            home.path().join(crate::projects::workspace_state_file_name(&key)),
            serde_json::json!({"currentDir": other.path().to_string_lossy()}).to_string(),
        )
        .unwrap();
        assert_eq!(session_dir_in(home.path(), &b), Some(normalize_path(mine.path())));
        // A plain chat follows the state file (its folder is the profile's).
        std::process::Command::new("git").arg("init").arg("-q").arg(other.path()).status().unwrap();
        let chat = test_binding("web:a", "chat", None);
        assert_eq!(session_dir_in(home.path(), &chat), Some(normalize_path(other.path())));
    }

    #[test]
    fn required_binds_only_code_and_project_chats() {
        let mut b = test_binding("web:x", "chat", None);
        assert_eq!(effective_flow(SpecFlow::Required, &b), SpecFlow::On);
        b.group_type = "code".into();
        assert_eq!(effective_flow(SpecFlow::Required, &b), SpecFlow::Required);
        assert_eq!(effective_flow(SpecFlow::Off, &b), SpecFlow::Off);
    }
}
