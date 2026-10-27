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
         - After they confirm, Write {rel} with exactly these headings:\n\
         # <title>\n*   **Status:** Brief\n*   **Date:** {date}\n## Overview\n## The Problem\n## Goals\n## Non-Goals\n\
         ## Open Questions\n## Anchored Hypothesis   (the 2-3 sentences the person confirmed)\n\
         ## Options   (2-3 approaches, with pros and cons)\n\
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
         - Commands in the Verification Plan come from the project's commands; no real code, only signatures and \
         schemas; unknowns go under Open Questions.\n\
         Template:\n\
         # <title>\n*   **Status:** Draft\n*   **Date:** <today>\n## Overview\n## The Problem\n## Goals\n## Non-Goals\n\
         ## Open Questions\n## Critical User Journeys   (Given / When / Then)\n## Technical Plan\n\
         ## Alternatives Considered\n## Risks\n## Detailed Implementation Plan\n### Target Files\n\
         ### Tasks   (- [ ] T1 …)\n### Verification Plan\n## Review Log   (always the last section)\n\
         Reply in the person's language.",
        header("DRAFT", text)
    )
}

pub fn review_prompt(text: &str, rel: &str, result: &review::Review) -> String {
    format!(
        "{}Four reviewers who never saw this conversation read {rel} (version {}, {} review): Cảnh = zoom out, \
         Kiểm = can it be built from this and does it match the code, Hiểu = a newcomer's reading, Phản biện = critic.\n\
         Findings:\n{}\n(The person can see the full report with the spec status command.)\n\
         Rules for this step:\n\
         - Do not change source files; only {rel} and other files in {DESIGNS_DIR}/.\n\
         - For each BLOCKER and MAJOR choose FIX (edit the spec), CONTEST (only with evidence a reviewer could read in \
         the spec or the code — never for a Hiểu finding), or ESCALATE (a choice only the person can make: ask them, \
         one question per reply).\n\
         - NIT: fix it if trivial, else DECLINE.\n\
         - Add one line per decision under ## Review Log, and keep that section the last one in the file.\n\
         - Then tell the person: the counts, what you changed, what needs their decision, and that the review command \
         checks the new version and the approve command unlocks the code.\n\
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
        assert!(prompt.contains("[Spec flow — REVIEW RESULTS]") && prompt.contains("## Review Log"), "{prompt}");
        assert!(prompt.contains("never for a Hiểu finding"));
        // The machine checks found the missing sections: not approvable as is.
        assert!(review.major > 0);
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
