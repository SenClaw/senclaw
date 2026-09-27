//! History → curated-memory consolidation.
//!
//! When compaction drops conversation history (replacing it with an LLM
//! summary), this module distills that summary into curated memory files
//! (see `curated.rs` / docs/curated-memory-design.md) so durable facts
//! survive across sessions — Claude-Code-style auto-memory.
//!
//! Two paths:
//! - **LLM distill** — a cognitive LLM extracts 0–3 structured memories
//!   (`{name, description, type, body}`) from the summary; each is saved via
//!   `curated::save` with `supersede=true` (re-emitting a known slug updates it).
//! - **Verbatim fallback** — no LLM configured (or the call/parse failed):
//!   the summary itself is saved as `conversation-summary-YYYY-MM-DD`, so the
//!   dropped history is still recallable via FTS. Same-day compactions update
//!   the same file.

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::memory::cognitive::llm::LlmClient;
use crate::memory::curated;

/// System prompt for the distill call. Mirrors the lifecycle rules from the
/// curated-memory design: durable facts only, nothing derivable or transient.
const DISTILL_SYSTEM: &str = r#"You distill a conversation summary into durable memories for an AI agent's long-term store.

Extract at most 3 memories worth keeping across future conversations: decisions made, user preferences, project facts, gotchas, unresolved follow-ups. Do NOT extract transient chit-chat, one-off task mechanics, or anything trivially re-derivable.

Return ONLY JSON, no prose:
{"memories":[{"name":"<kebab-case-slug>","description":"<one-line recall hook, <=120 chars>","type":"<project|reference|feedback|user>","body":"<markdown body; for project/feedback include **Why:** and **How to apply:** lines>"}]}

If nothing is worth keeping, return {"memories":[]}."#;

#[derive(Debug, Deserialize)]
struct RawMemory {
    name: String,
    description: String,
    #[serde(default)]
    #[serde(rename = "type")]
    mem_type: String,
    body: String,
}

#[derive(Debug, Deserialize)]
struct MemoryEnvelope {
    memories: Vec<RawMemory>,
}

/// Extract the JSON object from raw LLM output (strips ``` fences / preamble).
fn extract_json(raw: &str) -> Result<&str> {
    let trimmed = raw.trim();
    let start = trimmed.find('{');
    let end = trimmed.rfind('}');
    match (start, end) {
        (Some(s), Some(e)) if e > s => Ok(&trimmed[s..=e]),
        _ => bail!("no JSON object found in LLM output"),
    }
}

fn parse_memories(raw: &str) -> Result<Vec<RawMemory>> {
    let json = extract_json(raw)?;
    let envelope: MemoryEnvelope =
        serde_json::from_str(json).context("parse consolidation JSON envelope")?;
    Ok(envelope.memories)
}

/// Consolidate a compaction summary into curated memory files under `base`
/// (the agent dir holding `MEMORY.md` + `memory/`). Returns the number of
/// memory files written. Never fails the caller's turn — callers should log
/// and swallow errors.
pub async fn consolidate_summary(
    base: &Path,
    folder: &str,
    summary: &str,
    llm: Option<Arc<dyn LlmClient>>,
    date: &str,
) -> Result<usize> {
    let summary = summary.trim();
    if summary.is_empty() {
        return Ok(0);
    }

    // Path 1: LLM distill.
    if let Some(llm) = llm {
        match llm.complete(DISTILL_SYSTEM, summary).await {
            Ok(raw) => match parse_memories(&raw) {
                Ok(memories) => {
                    let mut saved = 0usize;
                    for m in memories.iter().take(3) {
                        let mem_type = if curated::MEMORY_TYPES.contains(&m.mem_type.as_str()) {
                            m.mem_type.as_str()
                        } else {
                            "project"
                        };
                        match curated::save(
                            base,
                            &m.name,
                            &m.description,
                            &m.body,
                            mem_type,
                            None,
                            folder,
                            date,
                            true, // supersede: re-emitted slugs update in place
                        ) {
                            Ok(s) => {
                                saved += 1;
                                tracing::info!(
                                    "[consolidate] {} memory '{}' from compaction summary",
                                    if s.updated { "updated" } else { "saved" },
                                    s.name
                                );
                            }
                            Err(e) => {
                                tracing::warn!("[consolidate] save '{}' failed: {e}", m.name)
                            }
                        }
                    }
                    // Distill succeeded (even with 0 memories — an explicit
                    // "nothing durable" verdict). Do not fall back.
                    return Ok(saved);
                }
                Err(e) => {
                    tracing::warn!("[consolidate] LLM output unparsable, falling back: {e}")
                }
            },
            Err(e) => tracing::warn!("[consolidate] LLM distill failed, falling back: {e}"),
        }
    }

    // Path 2: verbatim fallback — keep the dropped history recallable via
    // FTS, but do NOT pollute the curated MEMORY.md index. Write the file
    // directly into `memory/` so MemoryManager indexes it, but skip
    // `curated::save` (which would push a noisy index entry every compaction).
    let slug = format!("conversation-summary-{date}");
    let dir = base.join("memory");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{slug}.md"));

    let mut content = String::new();
    content.push_str("---\n");
    content.push_str(&format!("name: {slug}\n"));
    content.push_str(&format!(
        "description: Auto-saved conversation summary from compaction on {date}\n"
    ));
    content.push_str("metadata:\n");
    content.push_str("  node_type: conversation_summary\n");
    content.push_str(&format!("  originSessionId: {folder}\n"));
    content.push_str(&format!("  createdAt: {date}\n"));
    content.push_str("---\n\n");
    content.push_str(summary);
    content.push('\n');
    std::fs::write(&path, content)?;
    tracing::info!(
        "[consolidate] saved verbatim summary '{slug}' (FTS-only, not indexed in MEMORY.md)"
    );
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubLlm(String);

    #[async_trait::async_trait]
    impl LlmClient for StubLlm {
        async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
            Ok(self.0.clone())
        }
    }

    struct FailLlm;

    #[async_trait::async_trait]
    impl LlmClient for FailLlm {
        async fn complete(&self, _system: &str, _user: &str) -> Result<String> {
            bail!("boom")
        }
    }

    fn tmp_base(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("consolidate-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn parse_fenced_and_preamble() {
        let raw = "Sure!\n```json\n{\"memories\":[{\"name\":\"a-b\",\"description\":\"d\",\"type\":\"project\",\"body\":\"x\"}]}\n```";
        let m = parse_memories(raw).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, "a-b");

        let empty = parse_memories("{\"memories\":[]}").unwrap();
        assert!(empty.is_empty());

        assert!(parse_memories("no json here").is_err());
    }

    #[tokio::test]
    async fn distill_saves_memories() {
        let base = tmp_base("distill");
        let llm = StubLlm(
            r#"{"memories":[{"name":"user-prefers-vi","description":"User prefers Vietnamese replies","type":"user","body":"**Why:** asked twice.\n**How to apply:** reply in Vietnamese."}]}"#
                .into(),
        );
        let n = consolidate_summary(
            &base,
            "g1",
            "long summary text",
            Some(Arc::new(llm)),
            "2026-07-03",
        )
        .await
        .unwrap();
        assert_eq!(n, 1);
        let file = std::fs::read_to_string(base.join("memory/user-prefers-vi.md")).unwrap();
        assert!(file.contains("type: user"));
        assert!(std::fs::read_to_string(base.join("MEMORY.md"))
            .unwrap()
            .contains("user-prefers-vi.md"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn distill_empty_verdict_saves_nothing() {
        let base = tmp_base("empty");
        let llm = StubLlm(r#"{"memories":[]}"#.into());
        let n = consolidate_summary(&base, "g1", "summary", Some(Arc::new(llm)), "2026-07-03")
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert!(!base.join("MEMORY.md").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn llm_failure_falls_back_to_verbatim_without_index() {
        let base = tmp_base("fallback");
        let n = consolidate_summary(
            &base,
            "g1",
            "the summary",
            Some(Arc::new(FailLlm)),
            "2026-07-03",
        )
        .await
        .unwrap();
        assert_eq!(n, 1);
        let file = std::fs::read_to_string(base.join("memory/conversation-summary-2026-07-03.md"))
            .unwrap();
        assert!(file.contains("the summary"));
        assert!(file.contains("node_type: conversation_summary"));
        // Verbatim fallback must NOT create/pollute MEMORY.md.
        assert!(!base.join("MEMORY.md").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn no_llm_saves_verbatim_without_index() {
        let base = tmp_base("nollm");
        let n = consolidate_summary(&base, "g1", "raw summary", None, "2026-07-03")
            .await
            .unwrap();
        assert_eq!(n, 1);
        // File exists but MEMORY.md is not created.
        assert!(base
            .join("memory/conversation-summary-2026-07-03.md")
            .exists());
        assert!(!base.join("MEMORY.md").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn empty_summary_is_noop() {
        let base = tmp_base("noop");
        let n = consolidate_summary(&base, "g1", "  ", None, "2026-07-03")
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert!(!base.exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
