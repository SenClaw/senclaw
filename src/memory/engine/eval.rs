//! Retrieval eval: what does the agent see before a turn?
//!
//! `evals/memory/cases.json` holds a small corpus (curated notes, graph
//! chunks, one note written in another chat) and queries with the strings that
//! answer them. Each query is run through the two blocks recall used to
//! inject on their own ([`legacy`]) and through the unified engine, over the
//! same stores, and graded on:
//!
//! * **answered** — every `must_include` string is in the injected context;
//! * **leaked** — a `must_exclude` string (another chat's secret) is in it;
//! * **repeats** — how many times an answer string appears beyond the first,
//!   i.e. the same fact spending prompt budget twice;
//! * **tokens** — estimated size of what was injected.
//!
//! No model is called. The graph is built from canned triplets and given a
//! hashed-words stand-in for an embedding model (spreading activation seeds by
//! vector and returns nothing without one); curated notes are searched by
//! keyword only. The numbers reproduce but say nothing about the quality of a
//! real embedding. A second run drops the embedding model, which is the
//! default install, and shows where the old graph block goes empty. Set
//! `MEMORY_EVAL_OUT=<prefix>` to also write each table as JSON.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

use crate::config::{Config, EmbeddingProvider};
use crate::db::Db;
use crate::memory::chunker::estimate_tokens;
use crate::memory::cognitive::llm::LlmClient;
use crate::memory::cognitive::llm_openai::OpenAiCompatLlm;
use crate::memory::cognitive::{CognifyOptions, CognitiveSystem, NodeSet, NullEmbedder};
use crate::memory::curated;
use crate::memory::embedding::EmbeddingProvider as EmbeddingModel;
use crate::memory::manager::MemoryManager;
use crate::projects::{FactsWrite, Project};
use crate::security::replication::fold;

use super::facts::store::Scope;
use super::facts::{self, defense, extract, FactEngine, RetainRequest};
use super::{
    format, legacy, recall, CognitiveArm, CuratedArm, RecallArm, RecallConfig, RecallQuery,
    Recalled,
};

// ===== Fixture =====

#[derive(Deserialize)]
struct Fixture {
    folder: String,
    chat: String,
    now: String,
    notes: Vec<Note>,
    chunks: Vec<Chunk>,
    queries: Vec<Case>,
    #[serde(default)]
    fact_windows: Vec<FactWindow>,
    #[serde(default)]
    fact_queries: Vec<Case>,
    #[serde(default)]
    projects: Option<ProjectFixture>,
}

/// A conversation window and what the extraction model is scripted to answer
/// for it. `match` is a phrase that survives the secret scrub, so the script
/// still finds its window when the text it was written for has been redacted.
#[derive(Deserialize)]
struct FactWindow {
    #[serde(rename = "match")]
    needle: String,
    text: String,
    facts: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct Note {
    name: String,
    description: String,
    #[serde(rename = "type")]
    kind: String,
    created: String,
    origin: Option<String>,
    body: String,
}

#[derive(Deserialize)]
struct Chunk {
    text: String,
    triplets: Vec<serde_json::Value>,
}

#[derive(Deserialize, Clone)]
struct Case {
    id: String,
    text: String,
    #[serde(default)]
    must_include: Vec<String>,
    #[serde(default)]
    must_exclude: Vec<String>,
    /// `must_exclude` holds a credential that must not reach the extraction
    /// model at all. Without it the strings are only kept out of the context
    /// (a retired fact is still part of the conversation the model reads).
    #[serde(default)]
    secret: bool,
}

fn fixture() -> Fixture {
    serde_json::from_str(include_str!("../../../evals/memory/cases.json"))
        .expect("evals/memory/cases.json parses")
}

// ===== World =====

/// Answers the graph's extraction call from the fixture instead of a model:
/// the reply is keyed by the chunk text found in the prompt. A chunk with no
/// match gets no triplets, which the sanity check on edge count then catches.
struct Scripted(Vec<(String, String)>);

#[async_trait]
impl LlmClient for Scripted {
    async fn complete(&self, _system: &str, user: &str) -> Result<String> {
        let reply = self
            .0
            .iter()
            .find(|(text, _)| user.contains(text.as_str()))
            .map(|(_, reply)| reply.clone())
            .unwrap_or_else(|| r#"{"triplets":[]}"#.to_string());
        Ok(reply)
    }
}

/// The fact extractor's counterpart of [`Scripted`]. It also keeps every
/// prompt it was sent, so a test can prove what the model was never shown.
struct ScriptedFacts {
    script: Vec<(String, Vec<serde_json::Value>)>,
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl LlmClient for ScriptedFacts {
    async fn complete(&self, _system: &str, user: &str) -> Result<String> {
        self.seen.lock().unwrap().push(user.to_string());
        // Match on the window itself. The prompt also lists stored facts, and
        // those can contain another window's phrase.
        let (window, known) = match user.split_once("\n\nKNOWN FACTS:") {
            Some((w, k)) => (w, k),
            None => (user, ""),
        };
        let Some((_, facts)) = self
            .script
            .iter()
            .find(|(needle, _)| window.contains(needle.as_str()))
        else {
            return Ok(r#"{"facts":[]}"#.to_string());
        };
        // `replaces_matching` stands in for a model reading the KNOWN FACTS list
        // and naming the entry it contradicts: find the numbered line that
        // contains the phrase and say its number.
        let facts: Vec<serde_json::Value> = facts
            .iter()
            .map(|f| {
                let mut f = f.clone();
                if let Some(phrase) = f
                    .as_object_mut()
                    .and_then(|o| o.remove("replaces_matching"))
                    .and_then(|p| p.as_str().map(str::to_string))
                {
                    let tag = known
                        .lines()
                        .find(|l| l.contains(phrase.as_str()))
                        .and_then(|l| l.split_whitespace().next())
                        .map(|t| t.trim_matches(['[', ']']).to_string());
                    f["replaces"] = serde_json::json!(tag.into_iter().collect::<Vec<_>>());
                }
                f
            })
            .collect();
        Ok(serde_json::json!({ "facts": facts }).to_string())
    }
}

/// A deterministic stand-in for an embedding model: words hashed into a fixed
/// number of buckets (with a sign, so unrelated words tend to cancel), then
/// normalised. Cosine similarity becomes word overlap — enough to give the
/// graph vector seeds, which spreading activation requires, without a model.
/// It stands in for both pipelines equally; it says nothing about how a real
/// embedding would rank.
struct HashedWords;

const HASH_DIMS: usize = 256;

#[async_trait]
impl EmbeddingModel for HashedWords {
    fn name(&self) -> &str {
        "hashed-words"
    }
    fn model(&self) -> &str {
        "hashed-words"
    }
    fn dimensions(&self) -> u32 {
        HASH_DIMS as u32
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| hash_words(t)).collect())
    }
}

fn hash_words(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; HASH_DIMS];
    for word in text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 2)
    {
        // FNV-1a: stable across runs and platforms, unlike the std hasher.
        let mut h: u64 = 0xcbf29ce484222325;
        for b in word.bytes() {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
        let sign = if (h >> 63) == 0 { 1.0 } else { -1.0 };
        v[(h % HASH_DIMS as u64) as usize] += sign;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    v.iter().map(|x| x / norm).collect()
}

struct World {
    mgr: Arc<MemoryManager>,
    sys: Arc<CognitiveSystem>,
    facts: Arc<FactEngine>,
    /// Every prompt the fact extractor received.
    fact_prompts: Arc<Mutex<Vec<String>>>,
    folder: String,
    chat: String,
    _dir: tempfile::TempDir,
}

/// Noon UTC on the fixture's day: the same calendar date in every timezone
/// from UTC-11 to UTC+11, so "yesterday" means the same thing wherever the
/// test runs.
fn fixture_now(fx: &Fixture) -> i64 {
    chrono::NaiveDate::parse_from_str(&fx.now, "%Y-%m-%d")
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp()
}

/// `vectors`: give the graph and the fact store an embedding model. Without
/// one — the default install — both fall back to keyword, entity and date.
async fn build_world(fx: &Fixture, vectors: bool) -> World {
    build_world_with(fx, vectors, None).await.0
}

/// [`build_world`], with the fact windows optionally read by `real` instead of
/// the script. Returns the retain report of each window, in fixture order. The
/// script is held to storing everything it promised; a real model is only
/// measured.
async fn build_world_with(
    fx: &Fixture,
    vectors: bool,
    real: Option<&dyn LlmClient>,
) -> (World, Vec<facts::RetainReport>) {
    let embedder = || -> Option<Arc<dyn EmbeddingModel>> {
        vectors.then(|| Arc::new(HashedWords) as Arc<dyn EmbeddingModel>)
    };
    let graph_embedder: Arc<dyn EmbeddingModel> =
        embedder().unwrap_or_else(|| Arc::new(NullEmbedder));
    let dir = tempfile::tempdir().expect("temp dir");
    let mut cfg = Config::from_env();
    cfg.memory.embedding_provider = EmbeddingProvider::None;
    cfg.paths.profiles_dir = dir.path().join("profiles");
    let db = Arc::new(Db::open_in_memory(&cfg).expect("in-memory db"));

    let base = cfg.paths.profiles_dir.join(&fx.folder);
    for n in &fx.notes {
        curated::save(
            &base,
            &n.name,
            &n.description,
            &n.body,
            &n.kind,
            None,
            n.origin.as_deref().unwrap_or(&fx.folder),
            &n.created,
            false,
        )
        .expect("save note");
    }
    let mgr = Arc::new(MemoryManager::new(Arc::clone(&db), &cfg));
    mgr.init_agent(&fx.folder).await;
    // The initial scan indexed the notes; the poller is not needed.
    mgr.destroy_agent(&fx.folder);

    let script = fx
        .chunks
        .iter()
        .map(|c| {
            (
                c.text.clone(),
                serde_json::json!({ "triplets": c.triplets }).to_string(),
            )
        })
        .collect();
    let sys = Arc::new(CognitiveSystem::with_sqlite(
        Arc::clone(&db),
        graph_embedder,
        Arc::new(Scripted(script)),
    ));
    let opts = CognifyOptions {
        node_sets: vec![NodeSet::group(&fx.folder, "default_memory")],
        ..CognifyOptions::default()
    };
    for c in &fx.chunks {
        sys.cognify(&c.text, "chat", &opts)
            .await
            .expect("cognify chunk");
    }
    let expected_edges: usize = fx.chunks.iter().map(|c| c.triplets.len()).sum();
    let edges = sys.stats().expect("stats").edges as usize;
    assert!(
        edges >= expected_edges,
        "fixture built {edges} edges for {expected_edges} triplets — a chunk did not reach the extractor"
    );

    // The fact store, filled through the real retain pipeline (scrub, parse,
    // embed, dedupe, store, link) with the extraction model scripted.
    let facts = Arc::new(FactEngine::new(Arc::clone(&db), embedder()));
    let fact_prompts = Arc::new(Mutex::new(Vec::new()));
    let scripted = ScriptedFacts {
        script: fx
            .fact_windows
            .iter()
            .map(|w| (w.needle.clone(), w.facts.clone()))
            .collect(),
        seen: Arc::clone(&fact_prompts),
    };
    let extractor: &dyn LlmClient = real.unwrap_or(&scripted);
    let mut reports = Vec::new();
    for w in &fx.fact_windows {
        let report = facts::retain(
            &facts,
            extractor,
            defense::Mode::Redact,
            &RetainRequest {
                bank: &fx.folder,
                text: &w.text,
                source: "eval",
                now: fixture_now(fx),
                origin: "",
            },
        )
        .await;
        if real.is_none() {
            assert_eq!(
                report.stored,
                w.facts.len(),
                "window {:?} stored {} of {} scripted facts ({report:?})",
                w.needle,
                report.stored,
                w.facts.len()
            );
        }
        reports.push(report);
    }

    let world = World {
        mgr,
        sys,
        facts,
        fact_prompts,
        folder: fx.folder.clone(),
        chat: fx.chat.clone(),
        _dir: dir,
    };
    (world, reports)
}

// ===== Running and grading =====

const TIMING_REPS: usize = 5;

#[derive(Debug, Clone, Default, serde::Serialize)]
struct Row {
    id: String,
    answered: bool,
    leaked: bool,
    repeats: usize,
    tokens: usize,
    /// `answered`, were diacritics not compared: a model that writes "xóa"
    /// where the fixture has "xoá" is spelling, not forgetting. Only the real
    /// extractor's printout reads it.
    #[serde(skip)]
    folded: bool,
}

#[derive(Debug, Default, serde::Serialize)]
struct Totals {
    answered: usize,
    leaked: usize,
    repeats: usize,
    tokens: usize,
    micros_median: u128,
    micros_p95: u128,
}

fn grade(case: &Case, context: &str) -> Row {
    let loose = fold(context);
    Row {
        id: case.id.clone(),
        folded: case
            .must_include
            .iter()
            .all(|m| loose.contains(fold(m).as_str())),
        answered: case
            .must_include
            .iter()
            .all(|m| context.contains(m.as_str())),
        leaked: case
            .must_exclude
            .iter()
            .any(|m| context.contains(m.as_str())),
        repeats: case
            .must_include
            .iter()
            .map(|m| context.matches(m.as_str()).count().saturating_sub(1))
            .sum(),
        tokens: estimate_tokens(context),
    }
}

fn totals(rows: &[Row], mut micros: Vec<u128>) -> Totals {
    micros.sort_unstable();
    let pick = |q: f64| -> u128 {
        if micros.is_empty() {
            return 0;
        }
        micros[((micros.len() - 1) as f64 * q).round() as usize]
    };
    Totals {
        answered: rows.iter().filter(|r| r.answered).count(),
        leaked: rows.iter().filter(|r| r.leaked).count(),
        repeats: rows.iter().map(|r| r.repeats).sum(),
        tokens: rows.iter().map(|r| r.tokens).sum(),
        micros_median: pick(0.5),
        micros_p95: pick(0.95),
    }
}

async fn run_old(w: &World, q: &str, max: usize) -> String {
    let graph = legacy::cognitive_block(&w.sys, q, &w.folder, max).await;
    let notes = legacy::curated_block(&w.mgr, q, &w.folder, &w.chat, max).await;
    format!("{graph}\n{notes}")
}

/// The rendered block and what the pipeline reported about producing it.
async fn run_engine(
    w: &World,
    q: &str,
    max: usize,
    now: i64,
    cfg: &RecallConfig,
    with_facts: bool,
) -> (String, Recalled) {
    let mut arms: Vec<Arc<dyn RecallArm>> = vec![
        Arc::new(CuratedArm::new(Arc::clone(&w.mgr))),
        Arc::new(CognitiveArm::new(Arc::clone(&w.sys))),
    ];
    if with_facts {
        // The arms the daemon runs, the project ones included: asked from no
        // project they answer nothing.
        arms.extend(facts::fact_arms(Arc::clone(&w.facts)));
    }
    let query = RecallQuery {
        text: q.to_string(),
        folder: w.folder.clone(),
        chat_jid: w.chat.clone(),
        facts_by_chat: true,
        limit: max,
        now,
        project: None,
    };
    let recalled = recall(&arms, &query, cfg).await;
    (format::render(&recalled.items), recalled)
}

/// Both pipelines over the same fixture; prints the table and returns
/// `(old, unified)` totals.
async fn compare(label: &str, vectors: bool) -> (Totals, Totals) {
    let fx = fixture();
    // One world per pipeline: spreading activation strengthens the edges it
    // walks, so a shared graph would let whichever ran first change what the
    // other sees.
    let w_old = build_world(&fx, vectors).await;
    let w_new = build_world(&fx, vectors).await;
    let max = Config::from_env().memory.search_max_results as usize;
    let now = fixture_now(&fx);
    let cfg = RecallConfig::default();

    let (mut old_rows, mut new_rows) = (Vec::new(), Vec::new());
    let (mut old_us, mut new_us) = (Vec::new(), Vec::new());
    let (mut arm_hits, mut folded): (Vec<String>, Vec<usize>) = (Vec::new(), Vec::new());
    for case in &fx.queries {
        let old = run_old(&w_old, &case.text, max).await;
        let (new, recalled) = run_engine(&w_new, &case.text, max, now, &cfg, false).await;
        // One oversized result is allowed through (a match is never answered
        // with nothing); anything more must fit.
        assert!(
            recalled.tokens <= cfg.max_tokens || recalled.items.len() == 1,
            "{}: packed {} tokens ({} items) into a {} budget",
            case.id,
            recalled.tokens,
            recalled.items.len(),
            cfg.max_tokens
        );
        arm_hits.push(
            recalled
                .arms
                .iter()
                .map(|a| format!("{}:{}", &a.arm[..3], a.hits))
                .collect::<Vec<_>>()
                .join(" "),
        );
        folded.push(recalled.merged_duplicates);
        old_rows.push(grade(case, &old));
        new_rows.push(grade(case, &new));

        for _ in 0..TIMING_REPS {
            let t = Instant::now();
            run_old(&w_old, &case.text, max).await;
            old_us.push(t.elapsed().as_micros());
            let t = Instant::now();
            run_engine(&w_new, &case.text, max, now, &cfg, false).await;
            new_us.push(t.elapsed().as_micros());
        }
    }
    let old = totals(&old_rows, old_us);
    let new = totals(&new_rows, new_us);

    eprintln!(
        "\n{:<32} {:>22} {:>22}  engine arms",
        "query", "old blocks", "unified engine"
    );
    eprintln!(
        "{:<32} {:>22} {:>22}",
        "", "ans leak rep tok", "ans leak rep tok"
    );
    for (i, (o, n)) in old_rows.iter().zip(&new_rows).enumerate() {
        let cell = |r: &Row| {
            format!(
                "{:>3} {:>4} {:>3} {:>3}",
                r.answered as u8, r.leaked as u8, r.repeats, r.tokens
            )
        };
        eprintln!(
            "{:<32} {:>22} {:>22}  {} (folded {})",
            o.id,
            cell(o),
            cell(n),
            arm_hits[i],
            folded[i]
        );
    }
    let line = |label: &str, t: &Totals| {
        eprintln!(
            "{label:<14} answered {}/{}  leaked {}  repeats {}  tokens {}  latency median {}µs p95 {}µs",
            t.answered,
            fx.queries.len(),
            t.leaked,
            t.repeats,
            t.tokens,
            t.micros_median,
            t.micros_p95
        );
    };
    line("old blocks:", &old);
    line("unified:", &new);

    // `<MEMORY_EVAL_OUT>.<mode>.json` — one file per mode.
    if let Ok(path) = std::env::var("MEMORY_EVAL_OUT") {
        let report = serde_json::json!({
            "mode": label,
            "queries": fx.queries.len(),
            "old": { "totals": old, "rows": old_rows },
            "unified": { "totals": new, "rows": new_rows },
        });
        let mode = if vectors { "vectors" } else { "keyword-only" };
        std::fs::write(
            format!("{path}.{mode}.json"),
            serde_json::to_string_pretty(&report).unwrap(),
        )
        .unwrap();
    }

    assert_eq!(old.leaked, 0, "the old blocks leaked another chat's note");
    assert_eq!(new.leaked, 0, "the engine leaked another chat's note");
    assert!(
        new.repeats <= old.repeats,
        "the engine repeats answers more ({}) than the old blocks ({})",
        new.repeats,
        old.repeats
    );
    (old, new)
}

#[tokio::test]
async fn with_an_embedding_model_the_engine_answers_what_the_old_blocks_answered_and_repeats_less()
{
    let (old, new) = compare("graph seeded by vectors", true).await;
    assert!(
        new.answered >= old.answered,
        "the engine answered {} queries, the old blocks {}",
        new.answered,
        old.answered
    );
}

/// The default install has no embedding provider. Spreading activation seeds by
/// vector alone, so `<cognitive_memory>` is then empty and everything only the
/// graph knows is unreachable; the engine's graph arm falls back to keyword
/// seeds and still reaches it.
#[tokio::test]
async fn without_an_embedding_model_the_engine_still_reaches_the_graph() {
    let (old, new) = compare("no embedding provider (keyword only)", false).await;
    assert!(
        new.answered > old.answered,
        "the engine answered {} queries, the old blocks {}: the keyword fallback reached nothing new",
        new.answered,
        old.answered
    );
}

/// The engine with and without the fact store, on the questions only facts can
/// answer: a time in the question, a conversation that spans turns, something
/// the assistant did, an answer two entities away. Returns the per-query rows
/// `(without, with)`.
async fn compare_facts(label: &str, vectors: bool) -> (Vec<Row>, Vec<Row>) {
    let fx = fixture();
    assert!(
        !fx.fact_queries.is_empty(),
        "the fixture has no fact queries"
    );
    let world = build_world(&fx, vectors).await;
    let rows = ask_fact_queries(label, &fx, &fx.fact_queries, &world, true).await;
    assert_secrets_kept_out(&fx, &world.fact_prompts.lock().unwrap());
    rows
}

/// Whatever else happened, the secret typed in the chat reached neither the
/// extraction model nor anything it could have stored.
fn assert_secrets_kept_out(fx: &Fixture, prompts: &[String]) {
    assert!(!prompts.is_empty(), "the extractor was never called");
    for secret in fx
        .fact_queries
        .iter()
        .filter(|c| c.secret)
        .flat_map(|c| c.must_exclude.iter())
    {
        assert!(
            prompts.iter().all(|p| !p.contains(secret.as_str())),
            "{secret:?} was sent to the extraction model"
        );
    }
}

/// Every fact query through the engine without and with the fact arms, graded
/// and printed. `show_misses` also prints the context of a query the fact
/// store failed or leaked on, for a run whose extraction is not scripted.
async fn ask_fact_queries(
    label: &str,
    fx: &Fixture,
    cases: &[Case],
    world: &World,
    show_misses: bool,
) -> (Vec<Row>, Vec<Row>) {
    let max = Config::from_env().memory.search_max_results as usize;
    let now = fixture_now(fx);
    let cfg = RecallConfig::default();

    let (mut without, mut with) = (Vec::new(), Vec::new());
    eprintln!("\n== facts: {label} ==");
    eprintln!(
        "{:<26} {:>12} {:>12}  fact arms (sem/key/gra/tem)",
        "query", "no facts", "with facts"
    );
    for case in cases {
        let (plain, _) = run_engine(world, &case.text, max, now, &cfg, false).await;
        let (full, recalled) = run_engine(world, &case.text, max, now, &cfg, true).await;
        let (a, b) = (grade(case, &plain), grade(case, &full));
        let per_arm = [
            "facts_semantic",
            "facts_keyword",
            "facts_graph",
            "facts_temporal",
        ]
        .iter()
        .map(|name| {
            recalled
                .arms
                .iter()
                .find(|a| a.arm == *name)
                .map_or(0, |a| a.hits)
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("/");
        eprintln!(
            "{:<26} {:>12} {:>12}  {per_arm}",
            case.id,
            if a.answered { "answered" } else { "-" },
            match (b.leaked, b.answered) {
                (true, _) => "LEAKED",
                (false, true) => "answered",
                (false, false) => "MISSED",
            },
        );
        if show_misses && (b.leaked || !b.answered) {
            eprintln!(
                "    wanted {:?}, not {:?}{}",
                case.must_include,
                case.must_exclude,
                if !b.leaked && b.folded {
                    " — spelling only: it matches once diacritics are folded"
                } else {
                    ""
                }
            );
            eprintln!("    injected: {}", full.replace('\n', "\n      "));
        }
        without.push(a);
        with.push(b);
    }
    eprintln!(
        "{label}: without facts {}/{}, with facts {}/{}, leaked {}",
        answered(&without),
        without.len(),
        answered(&with),
        with.len(),
        with.iter().filter(|r| r.leaked).count()
    );
    (without, with)
}

fn answered(rows: &[Row]) -> usize {
    rows.iter().filter(|r| r.answered).count()
}

#[tokio::test]
async fn with_an_embedding_model_facts_answer_what_notes_and_the_graph_cannot() {
    let (without, with) = compare_facts("vectors", true).await;
    assert!(
        with.iter().all(|r| !r.leaked),
        "a secret leaked into the context"
    );
    let missed: Vec<_> = with.iter().filter(|r| !r.answered).map(|r| &r.id).collect();
    assert!(missed.is_empty(), "the fact store missed {missed:?}");
    assert!(answered(&with) > answered(&without));
}

#[tokio::test]
async fn without_an_embedding_model_facts_are_found_by_word_entity_and_date() {
    let (without, with) = compare_facts("keyword only", false).await;
    assert!(
        with.iter().all(|r| !r.leaked),
        "a secret leaked into the context"
    );
    let missed: Vec<_> = with.iter().filter(|r| !r.answered).map(|r| &r.id).collect();
    assert!(missed.is_empty(), "the fact store missed {missed:?}");
    assert!(answered(&with) > answered(&without));
}

/// One arm on its own: what would the agent be shown if only it ran?
async fn alone(w: &World, arm: Arc<dyn RecallArm>, text: &str, now: i64) -> String {
    let query = RecallQuery {
        text: text.to_string(),
        folder: w.folder.clone(),
        chat_jid: w.chat.clone(),
        facts_by_chat: true,
        limit: 5,
        now,
        project: None,
    };
    let recalled = recall(&[arm], &query, &RecallConfig::default()).await;
    format::render(&recalled.items)
}

fn case<'a>(fx: &'a Fixture, id: &str) -> &'a Case {
    fx.fact_queries
        .iter()
        .find(|c| c.id == id)
        .unwrap_or_else(|| panic!("no fact query {id:?}"))
}

/// An arm that finds nothing the others do not is dead weight. Each of these
/// questions is one the named arm answers and keyword search alone does not.
#[tokio::test]
async fn the_graph_and_temporal_arms_each_find_what_keyword_search_cannot() {
    let fx = fixture();
    let w = build_world(&fx, false).await;
    let now = fixture_now(&fx);
    let keyword = || -> Arc<dyn RecallArm> { Arc::new(facts::KeywordArm(Arc::clone(&w.facts))) };

    // The answer ("Lan nghỉ phép") shares no word with the question; only the
    // entity Lan joins it to the fact that names Falcon.
    let hop = case(&fx, "entity-hop-vi");
    let graph = alone(
        &w,
        Arc::new(facts::GraphArm(Arc::clone(&w.facts))),
        &hop.text,
        now,
    )
    .await;
    assert!(
        graph.contains(&hop.must_include[0]),
        "graph arm alone: {graph}"
    );
    let kw = alone(&w, keyword(), &hop.text, now).await;
    assert!(
        !kw.contains(&hop.must_include[0]),
        "keyword alone already answers: {kw}"
    );

    // The fact says "2026-10-15"; the question says "hôm qua". Only reading the
    // date out of the question connects them.
    let day = case(&fx, "yesterday-vi");
    let temporal = alone(
        &w,
        Arc::new(facts::TemporalArm(Arc::clone(&w.facts))),
        &day.text,
        now,
    )
    .await;
    assert!(
        temporal.contains(&day.must_include[0]),
        "temporal arm alone: {temporal}"
    );
    let kw = alone(&w, keyword(), &day.text, now).await;
    assert!(
        !kw.contains(&day.must_include[0]),
        "keyword alone already answers: {kw}"
    );

    // A question that names no time leaves the temporal arm silent.
    let silent = alone(
        &w,
        Arc::new(facts::TemporalArm(Arc::clone(&w.facts))),
        &hop.text,
        now,
    )
    .await;
    assert_eq!(silent, "");
}

#[tokio::test]
async fn the_semantic_arm_needs_an_embedding_model_and_is_silent_without_one() {
    let fx = fixture();
    // Worded to share enough with the fact for the stand-in embedder (word
    // overlap) to put them above the arm's similarity floor.
    let question = "Emily chuyển đến đâu và làm việc tại đâu";
    let now = fixture_now(&fx);

    let with = build_world(&fx, true).await;
    let found = alone(
        &with,
        Arc::new(facts::SemanticArm(Arc::clone(&with.facts))),
        question,
        now,
    )
    .await;
    assert!(found.contains("Huế"), "{found}");

    let without = build_world(&fx, false).await;
    let none = alone(
        &without,
        Arc::new(facts::SemanticArm(Arc::clone(&without.facts))),
        question,
        now,
    )
    .await;
    assert_eq!(none, "");
}

// ===== Projects =====
//
// A project keeps its facts in a bank of its own that every chat of the project
// reads and writes. These run the fixture's `projects` section through the same
// functions the agent pool uses to pick where a window is retained
// (`projects::fact_target`) and what a question reads (`projects::recall_scope`),
// then through the arms the daemon runs.

#[derive(Deserialize)]
struct ProjectFixture {
    p: String,
    q: String,
    windows: Vec<ProjectWindow>,
    queries: Vec<ProjectCase>,
}

impl ProjectFixture {
    /// The id of project `p` or `q`.
    fn id(&self, key: &str) -> &str {
        match key {
            "p" => &self.p,
            "q" => &self.q,
            other => panic!("the fixture has no project {other:?}"),
        }
    }
}

#[derive(Deserialize)]
struct ProjectWindow {
    project: String,
    /// The chat the window was spoken in.
    chat: String,
    #[serde(rename = "match")]
    needle: String,
    text: String,
    facts: Vec<serde_json::Value>,
}

/// A question, and where it is asked from.
#[derive(Deserialize)]
struct ProjectCase {
    #[serde(flatten)]
    case: Case,
    from: Asker,
}

#[derive(Deserialize)]
struct Asker {
    /// `p`, `q`, or none for a chat outside any project.
    project: Option<String>,
    chat: String,
    #[serde(default = "yes")]
    include_common: bool,
}

fn yes() -> bool {
    true
}

/// Strings that only a project's facts contain: they must never turn up in a
/// chat outside it.
const PROJECT_MARKERS: [&str; 5] = ["7421", "900 triệu", "thứ Sáu", "Mặt Trời", "Biển Xanh"];

fn project_row(id: &str, include_common: bool) -> Project {
    Project {
        id: id.into(),
        name: id.into(),
        root: "/nowhere".into(),
        description: String::new(),
        facts_write: FactsWrite::Project,
        inherit_common: include_common,
        created_at: 1,
        updated_at: 1,
        archived_at: None,
    }
}

fn project_fixture(fx: &Fixture) -> &ProjectFixture {
    fx.projects.as_ref().expect("the fixture has projects")
}

/// The fixture's project windows, retained where the project says: the real
/// pipeline with the extraction model scripted, in fixture order.
async fn retain_project_windows(w: &World, fx: &Fixture) {
    let pf = project_fixture(fx);
    let scripted = ScriptedFacts {
        script: pf
            .windows
            .iter()
            .map(|win| (win.needle.clone(), win.facts.clone()))
            .collect(),
        seen: Arc::new(Mutex::new(Vec::new())),
    };
    for win in &pf.windows {
        let project = project_row(pf.id(&win.project), true);
        let target = crate::projects::fact_target(Some(&project), &w.folder, &win.chat, true)
            .expect("a project that keeps its facts has somewhere to put them");
        let report = facts::retain(
            &w.facts,
            &scripted,
            defense::Mode::Redact,
            &RetainRequest {
                bank: &target.bank,
                text: &win.text,
                source: "eval",
                now: fixture_now(fx),
                origin: &target.origin,
            },
        )
        .await;
        assert_eq!(
            report.stored,
            win.facts.len(),
            "window {:?} stored {} of {} scripted facts ({report:?})",
            win.needle,
            report.stored,
            win.facts.len()
        );
    }
}

/// One project question through the fact arms the daemon runs.
async fn ask_from(w: &World, fx: &Fixture, pc: &ProjectCase) -> (String, Recalled) {
    let pf = project_fixture(fx);
    let project = pc.from.project.as_deref().map(|key| {
        crate::projects::recall_scope(Some(&project_row(pf.id(key), pc.from.include_common)))
            .expect("a project has a bank to read")
    });
    let query = RecallQuery {
        text: pc.case.text.clone(),
        folder: w.folder.clone(),
        chat_jid: pc.from.chat.clone(),
        facts_by_chat: true,
        limit: Config::from_env().memory.search_max_results as usize,
        now: fixture_now(fx),
        project,
    };
    let arms = facts::fact_arms(Arc::clone(&w.facts));
    let recalled = recall(&arms, &query, &RecallConfig::default()).await;
    (format::render(&recalled.items), recalled)
}

async fn run_project_cases(label: &str, vectors: bool) {
    let fx = fixture();
    let world = build_world(&fx, vectors).await;
    retain_project_windows(&world, &fx).await;

    eprintln!("\n== project facts: {label} ==");
    let mut rows = Vec::new();
    for pc in &project_fixture(&fx).queries {
        let (context, _) = ask_from(&world, &fx, pc).await;
        let row = grade(&pc.case, &context);
        eprintln!(
            "{:<34} {}",
            pc.case.id,
            match (row.leaked, row.answered) {
                (true, _) => "LEAKED",
                (false, true) => "ok",
                (false, false) => "MISSED",
            }
        );
        if row.leaked || !row.answered {
            eprintln!(
                "    wanted {:?}, not {:?}\n    injected: {}",
                pc.case.must_include,
                pc.case.must_exclude,
                context.replace('\n', "\n      ")
            );
        }
        rows.push(row);
    }
    let leaked: Vec<_> = rows.iter().filter(|r| r.leaked).map(|r| &r.id).collect();
    let missed: Vec<_> = rows.iter().filter(|r| !r.answered).map(|r| &r.id).collect();
    assert!(
        leaked.is_empty(),
        "leaked across a project's edge: {leaked:?}"
    );
    assert!(missed.is_empty(), "did not answer: {missed:?}");
}

#[tokio::test]
async fn with_an_embedding_model_a_projects_facts_stay_inside_it_and_follow_its_chats() {
    run_project_cases("vectors", true).await;
}

#[tokio::test]
async fn without_an_embedding_model_a_projects_facts_stay_inside_it_and_follow_its_chats() {
    run_project_cases("keyword only", false).await;
}

/// The questions of the rest of the fixture, asked from no project, are
/// answered with exactly what they were before a project held any fact — and
/// nothing a project knows is among it.
#[tokio::test]
async fn a_chat_outside_every_project_is_told_what_it_always_was() {
    let fx = fixture();
    let max = Config::from_env().memory.search_max_results as usize;
    let (now, cfg) = (fixture_now(&fx), RecallConfig::default());
    for vectors in [false, true] {
        let plain = build_world(&fx, vectors).await;
        let with = build_world(&fx, vectors).await;
        retain_project_windows(&with, &fx).await;
        // The facts found, whatever the order. Notes are left out of the
        // comparison: two worlds built apart break a tie between equally good
        // notes by the order the filesystem listed them, project or no project.
        let found = |r: &Recalled| {
            let mut bodies: Vec<String> = r
                .items
                .iter()
                .filter(|i| i.hit.arm == facts::FACTS)
                .map(|i| i.hit.body.clone())
                .collect();
            bodies.sort();
            bodies
        };
        for case in fx.queries.iter().chain(&fx.fact_queries) {
            let (_, before) = run_engine(&plain, &case.text, max, now, &cfg, true).await;
            let (after, recalled) = run_engine(&with, &case.text, max, now, &cfg, true).await;
            assert_eq!(
                found(&before),
                found(&recalled),
                "{}: a project's facts changed what the chat is told",
                case.id
            );
            for marker in PROJECT_MARKERS {
                assert!(
                    !after.contains(marker),
                    "{}: {marker:?} reached a chat outside the project",
                    case.id
                );
            }
        }
    }
}

/// The four project arms are the agent's four strategies pointed at another
/// bank, under the same `facts` label: one project fact that several of them
/// find is one result with a vote from each.
#[tokio::test]
async fn a_project_fact_found_by_several_arms_is_one_result_with_a_vote_from_each() {
    let fx = fixture();
    let world = build_world(&fx, true).await;
    retain_project_windows(&world, &fx).await;
    let pc = project_fixture(&fx)
        .queries
        .iter()
        .find(|c| c.case.id == "said-in-another-chat")
        .expect("the case exists");
    let (_, recalled) = ask_from(&world, &fx, pc).await;
    let port: Vec<_> = recalled
        .items
        .iter()
        .filter(|i| i.hit.body.contains("7421"))
        .collect();
    assert_eq!(port.len(), 1, "one fact, one result");
    assert_eq!(
        port[0].hit.arm,
        facts::FACTS,
        "same label as the agent's facts"
    );
    let strategies: Vec<&str> = port[0].sources.iter().map(|(s, _)| *s).collect();
    assert!(
        strategies.contains(&"facts_project_keyword")
            && strategies.contains(&"facts_project_semantic"),
        "{strategies:?}"
    );
    assert!(
        strategies.iter().all(|s| s.starts_with("facts_project_")),
        "the agent's own arms found nothing of the project's: {strategies:?}"
    );
    // All eight fact arms report, whether or not they found anything.
    let reported = recalled
        .arms
        .iter()
        .filter(|a| a.arm.starts_with("facts"))
        .count();
    assert_eq!(reported, 8);
    assert!(recalled.arms.iter().all(|a| a.error.is_none()));
}

/// A project is allowed to grow: thousands of facts must not make the question
/// of a chat in it slow. The budget is the one the plan set, on the path every
/// turn takes (no model is called), in a debug build.
#[tokio::test]
async fn the_project_arms_answer_within_150_ms_on_a_large_bank() {
    const FACTS: usize = 5_000;
    const BUDGET_MS: u64 = 150;
    let fx = fixture();
    let world = build_world(&fx, true).await;
    retain_project_windows(&world, &fx).await;
    let bank = "project:prj_0000000c";
    let now = fixture_now(&fx);
    let texts: Vec<String> = (0..FACTS)
        .map(|i| {
            format!(
                "Ghi chú số {i} về module {} của dịch vụ {} chạy ở cổng {}",
                i % 97,
                i % 31,
                7000 + i % 500
            )
        })
        .collect();
    let vectors = world.facts.embed_all(&texts).await;
    let model = world.facts.model_key();
    for (i, (text, embedding)) in texts.into_iter().zip(vectors).enumerate() {
        let day = now - (i as i64 % 60) * 86_400;
        world
            .facts
            .store
            .insert(
                bank,
                &facts::store::NewFact {
                    text,
                    kind: facts::store::FactKind::World,
                    occurred: (i % 5 == 0).then_some((day, day + 86_400)),
                    mentioned_at: now - i as i64,
                    entities: vec![format!("module {}", i % 97)],
                    embedding,
                    model: model.clone(),
                    source: "eval".into(),
                    origin: String::new(),
                    slot: None,
                },
            )
            .unwrap();
    }
    assert_eq!(world.facts.store.count(bank).unwrap(), FACTS);

    let questions = [
        "Module 12 của dịch vụ 5 chạy ở cổng nào?",
        "Ghi chú về cổng 7123",
        "Hôm qua có ghi chú nào về dịch vụ 7?",
        "Tháng này module 40 có gì",
        "dịch vụ 30 module 96",
        "Cổng 7499 là của dịch vụ nào?",
    ];
    let mut per_arm: std::collections::BTreeMap<&'static str, Vec<u64>> = Default::default();
    for _ in 0..5 {
        for text in questions {
            let query = RecallQuery {
                text: text.into(),
                folder: world.folder.clone(),
                chat_jid: "web:p-b".into(),
                facts_by_chat: true,
                limit: 5,
                now,
                project: Some(super::ProjectRecall {
                    bank: bank.into(),
                    include_common: true,
                }),
            };
            let arms = facts::fact_arms(Arc::clone(&world.facts));
            let recalled = recall(&arms, &query, &RecallConfig::default()).await;
            for a in recalled
                .arms
                .iter()
                .filter(|a| a.arm.starts_with("facts_project_"))
            {
                assert!(a.error.is_none(), "{}: {:?}", a.arm, a.error);
                per_arm.entry(a.arm).or_default().push(a.millis);
            }
        }
    }
    assert_eq!(per_arm.len(), 4);
    for (arm, mut ms) in per_arm {
        ms.sort_unstable();
        let p95 = ms[((ms.len() - 1) as f64 * 0.95).round() as usize];
        eprintln!(
            "{arm:<24} {} runs over {FACTS} facts: median {} ms, p95 {p95} ms, max {} ms",
            ms.len(),
            ms[ms.len() / 2],
            ms[ms.len() - 1]
        );
        assert!(
            p95 <= BUDGET_MS,
            "{arm}: p95 {p95} ms is over {BUDGET_MS} ms"
        );
    }
}

// ===== A real extraction model =====
//
// Everything above scripts the model that reads a conversation window into
// facts, so it proves the pipeline and says nothing about whether a small
// local model, reading Vietnamese, extracts what the fixture expects — or
// names the right `KNOWN FACTS` line in `replaces`. The tests below run the
// same windows and the same questions through the model at the endpoint named
// by the environment, with the production client (`OpenAiCompatLlm`, so its
// 60 s timeout is part of what is measured).
//
// They are `#[ignore]`: they cost model calls and the numbers move from run to
// run. They measure rather than gate — the only assertions are that the model
// answered at least once and that the secret never reached it.
//
//     SENCLAW_EVAL_LLM_BASE_URL=http://127.0.0.1:<port>/v1 \
//     SENCLAW_EVAL_LLM_MODEL=<model id> [SENCLAW_EVAL_LLM_API_KEY=<key>] \
//     <test binary> memory::engine::eval::real_extractor --ignored --nocapture

/// The live daemon's ports: a model reached through them is the user's own
/// install, which an eval must not touch.
const LIVE_DAEMON_PORTS: [u16; 2] = [18788, 18789];

/// One model call as the harness saw it.
struct Call {
    prompt: String,
    reply: std::result::Result<String, String>,
    millis: u128,
}

/// The model under test, remembering every call so a run can print what the
/// model was shown and what it answered.
struct Recording {
    inner: OpenAiCompatLlm,
    calls: Mutex<Vec<Call>>,
}

#[async_trait]
impl LlmClient for Recording {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let started = Instant::now();
        let reply = self.inner.complete(system, user).await;
        self.calls.lock().unwrap().push(Call {
            prompt: user.to_string(),
            reply: match &reply {
                Ok(r) => Ok(r.clone()),
                Err(e) => Err(format!("{e:#}")),
            },
            millis: started.elapsed().as_millis(),
        });
        reply
    }
}

/// The model named by `SENCLAW_EVAL_LLM_BASE_URL` / `_MODEL` / `_API_KEY`
/// (the key is optional: a local runtime wants none), and its name for the
/// printout. Read here, once per test, and nowhere in library code.
fn eval_model() -> (Recording, String) {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let (Some(base), Some(model)) = (
        var("SENCLAW_EVAL_LLM_BASE_URL"),
        var("SENCLAW_EVAL_LLM_MODEL"),
    ) else {
        panic!(
            "set SENCLAW_EVAL_LLM_BASE_URL (an OpenAI-compatible endpoint) and \
             SENCLAW_EVAL_LLM_MODEL; SENCLAW_EVAL_LLM_API_KEY is optional"
        );
    };
    let port = reqwest::Url::parse(&base)
        .ok()
        .and_then(|u| u.port_or_known_default());
    assert!(
        !port.is_some_and(|p| LIVE_DAEMON_PORTS.contains(&p)),
        "SENCLAW_EVAL_LLM_BASE_URL points at the live daemon's port; start a scratch runtime instead"
    );
    let key = var("SENCLAW_EVAL_LLM_API_KEY").unwrap_or_default();
    let inner = OpenAiCompatLlm::new(base, key, model.clone()).expect("build the model client");
    (
        Recording {
            inner,
            calls: Mutex::new(Vec::new()),
        },
        model,
    )
}

/// The `[F1] text` lines a prompt carried after `KNOWN FACTS:`, in order.
fn shown_facts(prompt: &str) -> Vec<String> {
    let Some((_, known)) = prompt.split_once("\n\nKNOWN FACTS:") else {
        return Vec::new();
    };
    known
        .lines()
        .filter_map(|l| l.trim().strip_prefix('['))
        .filter_map(|l| l.split_once("] "))
        .map(|(_, text)| text.to_string())
        .collect()
}

fn clip(s: &str, max_bytes: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut = crate::util::text::truncate_on_char_boundary(&flat, max_bytes);
    if cut.len() < flat.len() {
        format!("{cut}…")
    } else {
        flat
    }
}

/// `replaces` as the model wrote it, across the whole run.
#[derive(Default)]
struct ReplaceTally {
    /// Numbers the model named in `replaces`.
    claimed: usize,
    /// Of those, numbers beyond the `KNOWN FACTS` list it was shown.
    unshown: usize,
    /// Facts the model extracted, and how many of them came with a usable slot.
    facts: usize,
    slotted: usize,
}

/// Print what the model was shown and answered for one window, next to what
/// retain made of it.
fn print_window(
    n: usize,
    w: &FactWindow,
    report: &facts::RetainReport,
    calls: &[Call],
    tally: &mut ReplaceTally,
) -> Vec<extract::Extracted> {
    let mut all = Vec::new();
    eprintln!(
        "\n[{n}] {:?} — {} chunk(s), model said {} ms, scripted fixture has {} fact(s)",
        w.needle,
        report.chunks,
        calls.iter().map(|c| c.millis).sum::<u128>(),
        w.facts.len()
    );
    if !report.redacted.is_empty() {
        eprintln!("    scrubbed before the model: {:?}", report.redacted);
    }
    for call in calls {
        let shown = shown_facts(&call.prompt);
        for (i, text) in shown.iter().enumerate() {
            eprintln!("    shown   [F{}] {}", i + 1, clip(text, 160));
        }
        let raw = match &call.reply {
            Ok(raw) => raw,
            Err(e) => {
                eprintln!("    CALL FAILED: {}", clip(e, 300));
                continue;
            }
        };
        match extract::parse_facts(raw) {
            Err(e) => eprintln!(
                "    UNUSABLE REPLY ({}): {}",
                clip(&e.to_string(), 120),
                clip(raw, 1500)
            ),
            Ok(parsed) if parsed.is_empty() => eprintln!("    extracted nothing"),
            Ok(parsed) => {
                for f in &parsed {
                    let when = f
                        .occurred
                        .map_or("-".to_string(), |(s, e)| extract::span_label(s, e));
                    let replaces: Vec<String> =
                        f.replaces.iter().map(|r| format!("F{r}")).collect();
                    let slot = f.slot.as_ref().map_or("-".to_string(), |s| {
                        format!(
                            "{}{}",
                            s.label(),
                            if s.single_valued() { " (single)" } else { "" }
                        )
                    });
                    eprintln!(
                        "    extracted [{}] {} (when {when}, entities {:?}, slot {slot}, replaces {replaces:?})",
                        f.kind.as_str(),
                        clip(&f.text, 200),
                        f.entities
                    );
                    tally.facts += 1;
                    tally.slotted += f.slot.is_some() as usize;
                    tally.claimed += f.replaces.len();
                    tally.unshown += f.replaces.iter().filter(|r| **r > shown.len()).count();
                }
                all.extend(parsed);
            }
        }
    }
    eprintln!(
        "    retained: extracted {} stored {} reinforced {} superseded {} (by slot {}) failed chunks {}",
        report.extracted,
        report.stored,
        report.reinforced,
        report.superseded,
        report.superseded_by_slot,
        report.failed_chunks
    );
    all
}

/// What the real model wrote for the old stance of a changed-mind case: the
/// facts it extracted from the window whose scripted fact carries the stale
/// phrase, that name the same non-generic entity. The fixture's phrase alone
/// would miss "thích sử dụng React" for "thích dùng React", and a stale fact
/// nobody recognises is a leak nobody sees.
fn stale_facts(fx: &Fixture, case: &Case, parsed: &[Vec<extract::Extracted>]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for phrase in &case.must_exclude {
        for (w, window) in fx.fact_windows.iter().enumerate() {
            for scripted in &window.facts {
                let text = scripted["text"].as_str().unwrap_or_default();
                if !fold(text).contains(&fold(phrase)) {
                    continue;
                }
                let subjects: Vec<String> = scripted["entities"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.as_str())
                    .map(facts::store::entity_key)
                    .filter(|e| e != "user")
                    .collect();
                for f in parsed.get(w).into_iter().flatten() {
                    if f.entities.iter().any(|e| subjects.contains(e)) && !out.contains(&f.text) {
                        out.push(f.text.clone());
                    }
                }
            }
        }
    }
    out
}

/// The fixture's windows and questions through a real model.
async fn run_real_extractor(label: &str, vectors: bool) {
    let fx = fixture();
    let (model, model_name) = eval_model();
    let (world, reports) = build_world_with(&fx, vectors, Some(&model)).await;
    let calls = model.calls.lock().unwrap();

    eprintln!(
        "\n== real extractor: {label}, model {model_name}, {} window(s) ==",
        fx.fact_windows.len()
    );
    let mut tally = ReplaceTally::default();
    let mut parsed: Vec<Vec<extract::Extracted>> = Vec::new();
    let mut next = 0;
    for (n, (w, report)) in fx.fact_windows.iter().zip(&reports).enumerate() {
        // One model call per chunk, in order: a window owns the next
        // `chunks` calls (a blocked window made none).
        let mine = &calls[next..(next + report.chunks).min(calls.len())];
        next += report.chunks;
        parsed.push(print_window(n + 1, w, report, mine, &mut tally));
    }

    let honoured: usize = reports.iter().map(|r| r.superseded).sum();
    let by_slot: usize = reports.iter().map(|r| r.superseded_by_slot).sum();
    let ignored = tally.claimed.saturating_sub(honoured);
    eprintln!(
        "\nslots: the model gave one for {} of {} facts; {by_slot} fact(s) retired by rule, {} by a named `replaces`",
        tally.slotted,
        tally.facts,
        honoured.saturating_sub(by_slot)
    );
    eprintln!(
        "\nreplaces: the model named {} (honoured {honoured}, ignored {ignored}; \
         {} of the ignored pointed past the KNOWN FACTS list it was shown, the rest were \
         refused as another subject or came with a fact that was already stored)",
        tally.claimed, tally.unshown
    );
    let stored = world
        .facts
        .store
        .recent(&fx.folder, 100, true, Scope::Agent)
        .expect("list the facts");
    eprintln!("\nstore after the run (newest first):");
    for f in &stored {
        eprintln!(
            "    {} {}",
            if f.superseded_by.is_some() {
                "retired"
            } else {
                "live   "
            },
            clip(&f.text, 200)
        );
    }
    // A changed-mind case proves nothing unless the old fact made it into the
    // store to be retired. A stale fact that is still live is a leak even when
    // the model worded it differently from the fixture's phrase, so the
    // questions are graded against that wording too.
    let mut graded = fx.fact_queries.clone();
    for case in graded
        .iter_mut()
        .filter(|c| !c.secret && !c.must_exclude.is_empty())
    {
        let stale = stale_facts(&fx, case, &parsed);
        if stale.is_empty() {
            eprintln!(
                "    {}: no fact about the old stance was extracted, so this case did not exercise `replaces`",
                case.id
            );
        }
        for text in stale {
            let verdict = match stored.iter().find(|f| f.text == text) {
                None => "not in the store (folded into an earlier fact), so `replaces` was not exercised",
                Some(f) if f.superseded_by.is_some() => "retired by the new fact",
                Some(_) => {
                    case.must_exclude.push(text.clone());
                    "STILL LIVE: nothing retired it"
                }
            };
            eprintln!("    {}: {:?} {verdict}", case.id, clip(&text, 120));
        }
    }

    let failed_calls = calls.iter().filter(|c| c.reply.is_err()).count();
    assert!(
        failed_calls < calls.len(),
        "no call reached the model ({failed_calls} of {} failed): check SENCLAW_EVAL_LLM_BASE_URL",
        calls.len()
    );
    let mut millis: Vec<u128> = calls.iter().map(|c| c.millis).collect();
    millis.sort_unstable();
    let prompts: Vec<String> = calls.iter().map(|c| c.prompt.clone()).collect();
    drop(calls);

    let (without, with) = ask_fact_queries(label, &fx, &graded, &world, true).await;
    assert_secrets_kept_out(&fx, &prompts);

    let leaked = with.iter().filter(|r| r.leaked).count();
    eprintln!(
        "\nsummary [{model_name}, {label}]: extracted {} stored {} reinforced {} \
         failed chunks {}; replaces named {} honoured {honoured} ignored {ignored}; \
         slots {}/{} (retired by rule {by_slot}); \
         queries answered {}/{} ({} once diacritics are folded; without facts {}), leaked {leaked}; \
         model calls {} (failed {failed_calls}), median {} ms",
        reports.iter().map(|r| r.extracted).sum::<usize>(),
        reports.iter().map(|r| r.stored).sum::<usize>(),
        reports.iter().map(|r| r.reinforced).sum::<usize>(),
        reports.iter().map(|r| r.failed_chunks).sum::<usize>(),
        tally.claimed,
        tally.slotted,
        tally.facts,
        answered(&with),
        with.len(),
        with.iter().filter(|r| r.folded).count(),
        answered(&without),
        prompts.len(),
        millis[millis.len() / 2],
    );
}

/// The default install: no embedding provider, so the fact arms rest on
/// keyword, entity and date.
#[tokio::test]
#[ignore = "calls a real model: set SENCLAW_EVAL_LLM_BASE_URL and SENCLAW_EVAL_LLM_MODEL"]
async fn real_extractor_keyword_only() {
    run_real_extractor("keyword only", false).await;
}

/// With the hashed-words stand-in as the embedding model: the semantic arm
/// runs, and a reworded fact can be folded into one already stored.
#[tokio::test]
#[ignore = "calls a real model: set SENCLAW_EVAL_LLM_BASE_URL and SENCLAW_EVAL_LLM_MODEL"]
async fn real_extractor_with_vectors() {
    run_real_extractor("hashed-word vectors", true).await;
}
