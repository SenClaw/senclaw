//! RelationshipEdge — typed, Hebbian-dynamic edge between two DataPoints.
//!
//! Port of shodh-memory `RelationshipEdge`. The struct is **storage-shaped**:
//! every field is also a column in `cog_edges`, so loading/saving is a 1:1
//! mapping. Hebbian / decay / LTP logic operates on this struct and lets
//! callers persist the result.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::ltp::{detect_ltp_status, LtpStatus};
use super::tiers::EdgeTier;

const HEBBIAN_LR: f32 = 0.1; // η — base learning rate
const STRENGTHEN_IMPORTANCE_FLOOR: f32 = 0.1;
const ACTIVATION_RING_CAP: usize = 32;
const LTP_PRUNE_FLOOR: f32 = 0.02;
/// Strength floor an edge keeps when archived. Non-zero so spreading
/// activation can still traverse dormant knowledge (weakly) instead of
/// treating it as absent.
const ARCHIVE_STRENGTH_FLOOR: f32 = 0.05;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationshipEdge {
    pub src: Uuid,
    pub dst: Uuid,
    pub predicate: String,
    pub props: Value,

    /// World time — when the fact holds. `valid_to = None` means "this is
    /// still the current fact"; a value means a later fact superseded it.
    /// Only contradiction resolution writes `valid_to`; decay must not.
    pub valid_from: i64,
    pub valid_to: Option<i64>,
    /// The chunk/episode whose fact superseded this one. Provenance for
    /// "why does the graph no longer believe this?".
    pub invalidated_by: Option<Uuid>,

    /// System time — when decay consolidated the edge to dormant. Distinct
    /// from `valid_to` on purpose: "nobody has mentioned this lately" and
    /// "this is no longer true" are different claims, and only the second
    /// one should hide a fact from retrieval.
    pub archived_at: Option<i64>,

    pub strength: f32,
    pub tier: EdgeTier,
    pub activation_count: u32,
    pub last_activated: i64,
    pub ltp_status: LtpStatus,
    pub ltp_detected_at: Option<i64>,
    pub entity_confidence: Option<f32>,
    pub endpoint_selectivity: Option<f32>,
    pub forman_curvature: Option<f32>,
    pub activation_timestamps: Vec<i64>,

    pub source_episode_id: Option<Uuid>,
    pub context: String,
    pub created_at: i64,
}

impl RelationshipEdge {
    pub fn new(src: Uuid, dst: Uuid, predicate: impl Into<String>, now: i64) -> Self {
        Self {
            src,
            dst,
            predicate: predicate.into(),
            props: Value::Object(Default::default()),
            valid_from: now,
            valid_to: None,
            invalidated_by: None,
            archived_at: None,
            strength: 0.35,
            tier: EdgeTier::L1Working,
            activation_count: 0,
            last_activated: now,
            ltp_status: LtpStatus::None,
            ltp_detected_at: None,
            entity_confidence: None,
            endpoint_selectivity: None,
            forman_curvature: None,
            activation_timestamps: Vec::new(),
            source_episode_id: None,
            context: String::new(),
            created_at: now,
        }
    }

    /// Builder: override the starting tier. Extracted facts (cognify
    /// triplets, MENTIONS provenance, is_a typing) start in L2Episodic —
    /// L1Working's 2.9%/hour decay + 1-day max age is calibrated for
    /// transient working-set edges (e.g. inferred ASSOCIATED_WITH), and
    /// kills a once-mentioned fact within hours of extraction.
    pub fn with_tier(mut self, tier: EdgeTier) -> Self {
        self.tier = tier;
        self
    }

    /// True when the edge has been archived (consolidated to dormant state)
    /// by the decay sweep. Archived edges keep their row (the knowledge is
    /// preserved and still retrievable, just down-ranked) and are frozen: no
    /// further decay.
    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }

    /// True when this is still what the graph believes — no later fact has
    /// superseded it. Independent of [`Self::is_archived`]: a dormant edge is
    /// still current, an invalidated one is not.
    pub fn is_current(&self) -> bool {
        self.valid_to.is_none()
    }

    /// True when the fact held at `t` (world time). `[valid_from, valid_to)`,
    /// half-open so a supersession timestamp belongs to the new fact only.
    pub fn is_valid_at(&self, t: i64) -> bool {
        self.valid_from <= t && self.valid_to.is_none_or(|end| end > t)
    }

    /// Close the fact's validity: a later fact for the same subject and
    /// predicate replaced it. Never deletes — the row stays queryable as
    /// history and via `as_of`, matching how decay archives rather than
    /// prunes.
    ///
    /// Also stamps `archived_at`: a superseded fact is by definition not
    /// live knowledge, and freezing it keeps the decay sweep off rows that
    /// exist only for the record.
    pub fn invalidate(&mut self, at: i64, by: Uuid) {
        // Never let a supersession land before the fact started — an
        // out-of-order ingest would otherwise produce a negative interval.
        self.valid_to = Some(at.max(self.valid_from));
        self.invalidated_by = Some(by);
        if self.archived_at.is_none() {
            self.archived_at = Some(at);
            self.strength = self.strength.max(ARCHIVE_STRENGTH_FLOOR);
        }
    }

    /// The fact is being asserted again after having been superseded (someone
    /// moved back, the price returned). Re-opens the interval from `now`.
    ///
    /// Deliberately **not** part of [`Self::strengthen`]: only a fresh
    /// extraction may revive a fact's truth. Traversal-time Hebbian
    /// reinforcement — which fires merely because a retrieval walked past —
    /// must never resurrect something the graph knows to be outdated.
    pub fn reassert(&mut self, now: i64) {
        if self.valid_to.is_some() {
            self.valid_to = None;
            self.invalidated_by = None;
            self.valid_from = now;
        }
    }

    /// Read-only decay calculation — what the strength *would* be at `now`
    /// without mutating the edge. Used by retrievers to rank without
    /// triggering write traffic. Archived edges are frozen at their stored
    /// strength — time no longer erodes them.
    pub fn effective_strength(&self, now: i64) -> f32 {
        if self.is_archived() {
            return self.strength;
        }
        let elapsed = (now - self.last_activated).max(0) as f32;
        let raw_decay = self.tier.decay_rate() * elapsed;
        let protection = self
            .ltp_status
            .effective_protection(self.endpoint_selectivity);
        let net = raw_decay / protection;
        (self.strength - net).max(0.0)
    }

    /// Hebbian strengthen: `w_new = w_old + η·(1 - w_old)·boost·importance_scale`.
    /// Returns `Some((from, to))` if the edge was promoted to a new tier.
    pub fn strengthen(&mut self, importance: f32, now: i64) -> Option<(EdgeTier, EdgeTier)> {
        // Reactivation revives an *archived* edge: the knowledge is back in
        // active circulation, so the dormant marker comes off and decay
        // applies again from this activation. It does NOT touch `valid_to`
        // — mentioning an outdated fact does not make it true again (that is
        // `reassert`, which only extraction may call).
        self.archived_at = None;
        let imp = importance.clamp(STRENGTHEN_IMPORTANCE_FLOOR, 1.0);
        let boost = self.tier.co_access_boost() * imp;
        let delta = HEBBIAN_LR * (1.0 - self.strength).max(0.0) * boost;
        self.strength = (self.strength + delta).min(1.5); // allow mild overshoot
        self.activation_count = self.activation_count.saturating_add(1);
        self.last_activated = now;

        // Ring buffer: drop oldest if at cap.
        if self.activation_timestamps.len() >= ACTIVATION_RING_CAP {
            self.activation_timestamps.remove(0);
        }
        self.activation_timestamps.push(now);

        // LTP detection
        let new_ltp = detect_ltp_status(&self.activation_timestamps, self.activation_count, now);
        if new_ltp as u8 > self.ltp_status as u8 {
            self.ltp_status = new_ltp;
            self.ltp_detected_at = Some(now);
        }

        // Promotion
        if self.strength >= self.tier.promotion_threshold() {
            if let Some(next) = self.tier.next() {
                let from = self.tier;
                self.tier = next;
                // After promotion, reset strength into the new tier's working range.
                self.strength = next.prune_threshold() + 0.05;
                return Some((from, next));
            }
        }
        None
    }

    /// Apply decay; return `true` if the edge should be **archived**.
    ///
    /// Archiving replaced pruning: a faded edge is consolidated to dormant
    /// state (see [`Self::archive`]) instead of deleted, so extracted
    /// knowledge is never destroyed by the passage of time — it just stops
    /// competing at full weight until something reactivates it.
    pub fn decay(&mut self, now: i64) -> bool {
        // Already dormant — frozen, nothing to do. (The decay scan filters
        // archived edges out; this is a defensive short-circuit for direct
        // callers.)
        if self.is_archived() {
            return false;
        }
        let effective = self.effective_strength(now);
        self.strength = effective;

        // Zombie cleanup: LTP-protected but actually dead → strip protection.
        if effective <= LTP_PRUNE_FLOOR
            && matches!(
                self.ltp_status,
                LtpStatus::Full | LtpStatus::Weekly | LtpStatus::Burst
            )
        {
            self.ltp_status = LtpStatus::None;
            self.ltp_detected_at = None;
        }

        // Staleness-based archive (unless Full LTP protects it). Measured
        // from `last_activated`, NOT `created_at` — an old fact that is
        // still being mentioned/retrieved is live knowledge and must not
        // fade on a birthday deadline.
        if let Some(max_age) = self.tier.max_age_secs() {
            let stale_for = now - self.last_activated;
            if stale_for > max_age && !matches!(self.ltp_status, LtpStatus::Full) {
                return true;
            }
        }

        // Strength-based archive (LTP::Full edges stay active even when
        // weak — Hebbian permanence trumps simple threshold).
        if effective < self.tier.prune_threshold() && !matches!(self.ltp_status, LtpStatus::Full) {
            return true;
        }

        false
    }

    /// Consolidate the edge into dormant/archived state instead of deleting
    /// it: stamp `archived_at`, floor the strength so spreading retrieval can
    /// still traverse it weakly, and freeze it (archived edges skip decay).
    /// [`Self::strengthen`] revives it. The fact's truth is untouched — a
    /// dormant fact is still the current one until something supersedes it.
    pub fn archive(&mut self, now: i64) {
        self.archived_at = Some(now);
        self.strength = self.strength.max(ARCHIVE_STRENGTH_FLOOR);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn fresh() -> RelationshipEdge {
        RelationshipEdge::new(Uuid::new_v4(), Uuid::new_v4(), "rel", 0)
    }

    #[test]
    fn strengthen_increases_strength_monotonically() {
        let mut e = fresh();
        let before = e.strength;
        e.strengthen(1.0, 1);
        assert!(e.strength > before);
        assert_eq!(e.activation_count, 1);
        assert_eq!(e.activation_timestamps.len(), 1);
    }

    #[test]
    fn effective_strength_decays_over_time() {
        let mut e = fresh();
        e.strength = 0.5;
        e.last_activated = 0;
        // 1 day later for L1 = 0.029/h * 24 ≈ 0.696 decay → clamped at 0
        let after = e.effective_strength(86_400);
        assert!(after < 0.5);
    }

    #[test]
    fn full_ltp_protects_from_prune() {
        let mut e = fresh();
        // Below the L1 prune threshold (0.05) but above the zombie floor (0.02)
        // so Full LTP genuinely protects rather than being stripped.
        e.strength = 0.03;
        e.ltp_status = LtpStatus::Full;
        e.last_activated = 0;
        let should_prune = e.decay(1_000);
        assert!(!should_prune);
    }

    #[test]
    fn archive_freezes_strength_and_strengthen_revives() {
        let mut e = fresh();
        e.strength = 0.04;
        e.last_activated = 0;
        e.archive(100);
        assert!(e.is_archived());
        // Floored to the archive minimum, then frozen: effective_strength no
        // longer erodes with time.
        assert!(e.strength >= 0.05);
        let frozen = e.effective_strength(1_000_000);
        assert_eq!(frozen, e.strength, "archived edge must not keep decaying");
        // Archived edges short-circuit decay — never re-flagged.
        assert!(!e.decay(2_000_000));

        // Reactivation revives it.
        e.strengthen(1.0, 2_000_000);
        assert!(!e.is_archived(), "strengthen must clear the archive marker");
        assert!(e.strength > 0.05);
    }

    #[test]
    fn recently_activated_old_edge_is_not_age_archived() {
        let mut e = fresh();
        // Created long ago (10× the L1 max age) but activated just now:
        // staleness-based aging must keep it active.
        e.created_at = 0;
        let now = 10 * 86_400;
        e.strength = 0.5;
        e.last_activated = now - 60;
        assert!(!e.decay(now), "recently-used old edge must stay active");
    }

    // The bug the archived_at split exists to prevent: while decay's marker
    // and "no longer true" shared one column, any re-mention of a superseded
    // fact revived it as current.
    #[test]
    fn strengthen_wakes_a_dormant_edge_but_never_revives_a_false_one() {
        let mut dormant = fresh();
        dormant.archive(100);
        dormant.strengthen(1.0, 200);
        assert!(!dormant.is_archived(), "dormant knowledge wakes on mention");
        assert!(dormant.is_current());

        let mut superseded = fresh();
        superseded.invalidate(100, Uuid::new_v4());
        superseded.strengthen(1.0, 200);
        assert!(
            !superseded.is_current(),
            "mentioning an outdated fact must not make it true again"
        );
        assert_eq!(superseded.valid_to, Some(100));
    }

    #[test]
    fn invalidate_closes_the_interval_and_records_who_did_it() {
        let mut e = fresh();
        e.valid_from = 1_000;
        let culprit = Uuid::new_v4();
        e.invalidate(2_000, culprit);

        assert_eq!(e.valid_to, Some(2_000));
        assert_eq!(e.invalidated_by, Some(culprit));
        assert!(e.is_valid_at(1_500), "still true before the handover");
        assert!(!e.is_valid_at(2_000), "the seam belongs to the new fact");
        assert!(!e.is_valid_at(2_500));
        assert!(!e.is_valid_at(999), "not yet true before it was asserted");
        // Superseded facts stop decaying — they exist for the record now.
        assert!(e.is_archived());
    }

    // Out-of-order ingest: a fact discovered later can carry an earlier
    // timestamp. The interval must never run backwards.
    #[test]
    fn invalidate_cannot_close_before_the_fact_started() {
        let mut e = fresh();
        e.valid_from = 5_000;
        e.invalidate(1_000, Uuid::new_v4());
        assert_eq!(e.valid_to, Some(5_000));
        assert!(e.valid_to.unwrap() >= e.valid_from);
    }

    #[test]
    fn reassert_reopens_a_superseded_fact() {
        let mut e = fresh();
        e.valid_from = 1_000;
        e.invalidate(2_000, Uuid::new_v4());

        e.reassert(3_000);
        assert!(e.is_current());
        assert_eq!(e.invalidated_by, None);
        assert_eq!(e.valid_from, 3_000, "the new interval starts now");
        assert!(!e.is_valid_at(2_500), "the gap stays a gap");
        assert!(e.is_valid_at(3_500));
    }

    #[test]
    fn reassert_leaves_a_current_fact_alone() {
        let mut e = fresh();
        e.valid_from = 1_000;
        e.reassert(9_000);
        assert_eq!(
            e.valid_from, 1_000,
            "re-mentioning a live fact must not restart its history"
        );
    }

    #[test]
    fn promotion_advances_tier() {
        let mut e = fresh();
        // Strengthen repeatedly until we cross L1.promotion_threshold (0.6).
        let mut promoted = None;
        for t in 1..50 {
            if let Some(p) = e.strengthen(1.0, t) {
                promoted = Some(p);
                break;
            }
        }
        let (from, to) = promoted.expect("edge should promote within 50 strengthens");
        assert_eq!(from, EdgeTier::L1Working);
        assert_eq!(to, EdgeTier::L2Episodic);
    }
}
