//! Accept-until-N bookkeeping shared by every generating pipeline.
//!
//! The loop owns the counters and the stopping rule; the pipeline owns what a
//! round does. A run stops when it has its target, when the LLM-call budget is
//! spent, when several rounds in a row accept nothing (the seeds or the
//! filters leave no room — more calls would only burn budget), or when the
//! provider keeps failing.

use std::collections::BTreeMap;

use super::llm::LlmReply;

/// Rounds in a row with nothing accepted before the run gives up.
pub const MAX_STALL_ROUNDS: u32 = 5;
/// Provider failures in a row before the run gives up.
pub const MAX_CONSECUTIVE_ERRORS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Done,
    BudgetSpent,
    Stalled,
    ProviderFailing,
}

impl Stop {
    /// A note for the manifest; `None` when the run simply finished.
    pub fn note(self) -> Option<&'static str> {
        match self {
            Stop::Done => None,
            Stop::BudgetSpent => Some("stopped early: the LLM call budget was spent"),
            Stop::Stalled => Some("stopped early: several rounds in a row produced nothing new"),
            Stop::ProviderFailing => Some("stopped early: the model kept failing"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AcceptLoop {
    pub target: u32,
    pub max_llm_calls: u32,
    pub accepted: u32,
    /// Candidates looked at, accepted or not.
    pub attempts: u32,
    pub llm_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub rejected: BTreeMap<String, u64>,
    stall_rounds: u32,
    consecutive_errors: u32,
}

impl AcceptLoop {
    pub fn new(target: u32, max_llm_calls: u32) -> Self {
        AcceptLoop {
            target,
            max_llm_calls,
            accepted: 0,
            attempts: 0,
            llm_calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            rejected: BTreeMap::new(),
            stall_rounds: 0,
            consecutive_errors: 0,
        }
    }

    /// Why the run should stop now, if it should.
    pub fn stop(&self) -> Option<Stop> {
        if self.accepted >= self.target {
            Some(Stop::Done)
        } else if self.consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
            Some(Stop::ProviderFailing)
        } else if self.stall_rounds >= MAX_STALL_ROUNDS {
            Some(Stop::Stalled)
        } else if self.llm_calls >= self.max_llm_calls {
            Some(Stop::BudgetSpent)
        } else {
            None
        }
    }

    /// Whether `n` more calls fit in the budget.
    pub fn can_spend(&self, n: u32) -> bool {
        self.llm_calls + n <= self.max_llm_calls
    }

    pub fn remaining(&self) -> u32 {
        self.target.saturating_sub(self.accepted)
    }

    pub fn record_call(&mut self, reply: &LlmReply) {
        self.llm_calls += 1;
        self.input_tokens += reply.input_tokens;
        self.output_tokens += reply.output_tokens;
        self.consecutive_errors = 0;
    }

    /// A call that never produced a reply still costs budget.
    pub fn record_call_error(&mut self) {
        self.llm_calls += 1;
        self.consecutive_errors += 1;
        self.reject("llm_error");
    }

    pub fn reject(&mut self, reason: &str) {
        *self.rejected.entry(reason.to_string()).or_insert(0) += 1;
    }

    pub fn accept(&mut self) {
        self.accepted += 1;
    }

    pub fn end_round(&mut self, accepted_this_round: u32) {
        if accepted_this_round == 0 {
            self.stall_rounds += 1;
        } else {
            self.stall_rounds = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply() -> LlmReply {
        LlmReply { text: String::new(), model: "m".into(), input_tokens: 10, output_tokens: 5 }
    }

    #[test]
    fn stops_at_target() {
        let mut l = AcceptLoop::new(2, 100);
        assert_eq!(l.stop(), None);
        l.accept();
        l.accept();
        assert_eq!(l.stop(), Some(Stop::Done));
        assert_eq!(l.stop().unwrap().note(), None);
    }

    #[test]
    fn stops_when_the_budget_is_spent() {
        let mut l = AcceptLoop::new(10, 2);
        assert!(l.can_spend(2));
        l.record_call(&reply());
        assert!(!l.can_spend(2));
        l.record_call(&reply());
        assert_eq!(l.stop(), Some(Stop::BudgetSpent));
        assert_eq!((l.input_tokens, l.output_tokens), (20, 10));
    }

    #[test]
    fn stops_after_rounds_that_accept_nothing() {
        let mut l = AcceptLoop::new(10, 100);
        for _ in 0..MAX_STALL_ROUNDS {
            l.end_round(0);
        }
        assert_eq!(l.stop(), Some(Stop::Stalled));
        let mut l = AcceptLoop::new(10, 100);
        for _ in 0..MAX_STALL_ROUNDS - 1 {
            l.end_round(0);
        }
        l.end_round(1);
        assert_eq!(l.stop(), None, "an accepting round resets the stall count");
    }

    #[test]
    fn stops_when_the_provider_keeps_failing() {
        let mut l = AcceptLoop::new(10, 100);
        for _ in 0..MAX_CONSECUTIVE_ERRORS {
            l.record_call_error();
        }
        assert_eq!(l.stop(), Some(Stop::ProviderFailing));
        assert_eq!(l.rejected["llm_error"], MAX_CONSECUTIVE_ERRORS as u64);
    }
}
