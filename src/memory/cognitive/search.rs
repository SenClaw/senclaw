//! Search surface — port of cognee `SearchType` (subset).
//!
//! Seven modes implemented; the rest of cognee's 17-variant enum (CYPHER,
//! NATURAL_LANGUAGE, AGENTIC_COMPLETION, …) land in later phases. The
//! `SearchType` enum stays open so callers and MCP tools see one stable
//! surface.
//!
//! Every mode is temporal whether or not it says so: [`SearchQuery::as_of`]
//! decides which facts exist for the query at all, and its default (`None` =
//! now) is what keeps superseded facts out of ordinary recall.

use serde::{Deserialize, Serialize};

use super::data_point::DataPoint;
use super::node_set::NodeSet;
use super::triplet::RelationshipEdge;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SearchType {
    /// Dense-vector recall over chunk nodes.
    Chunks,
    /// Dense-vector recall on entities, returning their outgoing edges.
    Triplet,
    /// k-hop subgraph rooted at vector-seed nodes (no write-back).
    GraphCompletion,
    /// BFS with Hebbian write-back — strengthens activated edges so
    /// frequently-recalled paths become more salient over time.
    SpreadingActivation,
    /// BM25 full-text recall over node name + summary. No embeddings —
    /// the zero-cost path, available even when the embedder is dormant.
    Fts,
    /// Vector + FTS seeds merged (0.7 vector / 0.3 FTS), deduped by node.
    /// Degrades to FTS-only when the embedder is unavailable.
    Hybrid,
    /// Facts as of a point in world time. Entity seeds like `Triplet`, but
    /// the edges are filtered by [`SearchQuery::as_of`] and ranked by how
    /// close their validity interval sits to it — "what was the price on
    /// 31/07?" rather than "what is the price?".
    Temporal,
}

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub query_text: String,
    pub query_type: SearchType,
    /// Top-K results.
    pub limit: usize,
    /// Hops for graph-style searches (ignored for `Chunks`).
    pub hops: u8,
    /// Multiplicative decay per hop in spreading activation.
    pub decay_per_hop: f32,
    /// Restrict seed nodes to these node_sets (any-of). Empty = no scope.
    pub node_sets: Vec<NodeSet>,
    /// Run the configured `GraphScorer` (e.g. LightGCN) to re-rank the
    /// top-K candidates before returning. Off by default — opt in per call
    /// when the extra IO is worth it (typically: GraphCompletion).
    pub rerank: bool,
    /// Weight for blending base score with rerank score in [0, 1].
    /// `final = (1 - α) * base + α * rerank`. 0.5 = even mix.
    pub rerank_alpha: f32,
    /// World time to answer as of. `None` — the default everywhere — means
    /// "what the graph believes now": superseded facts are excluded, so a
    /// recall can no longer return last week's price as the current one.
    /// `Some(t)` time-travels, and is always opt-in: a bitemporal store
    /// measured a recall *drop* (50% → 37.5%) when historical versions were
    /// mixed into ordinary queries, so this must never become the default.
    pub as_of: Option<i64>,
}

impl SearchQuery {
    pub fn chunks(text: impl Into<String>, limit: usize) -> Self {
        Self {
            query_text: text.into(),
            query_type: SearchType::Chunks,
            limit,
            hops: 0,
            decay_per_hop: 1.0,
            node_sets: Vec::new(),
            rerank: false,
            rerank_alpha: 0.5,
            as_of: None,
        }
    }
    pub fn graph_completion(text: impl Into<String>, limit: usize, hops: u8) -> Self {
        Self {
            query_text: text.into(),
            query_type: SearchType::GraphCompletion,
            limit,
            hops,
            decay_per_hop: 0.6,
            node_sets: Vec::new(),
            rerank: false,
            rerank_alpha: 0.5,
            as_of: None,
        }
    }
    pub fn spreading(text: impl Into<String>, limit: usize, hops: u8) -> Self {
        Self {
            query_text: text.into(),
            query_type: SearchType::SpreadingActivation,
            limit,
            hops,
            decay_per_hop: 0.6,
            node_sets: Vec::new(),
            rerank: false,
            rerank_alpha: 0.5,
            as_of: None,
        }
    }
    pub fn triplet(text: impl Into<String>, limit: usize) -> Self {
        Self {
            query_text: text.into(),
            query_type: SearchType::Triplet,
            limit,
            hops: 1,
            decay_per_hop: 1.0,
            node_sets: Vec::new(),
            rerank: false,
            rerank_alpha: 0.5,
            as_of: None,
        }
    }
    pub fn fts(text: impl Into<String>, limit: usize) -> Self {
        Self {
            query_text: text.into(),
            query_type: SearchType::Fts,
            limit,
            hops: 0,
            decay_per_hop: 1.0,
            node_sets: Vec::new(),
            rerank: false,
            rerank_alpha: 0.5,
            as_of: None,
        }
    }
    pub fn hybrid(text: impl Into<String>, limit: usize) -> Self {
        Self {
            query_text: text.into(),
            query_type: SearchType::Hybrid,
            limit,
            hops: 0,
            decay_per_hop: 1.0,
            node_sets: Vec::new(),
            rerank: false,
            rerank_alpha: 0.5,
            as_of: None,
        }
    }

    /// Facts as they stood at `as_of`. Passing `None` is legal and means
    /// "now" — the same answer [`Self::triplet`] gives, so callers that
    /// build a temporal query from an optional user parameter don't need a
    /// branch.
    pub fn temporal(text: impl Into<String>, limit: usize, as_of: Option<i64>) -> Self {
        Self {
            query_text: text.into(),
            query_type: SearchType::Temporal,
            limit,
            hops: 1,
            decay_per_hop: 1.0,
            node_sets: Vec::new(),
            rerank: false,
            rerank_alpha: 0.5,
            as_of,
        }
    }

    /// Point every retrieval at a moment in world time. Chainable so callers
    /// keep using the existing constructors.
    pub fn at(mut self, as_of: Option<i64>) -> Self {
        self.as_of = as_of;
        self
    }
}

/// Parse an `as_of` value coming off a tool call or a query string.
///
/// Accepts, in order: unix seconds, RFC 3339, `YYYY-MM-DD HH:MM(:SS)` and
/// `YYYY-MM-DD`. A bare date means the **end** of that day — asking "what was
/// the price on 31/07" should see everything that happened on the 31st, not
/// only what was already true at midnight.
///
/// Naive (zone-less) forms are read as **local time**: they come from a human
/// typing a date, and storage is UTC epoch, so parsing them as UTC would slide
/// the answer by the timezone offset — 7 hours in Vietnam, enough to return
/// the previous day's fact.
pub fn parse_as_of(raw: &str) -> Option<i64> {
    use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(secs) = s.parse::<i64>() {
        // Milliseconds are a common slip from JS callers; anything past the
        // year 33658 is far likelier to be ms than a real timestamp.
        return Some(if secs > 1_000_000_000_000 {
            secs / 1000
        } else {
            secs
        });
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp());
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Local.from_local_datetime(&naive).single().map(|d| d.timestamp());
        }
    }
    for fmt in ["%Y-%m-%d", "%d/%m/%Y"] {
        if let Ok(date) = NaiveDate::parse_from_str(s, fmt) {
            let end = date.and_hms_opt(23, 59, 59)?;
            return Local.from_local_datetime(&end).single().map(|d| d.timestamp());
        }
    }
    None
}

#[cfg(test)]
mod as_of_tests {
    use super::*;

    #[test]
    fn reads_the_forms_callers_actually_send() {
        assert_eq!(parse_as_of("1754006400"), Some(1_754_006_400));
        // A JS caller's milliseconds, not a date in the year 57000.
        assert_eq!(parse_as_of("1754006400000"), Some(1_754_006_400));
        assert_eq!(
            parse_as_of("2026-07-31T00:00:00Z"),
            Some(chrono::DateTime::parse_from_rfc3339("2026-07-31T00:00:00Z")
                .unwrap()
                .timestamp())
        );
        assert!(parse_as_of("  2026-07-31  ").is_some());
        assert!(parse_as_of("31/07/2026").is_some());
        assert!(parse_as_of("2026-07-31 09:30").is_some());
    }

    #[test]
    fn a_bare_date_covers_the_whole_day() {
        // "what was the price on 31/07" must see the 31st, not just midnight.
        let midnight = parse_as_of("2026-07-31 00:00:00").unwrap();
        let day = parse_as_of("2026-07-31").unwrap();
        assert!(day > midnight);
        assert_eq!(day - midnight, 86_399);
    }

    #[test]
    fn naive_input_is_local_time_not_utc() {
        use chrono::{Local, TimeZone};
        // Storage is UTC epoch; reading a typed date as UTC would shift the
        // answer by the machine's offset — 7 hours in Vietnam, enough to
        // return the previous day's fact.
        let parsed = parse_as_of("2026-07-31 12:00:00").unwrap();
        let expected = Local
            .with_ymd_and_hms(2026, 7, 31, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn junk_is_rejected_rather_than_silently_meaning_now() {
        assert_eq!(parse_as_of(""), None);
        assert_eq!(parse_as_of("   "), None);
        assert_eq!(parse_as_of("yesterday"), None);
        assert_eq!(parse_as_of("hôm qua"), None);
        assert_eq!(parse_as_of("2026-13-45"), None);
    }
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub node: DataPoint,
    /// Accumulated relevance / activation score. Higher = better.
    pub score: f32,
    /// Path of edges leading to this node from a seed (empty for direct hits).
    pub path: Vec<RelationshipEdge>,
}
