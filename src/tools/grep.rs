//! Grep tool — ripgrep-powered regex search.
//!
//! Port of TS `node_modules/sema-core/dist/tools/Grep/`.

use std::path::PathBuf;
use std::process::Command;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolResultMessage};

const MAX_RESULTS: usize = 100;
const MAX_DISPLAY: usize = 10;

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn path_fields(&self) -> &'static [&'static str] {
        &["path"]
    }

    fn description(&self) -> &str {
        "Search files using regex patterns. Supports full ripgrep syntax, file type filtering, context lines."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to match against file contents (ripgrep regex syntax)"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search. Defaults to current working directory."
                },
                "glob": {
                    "type": "string",
                    "description": "Glob filter on file paths, e.g. \"*.js\", \"*.{ts,tsx}\", \"**/test/*.py\"."
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Output format. \"files_with_matches\" (default): list matching file paths. \"content\": matching lines with line numbers. \"count\": match count per file."
                },
                "-A": {
                    "type": "number",
                    "description": "Trailing context lines after each match (rg -A). content mode only."
                },
                "-B": {
                    "type": "number",
                    "description": "Leading context lines before each match (rg -B). content mode only."
                },
                "-C": {
                    "type": "number",
                    "description": "Context lines on both sides of each match; overrides -A/-B. content mode only."
                },
                "-i": {
                    "type": "boolean",
                    "description": "Case-insensitive search. Default is case-sensitive."
                },
                "type": {
                    "type": "string",
                    "description": "Filter by file type, e.g. js, ts, tsx, py, rust, go, java. (rg --type)"
                },
                "head_limit": {
                    "type": "number",
                    "description": "Return only the first N entries. 0 = unlimited."
                },
                "offset": {
                    "type": "number",
                    "description": "Skip the first N entries before head_limit. Use with head_limit for pagination."
                },
                "multiline": {
                    "type": "boolean",
                    "description": "Allow patterns to span line breaks and '.' to match newlines. Default false."
                }
            },
            "required": ["pattern"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(ctx.working_dir);
        let glob_filter = input.get("glob").and_then(|v| v.as_str());
        let output_mode = input
            .get("output_mode")
            .and_then(|v| v.as_str())
            .unwrap_or("files_with_matches");
        let after_context = input.get("-A").and_then(|v| v.as_u64());
        let before_context = input.get("-B").and_then(|v| v.as_u64());
        let context = input.get("-C").and_then(|v| v.as_u64());
        let case_insensitive = input.get("-i").and_then(|v| v.as_bool()).unwrap_or(false);
        let file_type = input.get("type").and_then(|v| v.as_str());
        let head_limit = input
            .get("head_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let offset = input.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let multiline = input
            .get("multiline")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut args: Vec<String> = Vec::new();

        // Output mode
        match output_mode {
            "files_with_matches" => {
                args.push("-l".into());
            }
            "content" => {
                args.push("-n".into());
            }
            "count" => {
                args.push("-c".into());
            }
            _ => {
                args.push("-l".into());
            }
        }

        // Case sensitivity
        if case_insensitive {
            args.push("-i".into());
        }

        // Context lines
        if let Some(c) = context {
            args.push("-C".into());
            args.push(c.to_string());
        } else {
            if let Some(b) = before_context {
                args.push("-B".into());
                args.push(b.to_string());
            }
            if let Some(a) = after_context {
                args.push("-A".into());
                args.push(a.to_string());
            }
        }

        // Multiline
        if multiline {
            args.push("-U".into());
            args.push("--multiline-dotall".into());
        }

        // File type filter
        if let Some(t) = file_type {
            args.push("--type".into());
            args.push(t.to_string());
        }

        // Glob filter
        if let Some(g) = glob_filter {
            args.push("--glob".into());
            args.push(g.to_string());
        }

        args.push(pattern.to_string());

        // Named absolute, built from whatever the model sent: a relative
        // "src" resolved against the daemon's cwd, not the chat's project.
        let absolute_path = crate::util::paths::resolve_in_workspace(path, ctx.working_dir);
        let search_dir: PathBuf = if absolute_path.is_dir() {
            absolute_path
        } else {
            absolute_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or(absolute_path)
        };

        // Try rg first, fall back to grep
        let result = run_rg(&args, &search_dir);

        let (raw_lines, _exit_code) = match result {
            Ok(lines) => (lines, 0),
            Err(_) => {
                // Fallback to grep
                let grep_args = build_grep_args(
                    pattern,
                    &search_dir,
                    output_mode,
                    case_insensitive,
                    file_type,
                );
                match run_grep(&grep_args, &search_dir) {
                    Ok(lines) => (lines, 0),
                    Err(e) => {
                        return Ok(vec![ToolOutput::Result {
                            data: serde_json::json!({"error": true}),
                            result_for_assistant: format!("Grep failed: {e}"),
                        }]);
                    }
                }
            }
        };

        let processed = apply_offset_limit(&raw_lines, offset, head_limit);
        let num_files = if output_mode == "content" {
            // Count actual match lines (not context lines, not separators)
            processed
                .iter()
                .filter(|l| *l != "--" && (l.contains(":") || l.contains(':')))
                .count()
        } else {
            processed.len()
        };

        let _title = get_title(pattern, path, glob_filter);
        let display = build_display(&processed, num_files, ctx.working_dir);

        Ok(vec![ToolOutput::Result {
            data: serde_json::json!({
                "pattern": pattern,
                "path": path,
                "glob": glob_filter,
                "filenames": processed,
                "numFiles": num_files,
                "durationMs": 0,
            }),
            result_for_assistant: display,
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let num = data.get("numFiles").and_then(|v| v.as_u64()).unwrap_or(0);
        let title = get_title(
            data.get("pattern").and_then(|v| v.as_str()).unwrap_or(""),
            data.get("path").and_then(|v| v.as_str()).unwrap_or(""),
            data.get("glob").and_then(|v| v.as_str()),
        );
        let summary = format!(
            "Found {} {}",
            num,
            if num == 1 { "match" } else { "matches" }
        );
        ToolResultMessage {
            title,
            summary,
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        get_title(
            input.get("pattern").and_then(|v| v.as_str()).unwrap_or(""),
            input.get("path").and_then(|v| v.as_str()).unwrap_or(""),
            input.get("glob").and_then(|v| v.as_str()),
        )
    }
}

fn get_title(pattern: &str, path: &str, glob: Option<&str>) -> String {
    let mut parts = vec![format!("pattern: \"{}\"", pattern)];
    if let Some(g) = glob {
        parts.push(format!("glob: \"{}\"", g));
    }
    if !path.is_empty() {
        parts.push(format!("path: \"{}\"", path));
    }
    let s = parts.join(", ");
    if s.len() > 100 {
        format!(
            "{}...",
            crate::util::text::truncate_on_char_boundary(&s, 100)
        )
    } else {
        s
    }
}

fn build_display(lines: &[String], num: usize, working_dir: &str) -> String {
    if num == 0 {
        return "No matches found".into();
    }
    let display: Vec<String> = lines
        .iter()
        .take(MAX_DISPLAY)
        .map(|line| {
            // Convert absolute paths to relative
            if let Some(captures) = regex::Regex::new(r"^(.+?):(\d+)(:.*)?$")
                .unwrap()
                .captures(line)
            {
                let file_path = captures.get(1).unwrap().as_str();
                let line_num = captures.get(2).unwrap().as_str();
                let rest = captures.get(3).map(|m| m.as_str()).unwrap_or("");
                if let Ok(rel) = PathBuf::from(file_path).strip_prefix(working_dir) {
                    return format!("{}:{}{}", rel.display(), line_num, rest);
                }
            }
            line.clone()
        })
        .collect();
    let mut s = display.join("\n");
    let remaining = num.saturating_sub(MAX_DISPLAY);
    if remaining > 0 {
        s.push_str(&format!("\n... (+{remaining} matches)"));
    }
    if num > MAX_RESULTS {
        s.push_str("\n(Too many matches. Use a narrower path or pattern.)");
    }
    s
}

fn apply_offset_limit(lines: &[String], offset: usize, limit: usize) -> Vec<String> {
    let skipped: Vec<String> = lines.iter().skip(offset).cloned().collect();
    if limit > 0 {
        skipped.into_iter().take(limit).collect()
    } else {
        skipped
    }
}

fn run_rg(args: &[String], dir: &PathBuf) -> Result<Vec<String>> {
    let output = Command::new("rg").args(args).current_dir(dir).output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(stdout.lines().map(String::from).collect())
}

fn run_grep(args: &[String], dir: &PathBuf) -> Result<Vec<String>> {
    let output = Command::new("grep").args(args).current_dir(dir).output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(stdout.lines().map(String::from).collect())
}

fn build_grep_args(
    pattern: &str,
    dir: &PathBuf,
    output_mode: &str,
    case_insensitive: bool,
    file_type: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["-r".into(), "-H".into()];
    match output_mode {
        "files_with_matches" => {
            args.push("-l".into());
        }
        "count" => {
            args.push("-c".into());
        }
        _ => {
            args.push("-n".into());
        }
    }
    if case_insensitive {
        args.push("-i".into());
    }
    if let Some(t) = file_type {
        args.push("--include".into());
        args.push(format!("*.{}", t));
    }
    args.push(pattern.to_string());
    args.push(dir.to_string_lossy().to_string());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grep_read_only() {
        let tool = GrepTool;
        assert!(tool.is_read_only());
        assert_eq!(tool.name(), "Grep");
    }

    #[test]
    fn test_apply_offset_limit() {
        let lines = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let result = apply_offset_limit(&lines, 1, 2);
        assert_eq!(result, vec!["b", "c"]);
    }

    #[test]
    fn test_apply_offset_limit_no_limit() {
        let lines = vec!["a".into(), "b".into()];
        let result = apply_offset_limit(&lines, 0, 0);
        assert_eq!(result, vec!["a", "b"]);
    }
}
