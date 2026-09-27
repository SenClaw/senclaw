//! NotebookEdit tool — Jupyter notebook cell editing.
//!
//! Port of TS `node_modules/sema-core/dist/tools/NotebookEdit/`.


use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::Value;

use crate::util::paths::resolve_in_workspace;
use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolPermissionInfo, ToolResultMessage};

pub struct NotebookEditTool;

#[async_trait]
impl Tool for NotebookEditTool {
    fn name(&self) -> &str {
        "NotebookEdit"
    }

    fn path_fields(&self) -> &'static [&'static str] {
        &["notebook_path"]
    }

    fn description(&self) -> &str {
        "Edit Jupyter notebook (.ipynb) cells — replace, insert, or delete"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "notebook_path": {
                    "type": "string",
                    "description": "Path to the .ipynb file. Absolute, or relative to the working directory."
                },
                "cell_number": {
                    "type": "number",
                    "description": "The 0-based index of the cell to edit"
                },
                "new_source": {
                    "type": "string",
                    "description": "The new source for the cell"
                },
                "cell_type": {
                    "type": "string",
                    "enum": ["code", "markdown"],
                    "description": "Cell type. Required for insert mode; defaults to current type otherwise."
                },
                "edit_mode": {
                    "type": "string",
                    "enum": ["replace", "insert", "delete"],
                    "description": "Edit mode (default: replace)"
                }
            },
            "required": ["notebook_path", "cell_number", "new_source"]
        })
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn validate_input(
        &self,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> std::result::Result<(), String> {
        let path = input
            .get("notebook_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if path.is_empty() {
            return Err("notebook_path is required".to_string());
        }
        let p = resolve_in_workspace(path, ctx.working_dir);
        if !p.exists() {
            return Err(format!("File not found: {path}"));
        }
        if p.extension().map(|e| e != "ipynb").unwrap_or(true) {
            return Err("Only .ipynb files are supported".to_string());
        }
        let cell_number = input
            .get("cell_number")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        if cell_number < 0 {
            return Err("Cell number must be non-negative".to_string());
        }
        let edit_mode = input
            .get("edit_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("replace");
        if edit_mode == "insert" && input.get("cell_type").is_none() {
            return Err("Must include cell_type when using edit_mode=insert".to_string());
        }
        Ok(())
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let notebook_path = input
            .get("notebook_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let cell_number = input
            .get("cell_number")
            .and_then(|v| v.as_i64())
            .unwrap_or(0) as usize;
        let new_source = input
            .get("new_source")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let cell_type = input.get("cell_type").and_then(|v| v.as_str());
        let edit_mode = input
            .get("edit_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("replace");

        let p = resolve_in_workspace(notebook_path, ctx.working_dir);

        let content = std::fs::read_to_string(&p).context("Failed to read notebook")?;
        let mut notebook: Value =
            serde_json::from_str(&content).context("Notebook is not valid JSON")?;

        let language = notebook
            .get("metadata")
            .and_then(|m| m.get("language_info"))
            .and_then(|l| l.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("python")
            .to_string();

        let cells = notebook
            .get_mut("cells")
            .and_then(|c| c.as_array_mut())
            .ok_or_else(|| anyhow::anyhow!("Notebook has no cells array"))?;

        match edit_mode {
            "delete" => {
                if cell_number >= cells.len() {
                    bail!(
                        "Cell {cell_number} out of bounds (notebook has {} cells)",
                        cells.len()
                    );
                }
                cells.remove(cell_number);
            }
            "insert" => {
                if cell_number > cells.len() {
                    bail!(
                        "Cell {cell_number} out of bounds for insert (max {})",
                        cells.len()
                    );
                }
                let ct = cell_type.unwrap_or("code");
                let mut new_cell = serde_json::json!({
                    "cell_type": ct,
                    "source": new_source,
                    "metadata": {},
                });
                if ct == "code" {
                    new_cell["outputs"] = serde_json::json!([]);
                }
                cells.insert(cell_number, new_cell);
            }
            _ => {
                // replace
                if cell_number >= cells.len() {
                    bail!(
                        "Cell {cell_number} out of bounds (notebook has {} cells)",
                        cells.len()
                    );
                }
                let cell = &mut cells[cell_number];
                cell["source"] = serde_json::json!(new_source);
                cell["execution_count"] = serde_json::Value::Null;
                cell["outputs"] = serde_json::json!([]);
                if let Some(ct) = cell_type {
                    cell["cell_type"] = serde_json::json!(ct);
                }
            }
        }

        let updated = serde_json::to_string_pretty(&notebook)?;
        std::fs::write(&p, &updated).context("Failed to write notebook")?;

        let summary = match edit_mode {
            "delete" => format!("Deleted cell {cell_number}"),
            "insert" => format!("Inserted cell {cell_number}"),
            _ => format!("Updated cell {cell_number}"),
        };

        Ok(vec![ToolOutput::Result {
            data: serde_json::json!({
                "notebook_path": notebook_path,
                "cell_number": cell_number,
                "new_source": new_source,
                "cell_type": cell_type.unwrap_or("code"),
                "language": language,
                "edit_mode": edit_mode,
            }),
            result_for_assistant: summary.clone(),
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let nb = data
            .get("notebook_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let cell = data
            .get("cell_number")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        ToolResultMessage {
            title: format!("{nb} cell:{cell}"),
            summary: format!("Updated cell {cell}"),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let path = input
            .get("notebook_path")
            .and_then(|v| v.as_str())
            .unwrap_or("notebook");
        let fname = std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());
        if let Some(cn) = input.get("cell_number").and_then(|v| v.as_i64()) {
            format!("{fname} cell:{cn}")
        } else {
            fname
        }
    }

    fn gen_tool_permission(&self, input: &Value) -> Option<ToolPermissionInfo> {
        let title = self.get_display_title(input);
        let path = input
            .get("notebook_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        Some(ToolPermissionInfo {
            title,
            content: serde_json::json!({"path": path}),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_notebook_edit_not_read_only() {
        let tool = NotebookEditTool;
        assert!(!tool.is_read_only());
        assert_eq!(tool.name(), "NotebookEdit");
    }
}
