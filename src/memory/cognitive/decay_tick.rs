//! Periodic decay sweep — the "make this layer alive" piece.
//!
//! Ported from shodh-memory: every N seconds, walk the **active** edges,
//! apply decay, **archive** the faded ones (consolidate to dormant state —
//! never delete; see [`RelationshipEdge::archive`]), advance LTP states for
//! the survivors, and record a summary row in `cog_decay_log`.
//!
//! Deletion was the original shodh behaviour and it destroyed knowledge:
//! a fact extracted once decayed below threshold in ~8 days, its edge was
//! pruned, and the orphan-entity sweep then deleted the entities too — the
//! graph could never accumulate. Archived edges keep their row (and their
//! entities), stay retrievable at floor weight, and revive on reactivation.
//!
//! ## Default cadence
//!
//! 300s (5 min). At that interval the cache stays warm for callers and the
//! per-edge IO cost is amortised. Override via [`DecayConfig::interval`].
//!
//! ## Boot wiring
//!
//! The daemon spawns one ticker per `GraphStore` instance:
//!
//! ```ignore
//! let handle = start_decay_ticker(graph.clone(), DecayConfig::default());
//! ```
//!
//! Dropping `handle` (or `abort()`) stops the loop. No graceful shutdown is
//! needed — decay is idempotent, a half-finished sweep just resumes next tick.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use tokio::task::JoinHandle;

use super::graph_store::GraphStore;

const BATCH_SIZE: usize = 256;

#[derive(Debug, Clone)]
pub struct DecayConfig {
    pub interval: Duration,
    /// Cap on edges processed per tick. `0` = unlimited (whole table).
    pub max_edges_per_tick: usize,
}

impl Default for DecayConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(300),
            max_edges_per_tick: 0,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct DecayReport {
    pub edges_scanned: usize,
    /// Edges consolidated to dormant state this sweep (formerly deleted —
    /// nothing is deleted anymore; archived rows persist in `cog_edges`
    /// with `valid_to` set and revive on reactivation).
    pub edges_archived: usize,
    pub edges_promoted: usize,
    pub duration_ms: i64,
}

/// Run one decay sweep over the graph. Returns a report; also persists into
/// `cog_decay_log` (the log's `edges_pruned` column now records archives).
pub fn run_decay(graph: &dyn GraphStore, cfg: &DecayConfig) -> Result<DecayReport> {
    let started = std::time::Instant::now();
    let now = Utc::now().timestamp();
    let total = graph.count_edges()?;
    let cap = if cfg.max_edges_per_tick == 0 {
        total
    } else {
        cfg.max_edges_per_tick.min(total)
    };

    let mut scanned = 0usize;
    let mut archived = 0usize;
    let mut promoted = 0usize;
    let mut offset = 0usize;

    while scanned < cap {
        let batch_size = BATCH_SIZE.min(cap - scanned);
        let batch = graph.scan_edges(batch_size, offset)?;
        if batch.is_empty() {
            break;
        }
        // Stable offset advance: only count *active survivors* toward the
        // offset. `scan_edges` filters archived rows out, so an edge that
        // gets archived here disappears from the next page exactly like a
        // deletion used to — do not advance past it.
        let mut survivors_this_batch = 0usize;
        for mut edge in batch {
            scanned += 1;
            let prev_tier = edge.tier;
            let should_archive = edge.decay(now);
            if should_archive {
                edge.archive(now);
                graph.upsert_edge(&edge)?;
                archived += 1;
            } else {
                if edge.tier != prev_tier {
                    promoted += 1;
                }
                graph.upsert_edge(&edge)?;
                survivors_this_batch += 1;
            }
        }
        offset += survivors_this_batch;
    }

    // NOTE: no orphan-entity sweep here anymore. Since edges are archived
    // in place, decay can no longer orphan an entity — nodes only lose
    // edges through explicit forget/junk-cleanup, and `cleanup_junk` owns
    // that path.

    let duration_ms = started.elapsed().as_millis() as i64;
    graph.record_decay_run(now, scanned, archived, promoted, duration_ms)?;

    Ok(DecayReport {
        edges_scanned: scanned,
        edges_archived: archived,
        edges_promoted: promoted,
        duration_ms,
    })
}

/// Spawn a background loop running [`run_decay`] every `cfg.interval`.
/// Drop the handle (or call `abort()`) to stop.
pub fn start_decay_ticker(graph: Arc<dyn GraphStore>, cfg: DecayConfig) -> JoinHandle<()> {
    tracing::info!(
        interval_sec = cfg.interval.as_secs(),
        "[cognitive] decay ticker started"
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(cfg.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Skip the immediate first tick so daemon boot stays snappy.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let graph_ref = Arc::clone(&graph);
            let cfg_ref = cfg.clone();
            let res = tokio::task::spawn_blocking(move || run_decay(&*graph_ref, &cfg_ref)).await;
            match res {
                Ok(Ok(rep)) => tracing::debug!(
                    scanned = rep.edges_scanned,
                    archived = rep.edges_archived,
                    promoted = rep.edges_promoted,
                    duration_ms = rep.duration_ms,
                    "[cognitive] decay sweep complete"
                ),
                Ok(Err(e)) => tracing::error!(error = %e, "[cognitive] decay sweep failed"),
                Err(e) => tracing::error!(error = %e, "[cognitive] decay sweep task panicked"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::memory::cognitive::data_point::DataPoint;
    use crate::memory::cognitive::graph_store::SqliteGraphStore;
    use crate::memory::cognitive::ltp::LtpStatus;
    use crate::memory::cognitive::triplet::RelationshipEdge;
    use std::sync::Arc;

    fn store() -> (Arc<Db>, Arc<SqliteGraphStore>) {
        let cfg = Config::from_env();
        let db = Arc::new(Db::open_in_memory(&cfg).unwrap());
        let g = Arc::new(SqliteGraphStore::new(Arc::clone(&db)));
        (db, g)
    }

    #[test]
    fn weak_stale_edges_get_archived_not_deleted() {
        let (_db, g) = store();
        let now = Utc::now().timestamp();
        let stale = now - 10 * 86_400; // 10 days ago (L1 archive-eligible)

        let a = DataPoint::entity("A", stale);
        let b = DataPoint::entity("B", stale);
        g.upsert_node(&a).unwrap();
        g.upsert_node(&b).unwrap();

        let mut edge = RelationshipEdge::new(a.id, b.id, "rel", stale);
        edge.strength = 0.04; // below L1 archive threshold (0.05) after any decay
        edge.last_activated = stale;
        g.upsert_edge(&edge).unwrap();

        let report = run_decay(&*g, &DecayConfig::default()).unwrap();
        assert_eq!(report.edges_scanned, 1);
        assert_eq!(report.edges_archived, 1);
        // The row is KEPT — knowledge is consolidated, not destroyed.
        assert_eq!(g.count_edges().unwrap(), 1);
        let kept = g.neighbors(a.id, 10).unwrap();
        assert_eq!(kept.len(), 1);
        assert!(kept[0].is_archived(), "edge must carry the archive marker");
        assert!(kept[0].strength >= 0.05, "archived strength is floored");

        // A second sweep scans nothing — archived edges are filtered out.
        let report2 = run_decay(&*g, &DecayConfig::default()).unwrap();
        assert_eq!(report2.edges_scanned, 0);
        assert_eq!(report2.edges_archived, 0);
    }

    #[test]
    fn archival_leaves_entities_alone() {
        let (_db, g) = store();
        let now = Utc::now().timestamp();
        let stale = now - 10 * 86_400;

        // A—B connected by a fading edge; C is a brand-new edge-less entity
        // (mid-cognify simulation). ALL of them must survive the sweep.
        let a = DataPoint::entity("A", stale);
        let b = DataPoint::entity("B", stale);
        let fresh = DataPoint::entity("FreshMidCognify", now);
        for n in [&a, &b, &fresh] {
            g.upsert_node(n).unwrap();
        }
        let mut edge = RelationshipEdge::new(a.id, b.id, "rel", stale);
        edge.strength = 0.04;
        edge.last_activated = stale;
        g.upsert_edge(&edge).unwrap();

        let report = run_decay(&*g, &DecayConfig::default()).unwrap();
        assert_eq!(report.edges_archived, 1);
        assert!(
            g.get_node(a.id).unwrap().is_some() && g.get_node(b.id).unwrap().is_some(),
            "entities must survive edge archival"
        );
        assert!(g.get_node(fresh.id).unwrap().is_some());
    }

    #[test]
    fn archived_edge_revives_on_strengthen() {
        let (_db, g) = store();
        let now = Utc::now().timestamp();
        let stale = now - 10 * 86_400;

        let a = DataPoint::entity("A", stale);
        let b = DataPoint::entity("B", stale);
        g.upsert_node(&a).unwrap();
        g.upsert_node(&b).unwrap();
        let mut edge = RelationshipEdge::new(a.id, b.id, "rel", stale);
        edge.strength = 0.04;
        edge.last_activated = stale;
        g.upsert_edge(&edge).unwrap();

        run_decay(&*g, &DecayConfig::default()).unwrap();
        let mut archived = g.neighbors(a.id, 10).unwrap().remove(0);
        assert!(archived.is_archived());

        // Re-mention the fact: strengthen + persist → active again.
        archived.strengthen(1.0, now);
        g.upsert_edge(&archived).unwrap();
        let revived = g.neighbors(a.id, 10).unwrap().remove(0);
        assert!(!revived.is_archived(), "strengthen must revive the edge");
        // And the decay scan sees it again.
        let report = run_decay(&*g, &DecayConfig::default()).unwrap();
        assert_eq!(report.edges_scanned, 1);
        assert_eq!(report.edges_archived, 0);
    }

    #[test]
    fn full_ltp_edges_survive_sweep() {
        let (_db, g) = store();
        let now = Utc::now().timestamp();
        // L1 max staleness is 1 day. We want the *age* check to want to
        // archive, but Full LTP to override it. Strength stays well above
        // zombie floor so LTP doesn't get auto-stripped.
        let stale = now - 25 * 3_600; // 25 hours ago

        let a = DataPoint::entity("A", stale);
        let b = DataPoint::entity("B", stale);
        g.upsert_node(&a).unwrap();
        g.upsert_node(&b).unwrap();

        let mut edge = RelationshipEdge::new(a.id, b.id, "rel", stale);
        edge.strength = 0.5;
        edge.ltp_status = LtpStatus::Full;
        edge.last_activated = stale;
        g.upsert_edge(&edge).unwrap();

        let report = run_decay(&*g, &DecayConfig::default()).unwrap();
        assert_eq!(
            report.edges_archived, 0,
            "Full LTP edges must stay active past max staleness"
        );
        assert_eq!(g.count_edges().unwrap(), 1);
        assert!(!g.neighbors(a.id, 10).unwrap()[0].is_archived());
    }

    #[test]
    fn decay_log_row_is_written() {
        let (db, g) = store();
        let report = run_decay(&*g, &DecayConfig::default()).unwrap();
        assert_eq!(report.edges_scanned, 0);

        // cog_decay_log lives on the cognitive connection (DB split — see
        // db/mod.rs::cog_conn). Probe the right handle.
        let count: i64 = db
            .with_cog_conn(|conn| {
                conn.query_row("SELECT COUNT(*) FROM cog_decay_log", [], |r| r.get(0))
                    .map_err(Into::into)
            })
            .unwrap();
        assert_eq!(count, 1);
    }
}
