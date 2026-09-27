//! Loop Controller vocabulary (§9): the 4-tier error taxonomy, a
//! `hash(tool, canonical args)` fingerprint, stuck detection and a generic
//! circuit breaker, plus the three budgets + a latency budget with defaults
//! equal to today's effective limits.
//!
//! This module is the *primitives*, not a second engine loop: the harness
//! already implements most of §9 and this maps onto it rather than
//! replacing it (reuse before adding) —
//!
//! | §9 concept | existing mechanism |
//! |---|---|
//! | exact-duplicate interception | `zen_core::conversation::tool_call_sig` + `is_duplicate_exempt` (`conversation.rs:764,787`) |
//! | tool error → nudge → hard stop | `TOOL_ERROR_NUDGE` / `TOOL_ERROR_FINAL_NUDGE` (`conversation.rs`) |
//! | stall (same tool, no text) | `SENCLAW_STALL_TOOL_TURNS`, default 4, hard stop at 2x (`conversation.rs::stall_tool_turns`) |
//! | turn budget | `SENCLAW_MAX_AGENT_TURNS`, default 30 (`conversation.rs::max_agent_turns`) |
//! | latency budget | `LLM_TURN_TIMEOUT` / `LLM_TURN_TIMEOUT_LOCAL` (`query_llm.rs`) |
//!
//! [`fingerprint`] and [`ErrorTier::classify`] are genuinely new: a stable
//! per-call signature usable outside the engine's own dedup pass (the trace,
//! future specs), and a taxonomy the existing mechanisms do not name today.
//! Turning per-tool circuit breaking and fingerprint-based stuck detection
//! into a second, independent enforcement path is deliberately **not** done —
//! per §17 the Loop Controller is switched on through a gate in P2, after an
//! ablation shows it helps; until then it is built and unit-tested,
//! and its [`Budgets::defaults`] document (not replace) what conversation.rs
//! already enforces.

use std::collections::VecDeque;
use std::time::Duration;

use sha2::{Digest, Sha256};

/// The four error tiers (§9's table), each with its own recovery shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorTier {
    /// 429, 5xx, timeout, provider refusal — backoff, then a fallback model.
    Api,
    /// Bad args, no permission, sandbox timeout — feed the 4-layer error back
    /// to the LLM; retry only on changed input.
    Tool,
    /// Near-full context, a failed compact, a tool call missing its result —
    /// compact in a batch; never silently drop the call.
    Context,
    /// Loop, stuck, a hung stream, runaway recursion — fingerprint + watchdog;
    /// never blind-retry, let Jev (or a person) pick a different direction.
    ControlFlow,
}

impl ErrorTier {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorTier::Api => "api",
            ErrorTier::Tool => "tool",
            ErrorTier::Context => "context",
            ErrorTier::ControlFlow => "control_flow",
        }
    }

    /// A conservative text classifier — used only to label a trace entry, so
    /// a wrong guess costs a mislabeled row, never a behaviour change. Order
    /// matters: control-flow phrases ("stuck", "recursion") are checked
    /// before the more generic "timeout", since a watchdog timeout on a hung
    /// stream is control-flow, not an API timeout.
    pub fn classify(message: &str) -> ErrorTier {
        let m = message.to_lowercase();
        let has = |needles: &[&str]| needles.iter().any(|n| m.contains(n));
        if has(&["stuck", "recursion", "recursive", "infinite loop", "stall", "hung"]) {
            ErrorTier::ControlFlow
        } else if has(&["context window", "context length", "compact", "too many tokens", "missing tool result"]) {
            ErrorTier::Context
        } else if has(&["429", "rate limit", "5xx", "500 ", "502", "503", "overloaded", "timed out waiting for", "timeout waiting"])
        {
            ErrorTier::Api
        } else if has(&["permission denied", "no such tool", "invalid argument", "validation failed", "sandbox timeout"]) {
            ErrorTier::Tool
        } else {
            ErrorTier::Tool
        }
    }
}

/// `hash(tool, canonical args)` — a stable id for "the same call again",
/// usable wherever the caller has the real input (unlike the trace's
/// best-effort per-event fingerprint, this is the primitive built for a real
/// stuck-detection consumer once one is wired). `serde_json::Value` objects in
/// this crate are `BTreeMap`-backed (no `preserve_order` feature), so
/// `to_string` is already key-order independent — matches
/// `zen_core::conversation::tool_call_sig`'s canonicalisation exactly.
pub fn fingerprint(tool: &str, args: &serde_json::Value) -> String {
    let canonical = serde_json::to_string(args).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(tool.as_bytes());
    hasher.update([0u8]);
    hasher.update(canonical.as_bytes());
    hex::encode(&hasher.finalize()[..8])
}

/// Why a rolling window of calls looks stuck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StuckReason {
    /// The same fingerprint repeated `SAME_FINGERPRINT_LIMIT` times running.
    RepeatedCall,
    /// The same error tier repeated `SAME_ERROR_TIER_LIMIT` times running.
    RepeatedErrorTier,
}

const SAME_FINGERPRINT_LIMIT: usize = 3;
const SAME_ERROR_TIER_LIMIT: usize = 2;

/// Same fingerprint 3x running, or the same error tier 2x running — the
/// §9 stuck rule. `retry_same` must not be offered when this returns `Some`.
pub fn is_stuck(recent_fingerprints: &[String], recent_error_tiers: &[ErrorTier]) -> Option<StuckReason> {
    if let Some(last) = recent_fingerprints.last() {
        let run = recent_fingerprints.iter().rev().take_while(|f| *f == last).count();
        if run >= SAME_FINGERPRINT_LIMIT {
            return Some(StuckReason::RepeatedCall);
        }
    }
    if let Some(last) = recent_error_tiers.last() {
        let run = recent_error_tiers.iter().rev().take_while(|t| *t == last).count();
        if run >= SAME_ERROR_TIER_LIMIT {
            return Some(StuckReason::RepeatedErrorTier);
        }
    }
    None
}

/// Per-key (usually per-tool) trip counter. Generic so the same type serves
/// a tool breaker and, later, a per-recovery-path breaker (§9 "circuit
/// breaker cho từng tool và từng đường khôi phục") without duplicating the
/// bookkeeping.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    threshold: usize,
    window: VecDeque<bool>,
}

impl CircuitBreaker {
    pub fn new(threshold: usize) -> CircuitBreaker {
        CircuitBreaker { threshold, window: VecDeque::with_capacity(threshold) }
    }

    /// Record one outcome. Returns `true` once the breaker trips (the last
    /// `threshold` outcomes were all failures) — it stays tripped until a
    /// success is recorded.
    pub fn record(&mut self, ok: bool) -> bool {
        if ok {
            self.window.clear();
            return false;
        }
        self.window.push_back(false);
        if self.window.len() > self.threshold {
            self.window.pop_front();
        }
        self.window.len() >= self.threshold && self.window.iter().all(|o| !o)
    }

    pub fn is_tripped(&self) -> bool {
        self.window.len() >= self.threshold && self.window.iter().all(|o| !o)
    }
}

/// Three budgets (turn/task/session) + a latency budget. Defaults mirror
/// today's effective limits exactly (see the module-level mapping table) —
/// this struct documents them in one place; it does not (yet) enforce them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Budgets {
    /// One user input's tool-calling turns. Mirrors `SENCLAW_MAX_AGENT_TURNS`
    /// (default 30, `conversation.rs::max_agent_turns`).
    pub turn_calls: u32,
    /// A task's wall-clock budget. No distinct concept exists today, so this
    /// defaults to the per-call LLM timeout times the turn budget — a rough
    /// upper bound consistent with what the engine already allows before its
    /// own turn cap or timeout fires first.
    pub task_seconds: u64,
    /// A whole session's wall-clock budget. Same reasoning as `task_seconds`,
    /// times ten — sessions are not bounded today; this is a generous ceiling
    /// for future enforcement, not today's behaviour.
    pub session_seconds: u64,
    /// Per-LLM-call latency budget. Mirrors `LLM_TURN_TIMEOUT_LOCAL`
    /// (`query_llm.rs`, 900s) — the larger of the two existing timeouts,
    /// since a budget should not be tighter than what already succeeds today
    /// on a local model.
    pub latency_ms: u64,
}

impl Budgets {
    pub fn defaults() -> Budgets {
        const MAX_AGENT_TURNS: u32 = 30;
        const LLM_TURN_TIMEOUT_LOCAL: Duration = Duration::from_secs(900);
        Budgets {
            turn_calls: MAX_AGENT_TURNS,
            task_seconds: LLM_TURN_TIMEOUT_LOCAL.as_secs() * MAX_AGENT_TURNS as u64,
            session_seconds: LLM_TURN_TIMEOUT_LOCAL.as_secs() * MAX_AGENT_TURNS as u64 * 10,
            latency_ms: LLM_TURN_TIMEOUT_LOCAL.as_millis() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_and_order_independent_but_sensitive_to_values() {
        let a = fingerprint("search", &serde_json::json!({"q": "gold", "n": 5}));
        let b = fingerprint("search", &serde_json::json!({"n": 5, "q": "gold"}));
        let c = fingerprint("search", &serde_json::json!({"q": "silver", "n": 5}));
        assert_eq!(a, b, "key order must not change the fingerprint");
        assert_ne!(a, c, "a different argument value must change it");
        assert_ne!(fingerprint("search", &serde_json::json!({})), fingerprint("fetch", &serde_json::json!({})));
    }

    #[test]
    fn error_tier_classifies_the_documented_examples() {
        assert_eq!(ErrorTier::classify("429 Too Many Requests"), ErrorTier::Api);
        assert_eq!(ErrorTier::classify("Permission denied: cannot write /etc"), ErrorTier::Tool);
        assert_eq!(ErrorTier::classify("context window exceeded, compact failed"), ErrorTier::Context);
        assert_eq!(ErrorTier::classify("stuck: same tool called 5 times"), ErrorTier::ControlFlow);
    }

    #[test]
    fn stuck_detection_fires_at_the_documented_thresholds_not_before() {
        let two = vec!["a".to_string(), "a".to_string()];
        assert_eq!(is_stuck(&two, &[]), None, "two repeats is not yet stuck");
        let three = vec!["a".to_string(), "a".to_string(), "a".to_string()];
        assert_eq!(is_stuck(&three, &[]), Some(StuckReason::RepeatedCall));
        let broken = vec!["a".to_string(), "a".to_string(), "b".to_string()];
        assert_eq!(is_stuck(&broken, &[]), None, "a different call in between resets the run");

        assert_eq!(is_stuck(&[], &[ErrorTier::Api]), None);
        assert_eq!(is_stuck(&[], &[ErrorTier::Api, ErrorTier::Api]), Some(StuckReason::RepeatedErrorTier));
        assert_eq!(is_stuck(&[], &[ErrorTier::Api, ErrorTier::Tool]), None);
    }

    #[test]
    fn circuit_breaker_trips_on_a_run_of_failures_and_resets_on_success() {
        let mut b = CircuitBreaker::new(3);
        assert!(!b.record(false));
        assert!(!b.record(false));
        assert!(b.record(false), "third consecutive failure trips it");
        assert!(b.is_tripped());
        assert!(!b.record(true), "a success clears it");
        assert!(!b.is_tripped());
    }

    #[test]
    fn budget_defaults_mirror_todays_effective_limits() {
        let b = Budgets::defaults();
        assert_eq!(b.turn_calls, 30, "must match conversation::max_agent_turns()'s default");
        assert_eq!(b.latency_ms, 900_000, "must match LLM_TURN_TIMEOUT_LOCAL");
    }
}
