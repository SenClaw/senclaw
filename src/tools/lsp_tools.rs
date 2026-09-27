//! `lsp_diagnostics` / `lsp_status` — ask the language server again, on
//! demand. The automatic path is the hook in `run_tools` that appends
//! diagnostics to every Edit/Write result; these tools exist for "what does
//! the compiler say about this file now?" and for seeing which servers run.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolResultMessage};

pub struct LspDiagnosticsTool;

#[async_trait]
impl Tool for LspDiagnosticsTool {
    fn name(&self) -> &str {
        "lsp_diagnostics"
    }
    fn description(&self) -> &str {
        "Language-server diagnostics (type errors, unresolved names, lints) for one file, or for every file the workspace's servers have reported on when `path` is omitted. Needs a server on PATH for the language (rust-analyzer, typescript-language-server, pyright, gopls, dart, clangd)."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Workspace-relative file to check. Omit for everything currently reported." }
            }
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let path = input.get("path").and_then(|v| v.as_str()).map(str::trim).filter(|p| !p.is_empty());
        match path {
            Some(p) => {
                let Some(report) = crate::lsp::diagnostics_for(ctx.working_dir, p).await else {
                    return Ok(vec![ToolOutput::Result {
                        data: serde_json::json!({ "path": p, "available": false }),
                        result_for_assistant: format!("No language server available for `{p}` (not installed, disabled, or unsupported language). Run the project's own build/lint instead."),
                    }]);
                };
                let text = report.render();
                let n = report.diagnostics.len();
                Ok(vec![ToolOutput::Result {
                    data: serde_json::json!({ "path": report.path, "server": report.server, "count": n, "fresh": report.fresh, "diagnostics": report.diagnostics }),
                    result_for_assistant: if text.is_empty() { format!("{} reports no diagnostics for {}.", report.server, report.path) } else { text.trim_start().to_string() },
                }])
            }
            None => {
                let all = crate::lsp::workspace_diagnostics(ctx.working_dir).await;
                let total: usize = all.iter().map(|(_, _, d)| d.len()).sum();
                let mut text = String::new();
                for (server, path, items) in &all {
                    let r = crate::lsp::DiagnosticsReport { server: server.clone(), path: path.clone(), diagnostics: items.clone(), fresh: true };
                    text.push_str(r.render().trim_start());
                    text.push('\n');
                }
                if text.is_empty() {
                    text = "No diagnostics reported yet (servers report files after they are opened by an Edit/Write or lsp_diagnostics with a path).".into();
                }
                Ok(vec![ToolOutput::Result {
                    data: serde_json::json!({ "files": all.len(), "count": total }),
                    result_for_assistant: text,
                }])
            }
        }
    }
    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let n = data.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        ToolResultMessage {
            title: data.get("path").and_then(|v| v.as_str()).unwrap_or("workspace").to_string(),
            summary: format!("{n} diagnostic{}", if n == 1 { "" } else { "s" }),
            content: data.clone(),
        }
    }
    fn get_display_title(&self, input: &Value) -> String {
        input.get("path").and_then(|v| v.as_str()).unwrap_or("workspace").to_string()
    }
}

pub struct LspStatusTool;

#[async_trait]
impl Tool for LspStatusTool {
    fn name(&self) -> &str {
        "lsp_status"
    }
    fn description(&self) -> &str {
        "Which language servers are installed, running, or disabled for this daemon, and the diagnostics timeout."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn should_defer(&self) -> bool {
        true
    }
    async fn call(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let st = crate::lsp::status().await;
        Ok(vec![ToolOutput::Result {
            result_for_assistant: serde_json::to_string_pretty(&st).unwrap_or_default(),
            data: st,
        }])
    }
    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        ToolResultMessage {
            title: "lsp status".into(),
            summary: format!("{} running", data.get("servers").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0)),
            content: data.clone(),
        }
    }
    fn get_display_title(&self, _input: &Value) -> String {
        "lsp status".into()
    }
}
