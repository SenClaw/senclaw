//! Symbol tools over the repo-map index: `find_symbol`, `find_references`,
//! `symbol_body`, `repo_map`. They read the same tree-sitter index the
//! `<repo_map>` prompt block is rendered from ([`crate::repo_map`]), so an
//! answer here is consistent with what the model was shown.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::repo_map;
use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolResultMessage};

const MAX_HITS: usize = 50;
const MAX_BODY_LINES: usize = 300;

fn not_a_project(dir: &str) -> Vec<ToolOutput> {
    vec![ToolOutput::Result {
        data: serde_json::json!({ "error": true, "reason": "not_a_project", "workingDir": dir }),
        result_for_assistant: format!(
            "`{dir}` is not indexed: the repo map only covers git repositories or folders with a project manifest (Cargo.toml, package.json, pyproject.toml, go.mod, …). Use Grep instead."
        ),
    }]
}

// ===== find_symbol =====

pub struct FindSymbolTool;

#[async_trait]
impl Tool for FindSymbolTool {
    fn name(&self) -> &str {
        "find_symbol"
    }
    fn description(&self) -> &str {
        "Find where a function/class/struct/type/module is DEFINED, by name, across the whole repository (tree-sitter index, not text search). Returns file:line and the signature. Faster and more precise than Grep for 'where is X defined'."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Symbol name. Exact match by default." },
                "kind": { "type": "string", "description": "Optional filter: function | method | class | interface | type | module | constant | macro" },
                "fuzzy": { "type": "boolean", "description": "Substring match instead of exact (default false)." }
            },
            "required": ["name"]
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
        let kind = input.get("kind").and_then(|v| v.as_str()).filter(|k| !k.is_empty());
        let fuzzy = input.get("fuzzy").and_then(|v| v.as_bool()).unwrap_or(false);
        if name.is_empty() {
            return Ok(vec![ToolOutput::Result {
                data: serde_json::json!({"error": true}),
                result_for_assistant: "name is required".into(),
            }]);
        }
        let Some(index) = repo_map::index_ready(ctx.working_dir).await else {
            return Ok(not_a_project(ctx.working_dir));
        };
        let guard = index.lock().unwrap();
        let hits = guard.find_symbol(name, kind, fuzzy);
        let total = hits.len();
        let rows: Vec<Value> = hits
            .iter()
            .take(MAX_HITS)
            .map(|(f, d)| serde_json::json!({ "path": f, "line": d.line, "endLine": d.end_line, "kind": d.kind, "name": d.name, "signature": d.sig }))
            .collect();
        let mut text = String::new();
        if total == 0 {
            text.push_str(&format!("No definition named `{name}` in the index ({} files). Try fuzzy=true or Grep.", guard.files.len()));
        } else {
            for (f, d) in hits.iter().take(MAX_HITS) {
                text.push_str(&format!("{f}:{} [{}] {}\n", d.line, d.kind, d.sig));
            }
            if total > MAX_HITS {
                text.push_str(&format!("… {} more\n", total - MAX_HITS));
            }
        }
        Ok(vec![ToolOutput::Result {
            data: serde_json::json!({ "name": name, "total": total, "hits": rows }),
            result_for_assistant: text,
        }])
    }
    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let n = data.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
        ToolResultMessage {
            title: format!("symbol: {}", data.get("name").and_then(|v| v.as_str()).unwrap_or("")),
            summary: format!("{n} definition{}", if n == 1 { "" } else { "s" }),
            content: data.clone(),
        }
    }
    fn get_display_title(&self, input: &Value) -> String {
        input.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string()
    }
}

// ===== find_references =====

pub struct FindReferencesTool;

#[async_trait]
impl Tool for FindReferencesTool {
    fn name(&self) -> &str {
        "find_references"
    }
    fn description(&self) -> &str {
        "Find which files CALL or USE a symbol (by name) across the repository, with line numbers — the blast radius before changing a function or type. Tree-sitter reference index; excludes the defining lines."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Symbol name (exact)." }
            },
            "required": ["name"]
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
        if name.is_empty() {
            return Ok(vec![ToolOutput::Result {
                data: serde_json::json!({"error": true}),
                result_for_assistant: "name is required".into(),
            }]);
        }
        let Some(index) = repo_map::index_ready(ctx.working_dir).await else {
            return Ok(not_a_project(ctx.working_dir));
        };
        let guard = index.lock().unwrap();
        let refs = guard.find_references(name);
        let total_lines: usize = refs.iter().map(|(_, l)| l.len()).sum();
        let rows: Vec<Value> = refs
            .iter()
            .take(MAX_HITS)
            .map(|(f, lines)| serde_json::json!({ "path": f, "lines": lines }))
            .collect();
        let mut text = String::new();
        if refs.is_empty() {
            text.push_str(&format!("No references to `{name}` found in the index ({} files). The grammar may not tag this usage kind; confirm with Grep.", guard.files.len()));
        } else {
            for (f, lines) in refs.iter().take(MAX_HITS) {
                let shown: Vec<String> = lines.iter().take(12).map(|l| l.to_string()).collect();
                let more = if lines.len() > 12 { format!(" (+{})", lines.len() - 12) } else { String::new() };
                text.push_str(&format!("{f}: {}{more}\n", shown.join(", ")));
            }
            if refs.len() > MAX_HITS {
                text.push_str(&format!("… {} more files\n", refs.len() - MAX_HITS));
            }
        }
        Ok(vec![ToolOutput::Result {
            data: serde_json::json!({ "name": name, "files": refs.len(), "references": total_lines, "hits": rows }),
            result_for_assistant: text,
        }])
    }
    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let files = data.get("files").and_then(|v| v.as_u64()).unwrap_or(0);
        let refs = data.get("references").and_then(|v| v.as_u64()).unwrap_or(0);
        ToolResultMessage {
            title: format!("references: {}", data.get("name").and_then(|v| v.as_str()).unwrap_or("")),
            summary: format!("{refs} reference{} in {files} file{}", if refs == 1 { "" } else { "s" }, if files == 1 { "" } else { "s" }),
            content: data.clone(),
        }
    }
    fn get_display_title(&self, input: &Value) -> String {
        input.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string()
    }
}

// ===== symbol_body =====

pub struct SymbolBodyTool;

#[async_trait]
impl Tool for SymbolBodyTool {
    fn name(&self) -> &str {
        "symbol_body"
    }
    fn description(&self) -> &str {
        "Read the full source of one definition (function/class/…) by name, without reading the whole file. Give `path` to disambiguate when the name is defined in several files."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Symbol name (exact)." },
                "path": { "type": "string", "description": "Workspace-relative file path, when the name is defined more than once." }
            },
            "required": ["name"]
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
        let path = input.get("path").and_then(|v| v.as_str()).map(|p| p.trim_start_matches("./"));
        if name.is_empty() {
            return Ok(vec![ToolOutput::Result {
                data: serde_json::json!({"error": true}),
                result_for_assistant: "name is required".into(),
            }]);
        }
        let Some(index) = repo_map::index_ready(ctx.working_dir).await else {
            return Ok(not_a_project(ctx.working_dir));
        };
        let (file, line, end_line, root) = {
            let guard = index.lock().unwrap();
            let hits = guard.find_symbol(name, None, false);
            let hits: Vec<_> = match path {
                Some(p) => hits.into_iter().filter(|(f, _)| *f == p).collect(),
                None => hits,
            };
            match hits.as_slice() {
                [] => {
                    return Ok(vec![ToolOutput::Result {
                        data: serde_json::json!({ "error": true, "reason": "not_found", "name": name }),
                        result_for_assistant: format!("No definition named `{name}`{}.", path.map(|p| format!(" in {p}")).unwrap_or_default()),
                    }]);
                }
                [(f, d)] => (f.to_string(), d.line, d.end_line, guard.root.clone()),
                many => {
                    let list: Vec<String> = many.iter().map(|(f, d)| format!("{f}:{}", d.line)).collect();
                    return Ok(vec![ToolOutput::Result {
                        data: serde_json::json!({ "error": true, "reason": "ambiguous", "candidates": list }),
                        result_for_assistant: format!("`{name}` is defined in several places; pass `path`:\n{}", list.join("\n")),
                    }]);
                }
            }
        };
        let abs = root.join(&file);
        let src = std::fs::read_to_string(&abs)?;
        let start = (line.max(1) - 1) as usize;
        let end = (end_line as usize).min(src.lines().count()).max(start + 1);
        let capped_end = end.min(start + MAX_BODY_LINES);
        let mut body = String::new();
        for (i, l) in src.lines().enumerate().skip(start).take(capped_end - start) {
            body.push_str(&format!("{:>5}│ {l}\n", i + 1));
        }
        if capped_end < end {
            body.push_str(&format!("… truncated at {MAX_BODY_LINES} lines (ends at line {end}); use Read with a line range for the rest.\n"));
        }
        Ok(vec![ToolOutput::Result {
            data: serde_json::json!({ "name": name, "path": file, "line": line, "endLine": end_line, "truncated": capped_end < end }),
            result_for_assistant: format!("{file}:{line}-{end_line}\n{body}"),
        }])
    }
    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        ToolResultMessage {
            title: format!("{}:{}", data.get("path").and_then(|v| v.as_str()).unwrap_or(""), data.get("line").and_then(|v| v.as_u64()).unwrap_or(0)),
            summary: data.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            content: data.clone(),
        }
    }
    fn get_display_title(&self, input: &Value) -> String {
        input.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string()
    }
}

// ===== repo_map =====

pub struct RepoMapTool;

#[async_trait]
impl Tool for RepoMapTool {
    fn name(&self) -> &str {
        "repo_map"
    }
    fn description(&self) -> &str {
        "Ranked outline of the repository (file → definitions with line numbers), optionally focused on given files. Use when the outline in the system prompt is missing or you need it centred on a different part of the tree."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "focus": { "type": "array", "items": { "type": "string" }, "description": "Workspace-relative paths to centre the ranking on." },
                "budget_tokens": { "type": "integer", "description": "Size of the outline (default 2000, max 8000)." }
            }
        })
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let focus: Vec<String> = input
            .get("focus")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.trim_start_matches("./").to_string())).collect())
            .unwrap_or_default();
        let budget = input
            .get("budget_tokens")
            .and_then(|v| v.as_u64())
            .map(|b| (b as usize).clamp(200, 8000))
            .unwrap_or(repo_map::DEFAULT_BUDGET_TOKENS);
        let Some(index) = repo_map::index_ready(ctx.working_dir).await else {
            return Ok(not_a_project(ctx.working_dir));
        };
        let guard = index.lock().unwrap();
        let text = guard.render(&focus, budget);
        Ok(vec![ToolOutput::Result {
            data: serde_json::json!({ "files": guard.files.len(), "budgetTokens": budget, "focus": focus }),
            result_for_assistant: if text.is_empty() { "The index is empty (no supported source files).".into() } else { text },
        }])
    }
    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        ToolResultMessage {
            title: "repo map".into(),
            summary: format!("{} files indexed", data.get("files").and_then(|v| v.as_u64()).unwrap_or(0)),
            content: data.clone(),
        }
    }
    fn get_display_title(&self, _input: &Value) -> String {
        "repo map".into()
    }
}
