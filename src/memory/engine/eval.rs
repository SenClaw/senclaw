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
use crate::memory::cognitive::{CognifyOptions, CognitiveSystem, NodeSet, NullEmbedder};
use crate::memory::curated;
use crate::memory::embedding::EmbeddingProvider as EmbeddingModel;
use crate::memory::manager::MemoryManager;

use super::facts::{self, defense, FactEngine, RetainRequest};
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

#[derive(Deserialize)]
struct Case {
    id: String,
    text: String,
    #[serde(default)]
    must_include: Vec<String>,
    #[serde(default)]
    must_exclude: Vec<String>,
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
    script: Vec<(String, String)>,
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl LlmClient for ScriptedFacts {
    async fn complete(&self, _system: &str, user: &str) -> Result<String> {
        self.seen.lock().unwrap().push(user.to_string());
        Ok(self
            .script
            .iter()
            .find(|(needle, _)| user.contains(needle.as_str()))
            .map(|(_, reply)| reply.clone())
            .unwrap_or_else(|| r#"{"facts":[]}"#.to_string()))
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
    let extractor = ScriptedFacts {
        script: fx
            .fact_windows
            .iter()
            .map(|w| {
                (
                    w.needle.clone(),
                    serde_json::json!({ "facts": w.facts }).to_string(),
                )
            })
            .collect(),
        seen: Arc::clone(&fact_prompts),
    };
    for w in &fx.fact_windows {
        let report = facts::retain(
            &facts,
            &extractor,
            defense::Mode::Redact,
            &RetainRequest {
                bank: &fx.folder,
                text: &w.text,
                source: "eval",
                now: fixture_now(fx),
            },
        )
        .await;
        assert_eq!(
            report.stored,
            w.facts.len(),
            "window {:?} stored {} of {} scripted facts ({report:?})",
            w.needle,
            report.stored,
            w.facts.len()
        );
    }

    World {
        mgr,
        sys,
        facts,
        fact_prompts,
        folder: fx.folder.clone(),
        chat: fx.chat.clone(),
        _dir: dir,
    }
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
    Row {
        id: case.id.clone(),
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
        arms.push(Arc::new(facts::SemanticArm(Arc::clone(&w.facts))));
        arms.push(Arc::new(facts::KeywordArm(Arc::clone(&w.facts))));
        arms.push(Arc::new(facts::GraphArm(Arc::clone(&w.facts))));
        arms.push(Arc::new(facts::TemporalArm(Arc::clone(&w.facts))));
    }
    let query = RecallQuery {
        text: q.to_string(),
        folder: w.folder.clone(),
        chat_jid: w.chat.clone(),
        limit: max,
        now,
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
    let max = Config::from_env().memory.search_max_results as usize;
    let now = fixture_now(&fx);
    let cfg = RecallConfig::default();

    let (mut without, mut with) = (Vec::new(), Vec::new());
    eprintln!("\n== facts: {label} ==");
    eprintln!(
        "{:<26} {:>12} {:>12}  fact arms (sem/key/gra/tem)",
        "query", "no facts", "with facts"
    );
    for case in &fx.fact_queries {
        let (plain, _) = run_engine(&world, &case.text, max, now, &cfg, false).await;
        let (full, recalled) = run_engine(&world, &case.text, max, now, &cfg, true).await;
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
            if b.answered { "answered" } else { "MISSED" },
        );
        without.push(a);
        with.push(b);
    }
    eprintln!(
        "{label}: without facts {}/{}, with facts {}/{}",
        without.iter().filter(|r| r.answered).count(),
        without.len(),
        with.iter().filter(|r| r.answered).count(),
        with.len()
    );

    // Whatever else happened, the secret typed in the chat reached neither the
    // extraction model nor anything it could have stored.
    let prompts = world.fact_prompts.lock().unwrap();
    assert!(!prompts.is_empty(), "the extractor was never called");
    for secret in fx.fact_queries.iter().flat_map(|c| c.must_exclude.iter()) {
        assert!(
            prompts.iter().all(|p| !p.contains(secret.as_str())),
            "{secret:?} was sent to the extraction model"
        );
    }
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
        limit: 5,
        now,
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
