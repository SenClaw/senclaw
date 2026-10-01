//! Ingest each agent's `SOUL.md` persona file into cognitive memory.
//!
//! SOUL.md is the agent's identity file at `~/senclaw/profiles/<folder>/SOUL.md`.
//! It already drives the agent's system prompt via the legacy core_prompt
//! field, but those facts live only in the prompt — they're not in the
//! cognitive graph, so:
//!
//!   * The agent can't *recall* its own persona via CogRecall.
//!   * Hebbian dynamics never apply to identity facts (frequent self-
//!     reference doesn't strengthen them).
//!   * Cross-session continuity is one-way: SOUL.md → prompt, never
//!     graph → SOUL.md.
//!
//! This module fixes the first two by parsing SOUL.md into sections and
//! cognify-ing each one under `NodeSet::persona(folder, …)` tags. The
//! third (graph → SOUL.md) is left as a separate "consolidate persona"
//! pass for later.
//!
//! ## Tagging scheme
//!
//! Every node produced from `SOUL.md` carries two NodeSets:
//!
//! 1. `Persona(folder, "soul")` — broad bucket, covers ALL persona facts.
//! 2. `Persona(folder, "soul:<section-slug>")` — fine-grained per H2
//!    section (e.g. `soul:identity`, `soul:guidelines`).
//!
//! Retrieval can query either: `Persona(_, "soul")` for "anything about
//! who I am", or `Persona(_, "soul:guidelines")` for behaviour rules only.

use anyhow::{Context, Result};
use std::sync::Arc;

use super::{CognifyOptions, CognifyReport, CognitiveSystem, NodeSet};

/// Parse a SOUL.md document into `(section_label, body)` pairs.
///
/// We use a simple H1/H2 split. Anything before the first `## ` header
/// (commonly the title `# Name` and a short intro paragraph) becomes one
/// implicit `"intro"` section so it's never dropped. Empty bodies are
/// elided.
///
/// The "label" is a slugified header (`Memory Management` → `memory-management`)
/// suitable for use as a NodeSet tag.
pub fn split_soul_sections(text: &str) -> Vec<(String, String)> {
    let mut sections: Vec<(String, String)> = Vec::new();
    let mut current_label: String = "intro".to_string();
    let mut current_body: Vec<&str> = Vec::new();

    let flush = |label: &str, body: &[&str], out: &mut Vec<(String, String)>| {
        let joined = body.join("\n").trim().to_string();
        if !joined.is_empty() {
            out.push((label.to_string(), joined));
        }
    };

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            // Boundary: flush the previous section, start a new one.
            flush(&current_label, &current_body, &mut sections);
            current_body.clear();
            current_label = slugify(rest.trim());
        } else if line.starts_with("# ") {
            // H1 = document title. Skip the line itself but keep the
            // following text under "intro" until the first H2.
            continue;
        } else {
            current_body.push(line);
        }
    }
    flush(&current_label, &current_body, &mut sections);
    sections
}

fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else if c.is_whitespace() {
                '-'
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Cognify every section of `soul_text` under persona-scoped NodeSets.
///
/// Returns a summed `CognifyReport` across all sections so the caller can
/// tell whether anything was actually extracted (e.g. when the LLM is
/// down, `llm_skipped` will latch true).
///
/// **Importance**: persona facts use `importance = 1.0` so the first
/// strengthen() call lifts edges close to the L2-promotion threshold —
/// identity facts shouldn't have to be repeated 10 times to stick.
pub async fn ingest_soul(
    sys: &CognitiveSystem,
    agent_folder: &str,
    soul_text: &str,
) -> Result<CognifyReport> {
    let sections = split_soul_sections(soul_text);
    let mut total = CognifyReport::default();
    for (label, body) in &sections {
        let opts = CognifyOptions {
            node_sets: vec![
                NodeSet::persona(agent_folder, "soul"),
                NodeSet::persona(agent_folder, format!("soul:{label}")),
            ],
            importance: 1.0,
            ..Default::default()
        };
        let report = sys
            .cognify(body, &format!("soul:{agent_folder}:{label}"), &opts)
            .await?;
        total.chunks_added += report.chunks_added;
        total.chunks_deduped += report.chunks_deduped;
        total.entities_added += report.entities_added;
        total.entities_reused += report.entities_reused;
        total.edges_added += report.edges_added;
        total.edges_strengthened += report.edges_strengthened;
        if report.llm_skipped {
            total.llm_skipped = true;
        }
    }
    Ok(total)
}

/// Convenience: read SOUL.md from disk and ingest. Returns Ok(None) when
/// the file is missing (an agent that never had a persona file written).
pub async fn ingest_soul_from_disk(
    sys: &CognitiveSystem,
    profiles_dir: &std::path::Path,
    agent_folder: &str,
) -> Result<Option<CognifyReport>> {
    let path = profiles_dir.join(agent_folder).join("SOUL.md");
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    let report = ingest_soul(sys, agent_folder, &text).await?;
    Ok(Some(report))
}

// =====================================================================
// Bidirectional: cognitive graph → SOUL.md ## Learned
// =====================================================================

/// Marker comment used to delimit the auto-generated `## Learned` section
/// in SOUL.md. Anything outside these markers is preserved verbatim so the
/// user's hand-written persona never gets clobbered.
const LEARNED_BEGIN: &str = "<!-- senclaw:learned:start -->";
const LEARNED_END: &str = "<!-- senclaw:learned:end -->";

/// Pull the strongest persona-tagged edges and write them back into the
/// agent's SOUL.md as a `## Learned` section.
///
/// Why: cognify accumulates facts via Hebbian dynamics over time. Edges
/// that frequently get traversed (CogRecall write-back) hit LTP states
/// and earn high strength — those represent *durable* knowledge the
/// agent should carry across sessions. Persisting them into SOUL.md
/// closes the loop: next boot, `ingest_all_souls` will re-read SOUL.md
/// and the Learned facts will refresh their edges, keeping them alive
/// against decay.
///
/// Idempotent: only the content between the LEARNED markers is replaced.
/// Existing hand-written sections (Identity, Guidelines, etc.) are
/// preserved.
pub async fn consolidate_to_soul(
    graph: &dyn super::GraphStore,
    profiles_dir: &std::path::Path,
    agent_folder: &str,
    min_strength: f32,
) -> Result<ConsolidateReport> {
    use super::NodeSet;
    let set = NodeSet::persona(agent_folder, "soul");
    // require_ltp=true → only edges that survived at least one LTP step,
    // i.e. actually used. Filters out raw cognify noise.
    let edges = graph.edges_from_set(&set, min_strength, /* require_ltp */ true, 200)?;

    if edges.is_empty() {
        return Ok(ConsolidateReport {
            edges_consolidated: 0,
            file_updated: false,
        });
    }

    // Group by subject so the bullet list reads as "I X, I Y" instead of
    // a flat dump. Predicate becomes the verb; object the value.
    use std::collections::BTreeMap;
    let mut by_subject: BTreeMap<String, Vec<(String, String, f32)>> = BTreeMap::new();
    for (edge, src, dst) in edges {
        let subj = if !src.name.is_empty() {
            src.name.clone()
        } else if !src.summary.is_empty() {
            src.summary.chars().take(40).collect()
        } else {
            continue;
        };
        let obj = if !dst.name.is_empty() {
            dst.name.clone()
        } else if !dst.summary.is_empty() {
            dst.summary.chars().take(80).collect()
        } else {
            continue;
        };
        by_subject.entry(subj).or_default().push((
            edge.predicate.replace('_', " "),
            obj,
            edge.strength,
        ));
    }

    let n_edges: usize = by_subject.values().map(|v| v.len()).sum();
    let mut body = String::from("## Learned\n\n");
    body.push_str(LEARNED_BEGIN);
    body.push('\n');
    body.push_str("*Auto-consolidated from cognitive memory — edges that hit LTP and survived decay. Edit freely; this block is rewritten on each consolidation pass.*\n\n");
    for (subj, facts) in &by_subject {
        body.push_str(&format!("**{subj}**\n"));
        for (pred, obj, strength) in facts {
            body.push_str(&format!("- {pred}: {obj}  _(strength {strength:.2})_\n"));
        }
        body.push('\n');
    }
    body.push_str(LEARNED_END);
    body.push('\n');

    let path = profiles_dir.join(agent_folder).join("SOUL.md");
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    let next = splice_learned_section(&current, &body);
    if next != current {
        std::fs::write(&path, next).context("write SOUL.md")?;
        Ok(ConsolidateReport {
            edges_consolidated: n_edges,
            file_updated: true,
        })
    } else {
        Ok(ConsolidateReport {
            edges_consolidated: n_edges,
            file_updated: false,
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConsolidateReport {
    pub edges_consolidated: usize,
    pub file_updated: bool,
}

/// Replace the existing managed section between LEARNED_BEGIN/END markers,
/// or append a fresh section at the end of the file. Preserves all other
/// content byte-for-byte.
pub(crate) fn splice_learned_section(existing: &str, new_section: &str) -> String {
    if let (Some(start), Some(end)) = (existing.find(LEARNED_BEGIN), existing.find(LEARNED_END)) {
        if end > start {
            // Replace the whole `## Learned` block that contains the
            // markers. Walk backwards to the nearest `## Learned` header
            // so we also strip the heading the previous write produced.
            let before_marker = &existing[..start];
            let block_start = before_marker.rfind("## Learned").unwrap_or(start);
            let after_end_marker = end + LEARNED_END.len();
            // Skip the trailing newline so we don't accumulate blank
            // lines across consolidations.
            let after = existing[after_end_marker..].trim_start_matches('\n');
            let mut out = String::with_capacity(existing.len() + new_section.len());
            out.push_str(&existing[..block_start]);
            out.push_str(new_section);
            out.push('\n');
            out.push_str(after);
            return out;
        }
    }
    // No existing managed section — append at the end with a separator
    // newline so the new block starts on its own line.
    let mut out = existing.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(new_section);
    out.push('\n');
    out
}

// =====================================================================
// Filesystem watcher (poll-based)
// =====================================================================
//
// Why poll instead of inotify/fsevents: we'd need the `notify` crate for
// portable native events, and that brings ~6 transitive deps for a single
// watch path per agent. A 30-second mtime poll is cheap, easy to reason
// about, and works identically on macOS / Linux / Windows.

/// Spawn a background loop that watches every agent's SOUL.md and re-ingests
/// any that changed since the last sweep. Mtime-based — the first sweep
/// just records baselines and triggers nothing. Subsequent sweeps compare.
///
/// Drop the returned `JoinHandle` (or `abort()`) to stop. Designed to be
/// called from `run_daemon` right after `ingest_all_souls`.
pub fn spawn_soul_watcher(
    sys: Arc<super::CognitiveSystem>,
    profiles_dir: std::path::PathBuf,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        use std::collections::HashMap;
        let mut mtimes: HashMap<std::path::PathBuf, std::time::SystemTime> = HashMap::new();
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Burn the immediate tick so we don't double-ingest right after
        // run_daemon's startup ingest_all_souls.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let entries = match std::fs::read_dir(&profiles_dir) {
                Ok(e) => e,
                Err(_) => continue, // dir disappeared / not yet created — retry next tick
            };
            for entry in entries.flatten() {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let folder_name = match entry.file_name().to_str() {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                let path = entry.path().join("SOUL.md");
                let mt = match std::fs::metadata(&path).and_then(|m| m.modified()) {
                    Ok(t) => t,
                    Err(_) => continue, // no SOUL.md — skip silently
                };
                let prev = mtimes.get(&path).copied();
                mtimes.insert(path.clone(), mt);
                if prev.is_none() {
                    // First time seeing this file — record baseline only.
                    continue;
                }
                if prev == Some(mt) {
                    continue;
                }
                // Changed since last sweep — re-ingest.
                tracing::info!(folder = %folder_name, "[soul-watcher] SOUL.md changed; re-ingesting");
                if let Err(e) = ingest_soul_from_disk(&sys, &profiles_dir, &folder_name).await {
                    tracing::warn!(folder = %folder_name, error = %e, "[soul-watcher] ingest failed");
                }
            }
        }
    })
}

/// Boot-time scan: ingest SOUL.md for every agent directory.
///
/// Fire-and-forget — failures per-agent log a warning and don't abort the
/// rest of the sweep. Designed to be called from `run_daemon` right after
/// `cognitive::init_daemon` succeeds.
pub async fn ingest_all_souls(sys: Arc<CognitiveSystem>, profiles_dir: std::path::PathBuf) {
    let entries = match std::fs::read_dir(&profiles_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                error = %e,
                dir = %profiles_dir.display(),
                "[soul] could not scan agents dir; skipping SOUL.md ingestion"
            );
            return;
        }
    };
    let mut count = 0usize;
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let folder_name = match entry.file_name().to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };
        match ingest_soul_from_disk(&sys, &profiles_dir, &folder_name).await {
            Ok(Some(r)) => {
                count += 1;
                tracing::info!(
                    folder = %folder_name,
                    chunks_added = r.chunks_added,
                    entities_added = r.entities_added,
                    edges_added = r.edges_added,
                    llm_skipped = r.llm_skipped,
                    "[soul] ingested SOUL.md"
                );
            }
            Ok(None) => { /* no SOUL.md, skip silently */ }
            Err(e) => {
                tracing::warn!(folder = %folder_name, error = %e, "[soul] ingest failed");
            }
        }
    }
    tracing::info!(count, "[soul] boot ingest complete");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Identity"), "identity");
        assert_eq!(slugify("Memory Management"), "memory-management");
        assert_eq!(slugify("Working  Directory"), "working-directory");
        assert_eq!(slugify("Q&A!?"), "q-a");
    }

    #[test]
    fn split_sections_handles_default_template() {
        // Mirrors what `default_soul_md` writes — verify the parser
        // recovers each H2 plus the intro paragraph.
        let text = "# main\n\nYou are a helpful AI assistant.\n\n\
                    ## Identity\n\nYour agent ID is `main`.\n\n\
                    ## Guidelines\n\n- Be helpful\n- Be concise\n\n\
                    ## Memory Management\n\nUpdate MEMORY.md.\n";
        let sections = split_soul_sections(text);
        let labels: Vec<&str> = sections.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            vec!["intro", "identity", "guidelines", "memory-management"]
        );
        assert!(sections[0].1.contains("helpful AI assistant"));
        assert!(sections[2].1.contains("Be concise"));
    }

    #[test]
    fn split_sections_skips_empty_bodies() {
        // Two H2s back-to-back → the first must not appear as an empty
        // section in the output.
        let text = "## Empty\n## Real\n\nbody";
        let sections = split_soul_sections(text);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].0, "real");
    }

    #[test]
    fn split_sections_no_headers_yields_one_intro() {
        let sections = split_soul_sections("Just one paragraph of persona text.");
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].0, "intro");
    }

    // ----- Full ingest path (cognify-backed) ----------------------------

    use crate::config::Config;
    use crate::db::Db;
    use crate::memory::cognitive::llm::test_support::StubLlm;
    use crate::memory::cognitive::{CognitiveSystem, LlmClient};
    use crate::memory::embedding::EmbeddingProvider;
    use async_trait::async_trait;

    struct FakeEmbedder;
    #[async_trait]
    impl EmbeddingProvider for FakeEmbedder {
        fn name(&self) -> &str {
            "fake"
        }
        fn model(&self) -> &str {
            "fake-model"
        }
        fn dimensions(&self) -> u32 {
            8
        }
        async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0.0f32; 8];
                    for (i, b) in t.bytes().enumerate() {
                        v[i % 8] += b as f32;
                    }
                    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
                    v.iter().map(|x| x / n).collect()
                })
                .collect())
        }
    }

    fn build_system(replies: Vec<String>) -> CognitiveSystem {
        let cfg = Config::from_env();
        let db = std::sync::Arc::new(Db::open_in_memory(&cfg).unwrap());
        let provider: std::sync::Arc<dyn EmbeddingProvider> = std::sync::Arc::new(FakeEmbedder);
        let llm: std::sync::Arc<dyn LlmClient> = std::sync::Arc::new(StubLlm::new(replies));
        CognitiveSystem::with_sqlite(db, provider, llm)
    }

    #[tokio::test]
    async fn ingest_soul_tags_nodes_under_persona_scope() {
        // Two H2 sections → two cognify calls → StubLlm needs two replies.
        let r1 =
            r#"{"triplets":[{"subject":"agent","predicate":"is","object":"helpful"}]}"#.to_string();
        let r2 = r#"{"triplets":[{"subject":"agent","predicate":"updates","object":"MEMORY.md"}]}"#
            .to_string();
        let sys = build_system(vec![r1, r2]);

        let soul = "# main\n\n## Identity\n\nYou are helpful.\n\n## Memory\n\nUpdate MEMORY.md.\n";
        let report = ingest_soul(&sys, "main", soul).await.unwrap();
        assert!(
            report.entities_added > 0,
            "should have created entity nodes"
        );

        // The persona NodeSets should be wired: lookup by "soul" tag returns
        // every node tagged from this ingest.
        let nodes = sys
            .graph
            .nodes_in_set(&NodeSet::persona("main", "soul"), 100)
            .unwrap();
        assert!(
            !nodes.is_empty(),
            "persona 'soul' tag must catch ingested nodes"
        );
    }

    #[test]
    fn splice_appends_when_no_markers_present() {
        let existing = "# Agent\n\nIntro paragraph.\n";
        let new_block =
            "## Learned\n<!-- senclaw:learned:start -->\nbody\n<!-- senclaw:learned:end -->\n";
        let out = splice_learned_section(existing, new_block);
        assert!(out.starts_with("# Agent"), "user content preserved at top");
        assert!(out.contains("## Learned"));
        assert!(out.contains("body"));
    }

    #[test]
    fn splice_replaces_existing_managed_block() {
        let block_v1 =
            "## Learned\n<!-- senclaw:learned:start -->\nold\n<!-- senclaw:learned:end -->\n";
        let block_v2 =
            "## Learned\n<!-- senclaw:learned:start -->\nnew\n<!-- senclaw:learned:end -->\n";
        let v1 = format!("# Agent\n\nIntro.\n\n{block_v1}");
        let v2 = splice_learned_section(&v1, block_v2);
        // Old body gone, new body present, intro preserved.
        assert!(!v2.contains("old"), "old learned content must be replaced");
        assert!(v2.contains("new"));
        assert!(v2.starts_with("# Agent"));
        // Idempotency: spliceing the same v2 again is a no-op (modulo
        // whitespace).
        let v3 = splice_learned_section(&v2, block_v2);
        assert_eq!(v2, v3, "second splice should produce identical output");
    }

    #[tokio::test]
    async fn ingest_soul_is_idempotent_via_content_hash() {
        // After P-cognify-skip (the "câu đã extract rồi, không extract
        // lại" change), re-ingesting an unchanged SOUL section short-
        // circuits the LLM entirely once the chunk has edges. So the
        // contract is now: 2nd pass = pure no-op modulo `chunks_deduped`.
        // Strengthening still happens, but on the *recall* side via
        // SpreadingActivation — not via wasted re-extraction.
        let r =
            r#"{"triplets":[{"subject":"agent","predicate":"is","object":"helpful"}]}"#.to_string();
        // Only the first reply should ever be consumed. The extras are a
        // canary — if the LLM gets called a second time something regressed.
        let sys = build_system(vec![r.clone(), r.clone(), r.clone(), r]);

        let soul = "## Identity\n\nYou are helpful.\n";
        let first = ingest_soul(&sys, "main", soul).await.unwrap();
        let second = ingest_soul(&sys, "main", soul).await.unwrap();

        assert!(first.chunks_added >= 1);
        assert_eq!(second.chunks_added, 0, "re-ingest must dedupe chunks");
        // Re-ingest with existing edges = no LLM, no edge churn.
        assert_eq!(second.edges_added, 0, "no new edges on identical content");
        assert_eq!(
            second.edges_strengthened, 0,
            "skip gate prevents extraction"
        );
        assert!(!second.llm_skipped);
    }
}
