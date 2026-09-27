//! Read tool — reads file contents with offset and limit support.
//!
//! Port of TS `node_modules/sema-core/dist/tools/Read/`.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;

use crate::util::paths::resolve_in_workspace;
use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolResultMessage};

const MAX_LINE_LENGTH: usize = 2000;
const MAX_READ_BYTES: u64 = 512 * 1024; // 512 KB

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn path_fields(&self) -> &'static [&'static str] {
        &["file_path"]
    }

    fn description(&self) -> &str {
        "Read a file from the local filesystem"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file. Absolute, or relative to the working directory."
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (1-indexed)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Number of lines to read"
                }
            },
            "required": ["file_path"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> std::result::Result<(), String> {
        let path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if path.is_empty() {
            return Err("file_path is required".to_string());
        }
        let p = resolve_in_workspace(path, ctx.working_dir);
        // The resolved path, not the string the model sent: "File not found:
        // app.py" hides where we actually looked, which is the whole problem
        // when the answer is "in the wrong directory".
        if !p.exists() {
            return Err(format!("File not found: {}", p.display()));
        }
        if !p.is_file() {
            return Err(format!("Not a file: {}", p.display()));
        }
        Ok(())
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let raw_path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let path = resolve_in_workspace(raw_path, ctx.working_dir);
        let offset = input
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1);
        let limit = input.get("limit").and_then(|v| v.as_u64());

        let metadata = std::fs::metadata(&path).context("Failed to stat file")?;
        if metadata.len() > MAX_READ_BYTES {
            return Ok(vec![ToolOutput::Result {
                data: serde_json::json!({
                    "content": "",
                    "lines": 0,
                    "totalLines": 0,
                    "tooLarge": true,
                    "sizeKb": metadata.len() / 1024,
                }),
                result_for_assistant: format!(
                    "[File too large: {} KB — max {} KB. Use offset+limit to read specific sections, \
                     or use grep/search to find relevant parts first.]",
                    metadata.len() / 1024,
                    MAX_READ_BYTES / 1024,
                ),
            }]);
        }

        let content = std::fs::read_to_string(&path).context("Failed to read file")?;

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len() as u64;

        let start = (offset - 1) as usize;
        let end = limit.map_or(lines.len(), |l| (start + l as usize).min(lines.len()));

        if start >= lines.len() {
            return Ok(vec![ToolOutput::Result {
                data: serde_json::json!({
                    "content": "",
                    "lines": 0,
                    "totalLines": total_lines,
                }),
                result_for_assistant: format!(
                    "File has {total_lines} lines. Offset {offset} exceeds file length."
                ),
            }]);
        }

        let selected: Vec<&str> = lines[start..end].to_vec();
        let output = selected
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let line_no = start + i + 1;
                let body = if l.len() > MAX_LINE_LENGTH {
                    format!("{}... [line truncated]", &l[..MAX_LINE_LENGTH])
                } else {
                    l.to_string()
                };
                format!("{line_no:>4}\t{body}")
            })
            .collect::<Vec<_>>()
            .join("\n");

        Ok(vec![ToolOutput::Result {
            data: serde_json::json!({
                "content": output,
                "lines": selected.len(),
                "totalLines": total_lines,
            }),
            result_for_assistant: output,
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        ToolResultMessage {
            title: "Read".into(),
            summary: format!(
                "{} lines",
                data.get("lines").and_then(|v| v.as_u64()).unwrap_or(0)
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
        format!("Read {fname}")
    }
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
    async fn a_relative_path_reads_the_file_in_the_working_dir() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("app.py"), "line one\n").unwrap();
        let wd = d.path().to_string_lossy().to_string();
        let input = serde_json::json!({"file_path": "app.py"});
        // Validation is what refused this before `call` was ever reached.
        ReadTool.validate_input(&input, &ctx(&wd)).await.unwrap();
        let outs = ReadTool.call(input, &ctx(&wd)).await.unwrap();
        assert!(text(&outs).contains("line one"), "{}", text(&outs));
    }

    #[tokio::test]
    async fn a_missing_file_names_the_path_we_actually_looked_at() {
        let d = tempfile::tempdir().unwrap();
        let wd = d.path().to_string_lossy().to_string();
        let err = ReadTool
            .validate_input(&serde_json::json!({"file_path": "nope.py"}), &ctx(&wd))
            .await
            .unwrap_err();
        // "File not found: nope.py" hid the directory — which was the whole
        // problem, since the directory was the wrong one.
        assert!(err.contains(&wd), "{err}");
    }

    #[tokio::test]
    async fn an_absolute_path_is_still_read_as_given() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("abs.txt");
        std::fs::write(&f, "here\n").unwrap();
        let input = serde_json::json!({"file_path": f.to_string_lossy()});
        let outs = ReadTool.call(input, &ctx("/somewhere/else")).await.unwrap();
        assert!(text(&outs).contains("here"));
    }
}
