//! Secondary ranking signals, applied on top of the fused order.
//!
//! Fusion knows only positions. A cross-encoder would read query and text
//! together; this build has none, so the relevance stand-in is the fused rank
//! itself, spread over `[0.1, 1.0]`. The boosts then *multiply* it, centred on
//! 1.0 and capped by `alpha`: a multiplicative nudge stays proportional to
//! relevance, where an additive one would let a barely-relevant note leapfrog
//! a strong one just for being new.

use super::fusion::Fused;
use super::types::Item;

/// Largest swing the recency signal may cause: `±alpha / 2` (±10%).
pub const RECENCY_ALPHA: f64 = 0.2;

/// Largest swing the proof signal may cause: `±alpha / 2` (±5%). Evidence
/// is the weakest of the signals — a fact said ten times is a little more
/// likely to be a standing truth than one said once, not a different answer.
pub const PROOF_ALPHA: f64 = 0.1;

/// A note this old or older is "a year ago" as far as recency goes.
const RECENCY_WINDOW_DAYS: f64 = 365.0;

const SECONDS_PER_DAY: f64 = 86_400.0;

/// 1.0 for something recorded now, falling linearly to 0.1 at a year; 0.5
/// (neutral) when the store does not know the date. A date in the future reads
/// as now.
pub fn recency_signal(at: Option<i64>, now: i64) -> f64 {
    let Some(at) = at else {
        return 0.5;
    };
    let days = (now - at).max(0) as f64 / SECONDS_PER_DAY;
    (1.0 - days / RECENCY_WINDOW_DAYS).clamp(0.1, 1.0)
}

/// How much repetition counts: neutral (0.5) for something said once, rising
/// slowly with the log of the count, capped at 1.0 (about 150 repeats).
pub fn proof_signal(count: u32) -> f64 {
    (0.5 + (count.max(1) as f64).ln() / 10.0).clamp(0.0, 1.0)
}

/// `1 + alpha * (signal - 0.5)`: neutral at a signal of 0.5.
pub fn multiplier(signal: f64, alpha: f64) -> f64 {
    1.0 + alpha * (signal - 0.5)
}

/// Relevance stand-in from position: first place 1.0, last 0.1, evenly
/// between. A single result is simply 1.0.
pub fn rank_score(index: usize, total: usize) -> f64 {
    if total <= 1 {
        return 1.0;
    }
    1.0 - 0.9 * index as f64 / (total - 1) as f64
}

/// Score a fused list (already sorted best first) and re-sort it.
pub fn score(fused: Vec<Fused>, now: i64) -> Vec<Item> {
    let total = fused.len();
    let mut items: Vec<Item> = fused
        .into_iter()
        .enumerate()
        .map(|(i, f)| {
            let relevance = rank_score(i, total);
            let recency = multiplier(recency_signal(f.hit.at, now), RECENCY_ALPHA);
            let proof = multiplier(proof_signal(f.hit.proof), PROOF_ALPHA);
            Item {
                score: relevance * recency * proof,
                sources: f.sources,
                tokens: 0,
                hit: f.hit,
            }
        })
        .collect();
    items.sort_by(|a, b| b.score.total_cmp(&a.score));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::engine::types::Hit;

    const NOW: i64 = 1_800_000_000;
    const DAY: i64 = 86_400;

    fn fused(key: &str, at: Option<i64>) -> Fused {
        Fused {
            hit: Hit {
                arm: "curated",
                key: key.into(),
                header: String::new(),
                body: String::new(),
                at,
                proof: 1,
            },
            rrf: 0.0,
            sources: vec![],
        }
    }

    #[test]
    fn recency_is_one_now_and_a_tenth_after_a_year() {
        assert_eq!(recency_signal(Some(NOW), NOW), 1.0);
        assert!((recency_signal(Some(NOW - 182 * DAY), NOW) - 0.501).abs() < 0.01);
        assert_eq!(recency_signal(Some(NOW - 400 * DAY), NOW), 0.1);
    }

    #[test]
    fn an_unknown_date_is_neutral_and_the_future_reads_as_now() {
        assert_eq!(recency_signal(None, NOW), 0.5);
        assert_eq!(multiplier(recency_signal(None, NOW), RECENCY_ALPHA), 1.0);
        assert_eq!(recency_signal(Some(NOW + 30 * DAY), NOW), 1.0);
    }

    #[test]
    fn the_boost_stays_inside_ten_percent() {
        assert!((multiplier(1.0, RECENCY_ALPHA) - 1.1).abs() < 1e-12);
        assert!((multiplier(0.1, RECENCY_ALPHA) - 0.92).abs() < 1e-12);
    }

    #[test]
    fn proof_is_neutral_once_and_grows_slowly_with_repeats() {
        assert_eq!(proof_signal(0), 0.5, "a missing count reads as once");
        assert_eq!(proof_signal(1), 0.5);
        assert!((proof_signal(3) - 0.610).abs() < 0.001);
        assert!((proof_signal(10) - 0.730).abs() < 0.001);
        assert_eq!(proof_signal(1_000_000), 1.0);
        assert_eq!(multiplier(proof_signal(1), PROOF_ALPHA), 1.0);
        assert!(multiplier(proof_signal(1_000_000), PROOF_ALPHA) <= 1.05 + 1e-12);
    }

    #[test]
    fn repetition_nudges_a_neighbour_up_but_never_a_distant_rank() {
        // Rank 1 said once vs rank 2 said ten times: 1.0 × 1.0 against
        // 0.9 × 1.0115 ≈ 0.91 — the repeat does not overtake.
        let mut list: Vec<Fused> = (0..10).map(|i| fused(&format!("k{i}"), None)).collect();
        list[1].hit.proof = 10;
        let keys: Vec<String> = score(list, NOW).into_iter().map(|i| i.hit.key).collect();
        assert_eq!(keys[0], "k0");
        assert_eq!(keys[1], "k1");
        // Two places apart with the same evidence on the lower one: still no.
        let mut list: Vec<Fused> = (0..10).map(|i| fused(&format!("k{i}"), None)).collect();
        list[2].hit.proof = 150;
        let keys: Vec<String> = score(list, NOW).into_iter().map(|i| i.hit.key).collect();
        assert_eq!(&keys[..3], ["k0", "k1", "k2"]);
    }

    #[test]
    fn rank_score_spans_one_to_a_tenth() {
        assert_eq!(rank_score(0, 1), 1.0);
        assert_eq!(rank_score(0, 5), 1.0);
        assert!((rank_score(4, 5) - 0.1).abs() < 1e-12);
    }

    #[test]
    fn recency_swaps_neighbours_but_cannot_lift_a_distant_rank() {
        // Ten results, all a year-plus old except k1, k2 and k9, from today.
        // Relevance falls 1.0, 0.9, 0.8 … 0.1; old ones ×0.92, fresh ones ×1.1:
        //   k1 0.99 · k0 0.92 · k2 0.88 · k3 0.644 …  k9 0.11
        let mut list: Vec<Fused> = (0..10)
            .map(|i| fused(&format!("k{i}"), Some(NOW - 400 * DAY)))
            .collect();
        for fresh in [1, 2, 9] {
            list[fresh].hit.at = Some(NOW);
        }
        let keys: Vec<String> = score(list, NOW).into_iter().map(|i| i.hit.key).collect();
        assert_eq!(
            keys,
            ["k1", "k0", "k2", "k3", "k4", "k5", "k6", "k7", "k8", "k9"],
            "a neighbour overtakes on freshness; a last place stays last"
        );
    }
}
