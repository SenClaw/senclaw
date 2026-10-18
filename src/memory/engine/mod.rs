//! Unified recall across the daemon's memory stores.
//!
//! Knowledge lives in three places that grew separately: curated notes
//! (`memory/*.md`), the cognitive graph, and the pinned `MEMORY.md`. Before
//! this module each injected its own prompt block, ranked by its own score,
//! with no shared budget — so the same fact could arrive twice and a long
//! block starved a short, better one.
//!
//! The engine reads the searchable stores — notes, the graph, and the fact
//! store ([`facts`]) — through [`RecallArm`]s and turns their answers into one
//! block:
//!
//! ```text
//! arms (parallel) → reciprocal rank fusion → fold duplicates
//!                 → recency boost → token budget → one <memory_context> block
//! ```
//!
//! `MEMORY.md` is deliberately *not* an arm. It is pinned context — standing
//! instructions the agent must always see — and a query-dependent retrieval
//! could leave them out.
//!
//! The ideas (parallel strategies fused by rank, multiplicative secondary
//! boosts, a token budget in place of a result count) follow Hindsight
//! (<https://github.com/vectorize-io/hindsight>, MIT). Nothing here calls a
//! model, so it is safe on the request path of every turn.

pub mod arms;
pub mod boost;
pub mod budget;
pub mod facts;
pub mod format;
pub mod fusion;
/// The blocks recall used to inject on their own: the baseline the eval
/// measures the engine against. Not compiled into the daemon.
#[cfg(test)]
pub mod legacy;
pub mod recall;
pub mod types;

#[cfg(test)]
mod eval;

use std::sync::Arc;

pub use arms::{CognitiveArm, CuratedArm, RecallArm};
pub use format::WRAPPER_TAG;
pub use recall::{recall, RecallConfig};
pub use types::{ArmStat, Hit, Item, ProjectRecall, RecallQuery, Recalled};

/// Which stores a turn may read. Notes follow `memoryRecall` and the graph
/// follows `preCognitive`, so a store the user switched off is never read.
#[derive(Debug, Clone, Copy, Default)]
pub struct ArmSelection {
    pub curated: bool,
    pub cognitive: bool,
    /// The fact store. No switch of its own: it is what conversations are
    /// retained into.
    pub facts: bool,
}

/// The arms for the stores that are up in this process.
pub fn default_arms(selection: ArmSelection) -> Vec<Arc<dyn RecallArm>> {
    let mut arms: Vec<Arc<dyn RecallArm>> = Vec::new();
    if selection.curated {
        if let Some(mgr) = crate::memory::manager::try_get_instance() {
            arms.push(Arc::new(CuratedArm::new(mgr)));
        }
    }
    if selection.cognitive {
        if let Some(sys) = crate::memory::cognitive::try_get_instance() {
            arms.push(Arc::new(CognitiveArm::new(sys)));
        }
    }
    if selection.facts {
        if let Some(engine) = facts::instance() {
            arms.extend(facts::fact_arms(engine));
        }
    }
    arms
}

/// The prompt text for a turn, or an empty string when nothing relevant is
/// found. Never fails.
pub async fn pre_retrieval(
    selection: ArmSelection,
    query: &RecallQuery,
    cfg: &RecallConfig,
) -> String {
    let arms = default_arms(selection);
    if arms.is_empty() {
        return String::new();
    }
    let recalled = recall(&arms, query, cfg).await;
    tracing::debug!(
        "[MemoryEngine] {} item(s), ~{} tokens, {} folded, {} over budget; arms: {}",
        recalled.items.len(),
        recalled.tokens,
        recalled.merged_duplicates,
        recalled.dropped_by_budget,
        recalled
            .arms
            .iter()
            .map(|a| format!("{} {} hit(s) {}ms", a.arm, a.hits, a.millis))
            .collect::<Vec<_>>()
            .join(", ")
    );
    format::render(&recalled.items)
}
