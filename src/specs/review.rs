//! `/review`: four reviewers that never saw the conversation (the "goldfish" of
//! Google's Elephant-Goldfish Model) read the spec, each from one direction —
//! Cảnh (zoom out), Kiểm (could you build it from this, does it match the code),
//! Hiểu (a newcomer's reading), Phản biện (the critic). Each is one model call
//! with an empty context and the evidence the daemon gathered for it, so a small
//! local model never has to search the repository itself. Kiểm also gets a
//! checklist run in code first, at no token cost.
//!
//! A lens counts only when its answer can be read: a list of findings, or the
//! word `NONE`. An answer cut at the token limit, unreadable, or failed keeps the
//! review incomplete, and `/approve` refuses an incomplete review — an
//! unreadable reply must never read as a clean one.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;

use super::{has_detailed_plan, open_questions, section, ReviewSummary, DETAILED_HEADING};

/// What a lens reads at most; a longer spec is reported and cut.
pub const MAX_SPEC_CHARS: usize = 24_000;
const MAX_TOKENS: u32 = 4096;
const LENS_TIMEOUT: Duration = Duration::from_secs(600);
const INDEX_TIMEOUT: Duration = Duration::from_secs(60);
const INSTRUCTIONS_CHARS: usize = 3000;
const OUTLINE_LINES_PER_FILE: usize = 40;
/// Findings shown to the agent in the turn after the review.
pub const PROMPT_FINDINGS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lens {
    Canh,
    Kiem,
    Hieu,
    PhanBien,
}

pub const LENSES: [Lens; 4] = [Lens::Canh, Lens::Kiem, Lens::Hieu, Lens::PhanBien];

impl Lens {
    pub fn id(self) -> &'static str {
        match self {
            Self::Canh => "canh",
            Self::Kiem => "kiem",
            Self::Hieu => "hieu",
            Self::PhanBien => "phan_bien",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Canh => "Cảnh",
            Self::Kiem => "Kiểm",
            Self::Hieu => "Hiểu",
            Self::PhanBien => "Phản biện",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A full spec (it has `## Detailed Implementation Plan`).
    Design,
    /// A brief: the idea is stress-tested, not the plan.
    Brainstorm,
}

impl Mode {
    pub fn of(text: &str) -> Self {
        if has_detailed_plan(text) {
            Self::Design
        } else {
            Self::Brainstorm
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Design => "design",
            Self::Brainstorm => "brainstorm",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Blocker,
    Major,
    Nit,
}

impl Severity {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Blocker => "BLOCKER",
            Self::Major => "MAJOR",
            Self::Nit => "NIT",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub lens: Lens,
    pub severity: Severity,
    pub place: String,
    pub problem: String,
    pub fix: String,
    /// Found by the checklist in code, not by a model.
    pub machine: bool,
}

impl Finding {
    fn machine(severity: Severity, place: &str, problem: impl Into<String>, fix: &str) -> Self {
        Self { lens: Lens::Kiem, severity, place: place.into(), problem: problem.into(), fix: fix.into(), machine: true }
    }

    pub fn line(&self) -> String {
        let who = if self.machine { format!("{} (machine)", self.lens.label()) } else { self.lens.label().to_string() };
        let mut s = format!("[{}] {} · {} :: {}", self.severity.tag(), who, self.place, self.problem);
        if !self.fix.is_empty() {
            s.push_str(&format!(" :: {}", self.fix));
        }
        s
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum LensStatus {
    Ok,
    /// The answer stopped at the token limit.
    Cut,
    /// No finding and not `NONE`.
    Unparsed,
    Failed(String),
}

impl LensStatus {
    pub fn as_string(&self) -> String {
        match self {
            Self::Ok => "ok".into(),
            Self::Cut => "cut".into(),
            Self::Unparsed => "unparsed".into(),
            Self::Failed(e) => format!("failed: {e}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LensRun {
    pub lens: Lens,
    pub status: LensStatus,
    pub raw: String,
}

#[derive(Debug, Clone)]
pub struct Review {
    pub path: String,
    pub hash: String,
    pub mode: Mode,
    pub at: String,
    pub findings: Vec<Finding>,
    pub runs: Vec<LensRun>,
}

impl Review {
    pub fn complete(&self) -> bool {
        self.runs.len() == LENSES.len() && self.runs.iter().all(|r| r.status == LensStatus::Ok)
    }

    pub fn count(&self, severity: Severity) -> usize {
        self.findings.iter().filter(|f| f.severity == severity).count()
    }

    pub fn summary(&self, report: &str) -> ReviewSummary {
        ReviewSummary {
            hash: self.hash.clone(),
            at: self.at.clone(),
            mode: self.mode.as_str().into(),
            complete: self.complete(),
            blocker: self.count(Severity::Blocker),
            major: self.count(Severity::Major),
            nit: self.count(Severity::Nit),
            lenses: self.runs.iter().map(|r| (r.lens.id().to_string(), r.status.as_string())).collect(),
            report: report.into(),
        }
    }

    /// Blocker → Major → Nit, at most `PROMPT_FINDINGS` lines.
    pub fn findings_for_prompt(&self) -> String {
        let mut sorted: Vec<&Finding> = self.findings.iter().collect();
        sorted.sort_by_key(|f| f.severity);
        let mut lines: Vec<String> = sorted.iter().take(PROMPT_FINDINGS).map(|f| f.line()).collect();
        if sorted.len() > PROMPT_FINDINGS {
            lines.push(format!("… and {} more in the report.", sorted.len() - PROMPT_FINDINGS));
        }
        if lines.is_empty() {
            lines.push("(no findings)".into());
        }
        for run in self.runs.iter().filter(|r| r.status != LensStatus::Ok) {
            lines.push(format!("Lens {} did not finish: {}.", run.lens.label(), run.status.as_string()));
        }
        lines.join("\n")
    }

    /// The report kept beside the state: what each lens said, verbatim, after
    /// the findings it was read as.
    pub fn render_report(&self) -> String {
        let mut out = format!(
            "# Spec review — {}\n\nversion {} · mode {} · {}\n\n| Lens | Status |\n|---|---|\n",
            self.path,
            &self.hash[..8.min(self.hash.len())],
            self.mode.as_str(),
            self.at
        );
        for run in &self.runs {
            out.push_str(&format!("| {} | {} |\n", run.lens.label(), run.status.as_string()));
        }
        out.push_str(&format!(
            "\n## Findings ({} blocker, {} major, {} nit)\n\n",
            self.count(Severity::Blocker),
            self.count(Severity::Major),
            self.count(Severity::Nit)
        ));
        let mut sorted: Vec<&Finding> = self.findings.iter().collect();
        sorted.sort_by_key(|f| f.severity);
        for f in sorted {
            out.push_str(&format!("- {}\n", f.line()));
        }
        out.push_str("\n## What each lens said\n");
        for run in &self.runs {
            out.push_str(&format!("\n### {}\n\n```text\n{}\n```\n", run.lens.label(), run.raw.trim()));
        }
        out
    }
}

/// The checklist run in code for a full spec. All of it lands under Kiểm.
pub fn machine_checks(root: &Path, text: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for (name, severity) in [("Goals", Severity::Blocker), ("Detailed Implementation Plan", Severity::Blocker)] {
        if section(text, name).iter().all(|l| l.trim().is_empty()) {
            out.push(Finding::machine(severity, &format!("## {name}"), "missing or empty", "write this section"));
        }
    }
    for name in [
        "Overview",
        "The Problem",
        "Non-Goals",
        "Open Questions",
        "Critical User Journeys",
        "Technical Plan",
        "Alternatives Considered",
        "Risks",
    ] {
        let heading = format!("## {name}");
        if !text.lines().any(|l| l.trim_end() == heading) {
            out.push(Finding::machine(Severity::Major, &heading, "missing", "add it (the draft template has it)"));
        }
    }
    for name in ["### Target Files", "### Tasks", "### Verification Plan"] {
        if !text.lines().any(|l| l.trim_end() == name) {
            out.push(Finding::machine(Severity::Major, name, "missing", "add it under ## Detailed Implementation Plan"));
        }
    }

    let entry = Regex::new(r"^####\s+\d+\.\s+`([^`]+)`(.*)$").expect("static regex");
    let mut unlabelled = 0;
    for line in subsection(text, "### Target Files") {
        let Some(c) = entry.captures(line.trim_end()) else { continue };
        let path = c[1].trim();
        let rest = c[2].to_ascii_lowercase();
        let exists = root.join(path).exists();
        if rest.contains("modify") || rest.contains("delete") {
            if !exists {
                out.push(Finding::machine(
                    Severity::Blocker,
                    path,
                    format!("marked {} but it does not exist in this folder", if rest.contains("delete") { "Delete" } else { "Modify" }),
                    "fix the path, or mark it Create",
                ));
            }
        } else if rest.contains("create") {
            if exists {
                out.push(Finding::machine(Severity::Major, path, "marked Create but it already exists", "mark it Modify"));
            }
        } else {
            unlabelled += 1;
        }
    }
    if unlabelled > 0 {
        out.push(Finding::machine(
            Severity::Major,
            "### Target Files",
            format!("{unlabelled} file(s) without (Create|Modify|Delete)"),
            "label every target file",
        ));
    }

    if !subsection(text, "### Verification Plan").iter().any(|l| l.contains('`')) {
        out.push(Finding::machine(
            Severity::Major,
            "### Verification Plan",
            "no command to run",
            "name the build/test commands in backticks",
        ));
    }

    let placeholder = Regex::new(r"\bTBD\b|\bTODO\b|\[File Path|\[Feature|<title>").expect("static regex");
    let found: Vec<&str> = placeholder.find_iter(text).map(|m| m.as_str()).collect();
    if !found.is_empty() {
        let mut uniq = found.clone();
        uniq.sort_unstable();
        uniq.dedup();
        out.push(Finding::machine(Severity::Major, "whole file", format!("placeholders left: {}", uniq.join(", ")), "decide and fill them in"));
    }

    let questions = open_questions(text);
    if questions > 0 {
        out.push(Finding::machine(
            Severity::Major,
            "## Open Questions",
            format!("{questions} question(s) still open"),
            "answer them, or say why the code can start anyway",
        ));
    }

    if text.chars().count() > MAX_SPEC_CHARS {
        out.push(Finding::machine(
            Severity::Major,
            "whole file",
            format!("too long to review in one pass (over {MAX_SPEC_CHARS} characters)"),
            "split the spec",
        ));
    }
    out
}

/// The lines under a `###` heading up to the next `##` or `###` heading.
fn subsection<'a>(text: &'a str, heading: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with("## ") || line.starts_with("### ") {
            inside = line.trim_end() == heading;
            continue;
        }
        if inside {
            out.push(line);
        }
    }
    out
}

/// Read a lens's answer. `cut` = the model stopped at the token limit.
pub fn parse_findings(lens: Lens, raw: &str, cut: bool) -> (Vec<Finding>, LensStatus) {
    let think = Regex::new(r"(?s)<think>.*?</think>").expect("static regex");
    let text = think.replace_all(raw, "");
    // An unclosed think block (cut mid-thought) hides nothing real after it.
    let text = text.split("<think>").next().unwrap_or_default();
    let severity = Regex::new(
        r"(?i)^(?:\[(blocker|major|minor|nit)\]|(blocker|major|minor|nit)s?\s*[:\-–—]|severity\s*:\s*\[?(blocker|major|minor|nit)\]?\s*[:\-–—]?|(🔴|🟡|🟢)\s*(?:blocker|major|minor|nit)?\s*[:\-–—]?)",
    )
    .expect("static regex");
    // A severity word at the start of a line that the pattern above did not
    // read (a `### Blockers` heading over a plain list, an unusual separator):
    // findings may be hiding there, so the lens must not read as complete.
    let stray = Regex::new(r"(?i)^[#>*\s]*(blockers?|majors?|nits?|minors?)\b").expect("static regex");
    let mut unread = false;
    let lead = Regex::new(r"^(?:[-*•]\s*|\d+[.)]\s*)+").expect("static regex");
    let mut out = Vec::new();
    for line in text.lines() {
        let line = lead.replace(line.trim(), "");
        let line = line.trim().trim_start_matches("**").replace("**", "");
        let line = line.trim();
        let Some(m) = severity.captures(line) else {
            unread |= stray.is_match(line);
            continue;
        };
        let word = (1..=4).find_map(|i| m.get(i)).map(|g| g.as_str().to_lowercase()).unwrap_or_default();
        let sev = match word.as_str() {
            "blocker" | "🔴" => Severity::Blocker,
            "major" | "🟡" => Severity::Major,
            _ => Severity::Nit,
        };
        let rest = line[m.get(0).map(|g| g.end()).unwrap_or(0)..].trim();
        let mut parts = rest.splitn(3, "::").map(str::trim);
        let (a, b, c) = (parts.next().unwrap_or(""), parts.next(), parts.next());
        let (place, problem, fix) = match (b, c) {
            (Some(b), Some(c)) => (a, b, c),
            (Some(b), None) => (a, b, ""),
            _ => ("", a, ""),
        };
        if problem.is_empty() && place.is_empty() {
            continue;
        }
        // "**BLOCKER:** None" says there is nothing of that kind.
        let said = |s: &str| s.trim().trim_end_matches('.').to_ascii_lowercase();
        if matches!(said(problem).as_str(), "none" | "n/a" | "-" | "no issues" | "nothing")
            || (problem.is_empty() && matches!(said(place).as_str(), "none" | "n/a"))
        {
            continue;
        }
        out.push(Finding {
            lens,
            severity: sev,
            place: place.into(),
            problem: problem.into(),
            fix: fix.into(),
            machine: false,
        });
    }
    let none = text.trim().trim_end_matches('.').trim().eq_ignore_ascii_case("none");
    let status = if cut {
        LensStatus::Cut
    } else if unread {
        LensStatus::Unparsed
    } else if !out.is_empty() || none || said_none_per_severity(&text) {
        LensStatus::Ok
    } else {
        LensStatus::Unparsed
    };
    (out, status)
}

/// Every line says a severity has nothing ("BLOCKER: none", "MAJOR: none"):
/// as clean as a bare `NONE`.
fn said_none_per_severity(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    !lines.is_empty()
        && lines.iter().all(|l| {
            let l = l.replace("**", "").to_ascii_lowercase();
            let l = l.trim_start_matches(['-', '*', ' ', '[']).replace(']', "");
            ["blocker", "major", "minor", "nit"].iter().any(|s| {
                l.strip_prefix(s).is_some_and(|rest| {
                    let rest = rest.strip_prefix('s').unwrap_or(rest);
                    rest.trim_start_matches([':', '-', '–', '—', ' ']).trim().trim_end_matches('.').eq_ignore_ascii_case("none")
                })
            })
        })
}

const GOLDFISH_RULES: &str = "You are one reviewer of a design document, and you have no history: you see only the \
document and the evidence given here. You do not rewrite the document and you do not chat; you list problems.";

const OUTPUT_FORMAT: &str = "Answer with one finding per line, nothing else:\n[BLOCKER] where :: problem :: fix\n\
[MAJOR] where :: problem :: fix\n[NIT] where :: problem :: fix\nBLOCKER = the code cannot be written correctly from \
this text; MAJOR = it can, but it is likely wrong or costly; NIT = wording. \"where\" is a section heading or a short \
quote. If you find nothing, reply exactly NONE.";

fn role(lens: Lens, mode: Mode) -> &'static str {
    match (lens, mode) {
        (Lens::Canh, Mode::Design) => {
            "You are the skeptical architect (lens: zoom out). Ask: does the design solve the root problem or only a \
             symptom? Is anything built for a future nobody asked for? Is there optimisation before a proven \
             bottleneck? Does it reinvent something the repository already has (check the repo map)? Does it sink into \
             code-level detail instead of components, interfaces and data flow? Is new logic put in the right layer, \
             as the project's instructions describe?"
        }
        (Lens::Kiem, Mode::Design) => {
            "You are the tech lead who must implement this plan tomorrow, and an auditor of the codebase. Ask: could \
             you write the code from this text without making a design decision yourself (data shape, API, error \
             handling, edge cases)? Is every file to create, modify or delete listed with what changes in it? Are the \
             steps concrete (\"add field X to struct Y in file Z\")? Does every file or signature the plan says exists \
             match the outline given? Is there a concrete test for the normal case and the edge cases, with a command \
             to run? Do not repeat the machine findings listed."
        }
        (Lens::Hieu, Mode::Design) => {
            "You are a newcomer who just joined and knows nothing about this project. Ask: which terms, components or \
             abbreviations are used without being defined in the document? Which steps assume knowledge the document \
             does not give? Where does it say something is handled without saying how? Where could two readers build \
             two different things?"
        }
        (Lens::PhanBien, Mode::Design) => {
            "You are a hostile design critic. Ask: are alternatives listed, each with a concrete reason for rejecting \
             it? Which claims about behaviour, performance or other systems are made without evidence? What fails — \
             a single point of failure, bad input, two things at once, a half-finished step, a rollback? What are the \
             security and privacy risks? Which limits (size, load, cost) are not stated?"
        }
        (Lens::Canh, Mode::Brainstorm) => {
            "You are the contrarian (lens: zoom out). Ask: does this need to exist now? Could something the repository \
             already has, or a standard library or tool, solve most of it without new code? What is the smallest \
             version worth building?"
        }
        (Lens::Kiem, Mode::Brainstorm) => {
            "You check migration and compatibility. Ask: which existing data, formats, APIs or users does this touch? \
             How do they move over? What breaks for people using it today? How is it rolled out, and rolled back \
             safely?"
        }
        (Lens::Hieu, Mode::Brainstorm) => {
            "You judge it as a user. Ask: is the proposed command, API or screen obvious to someone new? Are errors \
             actionable? What will confuse people?"
        }
        (Lens::PhanBien, Mode::Brainstorm) => {
            "You judge cost and failure. Ask: what does it cost to run (time, memory, tokens, money)? Where are the \
             bottlenecks and single points of failure? What happens when something it depends on is slow or down?"
        }
    }
}

pub fn lens_system(lens: Lens, mode: Mode) -> String {
    format!("{GOLDFISH_RULES}\n\n{}\n\n{OUTPUT_FORMAT}", role(lens, mode))
}

/// What the daemon gathered for the lenses that need the repository.
#[derive(Debug, Clone, Default)]
pub struct Evidence {
    pub instructions: Option<String>,
    pub repo_map: Option<String>,
    pub outline: Option<String>,
    pub commands: Option<String>,
}

fn clip_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// The part of the document a lens reads in each mode.
fn range(lens: Lens, mode: Mode, text: &str) -> String {
    let split = text.find(&format!("\n{DETAILED_HEADING}")).map(|i| i + 1);
    let part = match (mode, lens, split) {
        (Mode::Design, Lens::Kiem, Some(i)) => &text[i..],
        (Mode::Design, Lens::PhanBien, Some(i)) => &text[..i],
        _ => text,
    };
    clip_chars(part, MAX_SPEC_CHARS).to_string()
}

pub fn lens_user(lens: Lens, mode: Mode, path: &str, hash: &str, text: &str, evidence: &Evidence, machine: &[Finding]) -> String {
    let mut out = format!("Document: {path} (version {})\n\n", &hash[..8.min(hash.len())]);
    let block = |tag: &str, body: &Option<String>, empty: &str| match body {
        Some(b) => format!("<{tag}>\n{}\n</{tag}>\n\n", b.trim()),
        None => format!("<{tag}>\n{empty}\n</{tag}>\n\n"),
    };
    match (lens, mode) {
        (Lens::Canh, _) => {
            out.push_str(&block("project_instructions", &evidence.instructions, "(none)"));
            out.push_str(&block("repo_map", &evidence.repo_map, "(no repo map for this folder)"));
        }
        (Lens::Kiem, Mode::Design) => {
            let found = if machine.is_empty() {
                None
            } else {
                Some(machine.iter().map(Finding::line).collect::<Vec<_>>().join("\n"))
            };
            out.push_str(&block("machine_findings", &found, "(none)"));
            out.push_str(&block("outline", &evidence.outline, "(no outline)"));
            out.push_str(&block("project_commands", &evidence.commands, "(none detected)"));
        }
        (Lens::Kiem, Mode::Brainstorm) => {
            out.push_str(&block("repo_map", &evidence.repo_map, "(no repo map for this folder)"));
        }
        _ => {}
    }
    out.push_str(&format!("<document>\n{}\n</document>\n", range(lens, mode, text)));
    out
}

/// Gather what the lenses are shown about the repository. Nothing here may fail
/// the review: a missing piece is said to be missing.
pub async fn gather_evidence(root: &Path, text: &str, mode: Mode) -> Evidence {
    let instructions = ["SENCLAW.md", "AGENTS.md", "CLAUDE.md"]
        .iter()
        .find_map(|name| std::fs::read_to_string(root.join(name)).ok())
        .map(|s| clip_chars(&s, INSTRUCTIONS_CHARS).to_string());
    let root_str = root.to_string_lossy().into_owned();
    let index = tokio::time::timeout(INDEX_TIMEOUT, crate::repo_map::index_ready(&root_str))
        .await
        .ok()
        .flatten();
    let repo_map = crate::repo_map::map_for_prompt(&root_str, text);
    let outline = if mode == Mode::Design {
        index.and_then(|index| outline_of_modified(&index.lock().unwrap_or_else(|p| p.into_inner()), text))
    } else {
        None
    };
    let commands = crate::repo_map::commands::render(root);
    Evidence { instructions, repo_map, outline, commands }
}

fn outline_of_modified(index: &crate::repo_map::Index, text: &str) -> Option<String> {
    let entry = Regex::new(r"^####\s+\d+\.\s+`([^`]+)`.*(?i:modify)").expect("static regex");
    let mut out = String::new();
    for line in subsection(text, "### Target Files") {
        let Some(c) = entry.captures(line.trim_end()) else { continue };
        let rel = c[1].trim().trim_start_matches("./");
        let Some(file) = index.files.get(rel) else { continue };
        out.push_str(&format!("{rel}:\n"));
        for def in file.defs.iter().take(OUTLINE_LINES_PER_FILE) {
            out.push_str(&format!("  {}: {}\n", def.line, def.sig));
        }
    }
    (!out.is_empty()).then_some(out)
}

/// One empty-context model call. The daemon's own implementation is
/// [`ConfiguredModel`]; tests script the answers.
#[async_trait::async_trait]
pub trait LensModel: Send + Sync {
    /// `Ok((text, cut))`, `cut` when the answer stopped at the token limit.
    async fn ask(&self, system: &str, user: &str) -> Result<(String, bool), String>;
}

/// The chat's own model, thinking off, usage recorded as `spec:review`.
pub struct ConfiguredModel {
    pub config_path: std::path::PathBuf,
    pub profile: Option<String>,
    pub jid: String,
    pub usage: Option<Arc<crate::usage::UsageRecorder>>,
}

#[async_trait::async_trait]
impl LensModel for ConfiguredModel {
    async fn ask(&self, system: &str, user: &str) -> Result<(String, bool), String> {
        use crate::gateway::ui_server::llm_config::{chat_completion_with, record_completion, CompletionOptions};
        let call = chat_completion_with(
            &self.config_path,
            self.profile.as_deref(),
            system,
            user,
            MAX_TOKENS,
            None,
            CompletionOptions { thinking: Some(false) },
        );
        let result = tokio::time::timeout(LENS_TIMEOUT, call)
            .await
            .map_err(|_| format!("no answer in {} s", LENS_TIMEOUT.as_secs()))??;
        record_completion(&self.usage, "spec:review", &self.jid, &result);
        Ok((result.text, result.finish == "length"))
    }
}

/// Review `text` (read once by the caller, together with its hash). The four
/// lenses run one after another: a local runtime serves one request at a time.
pub async fn run(root: &Path, path: &str, text: &str, model: &dyn LensModel) -> Review {
    let hash = super::design_hash(text);
    let mode = Mode::of(text);
    let machine = if mode == Mode::Design { machine_checks(root, text) } else { Vec::new() };
    let evidence = gather_evidence(root, text, mode).await;
    let mut findings = machine.clone();
    let mut runs = Vec::new();
    for lens in LENSES {
        let system = lens_system(lens, mode);
        let user = lens_user(lens, mode, path, &hash, text, &evidence, &machine);
        let (status, raw) = match model.ask(&system, &user).await {
            Ok((raw, cut)) => {
                let (found, status) = parse_findings(lens, &raw, cut);
                findings.extend(found);
                (status, raw)
            }
            Err(e) => (LensStatus::Failed(e.clone()), String::new()),
        };
        runs.push(LensRun { lens, status, raw });
    }
    Review {
        path: path.into(),
        hash,
        mode,
        at: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        findings,
        runs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn parse_accepts_marker_variants() {
        let raw = "Here you go:\n\
                   [BLOCKER] Goals :: no goal says what done means :: add a measurable goal\n\
                   - **MAJOR**: Technical Plan :: retries unbounded\n\
                   2. Severity: Nit - wording\n\
                   🔴 Risks :: no rollback\n\
                   * [minor] typo\n\
                   a line that is not a finding";
        let (found, status) = parse_findings(Lens::Kiem, raw, false);
        assert_eq!(status, LensStatus::Ok);
        let sev: Vec<Severity> = found.iter().map(|f| f.severity).collect();
        assert_eq!(sev, vec![Severity::Blocker, Severity::Major, Severity::Nit, Severity::Blocker, Severity::Nit]);
        assert_eq!(found[0].place, "Goals");
        assert_eq!(found[0].fix, "add a measurable goal");
        assert_eq!(found[1].problem, "retries unbounded");
        assert_eq!(found[2].problem, "wording");
    }

    #[test]
    fn a_none_per_severity_is_not_a_finding_and_a_stray_heading_is_unread() {
        let (found, status) = parse_findings(Lens::Kiem, "**BLOCKER:** None\n**MAJOR:** None\n**NIT:** none.", false);
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(status, LensStatus::Ok);
        let (found, status) = parse_findings(Lens::Kiem, "Blockers: none\nMajors: None", false);
        assert!(found.is_empty() && status == LensStatus::Ok, "{found:?} {status:?}");
        let (found, status) = parse_findings(Lens::Kiem, "BLOCKER — no rollback for the migration", false);
        assert_eq!(found.len(), 1);
        assert_eq!(status, LensStatus::Ok);
        // Findings grouped under a heading the parser does not read: not "clean".
        let raw = "[MAJOR] Goals :: vague\n### Blockers\n- the plan never names the table";
        let (found, status) = parse_findings(Lens::Kiem, raw, false);
        assert_eq!(found.len(), 1);
        assert_eq!(status, LensStatus::Unparsed);
    }

    #[test]
    fn none_is_ok_empty_is_unparsed() {
        assert_eq!(parse_findings(Lens::Hieu, "NONE", false).1, LensStatus::Ok);
        assert_eq!(parse_findings(Lens::Hieu, " none. ", false).1, LensStatus::Ok);
        assert_eq!(parse_findings(Lens::Hieu, "", false).1, LensStatus::Unparsed);
        assert_eq!(parse_findings(Lens::Hieu, "Looks fine to me overall!", false).1, LensStatus::Unparsed);
    }

    #[test]
    fn length_finish_is_cut_even_with_findings() {
        let (found, status) = parse_findings(Lens::Canh, "[MAJOR] a :: b :: c\n[BLOCK", true);
        assert_eq!(found.len(), 1);
        assert_eq!(status, LensStatus::Cut);
    }

    #[test]
    fn think_block_is_stripped() {
        let raw = "<think>[BLOCKER] x :: thinking aloud</think>NONE";
        let (found, status) = parse_findings(Lens::PhanBien, raw, false);
        assert!(found.is_empty());
        assert_eq!(status, LensStatus::Ok);
        assert_eq!(parse_findings(Lens::PhanBien, "<think>never closed [BLOCKER] x", false).1, LensStatus::Unparsed);
    }

    fn full(extra: &str) -> String {
        format!(
            "# S\n## Overview\no\n## The Problem\np\n## Goals\n* g\n## Non-Goals\n* n\n## Open Questions\n\
             ## Critical User Journeys\nc\n## Technical Plan\nt\n## Alternatives Considered\na\n## Risks\nr\n\
             {DETAILED_HEADING}\n### Target Files\n{extra}### Tasks\n- [ ] T1\n### Verification Plan\n* `cargo test`\n"
        )
    }

    #[test]
    fn machine_checks_pass_a_complete_spec() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/a.rs"), "").unwrap();
        let text = full("#### 1. `src/a.rs` (Modify)\n#### 2. `src/b.rs` (Create)\n");
        assert_eq!(machine_checks(root.path(), &text), vec![]);
    }

    #[test]
    fn machine_checks_flag_paths_labels_placeholders_and_questions() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/a.rs"), "").unwrap();
        let text = full("#### 1. `src/missing.rs` (Modify)\n#### 2. `src/a.rs` (Create)\n#### 3. `src/c.rs`\nTBD\n")
            .replace("## Open Questions\n", "## Open Questions\n* Which store?\n")
            .replace("* `cargo test`", "* run the tests");
        let found = machine_checks(root.path(), &text);
        let has = |sev: Severity, needle: &str| found.iter().any(|f| f.severity == sev && f.line().contains(needle));
        assert!(has(Severity::Blocker, "src/missing.rs"), "{found:#?}");
        assert!(has(Severity::Major, "already exists"));
        assert!(has(Severity::Major, "1 file(s) without"));
        assert!(has(Severity::Major, "placeholders left: TBD"));
        assert!(has(Severity::Major, "1 question(s) still open"));
        assert!(has(Severity::Major, "no command to run"));
        assert!(found.iter().all(|f| f.machine && f.lens == Lens::Kiem));
    }

    #[test]
    fn machine_checks_block_a_spec_without_goals_or_plan() {
        let found = machine_checks(Path::new("/nowhere"), "# S\n## Goals\n\n");
        let blockers: Vec<&str> = found.iter().filter(|f| f.severity == Severity::Blocker).map(|f| f.place.as_str()).collect();
        assert_eq!(blockers, vec!["## Goals", "## Detailed Implementation Plan"]);
    }

    #[test]
    fn ranges_give_each_lens_its_part() {
        let text = full("#### 1. `src/a.rs` (Modify)\n");
        let kiem = range(Lens::Kiem, Mode::Design, &text);
        assert!(kiem.starts_with(DETAILED_HEADING) && !kiem.contains("## Goals"));
        let critic = range(Lens::PhanBien, Mode::Design, &text);
        assert!(critic.contains("## Risks") && !critic.contains(DETAILED_HEADING));
        assert_eq!(range(Lens::PhanBien, Mode::Brainstorm, &text), text);
    }

    struct Scripted {
        answers: Mutex<Vec<Result<(String, bool), String>>>,
        seen: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl LensModel for Scripted {
        async fn ask(&self, system: &str, user: &str) -> Result<(String, bool), String> {
            self.seen.lock().unwrap().push(format!("{system}\n---\n{user}"));
            self.answers.lock().unwrap().remove(0)
        }
    }

    #[tokio::test]
    async fn run_reads_four_lenses_and_an_unreadable_one_leaves_it_incomplete() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/a.rs"), "").unwrap();
        let text = full("#### 1. `src/a.rs` (Modify)\n");
        let model = Scripted {
            answers: Mutex::new(vec![
                Ok(("NONE".into(), false)),
                Ok(("[BLOCKER] Tasks :: T1 says nothing :: name the file".into(), false)),
                Ok(("I think it is fine".into(), false)),
                Err("connection refused".into()),
            ]),
            seen: Mutex::new(Vec::new()),
        };
        let review = run(root.path(), "docs/designs/s.md", &text, &model).await;
        assert_eq!(review.mode, Mode::Design);
        assert_eq!(review.count(Severity::Blocker), 1);
        assert!(!review.complete());
        let summary = review.summary("reviews/s.md");
        assert_eq!(summary.lenses["hieu"], "unparsed");
        assert!(summary.lenses["phan_bien"].starts_with("failed: connection refused"));
        let seen = model.seen.lock().unwrap();
        assert!(seen[0].contains("skeptical architect") && seen[0].contains("<repo_map>"));
        assert!(seen[1].contains("<machine_findings>") && seen[1].contains("<project_commands>"));
        assert!(!seen[2].contains("<repo_map>") && seen[2].contains("newcomer"));
        assert!(!seen[3].contains(DETAILED_HEADING));
        assert!(review.findings_for_prompt().contains("Lens Hiểu did not finish: unparsed."));
        assert!(review.render_report().contains("I think it is fine"));
    }

    #[tokio::test]
    async fn a_brief_is_reviewed_in_brainstorm_mode_without_machine_checks() {
        let root = tempfile::tempdir().unwrap();
        let model = Scripted {
            answers: Mutex::new((0..4).map(|_| Ok(("NONE".into(), false))).collect()),
            seen: Mutex::new(Vec::new()),
        };
        let review = run(root.path(), "docs/designs/b.md", "# B\n## Goals\n* g\n", &model).await;
        assert_eq!(review.mode, Mode::Brainstorm);
        assert!(review.complete());
        assert!(review.findings.is_empty());
        assert!(model.seen.lock().unwrap()[0].contains("contrarian"));
    }
}
