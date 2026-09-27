//! Periodic maintenance sweep — keeps the cognitive graph tidy *and* keeps
//! it reasoning.
//!
//! Runs [`run_maintenance`] every `cfg.interval` hours. Each pass:
//!   1. `cleanup_junk` — drop envelope-wrapped chunks + orphan entities.
//!   2. `merge_duplicate_entities` — collapse entities sharing a normalised
//!      name onto a canonical survivor and re-point their edges.
//!   3. `infer_associative_edges` — the "suy luận liên kết thông tin" step:
//!      link entities that co-occur in the same chunks but were never
//!      directly connected by an extracted triplet. Pure graph reasoning,
//!      no LLM call.
//!
//! Order matters: cleanup first (so we don't infer from junk), then merge
//! (so co-occurrence is counted against canonical entities, not duplicates),
//! then infer (over the cleaned, deduped graph).
//!
//! Cheap on small graphs (full-table scans of a few thousand rows finish
//! sub-ms). On large graphs the cadence dominates — default 24 h.
//!
//! Disabled when `cfg.interval_hours == 0`; the user can still trigger a
//! sweep manually via `POST /api/cognitive/maintenance`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::task::JoinHandle;

use super::graph_store::{CleanupReport, GraphStore, InferenceReport, MergeReport};

/// Default co-occurrence threshold for associative inference: two entities
/// must be mentioned together by at least this many chunks before we link
/// them. 2 keeps single-chunk coincidences out.
const INFER_MIN_COOCCURRENCE: usize = 2;
/// Cap on inferred edges per sweep so a dense graph can't explode.
const INFER_MAX_PER_RUN: usize = 500;
/// Cosine-similarity floor for embedding-based alias merge ("HN" ↔
/// "Hà Nội"). 0.9 keeps merges conservative — near-duplicates only.
const ALIAS_MIN_COSINE: f32 = 0.9;
/// Alias merge examines only the top-N entities by mention count so the
/// pairwise scan stays O(N²)-bounded per sweep.
const ALIAS_MAX_CANDIDATES: usize = 300;

#[derive(Debug, Clone)]
pub struct MaintenanceConfig {
    /// Cadence between sweeps. `Duration::ZERO` disables the loop.
    pub interval: Duration,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct MaintenanceReport {
    pub cleanup: CleanupReport,
    pub merge: MergeReport,
    pub alias_merge: super::graph_store::AliasMergeReport,
    pub inference: InferenceReport,
    pub duration_ms: i64,
}

pub fn run_maintenance(graph: &dyn GraphStore) -> Result<MaintenanceReport> {
    let started = std::time::Instant::now();
    let cleanup = graph.cleanup_junk()?;
    // Name-based merge first collapses exact duplicates cheaply; the
    // embedding pass then only has true aliases left to consider.
    let merge = graph.merge_duplicate_entities()?;
    let alias_merge = graph.merge_alias_entities(ALIAS_MIN_COSINE, ALIAS_MAX_CANDIDATES)?;
    let inference = graph.infer_associative_edges(INFER_MIN_COOCCURRENCE, INFER_MAX_PER_RUN)?;
    Ok(MaintenanceReport {
        cleanup,
        merge,
        alias_merge,
        inference,
        duration_ms: started.elapsed().as_millis() as i64,
    })
}

/// Spawn a background loop that runs [`run_maintenance`] every
/// `cfg.interval`. Returns `None` when the cadence is zero (caller treats
/// this as "disabled").
pub fn start_maintenance_ticker(
    graph: Arc<dyn GraphStore>,
    cfg: MaintenanceConfig,
) -> Option<JoinHandle<()>> {
    if cfg.interval.is_zero() {
        tracing::info!("[cognitive] maintenance ticker disabled (interval=0)");
        return None;
    }
    tracing::info!(
        interval_hours = cfg.interval.as_secs() / 3600,
        "[cognitive] maintenance ticker started"
    );
    Some(tokio::spawn(async move {
        // First sweep shortly after boot (not immediately — boot stays
        // snappy), then every `cfg.interval`. The old "skip the first
        // tick" pattern meant a daemon restarted more often than the
        // interval NEVER ran maintenance, so orphans/duplicates piled up
        // indefinitely on dev machines.
        let boot_delay = Duration::from_secs(120).min(cfg.interval);
        let mut ticker =
            tokio::time::interval_at(tokio::time::Instant::now() + boot_delay, cfg.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let graph_ref = Arc::clone(&graph);
            let res = tokio::task::spawn_blocking(move || run_maintenance(&*graph_ref)).await;
            match res {
                Ok(Ok(rep)) => tracing::info!(
                    envelope_chunks = rep.cleanup.envelope_chunks_removed,
                    markup_chunks = rep.cleanup.markup_chunks_removed,
                    junk_entities = rep.cleanup.junk_entities_removed,
                    orphans = rep.cleanup.orphan_entities_removed,
                    typeonly_entities = rep.cleanup.typeonly_entities_removed,
                    orphan_types = rep.cleanup.orphan_type_nodes_removed,
                    groups_merged = rep.merge.groups_merged,
                    entities_merged = rep.merge.entities_merged,
                    aliases_merged = rep.alias_merge.entities_merged,
                    associations_inferred = rep.inference.associations_created,
                    duration_ms = rep.duration_ms,
                    "[cognitive] maintenance sweep complete"
                ),
                Ok(Err(e)) => tracing::error!(error = %e, "[cognitive] maintenance sweep failed"),
                Err(e) => {
                    tracing::error!(error = %e, "[cognitive] maintenance sweep task panicked")
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::memory::cognitive::data_point::DataPoint;
    use crate::memory::cognitive::graph_store::SqliteGraphStore;
    use crate::memory::cognitive::triplet::RelationshipEdge;
    use chrono::Utc;
    use std::sync::Arc;

    fn store() -> (Arc<Db>, Arc<SqliteGraphStore>) {
        let cfg = Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        let g = Arc::new(SqliteGraphStore::new(Arc::clone(&db)));
        (db, g)
    }

    #[test]
    fn maintenance_merges_duplicate_entities_and_redirects_edges() {
        let (_db, g) = store();
        let now = Utc::now().timestamp();

        // Two entity nodes with the same name (case + whitespace variant).
        let mut a = DataPoint::entity("Acme", now - 1000);
        a.mention_count = 3;
        let mut b = DataPoint::entity("  acme ", now);
        b.mention_count = 1;
        let c = DataPoint::entity("Bob", now);
        let d = DataPoint::entity("Carol", now);
        g.upsert_node(&a).unwrap();
        g.upsert_node(&b).unwrap();
        g.upsert_node(&c).unwrap();
        g.upsert_node(&d).unwrap();

        // Edges: carol -> a (canonical), bob -> b (duplicate). After merge
        // both must point at a; the bob->b edge is the one being redirected.
        // Without an edge on `a` cleanup_junk (which runs before merge in
        // run_maintenance) would prune it as an orphan first.
        g.upsert_edge(&RelationshipEdge::new(d.id, a.id, "founded", now))
            .unwrap();
        g.upsert_edge(&RelationshipEdge::new(c.id, b.id, "works_at", now))
            .unwrap();

        let rep = run_maintenance(&*g).unwrap();
        assert_eq!(rep.merge.groups_merged, 1);
        assert_eq!(rep.merge.entities_merged, 1);
        assert_eq!(rep.merge.edges_redirected, 1);

        // b is gone; the edge from c now points at a.
        let nbrs = g.neighbors(c.id, 10).unwrap();
        assert_eq!(nbrs.len(), 1);
        // Edge previously pointed at b; after merge it points at a.
        assert_eq!(nbrs[0].dst, a.id);
        assert_eq!(nbrs[0].src, c.id);
    }

    #[test]
    fn maintenance_is_noop_when_no_duplicates() {
        let (_db, g) = store();
        let now = Utc::now().timestamp();
        let a = DataPoint::entity("Alice", now);
        let b = DataPoint::entity("Bob", now);
        g.upsert_node(&a).unwrap();
        g.upsert_node(&b).unwrap();
        let edge = RelationshipEdge::new(a.id, b.id, "knows", now);
        g.upsert_edge(&edge).unwrap();

        let rep = run_maintenance(&*g).unwrap();
        assert_eq!(rep.merge.groups_merged, 0);
        assert_eq!(rep.merge.entities_merged, 0);
        // The edge survives untouched.
        assert_eq!(g.count_edges().unwrap(), 1);
    }

    #[test]
    fn inference_links_co_mentioned_entities() {
        let (_db, g) = store();
        let now = Utc::now().timestamp();

        // Two entities co-mentioned by TWO distinct chunks → co-occurrence 2,
        // hits the default threshold. No direct edge between them exists.
        let alice = DataPoint::entity("Alice", now);
        let bob = DataPoint::entity("Bob", now);
        // Realistic prose — cleanup's sanitize-parity pass drops chunks the
        // ingest guard would reject (sub-10-char / markup-heavy), so bare
        // placeholders like "c1" would vanish before inference runs.
        let chunk1 = DataPoint::chunk("Alice met Bob at the market", Some("h1".into()), now);
        let chunk2 = DataPoint::chunk("Alice and Bob shipped the release", Some("h2".into()), now);
        for n in [&alice, &bob, &chunk1, &chunk2] {
            g.upsert_node(n).unwrap();
        }
        for chunk in [&chunk1, &chunk2] {
            g.upsert_edge(&RelationshipEdge::new(chunk.id, alice.id, "MENTIONS", now))
                .unwrap();
            g.upsert_edge(&RelationshipEdge::new(chunk.id, bob.id, "MENTIONS", now))
                .unwrap();
        }

        let rep = run_maintenance(&*g).unwrap();
        assert_eq!(
            rep.inference.associations_created, 1,
            "one ASSOCIATED_WITH expected"
        );

        // The inferred edge connects Alice and Bob (either direction).
        let nbrs = g.neighbors(alice.id, 10).unwrap();
        assert!(
            nbrs.iter()
                .any(|e| e.predicate == "ASSOCIATED_WITH" && (e.dst == bob.id || e.src == bob.id)),
            "expected inferred ASSOCIATED_WITH edge between Alice and Bob",
        );

        // Idempotent: a second sweep adds nothing (edge now exists).
        let rep2 = run_maintenance(&*g).unwrap();
        assert_eq!(rep2.inference.associations_created, 0);
    }

    #[test]
    fn inference_skips_single_chunk_coincidence() {
        let (_db, g) = store();
        let now = Utc::now().timestamp();
        // Co-mentioned by only ONE chunk → below threshold (2), no link.
        let a = DataPoint::entity("Carol", now);
        let b = DataPoint::entity("Dave", now);
        let chunk = DataPoint::chunk("only", Some("h".into()), now);
        for n in [&a, &b, &chunk] {
            g.upsert_node(n).unwrap();
        }
        g.upsert_edge(&RelationshipEdge::new(chunk.id, a.id, "MENTIONS", now))
            .unwrap();
        g.upsert_edge(&RelationshipEdge::new(chunk.id, b.id, "MENTIONS", now))
            .unwrap();

        let rep = run_maintenance(&*g).unwrap();
        assert_eq!(rep.inference.associations_created, 0);
    }
}
