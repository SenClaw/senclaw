//! The five chat commands of the spec flow.
//!
//! `/approve` and `/spec …` are answered by the gateway with no model call
//! ([`dispatch`]) — only a message a person sent through the gateway (WS, a
//! channel, the Code-session REST chat) reaches them, never a schedule, a DAG or
//! a tool. `/brainstorm`, `/draft` and `/review` are checked there too, then go
//! to the agent: the pool turns them into the instructions for that turn
//! ([`agent_prompt`]), running the review first for `/review`.

use std::path::{Path, PathBuf};

use super::review::{self, LensModel};
use super::{
    gate, has_detailed_plan, open_questions, session_dir, stage_of, task_counts, SpecEntry, SpecFlow,
    SpecStore, State, Stage, DESIGNS_DIR,
};
use crate::types::GroupBinding;

pub const OFF: &str = "Spec flow is off (Settings → Agent behavior → Spec flow).";
pub const NO_FOLDER: &str =
    "This chat has no project folder (a Code session, a project chat or a git repository is needed).";
pub const SKIPPED: &str = "Spec check skipped for this chat until its history is cleared.";
const BRAINSTORM_USAGE: &str = "Usage: /brainstorm <idea> — say in a sentence what you want built.";
const SPEC_USAGE: &str = "Usage: /spec · /spec use <docs/designs/file.md> · /spec done · /spec skip";

pub fn busy(path: &str) -> String {
    format!("{path} is still open. Continue it with /draft or /review, or close it with /spec done.")
}

pub fn no_spec(root: &Path) -> String {
    format!("No open spec in {}. Start one with /brainstorm <idea>.", root.display())
}

fn approve_needs(path: &str, why: &str, can_force: bool) -> String {
    if can_force {
        format!("Cannot approve {path}: {why}. /approve force approves it anyway.")
    } else {
        format!("Cannot approve {path}: {why}.")
    }
}

const NAMES: [&str; 5] = ["/brainstorm", "/draft", "/review", "/approve", "/spec"];

/// `("/review", "docs/designs/x.md")` for a message that is one of the five
/// commands: lower case, then a space or the end.
fn split_command(text: &str) -> Option<(&'static str, &str)> {
    let t = text.trim();
    NAMES.into_iter().find_map(|name| {
        let rest = t.strip_prefix(name)?;
        (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| (name, rest.trim()))
    })
}

/// A command the agent carries out. Such a message must not join a running
/// turn as mid-turn input: it waits in the queue so the pool can rewrite it.
pub fn is_agent_command(text: &str) -> bool {
    matches!(split_command(text), Some(("/brainstorm" | "/draft" | "/review", _)))
}

#[derive(Debug, Clone, PartialEq)]
pub enum AgentCommand {
    Brainstorm { idea: String },
    Draft { path: Option<String> },
    Review { path: Option<String> },
}

pub fn parse_agent_command(text: &str) -> Option<AgentCommand> {
    let (name, args) = split_command(text)?;
    let path = (!args.is_empty()).then(|| args.to_string());
    match name {
        "/brainstorm" => Some(AgentCommand::Brainstorm { idea: args.to_string() }),
        "/draft" => Some(AgentCommand::Draft { path }),
        "/review" => Some(AgentCommand::Review { path }),
        _ => None,
    }
}

/// Gateway entry: the reply for `/approve`, `/spec …` and for a spec command
/// that cannot run; `None` lets the message go on (not a command, or an agent
/// command that is fine).
pub fn dispatch(text: &str, binding: &GroupBinding, via: &str, configured: SpecFlow) -> Option<String> {
    split_command(text)?;
    dispatch_in(text, binding, via, configured, &SpecStore::default_store(), session_dir(binding))
}

pub fn dispatch_in(
    text: &str,
    binding: &GroupBinding,
    via: &str,
    configured: SpecFlow,
    store: &SpecStore,
    dir: Option<PathBuf>,
) -> Option<String> {
    let (name, args) = split_command(text)?;
    if configured == SpecFlow::Off {
        return Some(OFF.into());
    }
    if name == "/brainstorm" && args.is_empty() {
        return Some(BRAINSTORM_USAGE.into());
    }
    if name == "/spec" && args == "skip" {
        if configured != SpecFlow::Required || !super::is_code_or_project(binding) {
            return Some(format!(
                "Not needed: this chat is not held to a spec first (specFlow is {}).",
                configured.as_str()
            ));
        }
        gate::skip(&binding.jid);
        return Some(SKIPPED.into());
    }
    let Some(dir) = dir else { return Some(NO_FOLDER.into()) };
    let found = store.find_root(&dir);
    match name {
        "/brainstorm" => match &found {
            Some((root, state)) if root.join(&state.active).exists() => Some(busy(&state.active)),
            _ => None,
        },
        // A path given here comes from a person, so it may switch the open spec;
        // the pool, which schedules and DAGs also reach, never switches it.
        "/draft" | "/review" => match resolve(store, &dir, (!args.is_empty()).then_some(args)) {
            Ok((root, rel)) => match store.update(&root, |s| open_spec(s, &rel, "")) {
                Ok(()) => None,
                Err(e) => Some(format!("Cannot open {rel}: {e}")),
            },
            Err(reply) => Some(reply),
        },
        "/approve" => Some(match args {
            "" => approve(store, &dir, via, false),
            "force" => approve(store, &dir, via, true),
            _ => "Usage: /approve · /approve force".into(),
        }),
        _ => Some(spec_command(store, &dir, args)),
    }
}

/// The spec a `/draft` or `/review` works on: the one given, else the folder's
/// open spec. The file must exist.
fn resolve(store: &SpecStore, dir: &Path, given: Option<&str>) -> Result<(PathBuf, String), String> {
    let found = store.find_root(dir);
    if let Some(given) = given {
        let root = found.map(|(r, _)| r).unwrap_or_else(|| dir.to_path_buf());
        let rel = check_design_path(&root, given)?;
        return Ok((root, rel));
    }
    let Some((root, state)) = found else { return Err(no_spec(dir)) };
    if !root.join(&state.active).exists() {
        return Err(format!("{} has not been written yet: finish /brainstorm first.", state.active));
    }
    Ok((root, state.active))
}

/// [`resolve`], held to the open spec: the pool runs for schedules, DAGs and
/// kanban too, and none of them may switch which spec a folder is held to — a
/// scheduled `/draft docs/designs/old.md` would reopen an old approved spec
/// with no person involved. The gateway opens a person's path before this runs.
fn resolve_open(store: &SpecStore, dir: &Path, given: Option<&str>) -> Result<(PathBuf, String), String> {
    let (root, rel) = resolve(store, dir, given)?;
    let open = store.load(&root).map(|s| s.active).unwrap_or_default();
    if open != rel {
        return Err(format!("{rel} is not the open spec; only the person can switch it (/spec use {rel})."));
    }
    Ok((root, rel))
}

/// A path to an existing `docs/designs/*.md` file of `root`, relative to it.
fn check_design_path(root: &Path, given: &str) -> Result<String, String> {
    let given = given.trim().trim_start_matches("./");
    let abs = if Path::new(given).is_absolute() { PathBuf::from(given) } else { root.join(given) };
    let abs = super::normalize_path(&abs);
    let ok = abs.starts_with(root.join(DESIGNS_DIR)) && abs.extension().is_some_and(|e| e == "md") && abs.is_file();
    match abs.strip_prefix(root) {
        Ok(rel) if ok => Ok(rel.to_string_lossy().into_owned()),
        _ => Err(format!("Not a spec: {given} (a .md file in {DESIGNS_DIR}/ of {} is needed).", root.display())),
    }
}

fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

fn approve(store: &SpecStore, dir: &Path, via: &str, force: bool) -> String {
    let Some((root, state)) = store.find_root(dir) else { return no_spec(dir) };
    let rel = state.active.clone();
    let Ok(text) = std::fs::read_to_string(root.join(&rel)) else {
        return approve_needs(&rel, "the file does not exist", false);
    };
    if !has_detailed_plan(&text) {
        return approve_needs(&rel, "it has no \"## Detailed Implementation Plan\" yet (run /draft)", false);
    }
    let entry = state.specs.get(&rel);
    let (stage, hash) = stage_of(Some(&text), entry);
    let hash = hash.unwrap_or_default();
    if stage == Stage::Approved {
        return format!("{rel} is already approved (version {}).", &hash[..8]);
    }
    if !force {
        let review = entry.and_then(|e| e.review.as_ref()).filter(|r| r.hash == hash && r.mode == "design");
        let Some(review) = review else {
            return approve_needs(&rel, "it has not been reviewed in its current version (run /review)", true);
        };
        if !review.complete {
            let unfinished: Vec<String> =
                review.lenses.iter().filter(|(_, s)| s.as_str() != "ok").map(|(l, s)| format!("{l}: {s}")).collect();
            return approve_needs(&rel, &format!("the last review did not finish ({})", unfinished.join(", ")), true);
        }
        if review.blocker > 0 {
            return approve_needs(&rel, &format!("the last review has {} blocker(s)", review.blocker), true);
        }
    }
    let approval = super::Approval { hash: hash.clone(), at: now(), via: via.into(), forced: force };
    if let Err(e) = store.update(&root, |s| s.specs.entry(rel.clone()).or_default().approval = Some(approval)) {
        return format!("Cannot approve {rel}: the record could not be saved ({e}).");
    }
    format!(
        "Approved {rel} (version {}{}). Code changes in {} are unlocked; every chat reads this spec before its first \
         change. Editing the design text locks them again until /review and /approve.",
        &hash[..8],
        if force { ", forced" } else { "" },
        root.display()
    )
}

fn spec_command(store: &SpecStore, dir: &Path, args: &str) -> String {
    let (sub, rest) = args.split_once(char::is_whitespace).map(|(a, b)| (a, b.trim())).unwrap_or((args, ""));
    match sub {
        "" => match store.find_root(dir) {
            Some((root, state)) => status_text_in(&root, &state, Some(store)),
            None => no_spec(dir),
        },
        "done" => {
            let Some((root, state)) = store.find_root(dir) else { return no_spec(dir) };
            match store.update(&root, |s| s.active.clear()) {
                Ok(()) => format!(
                    "Closed {}. The file stays in the repo; changes in {} are no longer held by it.",
                    state.active,
                    root.display()
                ),
                Err(e) => format!("Cannot close {}: {e}", state.active),
            }
        }
        "use" if !rest.is_empty() => {
            let root = store.find_root(dir).map(|(r, _)| r).unwrap_or_else(|| dir.to_path_buf());
            let rel = match check_design_path(&root, rest) {
                Ok(rel) => rel,
                Err(reply) => return reply,
            };
            match store.update(&root, |s| open_spec(s, &rel, "")) {
                Ok(()) => format!("{rel} is now the open spec of {}.", root.display()),
                Err(e) => format!("Cannot open {rel}: {e}"),
            }
        }
        _ => SPEC_USAGE.into(),
    }
}

fn open_spec(state: &mut State, rel: &str, title: &str) {
    state.active = rel.to_string();
    let entry = state.specs.entry(rel.to_string()).or_insert_with(|| SpecEntry { created_at: now(), ..Default::default() });
    if entry.title.is_empty() {
        entry.title = title.to_string();
    }
}

/// `/spec`: five lines a person can read at a glance.
pub fn status_text(root: &Path, state: &State) -> String {
    status_text_in(root, state, None)
}

/// As [`status_text`], with the full path of the last review's report when the
/// store is known (the person can open it; the agent is never pointed there).
pub fn status_text_in(root: &Path, state: &State, store: Option<&SpecStore>) -> String {
    let rel = &state.active;
    let text = std::fs::read_to_string(root.join(rel)).ok();
    let entry = state.specs.get(rel);
    let (stage, hash) = stage_of(text.as_deref(), entry);
    let title = text
        .as_deref()
        .and_then(|t| t.lines().find_map(|l| l.strip_prefix("# ")))
        .map(str::trim)
        .or(entry.map(|e| e.title.as_str()))
        .unwrap_or("");
    let mut lines = vec![format!("{rel} — {title}")];
    lines.push(match &hash {
        Some(h) => format!("stage: {} · version {}", stage.as_str(), &h[..8]),
        None => format!("stage: {}", stage.as_str()),
    });
    lines.push(match entry.and_then(|e| e.review.as_ref()) {
        Some(r) => {
            let mut line = format!(
                "review: {} · {} blocker, {} major, {} nit · {}",
                r.at,
                r.blocker,
                r.major,
                r.nit,
                if r.complete { "complete" } else { "incomplete" }
            );
            if let (Some(store), false) = (store, r.report.is_empty()) {
                line.push_str(&format!(" · report {}", store.dir_for(root).join(&r.report).display()));
            }
            line
        }
        None => "review: none".into(),
    });
    lines.push(match entry.and_then(|e| e.approval.as_ref()) {
        Some(a) => format!("approval: {} via {}{}", a.at, a.via, if a.forced { " (forced)" } else { "" }),
        None => "approval: none".into(),
    });
    if let Some(text) = &text {
        let (done, total) = task_counts(text);
        lines.push(format!("tasks: {done}/{total} · open questions: {}", open_questions(text)));
    }
    lines.join("\n")
}

/// The turn the agent gets instead of the bare command, or `None` when the
/// message is not one of the three (or the flow is off: the words then go to
/// the agent as written). `/review` runs the four lenses before returning.
pub async fn agent_prompt(
    text: &str,
    binding: &GroupBinding,
    configured: SpecFlow,
    store: &SpecStore,
    model: &dyn LensModel,
) -> Option<String> {
    let command = parse_agent_command(text)?;
    if configured == SpecFlow::Off {
        return None;
    }
    Some(agent_prompt_in(text, command, session_dir(binding), store, model).await)
}

pub async fn agent_prompt_in(
    text: &str,
    command: AgentCommand,
    dir: Option<PathBuf>,
    store: &SpecStore,
    model: &dyn LensModel,
) -> String {
    let Some(dir) = dir else { return notice(text, NO_FOLDER) };
    let today = chrono::Local::now();
    match command {
        AgentCommand::Brainstorm { idea } => {
            if idea.is_empty() {
                return notice(text, BRAINSTORM_USAGE);
            }
            let found = store.find_root(&dir);
            if let Some((root, state)) = &found {
                if root.join(&state.active).exists() {
                    return notice(text, &busy(&state.active));
                }
            }
            let root = found.map(|(r, _)| r).unwrap_or(dir);
            let rel = super::slug_for(&idea, &today.format("%y%m%d").to_string(), &root);
            let title: String = idea.chars().take(80).collect();
            if let Err(e) = store.update(&root, |s| open_spec(s, &rel, &title)) {
                return notice(text, &format!("The spec could not be opened: {e}"));
            }
            brainstorm_prompt(text, &rel, &today.format("%Y-%m-%d").to_string())
        }
        AgentCommand::Draft { path } => match resolve_open(store, &dir, path.as_deref()) {
            Ok((root, rel)) => draft_prompt(text, &rel, crate::repo_map::commands::render(&root).as_deref()),
            Err(reply) => notice(text, &reply),
        },
        AgentCommand::Review { path } => match resolve_open(store, &dir, path.as_deref()) {
            Ok((root, rel)) => {
                let Ok(spec) = std::fs::read_to_string(root.join(&rel)) else {
                    return notice(text, &format!("{rel} could not be read."));
                };
                let result = review::run(&root, &rel, &spec, model).await;
                let report = save_report(store, &root, &rel, &result);
                let summary = result.summary(&report);
                if let Err(e) = store.update(&root, |s| s.specs.entry(rel.clone()).or_default().review = Some(summary)) {
                    tracing::warn!("[specs] review of {rel} not recorded: {e}");
                    return notice(text, &format!("The review of {rel} could not be recorded ({e}), so /approve cannot use it. Run /review again."));
                }
                review_prompt(text, &rel, &result)
            }
            Err(reply) => notice(text, &reply),
        },
    }
}

fn save_report(store: &SpecStore, root: &Path, rel: &str, result: &review::Review) -> String {
    let stem = Path::new(rel).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "spec".into());
    let name = format!("reviews/{stem}-{}.md", chrono::Local::now().format("%Y%m%dT%H%M%S"));
    let path = store.dir_for(root).join(&name);
    let written = path
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|_| crate::tools::file_io::write_atomic(&path, result.render_report().as_bytes()));
    if let Err(e) = written {
        tracing::warn!("[specs] review report {} not written: {e}", path.display());
    }
    name
}

/// A turn this module wrote (it starts with the step's header).
pub fn is_rewritten(prompt: &str) -> bool {
    prompt.starts_with("[Spec flow — ")
}

/// The person's own words in a turn this module wrote (between `<request>`
/// tags), else the prompt itself: what to read the person's language from — the
/// rest of a rewritten turn is English instructions and Vietnamese lens names.
pub fn request_of(prompt: &str) -> &str {
    if !is_rewritten(prompt) {
        return prompt;
    }
    prompt
        .split_once("<request>\n")
        .and_then(|(_, rest)| rest.split_once("\n</request>"))
        .map(|(request, _)| request)
        .unwrap_or(prompt)
}

/// The turn after a `/review`: the agent is to act on the findings.
pub fn is_review_results(prompt: &str) -> bool {
    prompt.starts_with("[Spec flow — REVIEW RESULTS]")
}

/// What the spec looked like when the review-results turn began, to tell at
/// its end whether the agent changed it. gemma-4-e4b went three review rounds
/// without touching the spec and said each time that it had fixed everything.
#[derive(Debug)]
pub struct TriageWatch {
    path: PathBuf,
    rel: String,
    before: String,
    reviewed_hash: String,
    blockers: usize,
    /// `/approve` takes this review, so the gate holds the spec while this lives.
    clears_approval: bool,
    chat_jid: String,
}

impl Drop for TriageWatch {
    fn drop(&mut self) {
        if self.clears_approval {
            self.release_design();
        }
    }
}

impl TriageWatch {
    /// The open spec of the folder `working_dir` is in, with its last review.
    pub fn start(working_dir: &str, chat_jid: &str, store: &SpecStore) -> Option<Self> {
        let (root, state) = store.find_root(Path::new(working_dir))?;
        let review = state.specs.get(&state.active)?.review.clone()?;
        let path = root.join(&state.active);
        let before = std::fs::read_to_string(&path).ok()?;
        // The record can be of an older version (another chat's review, a failed
        // write): then it says nothing about the spec this turn is reporting on.
        let current = review.hash == super::design_hash(&before);
        let clears_approval = current && review.clears_approval();
        if clears_approval {
            gate::hold_design(chat_jid, path.clone());
        }
        Some(Self {
            path,
            rel: state.active,
            before,
            reviewed_hash: review.hash,
            blockers: if current { review.blocker } else { 0 },
            clears_approval,
            chat_jid: chat_jid.to_string(),
        })
    }

    /// This turn only reports: the review stands and the spec stays as it is.
    pub fn holds_design(&self) -> bool {
        self.clears_approval
    }

    /// The person spoke during the turn: a change they ask for now is theirs to make.
    pub fn release_design(&self) {
        gate::release_design(&self.chat_jid);
    }

    /// The file is byte for byte what it was when the turn began, and the review
    /// it answers found blockers: the agent has not acted on them.
    pub fn untouched_with_blockers(&self) -> bool {
        self.blockers > 0 && std::fs::read_to_string(&self.path).is_ok_and(|now| now == self.before)
    }

    pub fn nudge(&self) -> String {
        format!(
            "{}You have not changed {}, and its review found {} blocker(s). Fix them now: call Edit or Write on {} and change \
             the design text the findings point at (or record under ## Review Log why a finding is wrong). Do not touch any \
             other file: code stays locked until the person approves.",
            crate::zen_core::stuck::AUTOMATIC,
            self.rel,
            self.blockers,
            self.rel
        )
    }

    /// One line for the person, from the files and not from the model: did the
    /// design change since the review, and what that means for `/approve`.
    pub fn note(&self, vietnamese: bool) -> String {
        let now = std::fs::read_to_string(&self.path).map(|t| super::design_hash(&t)).unwrap_or_default();
        let (was, is) = (&self.reviewed_hash[..8.min(self.reviewed_hash.len())], &now[..8.min(now.len())]);
        match (now == self.reviewed_hash, vietnamese) {
            (true, true) if self.blockers > 0 => format!(
                "📋 {}: nội dung thiết kế không đổi (phiên bản {was}), vẫn còn {} blocker — /approve sẽ từ chối cho tới khi spec được sửa và /review lại (hoặc /approve force).",
                self.rel, self.blockers
            ),
            (true, false) if self.blockers > 0 => format!(
                "📋 {}: the design text is unchanged (version {was}) and still has {} blocker(s) — /approve refuses until it is fixed and reviewed again (or /approve force).",
                self.rel, self.blockers
            ),
            (true, true) if self.clears_approval => format!(
                "📋 {}: phiên bản {was} không có blocker và thiết kế giữ nguyên — /approve sẽ mở khoá code; muốn sửa thì nói rõ phần cần sửa, xong /review lại.",
                self.rel
            ),
            (true, false) if self.clears_approval => format!(
                "📋 {}: version {was} has no blockers and the design is unchanged — /approve unlocks the code; to change something, say what, then /review again.",
                self.rel
            ),
            (true, true) => format!("📋 {}: nội dung thiết kế không đổi (phiên bản {was}).", self.rel),
            (true, false) => format!("📋 {}: the design text is unchanged (version {was}).", self.rel),
            (false, true) => format!("📋 {}: thiết kế đã đổi từ phiên bản {was} sang {is} — chạy /review để soát bản mới.", self.rel),
            (false, false) => format!("📋 {}: the design changed from version {was} to {is} — run /review to check the new version.", self.rel),
        }
    }
}

/// What the person reads after an answer in which the spec gate refused changes
/// that nothing applied later: `targets` are the files (or `Bash`), `reason` the
/// gate's own words. A model that was refused still tends to say it made the change.
pub fn blocked_warning(targets: &[String], reason: &str, vietnamese: bool) -> String {
    let list = targets.join(", ");
    let reason = reason.trim_start_matches(gate::REFUSAL_PREFIX).trim();
    let reason: String = reason.lines().next().unwrap_or("").chars().take(240).collect();
    if vietnamese {
        format!("⚠️ Luồng spec đã chặn — chưa có thay đổi nào được áp dụng cho: {list}. Lý do: {reason}")
    } else {
        format!("⚠️ Blocked by the spec flow — nothing was changed in: {list}. Reason: {reason}")
    }
}

fn header(kind: &str, text: &str) -> String {
    format!("[Spec flow — {kind}] The person ran a spec command. Their message, verbatim:\n<request>\n{}\n</request>\n", text.trim())
}

/// A command that cannot run: the agent only passes the reason on.
fn notice(text: &str, reason: &str) -> String {
    format!(
        "{}It cannot run: {reason}\nTell the person exactly that, in their language, and do nothing else.",
        header("NOTICE", text)
    )
}

pub fn brainstorm_prompt(text: &str, rel: &str, date: &str) -> String {
    format!(
        "{}Spec file for this idea: {rel} (not written yet).\n\
         Rules for this step:\n\
         - Source files are locked until the person approves a spec; you may read code.\n\
         - Read the code the idea touches before you ask anything.\n\
         - Ask ONE question per reply: the root problem, who or what uses it, what must not break, what is out of scope.\n\
         - When you can state the idea in 2-3 sentences, ask the person to confirm them.\n\
         - After they confirm, Write {rel} with exactly these lines as its headings, nothing added to them:\n\
         # <title>\n*   **Status:** Brief\n*   **Date:** {date}\n## Overview\n## The Problem\n## Goals\n## Non-Goals\n\
         ## Open Questions\n## Anchored Hypothesis\n## Options\n\
         - Under ## Anchored Hypothesis put the 2-3 sentences the person confirmed; under ## Options, 2-3 approaches \
         with their pros and cons.\n\
         - Then give the person the path and the next steps: the review command to stress-test the idea, or the draft \
         command to write the full spec.\n\
         Reply in the person's language.",
        header("BRAINSTORM", text)
    )
}

pub fn draft_prompt(text: &str, rel: &str, commands: Option<&str>) -> String {
    let commands = commands.map(|c| format!("{}\n", c.trim())).unwrap_or_default();
    format!(
        "{}Spec file: {rel}\n{commands}\
         Rules for this step:\n\
         - Source files stay locked; only files in {DESIGNS_DIR}/ may change.\n\
         - Read {rel}. Turn it into the full spec with the template below, keeping what was agreed.\n\
         - First write everything from Overview to Risks, then stop and ask the person ONE question about it. Only \
         after they agree, write the Detailed Implementation Plan.\n\
         - Every target file is a line `#### N. `path` (Create|Modify|Delete)` followed by what changes in it.\n\
         - Under ### Tasks, one line per task: `- [ ] T1 <what to do, in which file>` — a list line, never a heading.\n\
         - Under ## Critical User Journeys, each journey as Given / When / Then. ## Review Log stays the last section.\n\
         - Commands in the Verification Plan come from the project's commands; no real code, only signatures and \
         schemas; unknowns go under Open Questions.\n\
         Template — use exactly these lines as the headings, nothing added to them:\n\
         # <title>\n*   **Status:** Draft\n*   **Date:** <today>\n## Overview\n## The Problem\n## Goals\n## Non-Goals\n\
         ## Open Questions\n## Critical User Journeys\n## Technical Plan\n\
         ## Alternatives Considered\n## Risks\n## Detailed Implementation Plan\n### Target Files\n\
         ### Tasks\n### Verification Plan\n## Review Log\n\
         Reply in the person's language.",
        header("DRAFT", text)
    )
}

pub fn review_prompt(text: &str, rel: &str, result: &review::Review) -> String {
    let rules = if result.summary("").clears_approval() {
        format!(
            "- This version has no blockers, so it can be approved as it stands. Leave {rel} exactly as it is: a change to \
             its design makes this review stale and the person would have to review again. Touch no file and call no tool.\n\
             - Tell the person: the counts, each MAJOR in one line with what you recommend, and that the choice is theirs: \
             /approve unlocks the code now, or they name the findings to fix and /review then checks the new version.\n"
        )
    } else {
        let next = match result.mode {
            review::Mode::Design => Stage::Draft,
            review::Mode::Brainstorm => Stage::Brief,
        };
        format!(
            "- Code stays locked until the person approves: do not touch any file but {rel}. Your next tool call is Edit or \
             Write on {rel}.\n\
             - For each BLOCKER and MAJOR choose FIX (edit the spec), CONTEST (only with evidence a reviewer could read in \
             the spec or the code — never for a Hiểu finding), or ESCALATE (a choice only the person can make: ask them, \
             one question per reply).\n\
             - NIT: fix it if trivial, else DECLINE.\n\
             - Add one line per decision under ## Review Log, and keep that section the last one in the file.\n\
             - Then tell the person: the counts, what you changed, what needs their decision, and that code unlocks only \
             after {}, in that order.\n",
            next.steps_to_unlock()
        )
    };
    format!(
        "{}Four reviewers who never saw this conversation read {rel} (version {}, {} review): Cảnh = zoom out, \
         Kiểm = can it be built from this and does it match the code, Hiểu = a newcomer's reading, Phản biện = critic.\n\
         Findings:\n{}\n(The person can see the full report with the spec status command.)\n\
         Rules for this step:\n\
         {rules}\
         Reply in the person's language.",
        header("REVIEW RESULTS", text),
        &result.hash[..8.min(result.hash.len())],
        result.mode.as_str(),
        result.findings_for_prompt()
    )
}

#[cfg(test)]
mod tests {
    use super::super::{design_hash, normalize_path, test_binding, ReviewSummary, DETAILED_HEADING};
    use super::*;
    use std::sync::Mutex;

    struct Env {
        _store_dir: tempfile::TempDir,
        _repo: tempfile::TempDir,
        store: SpecStore,
        root: PathBuf,
    }

    fn env() -> Env {
        let store_dir = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let root = normalize_path(repo.path());
        std::fs::create_dir_all(root.join(DESIGNS_DIR)).unwrap();
        Env { store: SpecStore::at(store_dir.path()), _store_dir: store_dir, _repo: repo, root }
    }

    impl Env {
        fn run(&self, text: &str, jid: &str, flow: SpecFlow) -> Option<String> {
            let b = test_binding(jid, "code", Some(&self.root));
            dispatch_in(text, &b, "web", flow, &self.store, Some(self.root.clone()))
        }
    }

    struct AllNone(Mutex<usize>);

    #[async_trait::async_trait]
    impl LensModel for AllNone {
        async fn ask(&self, _s: &str, _u: &str) -> Result<(String, bool), String> {
            *self.0.lock().unwrap() += 1;
            Ok(("NONE".into(), false))
        }
    }

    #[test]
    fn commands_are_recognised_only_as_whole_words() {
        assert!(is_agent_command("/review"));
        assert!(is_agent_command("  /brainstorm add webhooks"));
        assert!(!is_agent_command("/reviewer please"));
        assert!(!is_agent_command("/approve"));
        assert!(!is_agent_command("please /review it"));
        assert_eq!(parse_agent_command("/draft docs/designs/a.md"), Some(AgentCommand::Draft { path: Some("docs/designs/a.md".into()) }));
        let b = test_binding("c", "code", None);
        assert!(dispatch_in("hello", &b, "web", SpecFlow::On, &SpecStore::at("/nowhere"), None).is_none());
    }

    #[test]
    fn off_replies_off() {
        let e = env();
        assert_eq!(e.run("/approve", "c", SpecFlow::Off).as_deref(), Some(OFF));
        assert_eq!(e.run("/brainstorm x", "c", SpecFlow::Off).as_deref(), Some(OFF));
    }

    #[test]
    fn no_folder_and_no_spec_are_explained() {
        let e = env();
        let b = test_binding("c", "code", None);
        assert_eq!(dispatch_in("/approve", &b, "web", SpecFlow::On, &e.store, None).as_deref(), Some(NO_FOLDER));
        assert!(e.run("/review", "c", SpecFlow::On).unwrap().starts_with("No open spec in"));
        assert!(e.run("/spec", "c", SpecFlow::On).unwrap().starts_with("No open spec in"));
        assert!(e.run("/brainstorm", "c", SpecFlow::On).unwrap().starts_with("Usage: /brainstorm"));
        // A valid brainstorm passes the gateway on to the agent.
        assert!(e.run("/brainstorm add webhooks", "c", SpecFlow::On).is_none());
    }

    #[tokio::test]
    async fn brainstorm_creates_active_spec_and_prompt_then_the_folder_is_busy() {
        let e = env();
        let model = AllNone(Mutex::new(0));
        let prompt = agent_prompt_in(
            "/brainstorm Thêm webhook khi đơn hàng đổi trạng thái",
            parse_agent_command("/brainstorm Thêm webhook khi đơn hàng đổi trạng thái").unwrap(),
            Some(e.root.clone()),
            &e.store,
            &model,
        )
        .await;
        let state = e.store.load(&e.root).unwrap();
        assert!(state.active.starts_with("docs/designs/") && state.active.ends_with("-them-webhook-khi-don-hang-doi.md"));
        assert!(prompt.contains("[Spec flow — BRAINSTORM]") && prompt.contains(&state.active), "{prompt}");
        assert!(prompt.contains("ONE question per reply"));
        assert!(prompt.contains("<request>\n/brainstorm Thêm webhook"));
        // Not written yet: another brainstorm may replace it.
        assert!(e.run("/brainstorm other", "c", SpecFlow::On).is_none());
        std::fs::write(e.root.join(&state.active), "# x\n").unwrap();
        assert_eq!(e.run("/brainstorm other", "c", SpecFlow::On), Some(busy(&state.active)));
        assert_eq!(*model.0.lock().unwrap(), 0);
    }

    fn full_spec() -> String {
        format!("# S\n## Goals\n* g\n{DETAILED_HEADING}\n### Tasks\n- [x] T1\n- [ ] T2\n")
    }

    fn open(e: &Env, text: &str) -> String {
        let rel = "docs/designs/s.md".to_string();
        std::fs::write(e.root.join(&rel), text).unwrap();
        e.store.update(&e.root, |s| open_spec(s, &rel, "S")).unwrap();
        rel
    }

    fn reviewed(e: &Env, rel: &str, complete: bool, blocker: usize) {
        let hash = design_hash(&std::fs::read_to_string(e.root.join(rel)).unwrap());
        e.store
            .update(&e.root, |s| {
                s.specs.get_mut(rel).unwrap().review = Some(ReviewSummary {
                    hash,
                    mode: "design".into(),
                    complete,
                    blocker,
                    lenses: [("hieu".to_string(), if complete { "ok" } else { "unparsed" }.to_string())].into(),
                    ..Default::default()
                })
            })
            .unwrap();
    }

    #[test]
    fn approve_conditions() {
        let e = env();
        let rel = open(&e, "# S\n## Goals\n* g\n");
        let brief = e.run("/approve force", "c", SpecFlow::On).unwrap();
        assert!(brief.contains("no \"## Detailed Implementation Plan\" yet") && !brief.contains("force approves"), "{brief}");
        std::fs::write(e.root.join(&rel), full_spec()).unwrap();
        assert!(e.run("/approve", "c", SpecFlow::On).unwrap().contains("not been reviewed in its current version"));
        reviewed(&e, &rel, false, 0);
        assert!(e.run("/approve", "c", SpecFlow::On).unwrap().contains("did not finish (hieu: unparsed)"));
        reviewed(&e, &rel, true, 2);
        assert!(e.run("/approve", "c", SpecFlow::On).unwrap().contains("has 2 blocker(s)"));
        reviewed(&e, &rel, true, 0);
        let ok = e.run("/approve", "c", SpecFlow::On).unwrap();
        assert!(ok.starts_with(&format!("Approved {rel} (version ")), "{ok}");
        let approval = e.store.load(&e.root).unwrap().specs[&rel].approval.clone().unwrap();
        assert_eq!(approval.via, "web");
        assert!(!approval.forced);
        assert!(e.run("/approve", "c", SpecFlow::On).unwrap().contains("already approved"));
        // A review of an older version does not count.
        std::fs::write(e.root.join(&rel), full_spec().replace("T2", "T2 changed")).unwrap();
        assert!(e.run("/approve", "c", SpecFlow::On).unwrap().contains("not been reviewed in its current version"));
    }

    #[test]
    fn approve_force_needs_detailed_section_and_is_recorded() {
        let e = env();
        let rel = open(&e, &full_spec());
        let out = e.run("/approve force", "c", SpecFlow::On).unwrap();
        assert!(out.contains(", forced)"), "{out}");
        assert!(e.store.load(&e.root).unwrap().specs[&rel].approval.as_ref().unwrap().forced);
    }

    #[test]
    fn spec_status_use_done() {
        let e = env();
        let rel = open(&e, &full_spec());
        let status = e.run("/spec", "c", SpecFlow::On).unwrap();
        assert!(status.starts_with(&format!("{rel} — S\nstage: draft · version ")), "{status}");
        assert!(status.contains("review: none") && status.contains("approval: none"));
        assert!(status.contains("tasks: 1/2 · open questions: 0"));
        std::fs::write(e.root.join("docs/designs/other.md"), "# O\n").unwrap();
        assert!(e.run("/spec use docs/designs/other.md", "c", SpecFlow::On).unwrap().contains("is now the open spec"));
        assert_eq!(e.store.load(&e.root).unwrap().active, "docs/designs/other.md");
        assert!(e.run("/spec use src/main.rs", "c", SpecFlow::On).unwrap().starts_with("Not a spec"));
        assert!(e.run("/spec use docs/designs/../../x.md", "c", SpecFlow::On).unwrap().starts_with("Not a spec"));
        assert!(e.run("/spec done", "c", SpecFlow::On).unwrap().starts_with("Closed docs/designs/other.md"));
        assert!(e.store.find_root(&e.root).is_none());
        assert!(e.run("/spec bogus", "c", SpecFlow::On).unwrap().starts_with("Usage: /spec"));
    }

    #[test]
    fn spec_skip_only_under_required() {
        let e = env();
        let jid = "cmd-test:skip";
        assert!(e.run("/spec skip", jid, SpecFlow::On).unwrap().starts_with("Not needed"));
        assert_eq!(e.run("/spec skip", jid, SpecFlow::Required).as_deref(), Some(SKIPPED));
        assert!(gate::is_skipped(jid));
        gate::forget(jid);
    }

    #[tokio::test]
    async fn review_runs_records_and_prompts() {
        let e = env();
        let rel = open(&e, &full_spec());
        let model = AllNone(Mutex::new(0));
        let prompt = agent_prompt_in("/review", AgentCommand::Review { path: None }, Some(e.root.clone()), &e.store, &model).await;
        assert_eq!(*model.0.lock().unwrap(), 4);
        let state = e.store.load(&e.root).unwrap();
        let review = state.specs[&rel].review.clone().unwrap();
        assert!(review.complete);
        assert_eq!(review.mode, "design");
        assert!(e.store.dir_for(&e.root).join(&review.report).is_file());
        assert!(prompt.contains("[Spec flow — REVIEW RESULTS]") && prompt.contains("exactly as it is"), "{prompt}");
        // The machine checks found the missing sections, which stop no approval.
        assert!(review.major > 0 && review.blocker == 0);
        let stage = stage_of(Some(&full_spec()), state.specs.get(&rel)).0;
        assert_eq!(stage, Stage::Reviewed);
    }

    #[tokio::test]
    async fn only_the_gateway_switches_the_open_spec() {
        let e = env();
        let rel = open(&e, &full_spec());
        std::fs::write(e.root.join("docs/designs/old.md"), full_spec()).unwrap();
        let model = AllNone(Mutex::new(0));
        // A schedule reaches the pool directly: it may not reopen another spec.
        let prompt = agent_prompt_in(
            "/draft docs/designs/old.md",
            AgentCommand::Draft { path: Some("docs/designs/old.md".into()) },
            Some(e.root.clone()),
            &e.store,
            &model,
        )
        .await;
        assert!(prompt.contains("only the person can switch it"), "{prompt}");
        assert_eq!(e.store.load(&e.root).unwrap().active, rel);
        // A person's message passes the gateway, which opens it first.
        assert!(e.run("/draft docs/designs/old.md", "c", SpecFlow::On).is_none());
        assert_eq!(e.store.load(&e.root).unwrap().active, "docs/designs/old.md");
        let prompt = agent_prompt_in(
            "/draft docs/designs/old.md",
            AgentCommand::Draft { path: Some("docs/designs/old.md".into()) },
            Some(e.root.clone()),
            &e.store,
            &model,
        )
        .await;
        assert!(prompt.contains("[Spec flow — DRAFT]"), "{prompt}");
    }

    #[test]
    fn templates_keep_hints_out_of_heading_lines() {
        let b = brainstorm_prompt("/brainstorm x", "docs/designs/x.md", "2026-10-27");
        let d = draft_prompt("/draft", "docs/designs/x.md", None);
        for prompt in [&b, &d] {
            for line in prompt.lines().filter(|l| l.starts_with("##")) {
                assert!(!line.contains('(') && !line.contains("- [ ]"), "hint inside a heading: {line}");
            }
        }
        assert!(d.contains("### Tasks\n") && d.contains("- [ ] T1 <what to do"));
    }

    #[test]
    fn triage_watch_sees_an_untouched_spec_and_says_so() {
        let e = env();
        let rel = open(&e, &full_spec());
        reviewed(&e, &rel, true, 2);
        let watch = TriageWatch::start(&e.root.to_string_lossy(), "triage-blocked", &e.store).unwrap();
        assert!(watch.untouched_with_blockers());
        assert!(watch.nudge().contains("Edit or Write on docs/designs/s.md") && watch.nudge().contains("2 blocker(s)"));
        assert!(watch.note(false).contains("unchanged") && watch.note(false).contains("2 blocker(s)"));
        assert!(watch.note(true).contains("không đổi"));
        // A Review Log line is a change of the file, not of the design.
        std::fs::write(e.root.join(&rel), format!("{}## Review Log\n* CONTEST x\n", full_spec())).unwrap();
        assert!(!watch.untouched_with_blockers());
        assert!(watch.note(false).contains("unchanged"));
        std::fs::write(e.root.join(&rel), full_spec().replace("T2", "T2 with dates")).unwrap();
        assert!(watch.note(false).contains("changed from version"));
    }

    #[test]
    fn the_language_of_a_rewritten_turn_is_the_persons_request() {
        let e = env();
        let rel = open(&e, &full_spec());
        reviewed(&e, &rel, true, 1);
        let review = review::Review {
            path: rel.clone(),
            hash: "0".repeat(64),
            mode: review::Mode::Design,
            at: String::new(),
            findings: Vec::new(),
            runs: Vec::new(),
        };
        let prompt = review_prompt("/review", &rel, &review);
        assert!(crate::docs::has_vietnamese_diacritics(&prompt), "lens names carry diacritics");
        assert_eq!(request_of(&prompt), "/review");
        assert_eq!(request_of("sửa lỗi này"), "sửa lỗi này");
    }

    fn finished_review(severity: review::Severity) -> review::Review {
        review::Review {
            path: "docs/designs/s.md".into(),
            hash: "0".repeat(64),
            mode: review::Mode::Design,
            at: String::new(),
            findings: vec![review::Finding {
                lens: review::Lens::Hieu,
                severity,
                place: "## Goals".into(),
                problem: "vague".into(),
                fix: "say it".into(),
                machine: false,
            }],
            runs: review::LENSES
                .iter()
                .map(|&lens| review::LensRun { lens, status: review::LensStatus::Ok, raw: String::new() })
                .collect(),
        }
    }

    #[test]
    fn only_a_review_with_blockers_asks_for_changes_to_the_design() {
        let rel = "docs/designs/s.md";
        let clean = review_prompt("/review", rel, &finished_review(review::Severity::Major));
        assert!(clean.contains("Leave docs/designs/s.md exactly as it is") && !clean.contains("choose FIX"), "{clean}");
        let blocked = review_prompt("/review", rel, &finished_review(review::Severity::Blocker));
        assert!(blocked.contains("choose FIX") && blocked.contains("only after /review and /approve"), "{blocked}");
        assert!(blocked.contains("never for a Hiểu finding") && blocked.contains("## Review Log"), "{blocked}");
    }

    #[test]
    fn a_clean_review_holds_the_design_for_its_turn_only() {
        let e = env();
        let rel = open(&e, &full_spec());
        let wd = e.root.to_string_lossy().into_owned();
        let edit = |file: &str, jid: &str| {
            let input = serde_json::json!({"file_path": e.root.join(file).to_string_lossy()});
            let call = gate::GateInput { tool: "Edit", input: &input, working_dir: &wd, chat_jid: jid, is_subagent: false, flow: SpecFlow::On };
            gate::check(&call, &e.store)
        };
        reviewed(&e, &rel, true, 0);
        let watch = TriageWatch::start(&wd, "triage-clean", &e.store).unwrap();
        assert_eq!(edit(&rel, "triage-clean").as_deref(), Some(gate::DESIGN_HELD));
        assert!(edit("docs/designs/other.md", "triage-clean").is_none());
        assert!(edit(&rel, "another-chat").is_none());
        assert!(watch.note(false).contains("/approve unlocks the code"));
        assert!(watch.note(true).contains("/approve sẽ mở khoá code"));
        drop(watch);
        assert!(edit(&rel, "triage-clean").is_none());
        // A person who speaks during the turn ends the hold early.
        let watch = TriageWatch::start(&wd, "triage-clean", &e.store).unwrap();
        watch.release_design();
        assert!(edit(&rel, "triage-clean").is_none());
        drop(watch);
        // A subagent has no chat id of its own, and the hold still binds it.
        let watch = TriageWatch::start(&wd, "triage-clean", &e.store).unwrap();
        let input = serde_json::json!({"file_path": e.root.join(&rel).to_string_lossy()});
        let call = gate::GateInput { tool: "Edit", input: &input, working_dir: &wd, chat_jid: "", is_subagent: true, flow: SpecFlow::On };
        assert_eq!(gate::check(&call, &e.store).as_deref(), Some(gate::DESIGN_HELD));
        drop(watch);
        // A record of an older version: no hold, no nudge about its blockers.
        reviewed(&e, &rel, true, 2);
        std::fs::write(e.root.join(&rel), full_spec().replace("T2", "T2 with dates")).unwrap();
        let watch = TriageWatch::start(&wd, "triage-clean", &e.store).unwrap();
        assert!(!watch.holds_design() && !watch.untouched_with_blockers());
        reviewed(&e, &rel, true, 0);
        std::fs::write(e.root.join(&rel), full_spec()).unwrap();
        let watch = TriageWatch::start(&wd, "triage-clean", &e.store).unwrap();
        assert!(!watch.holds_design());
        assert!(edit(&rel, "triage-clean").is_none());
        drop(watch);
        // What `/approve` would refuse anyway has nothing to protect.
        for (complete, blocker) in [(true, 1), (false, 0)] {
            reviewed(&e, &rel, complete, blocker);
            let _watch = TriageWatch::start(&wd, "triage-clean", &e.store).unwrap();
            assert!(edit(&rel, "triage-clean").is_none());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_review_that_cannot_be_recorded_is_not_reported_as_one() {
        use std::os::unix::fs::PermissionsExt;
        let e = env();
        open(&e, &full_spec());
        // The store repairs its own folder, so the record is made unwritable elsewhere.
        let state = e.store.dir_for(&e.root).join("state.json");
        let elsewhere = tempfile::tempdir().unwrap();
        let locked = elsewhere.path().join("state.json");
        std::fs::rename(&state, &locked).unwrap();
        std::os::unix::fs::symlink(&locked, &state).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o400)).unwrap();
        std::fs::set_permissions(elsewhere.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let model = AllNone(Mutex::new(0));
        let prompt = agent_prompt_in("/review", AgentCommand::Review { path: None }, Some(e.root.clone()), &e.store, &model).await;
        std::fs::set_permissions(elsewhere.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(prompt.contains("[Spec flow — NOTICE]") && prompt.contains("could not be recorded"), "{prompt}");
    }

    #[test]
    fn blocked_warning_quotes_the_gate() {
        let w = blocked_warning(&["todo.py".into()], "Error: [spec] No design spec for this folder yet. More.\nsecond line", false);
        assert_eq!(w, "⚠️ Blocked by the spec flow — nothing was changed in: todo.py. Reason: No design spec for this folder yet. More.");
        assert!(blocked_warning(&["todo.py".into()], "Error: [spec] x", true).starts_with("⚠️ Luồng spec đã chặn"));
    }

    #[tokio::test]
    async fn a_command_that_cannot_run_becomes_a_notice() {
        let e = env();
        let model = AllNone(Mutex::new(0));
        let prompt = agent_prompt_in("/draft", AgentCommand::Draft { path: None }, Some(e.root.clone()), &e.store, &model).await;
        assert!(prompt.contains("[Spec flow — NOTICE]") && prompt.contains("No open spec in"), "{prompt}");
        let prompt = agent_prompt_in("/review", AgentCommand::Review { path: None }, None, &e.store, &model).await;
        assert!(prompt.contains(NO_FOLDER));
    }
}
