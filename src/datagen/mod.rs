//! Synthetic dataset generation, run as background jobs in the daemon.
//!
//! Patterns ported from CAMEL's datagen (research report
//! `plans/reports/research-261012-2244-camel-flows-datagen-memory.md` §3),
//! with its known defects fixed rather than copied:
//!
//! | piece | file |
//! |---|---|
//! | verdict for one candidate; only errors and timeouts are retried | [`verdict`] |
//! | cheap gates with reason codes: length, banned phrases, duplicates, ROUGE-L | [`filters`] |
//! | accept-until-N counters and the stopping rule | [`generator`] |
//! | the model seam (daemon LLM configs, usage recorded) | [`llm`] |
//! | dataset folders: `data.jsonl` + `manifest.json`, private files | [`store`] |
//! | start / status / cancel | [`jobs`] |
//! | pipelines | [`expand_seeds`], [`doc_qa`], [`reasoning_traces`], [`executable_qa`], [`export_logs`] |
//! | shared prompt pieces, budgets, how an empty run fails | [`common`] |
//!
//! Reached at `/api/datagen/*` and through the `senclaw-datagen` MCP server.
//! Guide: `docs/datagen.md`.

pub mod common;
pub mod doc_qa;
pub mod executable_qa;
pub mod expand_seeds;
pub mod export_logs;
pub mod filters;
pub mod generator;
pub mod jobs;
pub mod llm;
pub mod reasoning_traces;
pub mod store;
pub mod verdict;

pub use jobs::{registry, Deps, JobSpec, JobState, JobStatus, Pipeline};
