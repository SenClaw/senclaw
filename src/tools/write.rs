//! Write tool — creates or overwrites a file.
//!
//! Port of TS `node_modules/sema-core/dist/tools/Write/`.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use similar::{ChangeTag, TextDiff};

use crate::util::paths::resolve_in_workspace;
use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolPermissionInfo, ToolResultMessage};

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn path_fields(&self) -> &'static [&'static str] {
        &["file_path"]
    }

    fn description(&self) -> &str {
        "Write a file to the local filesystem"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file. Absolute, or relative to the working directory."
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["file_path", "content"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolContext<'_>,
    ) -> std::result::Result<(), String> {
        let path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if path.is_empty() {
            return Err("file_path is required".to_string());
        }
        Ok(())
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");

        // A relative path used to land wherever the daemon process happened to
        // be started — the file was written, successfully, somewhere nobody
        // would look. Resolving against the working dir is the only outcome
        // that matches what the model asked for.
        let p = resolve_in_workspace(path, ctx.working_dir);

        // Ensure parent directory exists
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).context("Failed to create parent directory")?;
        }

        let old_content = std::fs::read_to_string(&p).unwrap_or_default();
        std::fs::write(&p, content).context("Failed to write file")?;

        let fname = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());
        let size = content.len();
        let diff = make_unified_diff(&old_content, content, path);
        let summary = format!("Wrote {fname} ({size} bytes)\n{diff}");

        Ok(vec![ToolOutput::Result {
            data: serde_json::json!({
                "path": path,
                "size": size,
                "diff": diff,
            }),
            result_for_assistant: summary,
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        ToolResultMessage {
            title: "Write".into(),
            summary: format!(
                "{} bytes",
                data.get("size").and_then(|v| v.as_u64()).unwrap_or(0)
            ),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("file");
        let fname = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());
        format!("Write {fname}")
    }

    fn gen_tool_permission(&self, input: &Value) -> Option<ToolPermissionInfo> {
        let title = self.get_display_title(input);
        let path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        Some(ToolPermissionInfo {
            title,
            content: serde_json::json!({
                "path": path,
            }),
        })
    }
}

fn make_unified_diff(old: &str, new: &str, file_path: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = format!("--- a/{file_path}\n+++ b/{file_path}\n");
    for group in diff.grouped_ops(3) {
        for op in &group {
            for change in diff.iter_changes(op) {
                let prefix = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                out.push_str(prefix);
                out.push_str(change.value());
                if !change.value().ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(working_dir: &'a str) -> ToolContext<'a> {
        ToolContext {
            agent_id: "t",
            working_dir,
            agent_data_dir: working_dir,
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: crate::zen_core::EditFormat::default(),
        }
    }

    fn text(outs: &[ToolOutput]) -> String {
        match &outs[0] {
            ToolOutput::Result { result_for_assistant, .. } => result_for_assistant.clone(),
            _ => panic!("unexpected output"),
        }
    }

    #[tokio::test]
    async fn a_relative_path_writes_inside_the_working_dir() {
        let d = tempfile::tempdir().unwrap();
        let wd = d.path().to_string_lossy().to_string();
        let input = serde_json::json!({"file_path": "notes/today.md", "content": "hi\n"});
        WriteTool.call(input, &ctx(&wd)).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("notes/today.md")).unwrap(),
            "hi\n"
        );
        // The read above is the discriminating assertion: with the fix
        // reverted the write lands in the daemon's own cwd instead (the app
        // bundle on a desktop install) and this file never appears. A cwd
        // canary was tried here and removed — in the failing case it creates
        // `notes/today.md` in the repo, which then keeps the test red on
        // every later run.
    }

    #[tokio::test]
    async fn an_absolute_path_writes_exactly_there() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("out.txt");
        let input = serde_json::json!({"file_path": f.to_string_lossy(), "content": "x\n"});
        WriteTool.call(input, &ctx("/somewhere/else")).await.unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "x\n");
    }
}
