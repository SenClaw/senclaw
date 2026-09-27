//! Long-Term Potentiation (LTP) state machine.
//!
//! Ported from shodh-memory: detects activation patterns and grants
//! tier-aware decay protection so repeatedly-used edges become permanent.
//!
//! | Status   | Trigger                             | Decay protection |
//! |----------|-------------------------------------|------------------|
//! | None     | default                             | 1×               |
//! | Burst    | 5+ activations in 24 h              | 2×               |
//! | Weekly   | ≥ 3/week for ≥ 2 weeks              | 3×               |
//! | Full     | 10+ total OR 5+ over 30 days        | 10×              |
//!
//! Effective protection is gated by endpoint selectivity (anti-habituation):
//!     effective_ltp = raw_ltp * (sel / (sel + HALF_SAT))

use serde::{Deserialize, Serialize};

const SELECTIVITY_HALF_SAT: f32 = 0.25;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LtpStatus {
    None = 0,
    Burst = 1,
    Weekly = 2,
    Full = 3,
}

impl LtpStatus {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Burst,
            2 => Self::Weekly,
            3 => Self::Full,
            _ => Self::None,
        }
    }

    pub fn raw_protection(self) -> f32 {
        match self {
            Self::None => 1.0,
            Self::Burst => 2.0,
            Self::Weekly => 3.0,
            Self::Full => 10.0,
        }
    }

    /// Selectivity-gated protection (anti-habituation).
    /// `selectivity` ∈ [0, 1]; missing → treat as 0.5 (neutral).
    pub fn effective_protection(self, selectivity: Option<f32>) -> f32 {
        let raw = self.raw_protection();
        let sel = selectivity.unwrap_or(0.5).clamp(0.0, 1.0);
        let gate = sel / (sel + SELECTIVITY_HALF_SAT);
        // No edge should drop below 1.0 — protection only multiplies *up*.
        (raw * gate).max(1.0)
    }
}

/// Detect LTP readiness from activation history.
///
/// `timestamps` — ring buffer of recent activation times (seconds), oldest first.
/// `total_count` — lifetime activation count.
/// `now` — current unix seconds.
pub fn detect_ltp_status(timestamps: &[i64], total_count: u32, now: i64) -> LtpStatus {
    let day = 86_400;
    let week = 7 * day;
    let thirty_days = 30 * day;

    // Full — strongest: lifetime ≥ 10 OR ≥ 10 activations in the last 30 days.
    let recent_30d = timestamps
        .iter()
        .filter(|t| now - **t <= thirty_days)
        .count();
    if total_count >= 10 || recent_30d >= 10 {
        return LtpStatus::Full;
    }

    // Weekly — ≥ 3 activations in each of the last two weeks.
    let last_2w: Vec<&i64> = timestamps
        .iter()
        .filter(|t| now - **t <= 2 * week)
        .collect();
    if last_2w.len() >= 6 {
        let mid = now - week;
        let recent_week = last_2w.iter().filter(|t| ***t > mid).count();
        let prior_week = last_2w.len() - recent_week;
        if recent_week >= 3 && prior_week >= 3 {
            return LtpStatus::Weekly;
        }
    }

    // Burst — ≥ 5 activations in last 24 h (and hasn't promoted to Weekly/Full).
    let recent_24h = timestamps.iter().filter(|t| now - **t <= day).count();
    if recent_24h >= 5 {
        return LtpStatus::Burst;
    }

    LtpStatus::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_detection() {
        let now = 1_000_000;
        let ts: Vec<i64> = (0..5).map(|i| now - i * 3_600).collect();
        assert_eq!(detect_ltp_status(&ts, 5, now), LtpStatus::Burst);
    }

    #[test]
    fn full_via_total_count() {
        let now = 1_000_000;
        let ts = vec![now - 100];
        assert_eq!(detect_ltp_status(&ts, 10, now), LtpStatus::Full);
    }

    #[test]
    fn weekly_detection() {
        let now = 14 * 86_400;
        // 3 in week 1 + 3 in week 2
        let week = 7 * 86_400;
        let ts = vec![
            now - 1,
            now - 2,
            now - 3, // recent week
            now - week - 1,
            now - week - 2,
            now - week - 3, // prior week
        ];
        assert_eq!(detect_ltp_status(&ts, 6, now), LtpStatus::Weekly);
    }

    #[test]
    fn selectivity_gates_protection() {
        // High selectivity → near full protection
        let p_high = LtpStatus::Full.effective_protection(Some(1.0));
        // Low selectivity → reduced protection (anti-habituation)
        let p_low = LtpStatus::Full.effective_protection(Some(0.05));
        assert!(p_high > p_low);
        assert!(p_low >= 1.0); // never drops below 1×
    }
}
