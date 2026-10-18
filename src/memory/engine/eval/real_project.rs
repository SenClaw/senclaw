// Measurements of the project feature through a real extraction model.
//
// The scripted project cases in `eval.rs` prove where a fact goes and who reads
// it. They say nothing about what a small model, reading real conversations of
// a project chat or the summary a kanban worker writes, extracts: how many
// personal facts end up in the project's bank, how many facts a finished card
// leaves and how many of them are worth keeping. These tests ask the model named
// by the environment, with the production client and the production retain
// pipeline, and print what it did. They are `#[ignore]`: they cost model calls,
// the numbers move from run to run, and they gate nothing.
//
//     SENCLAW_EVAL_LLM_BASE_URL=http://<host>:<port>/v1 \
//     SENCLAW_EVAL_LLM_MODEL=<model id> [SENCLAW_EVAL_LLM_API_KEY=<key>] \
//     <test binary> memory::engine::eval::real_project --ignored --nocapture --test-threads=1
//
// Inputs: `evals/memory/project-measure.json`.

use std::collections::HashSet;

use anyhow::anyhow;

use super::*;
use crate::projects::{fact_target, recall_scope, worker_recall_query};

#[derive(Deserialize)]
struct Measure {
    project: String,
    folder: String,
    now: String,
    /// A chat of the project: where the windows were spoken.
    chat: String,
    /// Another chat of the same project.
    sibling_chat: String,
    /// A chat of the same agent, outside every project.
    outside_chat: String,
    windows: Vec<MeasureWindow>,
    personal_questions: Vec<Case>,
    project_questions: Vec<Case>,
    cards: Vec<Card>,
    card_questions: Vec<Case>,
}

#[derive(Deserialize)]
struct MeasureWindow {
    id: String,
    /// `project`, `personal` or `mixed`: what the window is about.
    kind: String,
    text: String,
    project_markers: Vec<String>,
    personal_markers: Vec<String>,
}

#[derive(Deserialize)]
struct Card {
    id: String,
    class: String,
    /// `facts` (something worth keeping), `none` (nothing is) or `maybe`.
    expect: String,
    title: String,
    summary: String,
    worth: Vec<String>,
}

fn measure() -> Measure {
    serde_json::from_str(include_str!(
        "../../../../evals/memory/project-measure.json"
    ))
    .expect("evals/memory/project-measure.json parses")
}

fn now_of(m: &Measure) -> i64 {
    chrono::NaiveDate::parse_from_str(&m.now, "%Y-%m-%d")
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp()
}

/// The fact store and nothing else: these measurements read no notes and no graph.
struct Bench {
    facts: Arc<FactEngine>,
    _dir: tempfile::TempDir,
}

fn bench() -> Bench {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut cfg = Config::from_env();
    cfg.memory.embedding_provider = EmbeddingProvider::None;
    cfg.paths.profiles_dir = dir.path().join("profiles");
    let db = Arc::new(Db::open_in_memory(&cfg).expect("in-memory db"));
    Bench {
        facts: Arc::new(FactEngine::new(db, None)),
        _dir: dir,
    }
}

/// Plays back the replies a real model gave to the same windows, so a second
/// world can be built from exactly what the first one was told.
struct Replay(Vec<(String, std::result::Result<String, String>)>);

#[async_trait]
impl LlmClient for Replay {
    async fn complete(&self, _system: &str, user: &str) -> Result<String> {
        match self.0.iter().find(|(text, _)| user.contains(text.as_str())) {
            Some((_, Ok(reply))) => Ok(reply.clone()),
            Some((_, Err(e))) => Err(anyhow!("{e}")),
            None => Err(anyhow!("no recorded reply for this window")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Personal,
    Project,
    Both,
    Neither,
}

impl Side {
    fn label(self) -> &'static str {
        match self {
            Side::Personal => "personal",
            Side::Project => "project",
            Side::Both => "both",
            Side::Neither => "other",
        }
    }
}

fn mentions(text: &str, markers: &[&str]) -> bool {
    let loose = fold(text);
    markers.iter().any(|m| loose.contains(fold(m).as_str()))
}

fn side(m: &Measure, text: &str) -> Side {
    let personal: Vec<&str> = m
        .windows
        .iter()
        .flat_map(|w| w.personal_markers.iter().map(String::as_str))
        .collect();
    let project: Vec<&str> = m
        .windows
        .iter()
        .flat_map(|w| w.project_markers.iter().map(String::as_str))
        .collect();
    match (mentions(text, &personal), mentions(text, &project)) {
        (true, true) => Side::Both,
        (true, false) => Side::Personal,
        (false, true) => Side::Project,
        (false, false) => Side::Neither,
    }
}

/// The live facts of a bank that were not there before, newest first.
fn fresh(b: &Bench, bank: &str, seen: &mut HashSet<String>) -> Vec<facts::store::StoredFact> {
    let all = b
        .facts
        .store
        .recent(bank, 10_000, true, Scope::Agent)
        .expect("list the facts");
    let new: Vec<_> = all.into_iter().filter(|f| !seen.contains(&f.id)).collect();
    seen.extend(new.iter().map(|f| f.id.clone()));
    new
}

fn project_row_for(m: &Measure) -> Project {
    project_row(&m.project, true)
}

async fn retain_windows(
    b: &Bench,
    m: &Measure,
    llm: &dyn LlmClient,
    target: &facts::FactTarget,
    label: &str,
    show: bool,
) -> Vec<facts::RetainReport> {
    let mut seen = HashSet::new();
    let mut reports = Vec::new();
    for w in &m.windows {
        let report = facts::retain(
            &b.facts,
            llm,
            defense::Mode::Redact,
            &RetainRequest {
                bank: &target.bank,
                text: &w.text,
                source: "eval",
                now: now_of(m),
                origin: &target.origin,
            },
        )
        .await;
        if show {
            eprintln!(
                "\n[{label}] window {} ({}): extracted {} stored {} reinforced {} folded {} superseded {} failed chunks {}",
                w.id,
                w.kind,
                report.extracted,
                report.stored,
                report.reinforced,
                report.folded,
                report.superseded,
                report.failed_chunks
            );
            for f in fresh(b, &target.bank, &mut seen) {
                eprintln!(
                    "    stored [{}] {}",
                    side(m, &f.text).label(),
                    clip(&f.text, 220)
                );
            }
        }
        reports.push(report);
    }
    reports
}

/// One question through the fact arms the daemon runs.
async fn ask(
    b: &Bench,
    m: &Measure,
    case: &Case,
    chat: &str,
    project: Option<&Project>,
    by_chat: bool,
) -> Row {
    let limit = Config::from_env().memory.search_max_results as usize;
    let query = RecallQuery {
        text: case.text.clone(),
        folder: m.folder.clone(),
        chat_jid: chat.to_string(),
        facts_by_chat: by_chat,
        limit,
        now: now_of(m),
        project: recall_scope(project),
    };
    let arms = facts::fact_arms(Arc::clone(&b.facts));
    let recalled = recall(&arms, &query, &RecallConfig::default()).await;
    grade(case, &format::render(&recalled.items))
}

async fn answered_from(
    b: &Bench,
    m: &Measure,
    cases: &[Case],
    chat: &str,
    project: Option<&Project>,
    by_chat: bool,
) -> (usize, usize) {
    let mut got = 0;
    let mut leaked = 0;
    for c in cases {
        let row = ask(b, m, c, chat, project, by_chat).await;
        got += row.answered as usize;
        leaked += row.leaked as usize;
    }
    (got, leaked)
}

/// How many personal facts a project chat's conversations leave in the project's
/// bank, and what the person loses for it: the same personal questions, asked
/// from a chat outside the project and from another chat of the project, with
/// the same model replies stored in the project's bank (after) and in the
/// agent's bank tagged with the chat they were said in (before the project).
async fn run_personal_in_project() {
    let m = measure();
    let (model, model_name) = eval_model();
    let project = project_row_for(&m);

    // After: the windows of a project chat, retained where the project says.
    let after = bench();
    let target =
        fact_target(Some(&project), &m.folder, &m.chat, true).expect("project keeps facts");
    eprintln!(
        "\n== personal facts in a project chat: model {model_name}, {} windows, {} personal and {} project questions ==",
        m.windows.len(),
        m.personal_questions.len(),
        m.project_questions.len()
    );
    eprintln!("bank {}, origin {:?}", target.bank, target.origin);
    let reports = retain_windows(&after, &m, &model, &target, "after", true).await;

    let calls = model.calls.lock().unwrap();
    let failed = calls.iter().filter(|c| c.reply.is_err()).count();
    assert!(
        failed < calls.len(),
        "no call reached the model ({failed} of {} failed): check SENCLAW_EVAL_LLM_BASE_URL",
        calls.len()
    );
    let mut millis: Vec<u128> = calls.iter().map(|c| c.millis).collect();
    millis.sort_unstable();
    let replay = Replay(
        m.windows
            .iter()
            .filter_map(|w| {
                calls
                    .iter()
                    .find(|c| c.prompt.contains(w.text.as_str()))
                    .map(|c| (w.text.clone(), c.reply.clone()))
            })
            .collect(),
    );
    let model_calls = calls.len();
    drop(calls);

    // What the project's bank holds.
    let live = after
        .facts
        .store
        .recent(&target.bank, 10_000, false, Scope::Agent)
        .expect("list the facts");
    let mut by_side = [0usize; 4];
    eprintln!("\nproject bank after the run ({} live facts):", live.len());
    for f in &live {
        let s = side(&m, &f.text);
        by_side[s as usize] += 1;
        eprintln!("    [{}] {}", s.label(), clip(&f.text, 220));
    }
    let personal = by_side[Side::Personal as usize] + by_side[Side::Both as usize];
    eprintln!(
        "\nclassified by marker: personal {} / project {} / both {} / other {}",
        by_side[Side::Personal as usize],
        by_side[Side::Project as usize],
        by_side[Side::Both as usize],
        by_side[Side::Neither as usize]
    );

    // Before: the same replies, retained into the agent's bank tagged with the chat.
    let before = bench();
    let plain = fact_target(None, &m.folder, &m.chat, true).expect("a plain chat keeps facts");
    retain_windows(&before, &m, &replay, &plain, "before", false).await;

    let pq = &m.personal_questions;
    let n = pq.len();
    let row = |label: &str, before: Option<(usize, usize)>, after: Option<(usize, usize)>| {
        let cell = |v: Option<(usize, usize)>| {
            v.map_or("-".to_string(), |(a, l)| format!("{a}/{n} (leaked {l})"))
        };
        eprintln!("    {:<52} {:<18} {}", label, cell(before), cell(after));
    };
    eprintln!("\npersonal questions answered:");
    eprintln!(
        "    {:<52} {:<18} {}",
        "asked from", "no project", "in the project"
    );
    row(
        "the chat they were said in",
        Some(answered_from(&before, &m, pq, &m.chat, None, true).await),
        Some(answered_from(&after, &m, pq, &m.chat, Some(&project), true).await),
    );
    row(
        "a chat outside the project (facts scoped by chat)",
        Some(answered_from(&before, &m, pq, &m.outside_chat, None, true).await),
        Some(answered_from(&after, &m, pq, &m.outside_chat, None, true).await),
    );
    row(
        "a chat outside the project (facts shared by agent)",
        Some(answered_from(&before, &m, pq, &m.outside_chat, None, false).await),
        Some(answered_from(&after, &m, pq, &m.outside_chat, None, false).await),
    );
    row(
        "another chat of the project",
        None,
        Some(answered_from(&after, &m, pq, &m.sibling_chat, Some(&project), true).await),
    );

    let proj = &m.project_questions;
    let (got, _) = answered_from(&after, &m, proj, &m.sibling_chat, Some(&project), true).await;
    let (_, leaked) = answered_from(&after, &m, proj, &m.outside_chat, None, true).await;
    eprintln!(
        "\nproject questions answered from another chat of the project: {got}/{}; project markers that reached a chat outside it: {leaked}",
        proj.len()
    );
    for c in proj {
        let row = ask(&after, &m, c, &m.sibling_chat, Some(&project), true).await;
        eprintln!(
            "    {:<10} {}",
            c.id,
            match (row.leaked, row.answered) {
                (true, _) => "STALE VALUE RECALLED",
                (false, true) => "ok",
                (false, false) => "missed",
            }
        );
    }

    eprintln!(
        "\nsummary [{model_name}]: windows {} (project {}, personal {}, mixed {}); facts stored in the project bank {}; \
         personal or both {personal}; extracted {} stored {} reinforced {} failed chunks {}; model calls {model_calls} (failed {failed}), median {} ms",
        m.windows.len(),
        m.windows.iter().filter(|w| w.kind == "project").count(),
        m.windows.iter().filter(|w| w.kind == "personal").count(),
        m.windows.iter().filter(|w| w.kind == "mixed").count(),
        live.len(),
        reports.iter().map(|r| r.extracted).sum::<usize>(),
        reports.iter().map(|r| r.stored).sum::<usize>(),
        reports.iter().map(|r| r.reinforced).sum::<usize>(),
        reports.iter().map(|r| r.failed_chunks).sum::<usize>(),
        millis[millis.len() / 2],
    );
}

#[tokio::test]
#[ignore = "calls a real model: set SENCLAW_EVAL_LLM_BASE_URL and SENCLAW_EVAL_LLM_MODEL"]
async fn real_extractor_personal_facts_in_a_project_chat() {
    run_personal_in_project().await;
}

/// The summaries a kanban worker writes when it closes a card, retained the way
/// the dispatcher retains them: how many facts each leaves, how many are worth
/// keeping, and whether the next worker is told them.
async fn run_card_summaries() {
    let m = measure();
    let (model, model_name) = eval_model();
    let project = project_row_for(&m);
    let target = crate::projects::worker_fact_target(&project, true)
        .expect("the project keeps its workers' facts");
    let b = bench();
    eprintln!(
        "\n== card summaries: model {model_name}, {} summaries, bank {} ==",
        m.cards.len(),
        target.bank
    );

    let mut seen = HashSet::new();
    let mut useful = 0usize;
    let mut junk = 0usize;
    let mut total = 0usize;
    let mut empty_for_none = 0usize;
    let mut none_cards = 0usize;
    let mut secret_leaks = 0usize;
    let mut redacted = 0usize;
    let mut reports = Vec::new();
    for card in &m.cards {
        let text = format!(
            "Kanban task \"{}\" completed: {}",
            card.title.trim(),
            card.summary.trim()
        );
        let report = facts::retain(
            &b.facts,
            &model,
            defense::Mode::Redact,
            &RetainRequest {
                bank: &target.bank,
                text: &text,
                source: "kanban",
                now: now_of(&m),
                origin: &target.origin,
            },
        )
        .await;
        let new = fresh(&b, &target.bank, &mut seen);
        eprintln!(
            "\n[{}] {} ({}, expect {}): {:?}\n    extracted {} stored {} reinforced {} folded {} superseded {} failed chunks {}{}",
            card.id,
            card.title,
            card.class,
            card.expect,
            clip(&card.summary, 110),
            report.extracted,
            report.stored,
            report.reinforced,
            report.folded,
            report.superseded,
            report.failed_chunks,
            if report.redacted.is_empty() {
                String::new()
            } else {
                format!(", scrubbed {:?}", report.redacted)
            }
        );
        redacted += report.redacted.len();
        if card.expect == "none" {
            none_cards += 1;
            empty_for_none += new.is_empty() as usize;
        }
        for f in &new {
            let worth = !card.worth.is_empty()
                && mentions(
                    &f.text,
                    &card.worth.iter().map(String::as_str).collect::<Vec<_>>(),
                );
            let verdict = if card.expect == "none" || !worth {
                junk += 1;
                "JUNK?"
            } else {
                useful += 1;
                "useful"
            };
            total += 1;
            secret_leaks += f.text.contains("sk_live") as usize;
            eprintln!("    stored [{verdict}] {}", clip(&f.text, 220));
        }
        reports.push(report);
    }

    let live = b
        .facts
        .store
        .recent(&target.bank, 10_000, false, Scope::Agent)
        .expect("list the facts");
    eprintln!("\nproject bank after the run ({} live facts):", live.len());
    for f in &live {
        eprintln!("    [{}] {}", f.source, clip(&f.text, 220));
    }

    // The next worker of the project asks its title and description.
    eprintln!("\nwhat the next worker is told:");
    let limit = Config::from_env().memory.search_max_results as usize;
    let mut told = 0usize;
    let mut stale = 0usize;
    for c in &m.card_questions {
        let query = worker_recall_query(&m.project, &c.text, limit, now_of(&m));
        let arms = facts::fact_arms(Arc::clone(&b.facts));
        let recalled = recall(&arms, &query, &RecallConfig::default()).await;
        let row = grade(c, &format::render(&recalled.items));
        told += row.answered as usize;
        stale += row.leaked as usize;
        eprintln!(
            "    {:<14} {}",
            c.id,
            match (row.leaked, row.answered) {
                (true, _) => "STALE VALUE RECALLED",
                (false, true) => "ok",
                (false, false) => "missed",
            }
        );
    }

    let failed_calls = model
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.reply.is_err())
        .count();
    let calls = model.calls.lock().unwrap().len();
    assert!(
        failed_calls < calls,
        "no call reached the model ({failed_calls} of {calls} failed): check SENCLAW_EVAL_LLM_BASE_URL"
    );
    let mut millis: Vec<u128> = model
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| c.millis)
        .collect();
    millis.sort_unstable();
    eprintln!(
        "\nsummary [{model_name}]: summaries {} ({} the person would call empty); facts stored {total} \
         (useful by marker {useful}, junk candidates {junk}); {empty_for_none}/{none_cards} empty summaries left no fact; \
         extracted {} stored {} reinforced {} folded {} failed chunks {}; secret reached a fact: {secret_leaks}; scrubbed {redacted}; \
         next-worker questions answered {told}/{} (stale value recalled {stale}); model calls {calls} (failed {failed_calls}), median {} ms",
        m.cards.len(),
        none_cards,
        reports.iter().map(|r| r.extracted).sum::<usize>(),
        reports.iter().map(|r| r.stored).sum::<usize>(),
        reports.iter().map(|r| r.reinforced).sum::<usize>(),
        reports.iter().map(|r| r.folded).sum::<usize>(),
        reports.iter().map(|r| r.failed_chunks).sum::<usize>(),
        m.card_questions.len(),
        millis[millis.len() / 2],
    );
}

#[tokio::test]
#[ignore = "calls a real model: set SENCLAW_EVAL_LLM_BASE_URL and SENCLAW_EVAL_LLM_MODEL"]
async fn real_extractor_card_summaries() {
    run_card_summaries().await;
}
