//! The unified recall pipeline: arms in parallel → fusion → merge of
//! duplicates → boosts → token budget.
//!
//! No model is called anywhere on this path. It runs on every chat turn, so it
//! has to be cheap and it must never fail the turn: an arm that errors or runs
//! out of time contributes nothing and is reported, and the rest still answer.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;

use super::arms::RecallArm;
use super::boost;
use super::budget;
use super::fusion::{merge_near_duplicates, reciprocal_rank_fusion, DEFAULT_DUPLICATE_OVERLAP};
use super::types::{ArmStat, Hit, RecallQuery, Recalled};

#[derive(Debug, Clone)]
pub struct RecallConfig {
    /// Budget for the whole block, in estimated tokens (words, not model
    /// tokens — `chunker::estimate_tokens`; real counts run higher).
    pub max_tokens: usize,
    /// A store that has not answered by now is left out of this turn.
    pub arm_timeout: Duration,
    /// Overlap at which two results are one thing said twice.
    pub duplicate_overlap: f64,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            max_tokens: 600,
            arm_timeout: Duration::from_secs(3),
            duplicate_overlap: DEFAULT_DUPLICATE_OVERLAP,
        }
    }
}

pub async fn recall(
    arms: &[Arc<dyn RecallArm>],
    query: &RecallQuery,
    cfg: &RecallConfig,
) -> Recalled {
    let runs = join_all(
        arms.iter()
            .map(|arm| run_arm(arm.as_ref(), query, cfg.arm_timeout)),
    )
    .await;

    let mut stats = Vec::with_capacity(runs.len());
    let mut lists: Vec<(&'static str, Vec<Hit>)> = Vec::with_capacity(runs.len());
    for (stat, hits) in runs {
        lists.push((stat.arm, hits));
        stats.push(stat);
    }

    let fused = reciprocal_rank_fusion(&lists);
    let (fused, merged_duplicates) = merge_near_duplicates(fused, cfg.duplicate_overlap);
    let scored = boost::score(fused, query.now);
    let (items, dropped_by_budget) = budget::pack(scored, cfg.max_tokens);
    let tokens = items.iter().map(|i| i.tokens).sum();

    Recalled {
        items,
        arms: stats,
        merged_duplicates,
        dropped_by_budget,
        tokens,
    }
}

async fn run_arm(
    arm: &dyn RecallArm,
    query: &RecallQuery,
    timeout: Duration,
) -> (ArmStat, Vec<Hit>) {
    let started = Instant::now();
    let outcome = tokio::time::timeout(timeout, arm.search(query)).await;
    let millis = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(Ok(mut hits)) => {
            hits.truncate(query.limit.max(1));
            (
                ArmStat {
                    arm: arm.name(),
                    hits: hits.len(),
                    millis,
                    error: None,
                },
                hits,
            )
        }
        Ok(Err(e)) => {
            tracing::warn!("[MemoryEngine] {} arm failed: {e}", arm.name());
            (failed(arm, millis, e.to_string()), Vec::new())
        }
        Err(_) => {
            tracing::warn!(
                "[MemoryEngine] {} arm timed out after {timeout:?}",
                arm.name()
            );
            (
                failed(arm, millis, format!("timed out after {timeout:?}")),
                Vec::new(),
            )
        }
    }
}

fn failed(arm: &dyn RecallArm, millis: u64, error: String) -> ArmStat {
    ArmStat {
        arm: arm.name(),
        hits: 0,
        millis,
        error: Some(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{anyhow, Result};
    use async_trait::async_trait;

    struct Canned {
        name: &'static str,
        hits: Vec<Hit>,
    }

    #[async_trait]
    impl RecallArm for Canned {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn search(&self, _q: &RecallQuery) -> Result<Vec<Hit>> {
            Ok(self.hits.clone())
        }
    }

    struct Broken;

    #[async_trait]
    impl RecallArm for Broken {
        fn name(&self) -> &'static str {
            "broken"
        }
        async fn search(&self, _q: &RecallQuery) -> Result<Vec<Hit>> {
            Err(anyhow!("store unavailable"))
        }
    }

    struct Hangs;

    #[async_trait]
    impl RecallArm for Hangs {
        fn name(&self) -> &'static str {
            "hangs"
        }
        async fn search(&self, _q: &RecallQuery) -> Result<Vec<Hit>> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(Vec::new())
        }
    }

    fn hit(arm: &'static str, key: &str, body: &str) -> Hit {
        Hit {
            arm,
            key: key.into(),
            header: key.into(),
            body: body.into(),
            at: None,
            proof: 1,
        }
    }

    fn query() -> RecallQuery {
        RecallQuery {
            text: "q".into(),
            folder: "f".into(),
            chat_jid: "j".into(),
            limit: 5,
            now: 0,
        }
    }

    #[tokio::test]
    async fn results_of_every_arm_are_fused_in_one_list() {
        let arms: Vec<Arc<dyn RecallArm>> = vec![
            Arc::new(Canned {
                name: "curated",
                hits: vec![hit("curated", "n1", "alpha")],
            }),
            Arc::new(Canned {
                name: "cognitive",
                hits: vec![hit("cognitive", "c1", "beta")],
            }),
        ];
        let out = recall(&arms, &query(), &RecallConfig::default()).await;
        assert_eq!(out.items.len(), 2);
        assert_eq!(out.arms.len(), 2);
        assert!(out.arms.iter().all(|a| a.error.is_none() && a.hits == 1));
    }

    #[tokio::test]
    async fn a_failing_arm_costs_only_its_own_hits() {
        let arms: Vec<Arc<dyn RecallArm>> = vec![
            Arc::new(Broken),
            Arc::new(Canned {
                name: "curated",
                hits: vec![hit("curated", "n1", "alpha")],
            }),
        ];
        let out = recall(&arms, &query(), &RecallConfig::default()).await;
        assert_eq!(out.items.len(), 1);
        let broken = out.arms.iter().find(|a| a.arm == "broken").unwrap();
        assert_eq!(broken.hits, 0);
        assert!(broken
            .error
            .as_deref()
            .unwrap()
            .contains("store unavailable"));
    }

    #[tokio::test]
    async fn an_arm_that_hangs_is_left_out_instead_of_stalling_the_turn() {
        let arms: Vec<Arc<dyn RecallArm>> = vec![
            Arc::new(Hangs),
            Arc::new(Canned {
                name: "curated",
                hits: vec![hit("curated", "n1", "alpha")],
            }),
        ];
        let cfg = RecallConfig {
            arm_timeout: Duration::from_millis(50),
            ..RecallConfig::default()
        };
        let started = Instant::now();
        let out = recall(&arms, &query(), &cfg).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(out.items.len(), 1);
        let hung = out.arms.iter().find(|a| a.arm == "hangs").unwrap();
        assert!(hung.error.as_deref().unwrap().contains("timed out"));
    }

    #[tokio::test]
    async fn no_arms_and_no_hits_both_recall_nothing() {
        let out = recall(&[], &query(), &RecallConfig::default()).await;
        assert!(out.items.is_empty());
        let empty: Vec<Arc<dyn RecallArm>> = vec![Arc::new(Canned {
            name: "curated",
            hits: vec![],
        })];
        assert!(recall(&empty, &query(), &RecallConfig::default())
            .await
            .items
            .is_empty());
    }

    #[tokio::test]
    async fn the_same_fact_in_two_stores_is_shown_once() {
        let fact =
            "the daemon serves the web UI on port 18788 and the websocket gateway on port 18789";
        let arms: Vec<Arc<dyn RecallArm>> = vec![
            Arc::new(Canned {
                name: "curated",
                hits: vec![hit("curated", "note", fact)],
            }),
            Arc::new(Canned {
                name: "cognitive",
                hits: vec![hit("cognitive", "node", fact)],
            }),
        ];
        let out = recall(&arms, &query(), &RecallConfig::default()).await;
        assert_eq!(out.items.len(), 1);
        assert_eq!(out.merged_duplicates, 1);
        assert_eq!(out.items[0].sources.len(), 2);
    }

    #[tokio::test]
    async fn the_budget_bounds_the_block() {
        let long = vec!["word"; 80].join(" ");
        let arms: Vec<Arc<dyn RecallArm>> = vec![Arc::new(Canned {
            name: "curated",
            hits: (0..5)
                .map(|i| hit("curated", &format!("n{i}"), &format!("{long} {i}x{i}y")))
                .collect(),
        })];
        let cfg = RecallConfig {
            max_tokens: 200,
            ..RecallConfig::default()
        };
        let out = recall(&arms, &query(), &cfg).await;
        assert!(out.tokens <= 200);
        assert!(out.dropped_by_budget >= 2);
    }
}
