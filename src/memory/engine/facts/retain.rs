//! Retain: turn text into stored facts.
//!
//! ```text
//! sanitize → scrub secrets → chunk → one model call per chunk → parse
//!          → embed → drop what is already known → store → link
//! ```
//!
//! The model call is the only expensive step and it happens off the request
//! path (the reflection window flushes on idle or size). Every failure is
//! counted in the report and logged, never raised: a conversation that cannot
//! be extracted is skipped, and the chat it came from is unaffected.

use chrono::{DateTime, NaiveDate};

use super::defense::{self, Mode, Verdict};
use super::extract::{self, Extracted};
use super::store::{FactStore, Inserted, LinkType, NewFact, StoredFact};
use super::FactEngine;
use crate::memory::chunker::{chunk_text, ChunkerOptions};
use crate::memory::cognitive::llm::LlmClient;
use crate::memory::cognitive::sanitize_for_cognify;

/// Cosine similarity at which two statements are one fact said twice.
pub const DUPLICATE_SIMILARITY: f32 = 0.95;
/// Cosine similarity at which two facts are linked as related.
pub const LINK_SIMILARITY: f32 = 0.7;
const MAX_SEMANTIC_LINKS: usize = 5;
const TEMPORAL_LINK_HOURS: i64 = 24;
const MAX_TEMPORAL_LINKS: usize = 5;

#[derive(Debug, Clone, Copy)]
pub struct RetainRequest<'a> {
    /// The agent folder.
    pub bank: &'a str,
    pub text: &'a str,
    /// Where the text came from (`"reflection"`, `"upload"`); kept for audit.
    pub source: &'a str,
    /// Unix seconds: when this was said, and what "today" means to the model.
    pub now: i64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RetainReport {
    pub chunks: usize,
    /// Facts the model produced after validation.
    pub extracted: usize,
    pub stored: usize,
    /// Already known: their date was brought forward instead.
    pub reinforced: usize,
    /// Chunks whose model call or reply failed.
    pub failed_chunks: usize,
    /// What the scrubber removed.
    pub redacted: Vec<&'static str>,
    /// The window held a secret and the mode forbids keeping it.
    pub blocked: bool,
}

pub async fn retain(
    engine: &FactEngine,
    llm: &dyn LlmClient,
    mode: Mode,
    req: &RetainRequest<'_>,
) -> RetainReport {
    let mut report = RetainReport::default();

    let Some(clean) = sanitize_for_cognify(req.text) else {
        return report;
    };
    let text = match defense::scrub(&clean, mode) {
        Verdict::Clean(t) => t,
        Verdict::Redacted { text, kinds } => {
            report.redacted = kinds;
            text
        }
        Verdict::Blocked { kinds } => {
            tracing::info!(?kinds, "[facts] window held a secret; nothing retained");
            report.redacted = kinds;
            report.blocked = true;
            return report;
        }
    };

    let today = DateTime::from_timestamp(req.now, 0)
        .map(|d| d.with_timezone(&chrono::Local).date_naive())
        .unwrap_or(NaiveDate::MIN);

    let mut extracted: Vec<Extracted> = Vec::new();
    for chunk in chunk_text(&text, ChunkerOptions::default()) {
        report.chunks += 1;
        let reply = match llm
            .complete(
                extract::SYSTEM_PROMPT,
                &extract::user_prompt(&chunk.text, today),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "[facts] extraction call failed");
                report.failed_chunks += 1;
                continue;
            }
        };
        match extract::parse_facts(&reply) {
            Ok(facts) => extracted.extend(facts),
            Err(e) => {
                tracing::warn!(error = %e, "[facts] extraction reply unusable");
                report.failed_chunks += 1;
            }
        }
    }
    report.extracted = extracted.len();
    if extracted.is_empty() {
        return report;
    }

    let embeddings = engine
        .embed_all(&extracted.iter().map(|f| f.text.clone()).collect::<Vec<_>>())
        .await;
    let model = engine.model_key();

    for (fact, embedding) in extracted.into_iter().zip(embeddings) {
        match store_one(&engine.store, req, fact, embedding, model.as_deref()) {
            Ok(Stored::New) => report.stored += 1,
            Ok(Stored::Known) => report.reinforced += 1,
            Err(e) => tracing::warn!(error = %e, "[facts] could not store a fact"),
        }
    }
    if report.stored > 0 {
        tracing::info!(
            stored = report.stored,
            reinforced = report.reinforced,
            "[facts] retained from {}",
            req.source
        );
    }
    report
}

enum Stored {
    New,
    Known,
}

fn store_one(
    store: &FactStore,
    req: &RetainRequest<'_>,
    fact: Extracted,
    embedding: Option<Vec<f32>>,
    model: Option<&str>,
) -> anyhow::Result<Stored> {
    // Neighbours first: the closest one decides whether this is new at all
    // (a rewording of what is stored), and the rest become links.
    let neighbours = match (embedding.as_deref(), model) {
        (Some(v), Some(m)) => {
            store.search_vector(req.bank, m, v, LINK_SIMILARITY, MAX_SEMANTIC_LINKS + 1)?
        }
        _ => Vec::new(),
    };
    if let Some((known, sim)) = neighbours.first() {
        if *sim >= DUPLICATE_SIMILARITY && known.kind == fact.kind {
            store.touch(&known.id, req.now)?;
            return Ok(Stored::Known);
        }
    }

    let new = NewFact {
        text: fact.text,
        kind: fact.kind,
        occurred: fact.occurred,
        mentioned_at: req.now,
        entities: fact.entities,
        embedding,
        model: model.map(str::to_string),
        source: req.source.to_string(),
    };
    let id = match store.insert(req.bank, &new)? {
        Inserted::Existing(_) => return Ok(Stored::Known),
        Inserted::New(id) => id,
    };

    for (other, sim) in neighbours.iter().take(MAX_SEMANTIC_LINKS) {
        store.add_link(&id, &other.id, LinkType::Semantic, *sim)?;
    }
    if let Some((start, _)) = new.occurred {
        for other in store.near_in_time(
            req.bank,
            start,
            TEMPORAL_LINK_HOURS,
            MAX_TEMPORAL_LINKS,
            &id,
        )? {
            store.add_link(
                &id,
                &other.id,
                LinkType::Temporal,
                temporal_weight(start, &other),
            )?;
        }
    }
    Ok(Stored::New)
}

/// Closer in time, stronger; never below 0.3 inside the window.
fn temporal_weight(anchor: i64, other: &StoredFact) -> f32 {
    let hours = (other.anchor() - anchor).abs() as f32 / 3600.0;
    (1.0 - hours / TEMPORAL_LINK_HOURS as f32).max(0.3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::memory::embedding::EmbeddingProvider;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    /// Replies from a script, recording every prompt it was sent.
    struct Scripted {
        replies: Mutex<Vec<Result<String>>>,
        seen: Mutex<Vec<(String, String)>>,
    }

    impl Scripted {
        fn new(replies: Vec<Result<String>>) -> Self {
            Self {
                replies: Mutex::new(replies),
                seen: Mutex::new(Vec::new()),
            }
        }
        fn ok(reply: &str) -> Self {
            Self::new(vec![Ok(reply.to_string())])
        }
    }

    #[async_trait]
    impl LlmClient for Scripted {
        async fn complete(&self, system: &str, user: &str) -> Result<String> {
            self.seen.lock().unwrap().push((system.into(), user.into()));
            let mut r = self.replies.lock().unwrap();
            if r.is_empty() {
                anyhow::bail!("script exhausted");
            }
            r.remove(0)
        }
    }

    /// One dimension per known word, so similarity is shared vocabulary.
    struct Bag;

    const VOCAB: [&str; 8] = [
        "sen", "hanoi", "moved", "dark", "mode", "prefers", "lives", "deadline",
    ];

    #[async_trait]
    impl EmbeddingProvider for Bag {
        fn name(&self) -> &str {
            "bag"
        }
        fn model(&self) -> &str {
            "v1"
        }
        fn dimensions(&self) -> u32 {
            VOCAB.len() as u32
        }
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let t = t.to_lowercase();
                    VOCAB.iter().map(|w| t.matches(w).count() as f32).collect()
                })
                .collect())
        }
    }

    fn engine(with_embedder: bool) -> FactEngine {
        let db = Arc::new(Db::open_in_memory(&Config::from_env()).unwrap());
        let embedder: Option<Arc<dyn EmbeddingProvider>> =
            with_embedder.then(|| Arc::new(Bag) as Arc<dyn EmbeddingProvider>);
        FactEngine::new(db, embedder)
    }

    const NOW: i64 = 1_792_152_000; // 2026-10-16T12:00:00Z

    fn req<'a>(text: &'a str) -> RetainRequest<'a> {
        RetainRequest {
            bank: "main",
            text,
            source: "test",
            now: NOW,
        }
    }

    const REPLY: &str = r#"{"facts":[
        {"text":"The user's name is Sen and they live in Hanoi.","kind":"world","when":null,"entities":["Sen","Hanoi","user"]},
        {"text":"Sen moved to Hanoi in March 2026.","kind":"world","when":"2026-03","entities":["Sen","Hanoi"]}]}"#;

    #[tokio::test]
    async fn a_conversation_becomes_stored_dated_facts() {
        let e = engine(false);
        let llm = Scripted::ok(REPLY);
        let report = retain(
            &e,
            &llm,
            Mode::Redact,
            &req("User: tôi tên là Sen, sống ở Hà Nội từ tháng 3"),
        )
        .await;
        assert_eq!(report.stored, 2);
        assert_eq!(report.chunks, 1);
        assert_eq!(e.store.count("main").unwrap(), 2);

        let hits = e.store.search_keyword("main", "hanoi", None, 5).unwrap();
        assert_eq!(hits.len(), 2);
        let dated = hits.iter().find(|f| f.text.contains("March 2026")).unwrap();
        assert!(dated.occurred_start.is_some());
        assert_eq!(dated.mentioned_at, NOW);
        assert_eq!(dated.entities, vec!["hanoi", "sen"]);
    }

    #[tokio::test]
    async fn the_model_is_told_todays_date_and_shown_the_text() {
        let e = engine(false);
        let llm = Scripted::ok(r#"{"facts":[]}"#);
        retain(&e, &llm, Mode::Redact, &req("User: hôm qua tôi đi Huế")).await;
        let seen = llm.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, extract::SYSTEM_PROMPT);
        assert!(seen[0].1.starts_with("TODAY: 2026-10-16"), "{}", seen[0].1);
        assert!(seen[0].1.contains("hôm qua tôi đi Huế"));
    }

    #[tokio::test]
    async fn a_secret_never_reaches_the_model_or_the_store() {
        let e = engine(false);
        let llm = Scripted::ok(
            r#"{"facts":[{"text":"The user keeps the staging credentials in the vault.","kind":"world"}]}"#,
        );
        let report = retain(
            &e,
            &llm,
            Mode::Redact,
            &req("User: my key is sk-proj-abcdefghijklmnopqrstuvwxyz and the vault holds the rest"),
        )
        .await;
        assert_eq!(report.redacted, vec!["api_key"]);
        assert_eq!(report.stored, 1);
        let sent = &llm.seen.lock().unwrap()[0].1;
        assert!(sent.contains("[REDACTED:api_key]"));
        assert!(!sent.contains("abcdefghijklmnopqrstuvwxyz"));
    }

    #[tokio::test]
    async fn block_mode_keeps_nothing_from_a_window_with_a_secret() {
        let e = engine(false);
        let llm = Scripted::ok(REPLY);
        let report = retain(
            &e,
            &llm,
            Mode::Block,
            &req(concat!(
                "User: token gh",
                "p_abcdefghijklmnopqrstuvwxyz0123456789 is mine"
            )),
        )
        .await;
        assert!(report.blocked);
        assert_eq!(report.stored, 0);
        assert!(
            llm.seen.lock().unwrap().is_empty(),
            "no model call on a blocked window"
        );
        assert_eq!(e.store.count("main").unwrap(), 0);
    }

    #[tokio::test]
    async fn a_failing_model_is_a_counted_skip_not_an_error() {
        let e = engine(false);
        let llm = Scripted::new(vec![Err(anyhow::anyhow!("connection refused"))]);
        let report = retain(
            &e,
            &llm,
            Mode::Redact,
            &req("User: a perfectly fine conversation window"),
        )
        .await;
        assert_eq!(report.failed_chunks, 1);
        assert_eq!(report.stored, 0);

        let llm = Scripted::ok("I am sorry, I cannot do that.");
        let report = retain(
            &e,
            &llm,
            Mode::Redact,
            &req("User: another fine conversation window"),
        )
        .await;
        assert_eq!(report.failed_chunks, 1);
        assert_eq!(e.store.count("main").unwrap(), 0);
    }

    #[tokio::test]
    async fn the_same_statement_twice_is_stored_once() {
        let e = engine(false);
        for _ in 0..2 {
            retain(
                &e,
                &Scripted::ok(REPLY),
                Mode::Redact,
                &req("User: tôi tên là Sen"),
            )
            .await;
        }
        assert_eq!(e.store.count("main").unwrap(), 2);
        let report = retain(
            &e,
            &Scripted::ok(REPLY),
            Mode::Redact,
            &req("User: tôi tên là Sen"),
        )
        .await;
        assert_eq!((report.stored, report.reinforced), (0, 2));
    }

    #[tokio::test]
    async fn a_reworded_statement_is_recognised_by_its_vector() {
        let e = engine(true);
        retain(
            &e,
            &Scripted::ok(r#"{"facts":[{"text":"Sen prefers dark mode","kind":"world"}]}"#),
            Mode::Redact,
            &req("User: tôi thích dark mode"),
        )
        .await;
        // Different words to the content hash, the same vector.
        let report = retain(
            &e,
            &Scripted::ok(r#"{"facts":[{"text":"Sen prefers dark mode.","kind":"world"}]}"#),
            Mode::Redact,
            &req("User: dark mode nhé"),
        )
        .await;
        assert_eq!((report.stored, report.reinforced), (0, 1));
        assert_eq!(e.store.count("main").unwrap(), 1);
    }

    #[tokio::test]
    async fn related_facts_are_linked_by_meaning_and_by_date() {
        let e = engine(true);
        let reply = r#"{"facts":[
            {"text":"Sen lives in Hanoi.","kind":"world","when":"2026-03-09","entities":["Sen"]},
            {"text":"Sen lives in Hanoi and moved there.","kind":"world","when":"2026-03-09","entities":["Sen"]},
            {"text":"The deadline is in August.","kind":"world","when":"2026-08-20"}]}"#;
        let report = retain(
            &e,
            &Scripted::ok(reply),
            Mode::Redact,
            &req("User: chuyện nhà và công việc"),
        )
        .await;
        assert_eq!(report.stored, 3);

        let all = e
            .store
            .search_keyword("main", "sen OR deadline", None, 10)
            .unwrap();
        let lives = all
            .iter()
            .find(|f| f.text == "Sen lives in Hanoi.")
            .unwrap();
        let moved = all.iter().find(|f| f.text.contains("moved")).unwrap();
        let deadline = all.iter().find(|f| f.text.contains("deadline")).unwrap();

        let links = e.store.linked("main", &[lives.id.clone()]).unwrap();
        let to_moved: Vec<_> = links.iter().filter(|l| l.id == moved.id).collect();
        assert!(
            to_moved.iter().any(|l| l.link_type == "semantic"),
            "three shared words out of four: cosine 0.87, above the link floor"
        );
        assert!(
            to_moved
                .iter()
                .any(|l| l.link_type == "temporal" && (l.weight - 1.0).abs() < 1e-6),
            "same day, full temporal weight"
        );
        assert!(
            links.iter().all(|l| l.id != deadline.id),
            "five months apart and no shared words: unlinked"
        );
    }

    #[tokio::test]
    async fn text_with_nothing_in_it_costs_no_model_call() {
        let e = engine(false);
        let llm = Scripted::ok(REPLY);
        let report = retain(&e, &llm, Mode::Redact, &req("   \n  ")).await;
        assert_eq!(report, RetainReport::default());
        assert!(llm.seen.lock().unwrap().is_empty());
    }

    #[test]
    fn temporal_weight_falls_with_distance_and_has_a_floor() {
        let at = |secs: i64| StoredFact {
            id: "x".into(),
            kind: super::super::store::FactKind::World,
            text: String::new(),
            occurred_start: Some(secs),
            occurred_end: None,
            mentioned_at: 0,
            entities: vec![],
        };
        assert!((temporal_weight(0, &at(0)) - 1.0).abs() < 1e-6);
        assert!((temporal_weight(0, &at(12 * 3600)) - 0.5).abs() < 1e-6);
        assert!((temporal_weight(0, &at(23 * 3600)) - 0.3).abs() < 1e-6);
    }
}
