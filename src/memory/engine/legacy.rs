//! The two prompt blocks recall used to inject on their own.
//!
//! `<cognitive_memory>` and `<memory_recall>` each ranked, trimmed and
//! formatted independently, with no shared budget. They remain the behaviour
//! when the unified engine is off, and the baseline the engine is measured
//! against. They take their store as an argument so a test can hand them one;
//! the agent pool resolves the process-wide singletons and delegates here.
//!
//! Failure contract, unchanged: any error logs and yields an empty string —
//! pre-retrieval never fails the turn.

use crate::memory::cognitive::{format_hits_for_prompt, CognitiveSystem};
use crate::memory::manager::MemoryManager;

use super::arms::{curated_candidates, graph_hits};

/// Spreading-activation context for `<cognitive_memory>`.
pub async fn cognitive_block(
    sys: &CognitiveSystem,
    prompt: &str,
    folder: &str,
    max_results: usize,
) -> String {
    match graph_hits(sys, prompt, folder, max_results).await {
        Ok(hits) => format_hits_for_prompt(&hits, 200),
        Err(e) => {
            tracing::warn!("[AgentPool] Cognitive pre-retrieval failed: {e}");
            String::new()
        }
    }
}

/// Curated-note context for `<memory_recall>`, ending with the hint that
/// teaches the agent how to dig deeper.
pub async fn curated_block(
    mgr: &MemoryManager,
    query: &str,
    folder: &str,
    chat_jid: &str,
    max_results: usize,
) -> String {
    let notes = match curated_candidates(mgr, query, folder, chat_jid, max_results).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!("[AgentPool] Curated pre-retrieval failed: {e}");
            return String::new();
        }
    };

    let mut out = String::new();
    let mut n = 0usize;
    for note in notes.into_iter().take(max_results) {
        n += 1;
        out.push_str(&format!("[{n}] {}\n{}\n\n", note.header, note.snippet));
    }
    if n == 0 {
        return String::new();
    }
    format!(
        "Saved memories relevant to this request (point-in-time notes — verify before asserting):\n\n{}\n\nIf you need more detail, call the memory_recall tool (search) or memory_get (read a full memory file).",
        out.trim()
    )
}
