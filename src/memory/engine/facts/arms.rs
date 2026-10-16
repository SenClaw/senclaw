//! Four ways to find a fact, each a [`RecallArm`] so fusion can reward the
//! facts that more than one of them finds.
//!
//! | arm | finds | needs |
//! |---|---|---|
//! | `facts_semantic` | facts whose meaning is close to the question | an embedding model |
//! | `facts_keyword` | facts sharing the question's words (BM25) | nothing |
//! | `facts_graph` | facts *connected* to the best matches: same entities, linked by meaning or by date | nothing |
//! | `facts_temporal` | facts from the period the question names | a time in the question |
//!
//! They share one identity (`arm = "facts"`, `key = fact id`) so fusion sees a
//! fact found by two of them as one fact with two votes.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate};

use super::extract::span_label;
use super::store::StoredFact;
use super::temporal::parse_window;
use super::FactEngine;
use crate::memory::cognitive::retrievers::build_fts_match;
use crate::memory::engine::arms::RecallArm;
use crate::memory::engine::types::{Hit, RecallQuery};

pub const FACTS: &str = "facts";

/// Below this a vector neighbour is a different subject.
const MIN_SEMANTIC_SIMILARITY: f32 = 0.25;
/// How many top matches the graph arm grows from.
const GRAPH_SEEDS: usize = 3;
/// Candidates the temporal arm considers before spreading them over the window.
const TEMPORAL_POOL: usize = 60;
const TEMPORAL_BUCKETS: usize = 4;

fn to_hit(f: &StoredFact) -> Hit {
    let header = match (f.occurred_start, f.occurred_end) {
        (Some(s), Some(e)) => span_label(s, e),
        (Some(s), None) => span_label(s, s + 86_400),
        _ => format!(
            "noted {}",
            DateTime::from_timestamp(f.mentioned_at, 0)
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_default()
        ),
    };
    Hit {
        arm: FACTS,
        key: f.id.clone(),
        header,
        body: f.text.clone(),
        at: Some(f.anchor()),
    }
}

fn hits(facts: &[StoredFact]) -> Vec<Hit> {
    facts.iter().map(to_hit).collect()
}

/// Nearest facts by meaning; empty when no embedding model is configured.
async fn semantic(engine: &FactEngine, q: &RecallQuery, limit: usize) -> Result<Vec<StoredFact>> {
    let (Some(model), Some(v)) = (engine.model_key(), engine.embed_query(&q.text).await) else {
        return Ok(Vec::new());
    };
    Ok(engine
        .store
        .search_vector(&q.folder, &model, &v, MIN_SEMANTIC_SIMILARITY, limit)?
        .into_iter()
        .map(|(f, _)| f)
        .collect())
}

fn keyword(
    engine: &FactEngine,
    q: &RecallQuery,
    window: Option<(i64, i64)>,
    limit: usize,
) -> Result<Vec<StoredFact>> {
    match build_fts_match(&q.text) {
        Some(m) => engine.store.search_keyword(&q.folder, &m, window, limit),
        None => Ok(Vec::new()),
    }
}

// ===== semantic =====

pub struct SemanticArm(pub Arc<FactEngine>);

#[async_trait]
impl RecallArm for SemanticArm {
    fn name(&self) -> &'static str {
        "facts_semantic"
    }
    async fn search(&self, q: &RecallQuery) -> Result<Vec<Hit>> {
        Ok(hits(&semantic(&self.0, q, q.limit).await?))
    }
}

// ===== keyword =====

pub struct KeywordArm(pub Arc<FactEngine>);

#[async_trait]
impl RecallArm for KeywordArm {
    fn name(&self) -> &'static str {
        "facts_keyword"
    }
    async fn search(&self, q: &RecallQuery) -> Result<Vec<Hit>> {
        Ok(hits(&keyword(&self.0, q, None, q.limit)?))
    }
}

// ===== graph =====

pub struct GraphArm(pub Arc<FactEngine>);

/// `tanh(0.5 · shared entities)`: the first shared entity counts for a lot,
/// the fifth for little, so a heavily-tagged fact cannot drown a fact that is
/// connected by a link as well.
pub(crate) fn entity_score(shared: usize) -> f32 {
    (0.5 * shared as f32).tanh()
}

#[async_trait]
impl RecallArm for GraphArm {
    fn name(&self) -> &'static str {
        "facts_graph"
    }

    async fn search(&self, q: &RecallQuery) -> Result<Vec<Hit>> {
        let engine = &self.0;
        // Grow from the best direct matches — by meaning when there is an
        // embedding model, by words when there is not.
        let mut seeds = semantic(engine, q, GRAPH_SEEDS).await?;
        if seeds.is_empty() {
            seeds = keyword(engine, q, None, GRAPH_SEEDS)?;
        }
        if seeds.is_empty() {
            return Ok(Vec::new());
        }
        let seed_ids: Vec<String> = seeds.iter().map(|f| f.id.clone()).collect();

        // Three independent kinds of evidence, added: a fact reached by an
        // entity AND a link outranks one reached by either alone.
        let mut score: HashMap<String, f32> = HashMap::new();
        for (id, shared) in engine
            .store
            .related_by_entity(&q.folder, &seed_ids, q.limit * 4)?
        {
            *score.entry(id).or_default() += entity_score(shared);
        }
        let mut best_link: HashMap<(String, String), f32> = HashMap::new();
        for l in engine.store.linked(&q.folder, &seed_ids)? {
            if seed_ids.contains(&l.id) {
                continue;
            }
            let slot = best_link.entry((l.id, l.link_type)).or_default();
            *slot = slot.max(l.weight);
        }
        for ((id, _), w) in best_link {
            *score.entry(id).or_default() += w;
        }

        let mut ranked: Vec<(String, f32)> = score.into_iter().collect();
        // Ties by id so the order is stable between runs.
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(q.limit);
        let ids: Vec<String> = ranked.into_iter().map(|(id, _)| id).collect();
        Ok(hits(&engine.store.get_many(&ids)?))
    }
}

// ===== temporal =====

pub struct TemporalArm(pub Arc<FactEngine>);

/// The calendar date `now` falls on in the machine's timezone.
fn local_date(now: i64) -> NaiveDate {
    DateTime::from_timestamp(now, 0)
        .map(|d| d.with_timezone(&chrono::Local).date_naive())
        .unwrap_or(NaiveDate::MIN)
}

/// Take `limit` facts from `ranked` (best first) spread over the window: the
/// window is cut into `buckets` equal parts and each part gives its best
/// remaining fact in turn. A question about "last year" then sees the whole
/// year, not whichever month happens to hold the most matches.
pub(crate) fn spread(
    ranked: Vec<StoredFact>,
    window: (i64, i64),
    buckets: usize,
    limit: usize,
) -> Vec<StoredFact> {
    let buckets = buckets.max(1);
    let width = ((window.1 - window.0) as f64 / buckets as f64).max(1.0);
    let mut by_bucket: Vec<std::collections::VecDeque<StoredFact>> =
        (0..buckets).map(|_| Default::default()).collect();
    for f in ranked {
        let at = ((f.anchor() - window.0) as f64 / width).floor();
        let b = (at.max(0.0) as usize).min(buckets - 1);
        by_bucket[b].push_back(f);
    }
    let mut out = Vec::with_capacity(limit);
    while out.len() < limit && by_bucket.iter().any(|b| !b.is_empty()) {
        for b in by_bucket.iter_mut() {
            if let Some(f) = b.pop_front() {
                out.push(f);
                if out.len() == limit {
                    break;
                }
            }
        }
    }
    out
}

#[async_trait]
impl RecallArm for TemporalArm {
    fn name(&self) -> &'static str {
        "facts_temporal"
    }

    async fn search(&self, q: &RecallQuery) -> Result<Vec<Hit>> {
        let Some(window) = parse_window(&q.text, local_date(q.now)) else {
            return Ok(Vec::new());
        };
        let engine = &self.0;
        // Relevance first, then whatever else happened in the period, so a
        // question that names only a time ("what did we do yesterday?") still
        // gets the period's facts.
        let mut pool = keyword(engine, q, Some(window), TEMPORAL_POOL)?;
        let known: Vec<String> = pool.iter().map(|f| f.id.clone()).collect();
        for f in engine.store.in_window(&q.folder, window, TEMPORAL_POOL)? {
            if !known.contains(&f.id) {
                pool.push(f);
            }
        }
        Ok(hits(&spread(pool, window, TEMPORAL_BUCKETS, q.limit)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::engine::facts::store::FactKind;

    fn at(id: &str, secs: i64) -> StoredFact {
        StoredFact {
            id: id.into(),
            kind: FactKind::World,
            text: id.into(),
            occurred_start: Some(secs),
            occurred_end: None,
            mentioned_at: 0,
            entities: vec![],
        }
    }

    fn ids(v: &[StoredFact]) -> Vec<&str> {
        v.iter().map(|f| f.id.as_str()).collect()
    }

    #[test]
    fn spreading_takes_the_best_of_each_period_before_a_second_of_any() {
        // Window 0..400 in four buckets of 100. The dense bucket holds the three
        // best matches; a plain top-3 would take only from it.
        let ranked = vec![
            at("a1", 10),
            at("a2", 20),
            at("a3", 30),
            at("c1", 250),
            at("d1", 390),
            at("b1", 150),
        ];
        let got = spread(ranked, (0, 400), 4, 4);
        assert_eq!(ids(&got), ["a1", "b1", "c1", "d1"]);
    }

    #[test]
    fn spreading_fills_from_the_dense_bucket_once_the_others_are_spent() {
        let ranked = vec![at("a1", 10), at("a2", 20), at("a3", 30), at("d1", 390)];
        let got = spread(ranked, (0, 400), 4, 3);
        assert_eq!(ids(&got), ["a1", "d1", "a2"]);
        assert_eq!(spread(vec![], (0, 400), 4, 3), vec![]);
    }

    #[test]
    fn a_fact_outside_the_window_edges_lands_in_the_nearest_bucket() {
        let got = spread(vec![at("early", -50), at("late", 999)], (0, 400), 4, 5);
        assert_eq!(ids(&got), ["early", "late"]);
    }

    #[test]
    fn entity_score_saturates() {
        assert!((entity_score(1) - 0.462).abs() < 1e-3);
        assert!((entity_score(2) - 0.762).abs() < 1e-3);
        assert!(entity_score(10) < 1.0);
        assert_eq!(entity_score(0), 0.0);
    }

    #[test]
    fn a_hit_is_headed_by_when_it_happened_at_the_precision_known() {
        let mut f = at("x", 0);
        f.occurred_start = Some(1_772_323_200); // 2026-03-01
        f.occurred_end = Some(1_775_001_600); // 2026-04-01
        assert_eq!(to_hit(&f).header, "2026-03");
        f.occurred_end = None;
        assert_eq!(to_hit(&f).header, "2026-03-01");
        f.occurred_start = None;
        f.mentioned_at = 1_792_152_000;
        assert_eq!(to_hit(&f).header, "noted 2026-10-16");
        assert_eq!(to_hit(&f).arm, FACTS);
    }
}
