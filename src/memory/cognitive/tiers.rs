//! Edge tiers — multi-scale memory consolidation.
//!
//! Ported from shodh-memory `EdgeTier`:
//!   * L1Working  — short-term, decays ~2.9 %/hour
//!   * L2Episodic — recent events, decays ~3.1 %/day
//!   * L3Semantic — consolidated knowledge, decays ~2 %/month
//!
//! Decay rates are stored as **per-second** floats so [`apply_decay`] needs
//! only `(now - last_activated)` seconds, regardless of tier.

use serde::{Deserialize, Serialize};

const HOUR_S: f32 = 3_600.0;
const DAY_S: f32 = 86_400.0;
const MONTH_S: f32 = 86_400.0 * 30.0;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeTier {
    L1Working = 0,
    L2Episodic = 1,
    L3Semantic = 2,
}

impl EdgeTier {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::L2Episodic,
            2 => Self::L3Semantic,
            _ => Self::L1Working,
        }
    }

    /// Per-second linear decay coefficient.
    /// `new_strength = old - rate * elapsed_seconds` (clamped at 0).
    pub fn decay_rate(self) -> f32 {
        match self {
            Self::L1Working => 0.029 / HOUR_S,
            Self::L2Episodic => 0.031 / DAY_S,
            Self::L3Semantic => 0.02 / MONTH_S,
        }
    }

    /// Strength threshold to promote into the next tier.
    pub fn promotion_threshold(self) -> f32 {
        match self {
            Self::L1Working => 0.6,
            Self::L2Episodic => 0.75,
            Self::L3Semantic => 1.0, // top tier — no further promotion
        }
    }

    /// Edges weaker than this get pruned (unless LTP-protected).
    pub fn prune_threshold(self) -> f32 {
        match self {
            Self::L1Working => 0.05,
            Self::L2Episodic => 0.10,
            Self::L3Semantic => 0.15,
        }
    }

    /// Max edge age (seconds) before forced prune, unless LTP-protected.
    pub fn max_age_secs(self) -> Option<i64> {
        match self {
            Self::L1Working => Some(60 * 60 * 24),       // 1 day
            Self::L2Episodic => Some(60 * 60 * 24 * 30), // 30 days
            Self::L3Semantic => Some(60 * 60 * 24 * 90), // 90 days
        }
    }

    /// Co-activation boost per `strengthen()` call, varies by tier
    /// (shodh `TIER_CO_ACCESS_BOOST`).
    pub fn co_access_boost(self) -> f32 {
        match self {
            Self::L1Working => 0.20,
            Self::L2Episodic => 0.10,
            Self::L3Semantic => 0.05,
        }
    }

    pub fn next(self) -> Option<Self> {
        match self {
            Self::L1Working => Some(Self::L2Episodic),
            Self::L2Episodic => Some(Self::L3Semantic),
            Self::L3Semantic => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_of_thresholds() {
        // promotion thresholds are strictly increasing
        assert!(
            EdgeTier::L1Working.promotion_threshold() < EdgeTier::L2Episodic.promotion_threshold()
        );
        assert!(
            EdgeTier::L2Episodic.promotion_threshold() < EdgeTier::L3Semantic.promotion_threshold()
        );
        // L1 decays faster than L2 faster than L3
        assert!(EdgeTier::L1Working.decay_rate() > EdgeTier::L2Episodic.decay_rate());
        assert!(EdgeTier::L2Episodic.decay_rate() > EdgeTier::L3Semantic.decay_rate());
    }

    #[test]
    fn roundtrip_u8() {
        for t in [
            EdgeTier::L1Working,
            EdgeTier::L2Episodic,
            EdgeTier::L3Semantic,
        ] {
            assert_eq!(EdgeTier::from_u8(t as u8), t);
        }
    }
}
