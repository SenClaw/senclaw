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

use std::sync::Arc;

use chrono::{DateTime, NaiveDate};

use super::defense::{self, Mode, Verdict};
use super::extract::{self, Extracted};
use super::restated;
use super::store::{FactStore, Inserted, LinkType, NewFact, Scope, StoredFact};
use super::FactEngine;
use crate::memory::chunker::{chunk_text, ChunkerOptions};
use crate::memory::cognitive::llm::LlmClient;
use crate::memory::cognitive::retrievers::build_fts_match;
use crate::memory::cognitive::sanitize_for_cognify;
use crate::memory::engine::fusion::{overlap_coefficient, text_terms};

/// Cosine similarity at which two statements are one fact said twice.
pub const DUPLICATE_SIMILARITY: f32 = 0.95;
/// Cosine similarity at which two facts are linked as related.
pub const LINK_SIMILARITY: f32 = 0.7;
const MAX_SEMANTIC_LINKS: usize = 5;
const TEMPORAL_LINK_HOURS: i64 = 24;
const MAX_TEMPORAL_LINKS: usize = 5;
/// How many stored facts the extractor is shown beside each chunk, so it can
/// say which ones a new fact replaces.
const KNOWN_FACTS: usize = 8;
/// A known fact is shown cut to this many characters.
const KNOWN_FACT_CHARS: usize = 240;
/// Subjects every fact in a conversation shares. Two facts about "the user" are
/// not thereby about the same thing.
pub(crate) const GENERIC_SUBJECTS: [&str; 6] = [
    "user",
    "người dùng",
    "nguoi dung",
    "assistant",
    "trợ lý",
    "tro ly",
];
/// Without a shared specific entity, a replacement must at least share this
/// much wording with what it replaces.
const MIN_OVERLAP_TO_REPLACE: f64 = 0.4;

#[derive(Debug, Clone, Copy)]
pub struct RetainRequest<'a> {
    /// The agent folder.
    pub bank: &'a str,
    pub text: &'a str,
    /// Where the text came from (`"reflection"`, `"upload"`); kept for audit.
    pub source: &'a str,
    /// Unix seconds: when this was said, and what "today" means to the model.
    pub now: i64,
    /// The chat the text came from (its JID), or empty for the whole agent.
    /// What is stored carries it, and what the extractor is shown beside the
    /// text — and may therefore be told it replaces — is limited to what that
    /// chat may see.
    pub origin: &'a str,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RetainReport {
    pub chunks: usize,
    /// Facts the model produced after validation.
    pub extracted: usize,
    pub stored: usize,
    /// Older facts retired because a new one replaced them.
    pub superseded: usize,
    /// Of `superseded`, how many were replaced by rule — a new value for a
    /// single-valued attribute of the same subject (see `slots`) — rather than
    /// because the model named the older fact.
    pub superseded_by_slot: usize,
    /// Replacements the model named. What it named and `superseded` did not
    /// accept was ignored: a number it was never shown, or a different subject.
    pub replaces_claimed: usize,
    /// Already known: their date was brought forward instead.
    pub reinforced: usize,
    /// Said twice in one reply: left out because another fact of the same reply
    /// says everything they do. Nothing stored was touched — one reply is one
    /// mention, so unlike `reinforced` this adds no proof.
    pub folded: usize,
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

    // Each extracted fact keeps the stored facts its chunk was shown beside, since
    // "replaces F2" means the second of THOSE.
    let mut extracted: Vec<(Extracted, Arc<Vec<StoredFact>>)> = Vec::new();
    for chunk in chunk_text(&text, ChunkerOptions::default()) {
        report.chunks += 1;
        let known = known_facts(&engine.store, req.bank, req.origin, &chunk.text);
        // Each is shown with its slot, when it has one, so the model can name the
        // same attribute again when it states a new value.
        let shown: Vec<String> = known
            .iter()
            .map(|f| {
                let text: String = f.text.chars().take(KNOWN_FACT_CHARS).collect();
                match &f.slot {
                    Some(slot) => format!("<{}> {text}", slot.label()),
                    None => text,
                }
            })
            .collect();
        let reply = match llm
            .complete(
                extract::SYSTEM_PROMPT,
                &extract::user_prompt(&chunk.text, today, &shown),
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
            Ok(facts) => {
                let (facts, folded) = restated::without_restated_siblings(facts);
                report.folded += folded;
                let known = Arc::new(known);
                extracted.extend(facts.into_iter().map(|f| (f, Arc::clone(&known))));
            }
            Err(e) => {
                tracing::warn!(error = %e, "[facts] extraction reply unusable");
                report.failed_chunks += 1;
            }
        }
    }
    report.extracted = extracted.len() + report.folded;
    if extracted.is_empty() {
        return report;
    }

    let embeddings = engine
        .embed_all(
            &extracted
                .iter()
                .map(|(f, _)| f.text.clone())
                .collect::<Vec<_>>(),
        )
        .await;
    let model = engine.model_key();

    for ((fact, known), embedding) in extracted.into_iter().zip(embeddings) {
        match store_one(
            &engine.store,
            req,
            fact,
            &known,
            embedding,
            model.as_deref(),
        ) {
            Ok(Stored::New {
                claimed,
                superseded,
                by_slot,
            }) => {
                report.stored += 1;
                report.replaces_claimed += claimed;
                report.superseded += superseded;
                report.superseded_by_slot += by_slot;
            }
            Ok(Stored::Known) => report.reinforced += 1,
            Err(e) => tracing::warn!(error = %e, "[facts] could not store a fact"),
        }
    }
    if report.stored > 0 {
        tracing::info!(
            stored = report.stored,
            reinforced = report.reinforced,
            folded = report.folded,
            replaces_claimed = report.replaces_claimed,
            superseded = report.superseded,
            superseded_by_slot = report.superseded_by_slot,
            failed_chunks = report.failed_chunks,
            "[facts] retained from {}",
            req.source
        );
    }
    report
}

enum Stored {
    New {
        claimed: usize,
        superseded: usize,
        by_slot: usize,
    },
    Known,
}

/// Stored facts that look related to this text, best match first: what the
/// extractor is shown so it can notice a contradiction. Words only — it must
/// cost no model call and no embedding.
fn known_facts(store: &FactStore, bank: &str, origin: &str, text: &str) -> Vec<StoredFact> {
    let Some(m) = build_fts_match(text) else {
        return Vec::new();
    };
    store
        .search_keyword(bank, &m, None, KNOWN_FACTS, scope_of(origin))
        .unwrap_or_default()
}

/// What a text from `origin` may be compared with: its own chat's facts and the
/// agent-wide ones, or every fact of the agent when it has no chat.
fn scope_of(origin: &str) -> Scope<'_> {
    if origin.is_empty() {
        Scope::Agent
    } else {
        Scope::Chat(origin)
    }
}

/// Is `new` plausibly about the same thing as `old`? A model asked to name what
/// a fact replaces will sometimes name the wrong one; replacing hides a fact
/// from recall, so the claim is checked: a shared entity beyond the generic
/// subject, or enough shared wording.
fn same_subject(old: &StoredFact, new_text: &str, new_entities: &[String]) -> bool {
    let shares_entity = old
        .entities
        .iter()
        .any(|e| !GENERIC_SUBJECTS.contains(&e.as_str()) && new_entities.contains(e));
    if shares_entity {
        return true;
    }
    let (a, b) = (text_terms(&old.text), text_terms(new_text));
    a.len() >= 3 && b.len() >= 3 && overlap_coefficient(&a, &b) >= MIN_OVERLAP_TO_REPLACE
}

fn store_one(
    store: &FactStore,
    req: &RetainRequest<'_>,
    fact: Extracted,
    known: &[StoredFact],
    embedding: Option<Vec<f32>>,
    model: Option<&str>,
) -> anyhow::Result<Stored> {
    // The model was told not to repeat the facts it was shown, and small ones do.
    // A repeat in other words is one more mention of that fact, not a second fact
    // — with no embedding model nothing else would notice.
    if let Some(old) = restated::restated_by(known, &fact) {
        store.touch(&old.id, req.now)?;
        if let Some((start, end)) = fact.occurred {
            store.date_if_missing(&old.id, start, end)?;
        }
        return Ok(Stored::Known);
    }

    // Neighbours first: the closest one decides whether this is new at all
    // (a rewording of what is stored), and the rest become links.
    let neighbours = match (embedding.as_deref(), model) {
        (Some(v), Some(m)) => store.search_vector(
            req.bank,
            m,
            v,
            LINK_SIMILARITY,
            MAX_SEMANTIC_LINKS + 1,
            scope_of(req.origin),
        )?,
        _ => Vec::new(),
    };
    if let Some((known, sim)) = neighbours.first() {
        if *sim >= DUPLICATE_SIMILARITY && known.kind == fact.kind {
            store.touch(&known.id, req.now)?;
            return Ok(Stored::Known);
        }
    }

    let replaces = fact.replaces;
    let slot = fact.slot;
    let new = NewFact {
        text: fact.text,
        kind: fact.kind,
        occurred: fact.occurred,
        mentioned_at: req.now,
        entities: fact.entities,
        embedding,
        model: model.map(str::to_string),
        source: req.source.to_string(),
        origin: req.origin.to_string(),
        slot: slot.clone(),
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

    // The model said what this replaces; keep only what is plausible.
    let claimed = replaces.len();
    let mut superseded = 0;
    for number in replaces {
        let Some(old) = known.get(number - 1) else {
            continue; // a number it was never shown
        };
        if same_subject(old, &new.text, &new.entities)
            && store.supersede(req.bank, &old.id, &id, req.now)?
        {
            superseded += 1;
        }
    }

    // A new value for an attribute that holds one value at a time replaces the
    // current one by rule, with nobody having to name it. Only what this chat may
    // see can be replaced, and only by a statement that is not about an earlier
    // time than the one it would hide ("in 2025 I worked at FPT" says nothing
    // about where the user works now).
    let mut by_slot = 0;
    if let Some(slot) = new.slot.as_ref().filter(|s| s.single_valued()) {
        let anchor = new.occurred.map_or(req.now, |(start, _)| start);
        for old in store.in_slot(req.bank, slot, &id, scope_of(req.origin))? {
            if anchor < old.anchor() {
                continue;
            }
            if store.supersede(req.bank, &old.id, &id, req.now)? {
                superseded += 1;
                by_slot += 1;
            }
        }
    }
    Ok(Stored::New {
        claimed,
        superseded,
        by_slot,
    })
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
            origin: "",
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

        let hits = e
            .store
            .search_keyword("main", "hanoi", None, 5, Scope::Agent)
            .unwrap();
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
        // Related, but neither says all the other does: a first fact wholly inside
        // the second ("… and moved there") would be one statement said twice.
        let reply = r#"{"facts":[
            {"text":"Sen lives in Hanoi.","kind":"world","when":"2026-03-09","entities":["Sen"]},
            {"text":"Sen moved to Hanoi and lives there.","kind":"world","when":"2026-03-09","entities":["Sen"]},
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
            .search_keyword("main", "sen OR deadline", None, 10, Scope::Agent)
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

    const REACT: &str = r#"{"facts":[{"text":"Người dùng thích dùng React cho frontend.","kind":"world","entities":["user","React"]}]}"#;

    fn req_in<'a>(origin: &'a str, text: &'a str) -> RetainRequest<'a> {
        RetainRequest {
            origin,
            ..req(text)
        }
    }

    // ---- slots: a new value replaces the old by rule ----

    fn lives(text: &str, place: &str, when: Option<&str>) -> String {
        let when = when.map_or("null".to_string(), |w| format!("\"{w}\""));
        format!(
            r#"{{"facts":[{{"text":"{text}","kind":"world","when":{when},"entities":["user","{place}"],"subject":"Người dùng","predicate":"Lives in"}}]}}"#
        )
    }

    async fn say(e: &FactEngine, origin: &str, reply: &str, window: &str) -> RetainReport {
        retain(
            e,
            &Scripted::ok(reply),
            Mode::Redact,
            &req_in(origin, window),
        )
        .await
    }

    fn current(e: &FactEngine, scope: Scope<'_>) -> Vec<String> {
        let mut t: Vec<String> = e
            .store
            .recent("main", 20, false, scope)
            .unwrap()
            .into_iter()
            .map(|f| f.text)
            .collect();
        t.sort();
        t
    }

    #[tokio::test]
    async fn a_new_value_for_a_single_valued_attribute_replaces_the_old_without_being_told() {
        let e = engine(false);
        say(
            &e,
            "",
            &lives("Người dùng sống ở Huế.", "Huế", None),
            "User: tôi sống ở Huế",
        )
        .await;
        // The model names no fact to replace: it only says what the fact is about.
        let report = say(
            &e,
            "",
            &lives("Người dùng đã chuyển ra Hà Nội.", "Hà Nội", None),
            "User: tôi chuyển ra Hà Nội rồi",
        )
        .await;
        assert_eq!(
            (report.stored, report.superseded, report.superseded_by_slot),
            (1, 1, 1)
        );
        assert_eq!(report.replaces_claimed, 0);
        assert_eq!(
            current(&e, Scope::Agent),
            ["Người dùng đã chuyển ra Hà Nội."]
        );
        // The old one is kept, for history, and says what replaced it.
        let all = e.store.recent("main", 20, true, Scope::Agent).unwrap();
        let old = all
            .iter()
            .find(|f| f.text == "Người dùng sống ở Huế.")
            .unwrap();
        assert!(old.superseded_by.is_some());
        let slot = old.slot.as_ref().unwrap();
        assert_eq!(
            (slot.subject.as_str(), slot.predicate.as_str()),
            ("user", "lives_in")
        );
    }

    #[tokio::test]
    async fn an_attribute_that_can_have_many_values_keeps_all_of_them() {
        let e = engine(false);
        let likes = |text: &str, thing: &str| {
            format!(
                r#"{{"facts":[{{"text":"{text}","kind":"world","entities":["user","{thing}"],"subject":"user","predicate":"likes"}}]}}"#
            )
        };
        say(
            &e,
            "",
            &likes("Người dùng thích cà phê.", "cà phê"),
            "User: tôi thích cà phê",
        )
        .await;
        let report = say(
            &e,
            "",
            &likes("Người dùng cũng thích trà.", "trà"),
            "User: tôi cũng thích trà",
        )
        .await;
        assert_eq!((report.stored, report.superseded), (1, 0));
        assert_eq!(current(&e, Scope::Agent).len(), 2);
    }

    #[tokio::test]
    async fn a_choice_named_favorite_or_current_has_one_value() {
        let e = engine(false);
        let pick = |text: &str, thing: &str| {
            format!(
                r#"{{"facts":[{{"text":"{text}","kind":"world","entities":["user","{thing}"],"subject":"tôi","predicate":"favourite frontend framework"}}]}}"#
            )
        };
        say(
            &e,
            "",
            &pick("Người dùng thích dùng React.", "React"),
            "User: tôi thích React",
        )
        .await;
        let report = say(
            &e,
            "",
            &pick("Người dùng giờ thích Vue hơn.", "Vue"),
            "User: giờ tôi thích Vue",
        )
        .await;
        assert_eq!(report.superseded_by_slot, 1);
        assert_eq!(current(&e, Scope::Agent), ["Người dùng giờ thích Vue hơn."]);
    }

    #[tokio::test]
    async fn a_statement_about_an_earlier_time_does_not_replace_the_current_value() {
        let e = engine(false);
        say(
            &e,
            "",
            &lives("Người dùng sống ở Hà Nội.", "Hà Nội", None),
            "User: tôi sống ở Hà Nội",
        )
        .await;
        // "In 2019 I lived in Huế" is history; it must not hide where they live now.
        let report = say(
            &e,
            "",
            &lives("Năm 2019 người dùng sống ở Huế.", "Huế", Some("2019")),
            "User: năm 2019 tôi sống ở Huế",
        )
        .await;
        assert_eq!((report.stored, report.superseded), (1, 0));
        assert_eq!(current(&e, Scope::Agent).len(), 2);
    }

    #[tokio::test]
    async fn another_subjects_attribute_is_not_replaced() {
        let e = engine(false);
        let of = |who: &str, text: &str, place: &str| {
            format!(
                r#"{{"facts":[{{"text":"{text}","kind":"world","entities":["{who}","{place}"],"subject":"{who}","predicate":"lives_in"}}]}}"#
            )
        };
        say(
            &e,
            "",
            &of("Emily", "Emily sống ở Huế.", "Huế"),
            "User: Emily sống ở Huế",
        )
        .await;
        let report = say(
            &e,
            "",
            &of("Sarah", "Sarah sống ở Hà Nội.", "Hà Nội"),
            "User: Sarah sống ở Hà Nội",
        )
        .await;
        assert_eq!(report.superseded, 0, "two people, two slots");
        assert_eq!(current(&e, Scope::Agent).len(), 2);
    }

    #[tokio::test]
    async fn a_fact_with_no_slot_replaces_nothing_by_rule() {
        let e = engine(false);
        say(
            &e,
            "",
            &lives("Người dùng sống ở Huế.", "Huế", None),
            "User: tôi sống ở Huế",
        )
        .await;
        let no_slot = r#"{"facts":[{"text":"Người dùng chuyển ra Hà Nội.","kind":"world","entities":["user","Hà Nội"]}]}"#;
        let report = say(&e, "", no_slot, "User: tôi chuyển ra Hà Nội").await;
        assert_eq!((report.stored, report.superseded), (1, 0));
    }

    #[tokio::test]
    async fn a_value_said_in_one_chat_is_not_replaced_from_another() {
        let e = engine(false);
        say(
            &e,
            "web:a",
            &lives("Người dùng sống ở Huế.", "Huế", None),
            "User: tôi sống ở Huế",
        )
        .await;
        let report = say(
            &e,
            "web:b",
            &lives("Người dùng sống ở Hà Nội.", "Hà Nội", None),
            "User: tôi sống ở Hà Nội",
        )
        .await;
        assert_eq!(report.superseded, 0);
        assert_eq!(
            current(&e, Scope::Chat("web:a")),
            ["Người dùng sống ở Huế."]
        );
        assert_eq!(
            current(&e, Scope::Chat("web:b")),
            ["Người dùng sống ở Hà Nội."]
        );
    }

    #[tokio::test]
    async fn naming_the_old_fact_and_the_rule_agreeing_replaces_it_once() {
        let e = engine(false);
        say(
            &e,
            "",
            &lives("Người dùng sống ở Huế.", "Huế", None),
            "User: tôi sống ở Huế",
        )
        .await;
        let both = r#"{"facts":[{"text":"Người dùng đã chuyển ra Hà Nội.","kind":"world","entities":["user","Hà Nội","Huế"],"subject":"user","predicate":"lives_in","replaces":["F1"]}]}"#;
        let report = say(
            &e,
            "",
            both,
            "User: tôi chuyển ra Hà Nội rồi, không ở Huế nữa",
        )
        .await;
        assert_eq!((report.stored, report.superseded), (1, 1));
    }

    #[tokio::test]
    async fn the_extractor_is_shown_each_known_fact_with_its_slot() {
        let e = engine(false);
        say(
            &e,
            "",
            &lives("Người dùng sống ở Huế.", "Huế", None),
            "User: tôi sống ở Huế",
        )
        .await;
        let llm = Scripted::ok(r#"{"facts":[]}"#);
        retain(
            &e,
            &llm,
            Mode::Redact,
            &req("User: giờ tôi sống ở đâu nhỉ, Huế đúng không"),
        )
        .await;
        let prompt = llm.seen.lock().unwrap()[0].1.clone();
        assert!(
            prompt.contains("[F1] <user · lives_in> Người dùng sống ở Huế."),
            "{prompt}"
        );
    }

    #[tokio::test]
    async fn a_fact_is_tagged_with_its_chat_and_each_chat_keeps_its_own_copy() {
        let e = engine(false);
        let said = "User: tôi thích dùng React cho frontend";
        let a = retain(
            &e,
            &Scripted::ok(REACT),
            Mode::Redact,
            &req_in("web:a", said),
        )
        .await;
        let b = retain(
            &e,
            &Scripted::ok(REACT),
            Mode::Redact,
            &req_in("web:b", said),
        )
        .await;
        // The second chat is not told the first already has it: it stores its own.
        assert_eq!((a.stored, b.stored, b.reinforced), (1, 1, 0));
        let all = e.store.recent("main", 10, false, Scope::Agent).unwrap();
        let mut origins: Vec<&str> = all.iter().map(|f| f.origin.as_str()).collect();
        origins.sort_unstable();
        assert_eq!(origins, ["web:a", "web:b"]);
    }

    #[tokio::test]
    async fn what_the_extractor_is_shown_beside_the_text_is_only_what_that_chat_may_see() {
        let e = engine(false);
        retain(
            &e,
            &Scripted::ok(REACT),
            Mode::Redact,
            &req_in("web:a", "User: tôi thích dùng React cho frontend"),
        )
        .await;

        // Chat b changes its mind about React; the model, if it were shown chat a's
        // fact, would name it. It must neither see it nor be able to replace it.
        let llm = Scripted::ok(
            r#"{"facts":[{"text":"Người dùng đã chuyển sang Vue và không còn dùng React.","kind":"world","entities":["user","Vue","React"],"replaces":["F1"]}]}"#,
        );
        let report = retain(
            &e,
            &llm,
            Mode::Redact,
            &req_in(
                "web:b",
                "User: tôi chuyển sang Vue rồi, không dùng React nữa",
            ),
        )
        .await;
        let prompt = llm.seen.lock().unwrap()[0].1.clone();
        assert!(
            !prompt.contains("KNOWN FACTS") && !prompt.contains("thích dùng React"),
            "another chat's fact reached the extractor: {prompt}"
        );
        assert_eq!((report.stored, report.superseded), (1, 0));
        let a = e
            .store
            .recent("main", 10, false, Scope::Chat("web:a"))
            .unwrap();
        assert_eq!(a.len(), 1, "chat a's fact is untouched and still current");
        assert!(a[0].text.contains("thích dùng React"));
    }

    #[tokio::test]
    async fn a_fact_with_no_chat_belongs_to_the_agent_and_every_chat_may_replace_it() {
        let e = engine(false);
        retain(
            &e,
            &Scripted::ok(REACT),
            Mode::Redact,
            &req("User: tôi thích dùng React cho frontend"),
        )
        .await;
        let llm = Scripted::ok(
            r#"{"facts":[{"text":"Người dùng đã chuyển sang Vue và không còn dùng React.","kind":"world","entities":["user","Vue","React"],"replaces":["F1"]}]}"#,
        );
        let report = retain(
            &e,
            &llm,
            Mode::Redact,
            &req_in(
                "web:b",
                "User: tôi chuyển sang Vue rồi, không dùng React nữa",
            ),
        )
        .await;
        assert_eq!((report.stored, report.superseded), (1, 1));
    }

    #[tokio::test]
    async fn a_changed_mind_retires_the_old_fact_and_keeps_it_for_history() {
        let e = engine(false);
        retain(
            &e,
            &Scripted::ok(REACT),
            Mode::Redact,
            &req("User: tôi thích dùng React cho frontend"),
        )
        .await;

        let llm = Scripted::ok(
            r#"{"facts":[{"text":"Người dùng đã chuyển sang Vue và không còn dùng React.","kind":"world","entities":["user","Vue","React"],"replaces":["F1"]}]}"#,
        );
        let report = retain(
            &e,
            &llm,
            Mode::Redact,
            &req("User: tôi chuyển sang Vue rồi, không dùng React nữa"),
        )
        .await;
        assert_eq!((report.stored, report.superseded), (1, 1));
        assert_eq!(report.replaces_claimed, 1);

        // The model was shown what was stored, numbered, to be able to say so.
        let prompt = llm.seen.lock().unwrap()[0].1.clone();
        assert!(
            prompt.contains("KNOWN FACTS:\n[F1] Người dùng thích dùng React cho frontend."),
            "{prompt}"
        );

        let current = e
            .store
            .search_keyword("main", "react", None, 10, Scope::Agent)
            .unwrap();
        assert_eq!(current.len(), 1);
        assert!(current[0].text.contains("chuyển sang Vue"));
        let history = e.store.recent("main", 10, true, Scope::Agent).unwrap();
        assert_eq!(history.len(), 2);
        let old = history
            .iter()
            .find(|f| f.text.contains("thích dùng React"))
            .unwrap();
        assert_eq!(old.superseded_by.as_deref(), Some(current[0].id.as_str()));
    }

    #[tokio::test]
    async fn a_replacement_the_model_got_wrong_is_ignored() {
        let e = engine(false);
        retain(
            &e,
            &Scripted::ok(r#"{"facts":[{"text":"Sen sống ở Hà Nội và làm việc tại FPT.","kind":"world","entities":["Sen","Hà Nội","FPT"]}]}"#),
            Mode::Redact,
            &req("User: Sen sống ở Hà Nội và làm việc tại FPT"),
        )
        .await;
        // The window mentions Sen, so the Hà Nội fact is shown as F1; the model
        // wrongly says a tea preference replaces it.
        let llm = Scripted::ok(
            r#"{"facts":[{"text":"Người dùng thích trà xanh vào buổi tối.","kind":"world","entities":["user"],"replaces":["F1"]}]}"#,
        );
        let report = retain(
            &e,
            &llm,
            Mode::Redact,
            &req("User: Sen ơi, tôi thích trà xanh buổi tối"),
        )
        .await;
        assert!(llm.seen.lock().unwrap()[0]
            .1
            .contains("[F1] Sen sống ở Hà Nội"));
        assert_eq!((report.stored, report.superseded), (1, 0));
        assert_eq!(
            report.replaces_claimed, 1,
            "named, then ignored as another subject"
        );
        assert_eq!(
            e.store
                .recent("main", 10, false, Scope::Agent)
                .unwrap()
                .len(),
            2,
            "nothing was hidden"
        );
    }

    #[tokio::test]
    async fn a_number_the_model_was_never_shown_replaces_nothing() {
        let e = engine(false);
        retain(
            &e,
            &Scripted::ok(REACT),
            Mode::Redact,
            &req("User: tôi thích dùng React cho frontend"),
        )
        .await;
        let report = retain(
            &e,
            &Scripted::ok(
                r#"{"facts":[{"text":"Người dùng đã chuyển sang Vue và bỏ React.","kind":"world","entities":["user","React","Vue"],"replaces":["F7","F0",3]}]}"#,
            ),
            Mode::Redact,
            &req("User: tôi chuyển sang Vue rồi, bỏ React"),
        )
        .await;
        assert_eq!((report.stored, report.superseded), (1, 0));
        // 7 and 3 were named (0 is not a number it can mean); only 1 and 2 were shown.
        assert_eq!(report.replaces_claimed, 2);
        assert_eq!(
            e.store
                .recent("main", 10, false, Scope::Agent)
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn the_same_statement_said_again_is_more_evidence_not_a_second_fact() {
        let e = engine(false);
        for _ in 0..3 {
            retain(
                &e,
                &Scripted::ok(REACT),
                Mode::Redact,
                &req("User: tôi thích dùng React cho frontend"),
            )
            .await;
        }
        let facts = e.store.recent("main", 10, false, Scope::Agent).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].proof_count, 3);
    }

    const LOGIN: &str = r#"{"facts":[{"text":"Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ.","kind":"world","entities":["user"]}]}"#;

    #[tokio::test]
    async fn a_known_fact_repeated_in_other_words_is_reinforced_not_stored_again() {
        let e = engine(false);
        retain(
            &e,
            &Scripted::ok(LOGIN),
            Mode::Redact,
            &req("User: tôi sửa xong lỗi đăng nhập ứng dụng thanh toán"),
        )
        .await;

        // Another word, another kind: the text hash and the vector floor see a
        // new statement; it is the same one.
        let llm = Scripted::ok(
            r#"{"facts":[{"text":"Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ rồi.","kind":"experience","entities":["user"]}]}"#,
        );
        let report = retain(
            &e,
            &llm,
            Mode::Redact,
            &req("User: lỗi đăng nhập ứng dụng thanh toán xong rồi nhé"),
        )
        .await;
        assert!(
            llm.seen.lock().unwrap()[0]
                .1
                .contains("KNOWN FACTS:\n[F1] Người dùng đã sửa lỗi"),
            "the model was shown it"
        );
        assert_eq!(
            (report.extracted, report.stored, report.reinforced),
            (1, 0, 1)
        );
        let facts = e.store.recent("main", 10, false, Scope::Agent).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].proof_count, 2, "one more mention of the same fact");
    }

    #[tokio::test]
    async fn a_changed_fact_the_model_did_not_mark_is_stored_not_taken_for_a_repeat() {
        let e = engine(false);
        retain(
            &e,
            &Scripted::ok(REACT),
            Mode::Redact,
            &req("User: tôi thích dùng React cho frontend"),
        )
        .await;
        // One name apart from the stored fact, and no `replaces`: four fifths of
        // the same words, and a different thing said.
        let llm = Scripted::ok(
            r#"{"facts":[{"text":"Người dùng thích dùng Vue cho frontend.","kind":"world","entities":["user","Vue"]}]}"#,
        );
        let report = retain(
            &e,
            &llm,
            Mode::Redact,
            &req("User: tôi thích dùng Vue cho frontend"),
        )
        .await;
        assert!(llm.seen.lock().unwrap()[0]
            .1
            .contains("[F1] Người dùng thích dùng React"));
        assert_eq!((report.stored, report.reinforced), (1, 0));
        assert_eq!(
            e.store
                .recent("main", 10, false, Scope::Agent)
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn a_repeat_that_brings_a_date_gives_it_to_the_fact_it_repeats() {
        let e = engine(false);
        retain(
            &e,
            &Scripted::ok(
                r#"{"facts":[{"text":"Hôm qua, người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ.","kind":"world","entities":["user"]}]}"#,
            ),
            Mode::Redact,
            &req("User: hôm qua tôi sửa xong lỗi đăng nhập ứng dụng thanh toán"),
        )
        .await;
        let report = retain(
            &e,
            &Scripted::ok(
                r#"{"facts":[{"text":"Hôm qua, người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ rồi.","kind":"world","when":"2026-10-15","entities":["user"]}]}"#,
            ),
            Mode::Redact,
            &req("User: lỗi đăng nhập ứng dụng thanh toán xong rồi nhé"),
        )
        .await;
        assert_eq!((report.stored, report.reinforced), (0, 1));
        let facts = e.store.recent("main", 10, false, Scope::Agent).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].occurred_start, Some(1_792_022_400), "2026-10-15");
    }

    const SHORT_AND_LONG: &str = r#"{"facts":[
        {"text":"Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán.","kind":"world","entities":["user","ứng dụng thanh toán"]},
        {"text":"Hôm qua, người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ và cấu hình lại phiên, mất khoảng hai tiếng.","kind":"world","when":"2026-10-15","entities":["user"]}]}"#;

    #[tokio::test]
    async fn a_short_fact_and_its_detailed_twin_in_one_reply_are_stored_once() {
        let e = engine(false);
        let report = retain(
            &e,
            &Scripted::ok(SHORT_AND_LONG),
            Mode::Redact,
            &req("User: hôm qua tôi sửa xong lỗi đăng nhập ứng dụng thanh toán"),
        )
        .await;
        // The short one was said first and is the one left out; it was never stored.
        assert_eq!(
            (
                report.extracted,
                report.stored,
                report.reinforced,
                report.folded
            ),
            (2, 1, 0, 1)
        );
        let facts = e.store.recent("main", 10, false, Scope::Agent).unwrap();
        assert_eq!(facts.len(), 1);
        assert!(facts[0].text.contains("xóa cookie cũ"), "{}", facts[0].text);
        assert_eq!(facts[0].occurred_start, Some(1_792_022_400), "2026-10-15");
        assert_eq!(facts[0].proof_count, 1, "one reply is one mention");
    }

    #[tokio::test]
    async fn two_facts_of_a_reply_one_number_apart_are_both_stored() {
        let e = engine(false);
        let report = retain(
            &e,
            &Scripted::ok(
                r#"{"facts":[
                {"text":"Assistant đã đổi cổng mặc định của ứng dụng thử nghiệm sang 4802 và khởi động lại daemon.","kind":"experience","entities":["assistant"]},
                {"text":"Assistant đã đổi cổng mặc định của ứng dụng thử nghiệm sang 4803 và khởi động lại daemon.","kind":"experience","entities":["assistant"]}]}"#,
            ),
            Mode::Redact,
            &req("Assistant: tôi đã thử cổng 4802, rồi 4803"),
        )
        .await;
        assert_eq!((report.stored, report.folded), (2, 0));
        assert_eq!(
            e.store
                .recent("main", 10, false, Scope::Agent)
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn a_sibling_that_stays_is_still_judged_against_what_is_stored() {
        let e = engine(false);
        retain(
            &e,
            &Scripted::ok(LOGIN),
            Mode::Redact,
            &req("User: tôi sửa xong lỗi đăng nhập ứng dụng thanh toán"),
        )
        .await;

        // Both restate the stored fact, and each other: it is one more mention of
        // the stored fact, not two.
        let llm = Scripted::ok(
            r#"{"facts":[
            {"text":"Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán.","kind":"world","entities":["user"]},
            {"text":"Người dùng đã sửa lỗi đăng nhập của ứng dụng thanh toán bằng cách xóa cookie cũ rồi.","kind":"world","entities":["user"]}]}"#,
        );
        let report = retain(
            &e,
            &llm,
            Mode::Redact,
            &req("User: lỗi đăng nhập ứng dụng thanh toán xong rồi nhé"),
        )
        .await;
        assert!(llm.seen.lock().unwrap()[0]
            .1
            .contains("KNOWN FACTS:\n[F1] Người dùng đã sửa lỗi"));
        assert_eq!(
            (
                report.extracted,
                report.stored,
                report.reinforced,
                report.folded
            ),
            (2, 0, 1, 1)
        );
        let facts = e.store.recent("main", 10, false, Scope::Agent).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].proof_count, 2);
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
            proof_count: 1,
            superseded_by: None,
            origin: String::new(),
            slot: None,
        };
        assert!((temporal_weight(0, &at(0)) - 1.0).abs() < 1e-6);
        assert!((temporal_weight(0, &at(12 * 3600)) - 0.5).abs() < 1e-6);
        assert!((temporal_weight(0, &at(23 * 3600)) - 0.3).abs() < 1e-6);
    }
}
