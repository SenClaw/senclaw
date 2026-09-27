//! PersonaUpdate tool — agent-facing SOUL.md editor.
//!
//! Whenever the user says something that should shape the agent's behaviour
//! across sessions ("from now on respond more concisely", "remember that
//! you're MyAssistant", "stop using emojis"), the agent calls this tool
//! instead of rewriting SOUL.md by hand. Benefits over the bare `Write`
//! tool:
//!
//!   * **Structured** — section + action + content; the LLM can't
//!     accidentally rewrite the whole file when only one line should change.
//!   * **Idempotent** — repeating an instruction doesn't append a duplicate.
//!   * **Preserves Learned** — the auto-managed `## Learned` block from
//!     `consolidate_to_soul` survives every edit.
//!   * **Auto-ingest** — after writing, we spawn re-ingest so the cognitive
//!     graph picks up the change in seconds (also caught by the watcher
//!     but this is faster).
//!
//! The tool's description teaches the LLM the trigger phrases ("from now
//! on", "always", "never", Vietnamese equivalents) so it learns to route
//! persona-shaping requests here, not to chat-only replies.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::Value;

use crate::config::Config;
use crate::memory::cognitive::soul_editor::{apply_patch, PatchAction, SoulPatch};
use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolResultMessage};

pub struct PersonaUpdateTool;

#[async_trait]
impl Tool for PersonaUpdateTool {
    fn name(&self) -> &str {
        "PersonaUpdate"
    }

    fn description(&self) -> &str {
        "Update your persona (SOUL.md) when the user gives a behaviour-shaping instruction. \
         Examples: \"from now on respond more concisely\" → action=add_bullet, section=Guidelines. \
         \"stop using emojis\" → action=add_bullet, section=Style, content=\"Never use emojis.\" \
         \"forget I said X\" → action=remove_bullet. \"you are now MyAssistant who specializes in Y\" \
         → action=replace_section, section=Identity. \
         Multilingual: trigger on equivalent phrases (\"từ giờ trở đi\", \"luôn luôn\", \"đừng\", \
         \"never\", \"always\", \"from now on\"). \
         Section names: Identity, Guidelines, Style, Boundaries, Memory Management — or any \
         custom H2 you propose. The Learned section is auto-managed; do NOT write to it. \
         Idempotent: re-issuing the same instruction is safe. After the edit, your cognitive \
         memory re-ingests automatically — you don't need to call CogAdd."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "section": {
                    "type": "string",
                    "description": "Target H2 section. Common: Identity, Guidelines, Style, Boundaries. Created if missing.",
                },
                "action": {
                    "type": "string",
                    "enum": ["add_bullet", "append_line", "replace_section", "remove_bullet"],
                    "description": "How to apply `content` to the section.",
                },
                "content": {
                    "type": "string",
                    "description": "The new bullet text, line, or replacement body. Phrase as a directive in second person (\"You respond concisely.\").",
                },
                "reason": {
                    "type": "string",
                    "description": "Optional: short note on what user instruction prompted this edit. Stored in the tool result but not persisted to SOUL.md.",
                }
            },
            "required": ["section", "action", "content"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let section = input
            .get("section")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing `section`"))?
            .trim();
        if section.is_empty() {
            anyhow::bail!("`section` must not be empty");
        }
        // Guard against editing the Learned block. Consolidate owns it.
        if section.eq_ignore_ascii_case("Learned") {
            anyhow::bail!(
                "Section `Learned` is auto-managed by cognitive consolidation. \
                 Pick a different section (Identity / Guidelines / Style / …)."
            );
        }

        let action_str = input
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing `action`"))?;
        let action = match action_str {
            "add_bullet" => PatchAction::AddBullet,
            "append_line" => PatchAction::AppendLine,
            "replace_section" => PatchAction::ReplaceSection,
            "remove_bullet" => PatchAction::RemoveBullet,
            other => anyhow::bail!("invalid `action`: {other}"),
        };

        let content = input
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("missing `content`"))?
            .trim()
            .to_string();
        if content.is_empty() {
            anyhow::bail!("`content` must not be empty");
        }
        let reason = input
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Resolve SOUL.md path. ctx.agent_id == the agent folder slug, per
        // the convention in `agent_pool::pool::mcp_servers.push(...)`.
        let cfg = Config::from_env();
        let soul_path = cfg.paths.agents_dir.join(ctx.agent_id).join("SOUL.md");

        // Read current — missing file is OK; editor will create the
        // section. Build a minimal stub with H1 so the file is well-formed.
        let existing = match std::fs::read_to_string(&soul_path) {
            Ok(s) => s,
            Err(_) => format!("# {}\n\n", ctx.agent_id),
        };

        let patch = SoulPatch {
            section: section.to_string(),
            action,
            content: content.clone(),
        };
        let next = apply_patch(&existing, &patch).context("apply patch")?;

        if next == existing {
            // Idempotent no-op (duplicate add, missing remove target).
            return Ok(vec![ToolOutput::Result {
                data: serde_json::json!({
                    "section": section,
                    "action": action_str,
                    "applied": false,
                    "reason": "no change (idempotent)",
                }),
                result_for_assistant: format!(
                    "Persona unchanged ({} / {}): the instruction was already in effect.",
                    section, action_str,
                ),
            }]);
        }

        // Atomic write: temp file + rename. Prevents half-written SOUL.md
        // when the editor parses while we're mid-flush.
        if let Some(parent) = soul_path.parent() {
            std::fs::create_dir_all(parent).context("ensure SOUL.md parent")?;
        }
        let tmp = soul_path.with_extension("md.tmp");
        std::fs::write(&tmp, &next).context("write tmp")?;
        std::fs::rename(&tmp, &soul_path).context("rename tmp → SOUL.md")?;

        // Best-effort: trigger immediate cognitive re-ingest so the next
        // turn's pre-retrieval already sees the new persona facts. The
        // poll-based watcher would catch this in ~30 s anyway; this just
        // shortens the gap.
        spawn_ingest(cfg.paths.agents_dir.clone(), ctx.agent_id.to_string());

        let data = serde_json::json!({
            "section": section,
            "action": action_str,
            "applied": true,
            "reason": reason,
            "path": soul_path.to_string_lossy(),
        });
        let summary = format!("SOUL.md updated · {section} · {action_str}");
        Ok(vec![ToolOutput::Result {
            data,
            result_for_assistant: summary,
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        ToolResultMessage {
            title: "Persona update".into(),
            summary: format!(
                "{} · {}",
                data.get("section").and_then(|v| v.as_str()).unwrap_or(""),
                data.get("action").and_then(|v| v.as_str()).unwrap_or(""),
            ),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let section = input.get("section").and_then(|v| v.as_str()).unwrap_or("?");
        let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("?");
        format!("Persona · {section}/{action}")
    }
}

/// Mirror of `agent_manager::spawn_soul_ingest` — kept here so tools/
/// doesn't depend on gateway/. Same fire-and-forget semantics.
fn spawn_ingest(agents_dir: std::path::PathBuf, folder: String) {
    tokio::spawn(async move {
        let Some(sys) = crate::memory::cognitive::try_get_instance() else {
            return;
        };
        match crate::memory::cognitive::ingest_soul_from_disk(&sys, &agents_dir, &folder).await {
            Ok(Some(_)) => {}
            Ok(None) => {}
            Err(e) => tracing::warn!(
                folder = %folder, error = %e,
                "[persona-update] re-ingest after SOUL edit failed"
            ),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext<'static> {
        ToolContext {
            agent_id: "test-agent",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        }
    }

    #[test]
    fn name_and_description_present() {
        let t = PersonaUpdateTool;
        assert_eq!(t.name(), "PersonaUpdate");
        assert!(t.description().len() > 200);
        assert!(t.description().contains("from now on"));
    }

    #[test]
    fn input_schema_is_object_with_required_fields() {
        let s = PersonaUpdateTool.input_schema();
        assert_eq!(s["type"], "object");
        let required: Vec<&str> = s["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        for f in ["section", "action", "content"] {
            assert!(required.contains(&f), "schema must require {f}");
        }
    }

    #[tokio::test]
    async fn rejects_invalid_action() {
        let tool = PersonaUpdateTool;
        let res = tool
            .call(
                serde_json::json!({
                    "section": "Guidelines",
                    "action": "explode",
                    "content": "boom"
                }),
                &ctx(),
            )
            .await;
        assert!(res.is_err());
        assert!(format!("{:?}", res.unwrap_err()).contains("invalid `action`"));
    }

    #[tokio::test]
    async fn rejects_learned_section() {
        let tool = PersonaUpdateTool;
        let res = tool
            .call(
                serde_json::json!({
                    "section": "Learned",
                    "action": "add_bullet",
                    "content": "you are nice"
                }),
                &ctx(),
            )
            .await;
        assert!(res.is_err());
        assert!(format!("{:?}", res.unwrap_err()).contains("auto-managed"));
    }

    #[tokio::test]
    async fn rejects_empty_content() {
        let tool = PersonaUpdateTool;
        let res = tool
            .call(
                serde_json::json!({
                    "section": "Guidelines",
                    "action": "add_bullet",
                    "content": "   "
                }),
                &ctx(),
            )
            .await;
        assert!(res.is_err());
    }
}
