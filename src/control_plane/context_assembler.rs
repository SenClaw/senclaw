//! Context Assembler primitives (§7): a byte-stable prefix hash for the
//! cache-boundary invariant, and `<agent_status>` — a tail computed entirely
//! by code, never by the model.
//!
//! **Scope note (why this is primitives, not a rewrite):** the harness
//! already keeps Zone A mostly stable — `SYSTEM_PROMPT` plus the skills/tool
//! catalogue is rebuilt per turn but from turn-invariant sources, and
//! per-turn content (user message, tool results) already lands after it, in
//! the message list, never spliced into the system block. Turning
//! [`agent_status::render`] into a real per-*call* addition (§7 says appended
//! on every LLM call within a turn, not just the first) needs a seam inside
//! `zen_core::conversation`'s tool-calling loop that does not exist yet — the
//! one seam that already injects a `<system-reminder>` block
//! (`zen_core::engine`, `SENCLAW.md` + date) only fires once, on a turn's
//! first call. Wiring a second, per-call injection point into that loop is
//! exactly the highest-risk seam the phase's own seam map calls out
//! ("moving content between system and user roles changes token accounting
//! and cache hits"; "editing messages mid-turn can drop injected pending
//! inputs"). This module ships the renderer complete and unit-tested,
//! `controlPlane.agentStatus` defaulted on per spec, ready for that seam —
//! wiring it live is left to the phase that adds the per-call hook
//! (tracked as an open question in this phase's report, not silently
//! dropped).

use sha2::{Digest, Sha256};

/// Roughly 4 chars/token — the same ballpark estimate used elsewhere in this
/// codebase for a *budget*, never for provider billing. `agent_status` must
/// stay well under this since a wrong estimate should fail toward "too
/// short", not "silently over budget".
const CHARS_PER_TOKEN: usize = 4;
const MAX_TOKENS: usize = 300;
const MAX_CHARS: usize = MAX_TOKENS * CHARS_PER_TOKEN;

/// Sha-256 of the given bytes, hex-encoded. Used to log one prefix hash per
/// LLM call — a cheap way to prove Zone A did not move without diffing full
/// requests in the trace.
pub fn prefix_hash(static_prefix: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(static_prefix.as_bytes());
    hex::encode(hasher.finalize())
}

/// Purely structured inputs — there is no field here a tool result or a web
/// page could populate, so "never contains web content" holds by
/// construction, not by a filter that could miss a case.
#[derive(Debug, Clone, Default)]
pub struct AgentStatusInput {
    pub todo_open: usize,
    pub todo_total: usize,
    pub call_index: u32,
    pub call_budget: u32,
    pub elapsed_secs: u64,
    pub pending_events: u32,
    /// The user's original request for this task, truncated if needed to
    /// keep the whole block under budget — never re-summarized by a model.
    pub original_goal: String,
}

const GOAL_ELLIPSIS: &str = "…[truncated]";

/// Render the `<agent_status>` tail. Deterministic and pure: the same input
/// always renders the same bytes, and the length is bounded by construction
/// (the goal is what gets cut, never the fixed fields), so a caller can
/// always append this without doing its own budget check.
pub fn render(input: &AgentStatusInput) -> String {
    let head = format!(
        "<agent_status>\n\
         todo: {}/{} open\n\
         call: {}/{}\n\
         elapsed: {}s\n\
         pending_events: {}\n\
         goal: ",
        input.todo_open, input.todo_total, input.call_index, input.call_budget, input.elapsed_secs, input.pending_events,
    );
    let tail = "\n</agent_status>";
    let budget_for_goal = MAX_CHARS.saturating_sub(head.len() + tail.len());
    let goal = truncate_goal(&input.original_goal, budget_for_goal);
    format!("{head}{goal}{tail}")
}

fn truncate_goal(goal: &str, max_chars: usize) -> String {
    if goal.chars().count() <= max_chars {
        return goal.to_string();
    }
    let keep = max_chars.saturating_sub(GOAL_ELLIPSIS.chars().count());
    let mut out: String = goal.chars().take(keep).collect();
    out.push_str(GOAL_ELLIPSIS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_hash_is_stable_across_calls_with_identical_bytes() {
        let a = prefix_hash("SYSTEM_PROMPT + tool catalogue v1");
        let b = prefix_hash("SYSTEM_PROMPT + tool catalogue v1");
        let c = prefix_hash("SYSTEM_PROMPT + tool catalogue v2");
        assert_eq!(a, b, "byte-identical Zone A must hash identically across turns");
        assert_ne!(a, c);
    }

    #[test]
    fn render_is_deterministic_and_stays_under_the_token_budget() {
        let input = AgentStatusInput {
            todo_open: 2,
            todo_total: 5,
            call_index: 3,
            call_budget: 30,
            elapsed_secs: 42,
            pending_events: 1,
            original_goal: "Fix the retry logic in fetch_user".into(),
        };
        let a = render(&input);
        let b = render(&input);
        assert_eq!(a, b);
        assert!(a.starts_with("<agent_status>\n"));
        assert!(a.ends_with("</agent_status>"));
        assert!(a.chars().count() <= MAX_CHARS);
        assert!(a.contains("todo: 2/5 open"));
        assert!(a.contains("call: 3/30"));
    }

    #[test]
    fn a_very_long_goal_is_truncated_not_the_fixed_fields() {
        let input = AgentStatusInput { original_goal: "x".repeat(5000), ..Default::default() };
        let out = render(&input);
        assert!(out.chars().count() <= MAX_CHARS);
        assert!(out.contains("todo: 0/0 open"), "fixed fields survive truncation");
        assert!(out.contains(GOAL_ELLIPSIS));
    }

    #[test]
    fn never_carries_a_field_that_could_hold_web_content() {
        // Structural guarantee, not a runtime filter: AgentStatusInput has no
        // string field except the caller's own goal text.
        let input = AgentStatusInput::default();
        assert_eq!(render(&input).matches("http").count(), 0);
    }
}
