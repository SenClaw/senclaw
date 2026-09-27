//! Glob tool — file pattern matching.
//!
//! Port of TS `node_modules/sema-core/dist/tools/Glob/`.


use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolResultMessage};

const MAX_RESULTS: usize = 100;
const MAX_DISPLAY: usize = 10;

pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn path_fields(&self) -> &'static [&'static str] {
        &["path"]
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern. Supports *, **, ?, [abc], {a,b}."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern (not regex) to match file paths. Supports *, **, ?, [abc], {a,b}. Example: src/**/*.rs"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in. Defaults to current working directory."
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
        let search_path = input
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(ctx.working_dir);

        // `search_path` defaults to the working dir (absolute), but a model
        // that passes one explicitly usually passes a relative one — "src",
        // straight out of a previous result — and `PathBuf::from` would point
        // it at the daemon's own cwd instead.
        let base = crate::util::paths::resolve_in_workspace(search_path, ctx.working_dir);

        let mut files: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // The Rust `glob` crate supports *, **, ?, [abc] but NOT brace
        // alternation `{a,b,c}` — yet our tool description (and agents, following
        // Node/Bash glob habits) rely on patterns like `**/*.{ts,tsx,json}`.
        // Without expansion those match nothing → "No files found" → agents
        // conclude the project is empty. Expand braces into concrete patterns
        // and union the results (dedup, first-seen order preserved).
        'outer: for expanded in expand_braces(pattern) {
            let full_pattern = base.join(&expanded);
            let pattern_str = full_pattern.to_string_lossy().to_string();
            if let Ok(paths) = glob::glob(&pattern_str) {
                for entry in paths.flatten() {
                    if files.len() >= MAX_RESULTS {
                        break 'outer;
                    }
                    // Convert to relative path from working_dir
                    let rel = match entry.strip_prefix(ctx.working_dir) {
                        Ok(rel) => rel.to_string_lossy().to_string(),
                        Err(_) => entry.to_string_lossy().to_string(),
                    };
                    if seen.insert(rel.clone()) {
                        files.push(rel);
                    }
                }
            }
        }

        let truncated = files.len() >= MAX_RESULTS;
        let num_files = files.len();

        let output_text = if files.is_empty() {
            "No files found".to_string()
        } else {
            let mut s = files
                .iter()
                .take(MAX_DISPLAY)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            let remaining = num_files.saturating_sub(MAX_DISPLAY);
            if remaining > 0 {
                s.push_str(&format!("\n... (+{remaining} files)"));
            }
            if truncated {
                s.push_str(
                    "\n(Results are truncated. Consider using a more specific path or pattern.)",
                );
            }
            s
        };

        let _title = get_title(pattern, search_path);

        Ok(vec![ToolOutput::Result {
            data: serde_json::json!({
                "pattern": pattern,
                "path": search_path,
                "filenames": files,
                "numFiles": num_files,
                "truncated": truncated,
                "durationMs": 0,
            }),
            result_for_assistant: output_text,
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let num = data.get("numFiles").and_then(|v| v.as_u64()).unwrap_or(0);
        let title = format!(
            "pattern: \"{}\"",
            data.get("pattern").and_then(|v| v.as_str()).unwrap_or("")
        );
        let summary = format!("Found {} {}", num, if num == 1 { "file" } else { "files" });
        ToolResultMessage {
            title,
            summary,
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
        let path = input.get("path").and_then(|v| v.as_str());
        get_title(pattern, path.unwrap_or(""))
    }
}

fn get_title(pattern: &str, path: &str) -> String {
    let mut parts = vec![format!("pattern: \"{}\"", pattern)];
    if !path.is_empty() {
        // Show relative path if possible
        parts.push(format!("path: \"{}\"", path));
    }
    parts.join(", ")
}

/// Expand shell-style brace alternation (`{a,b,c}`) into concrete patterns,
/// which the `glob` crate does not handle natively. Braces cartesian-multiply:
/// `{a,b}/{c,d}` → `a/c`, `a/d`, `b/c`, `b/d`. A pattern with no braces returns
/// itself. Unbalanced or empty (`{}`, `{a}`) braces are treated literally so we
/// never silently drop a pattern.
fn expand_braces(pattern: &str) -> Vec<String> {
    // Locate the first top-level `{ ... }` group (respecting nesting).
    let bytes = pattern.as_bytes();
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    let mut depth = 0usize;
    let mut close = None;
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else {
        // Unbalanced brace — treat literally.
        return vec![pattern.to_string()];
    };

    let prefix = &pattern[..open];
    let inner = &pattern[open + 1..close];
    let suffix = &pattern[close + 1..];

    // Split the group body on top-level commas (ignore commas inside nested {}).
    let alts = split_top_level_commas(inner);
    // `{a}` / `{}` with no top-level comma isn't real alternation — keep literal
    // so paths that legitimately contain braces still work.
    if alts.len() < 2 {
        let mut out = Vec::new();
        for tail in expand_braces(suffix) {
            out.push(format!("{prefix}{{{inner}}}{tail}"));
        }
        return out;
    }

    let mut out = Vec::new();
    for alt in alts {
        // Recursively expand alternatives (nested braces) and the suffix.
        for alt_expanded in expand_braces(&alt) {
            for tail in expand_braces(suffix) {
                out.push(format!("{prefix}{alt_expanded}{tail}"));
            }
        }
    }
    out
}

/// Split on commas that are not nested inside another `{ }` group.
fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut cur = String::new();
    for ch in s.chars() {
        match ch {
            '{' => {
                depth += 1;
                cur.push(ch);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                cur.push(ch);
            }
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    parts.push(cur);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_tool_read_only() {
        let tool = GlobTool;
        assert!(tool.is_read_only());
        assert_eq!(tool.name(), "Glob");
    }

    #[test]
    fn test_expand_braces_none() {
        assert_eq!(expand_braces("src/**/*.rs"), vec!["src/**/*.rs"]);
    }

    #[test]
    fn test_expand_braces_simple() {
        // The exact pattern from the failing review dispatch.
        assert_eq!(
            expand_braces("**/*.{ts,tsx,json}"),
            vec!["**/*.ts", "**/*.tsx", "**/*.json"]
        );
    }

    #[test]
    fn test_expand_braces_cartesian() {
        assert_eq!(
            expand_braces("{a,b}/{c,d}"),
            vec!["a/c", "a/d", "b/c", "b/d"]
        );
    }

    #[test]
    fn test_expand_braces_nested() {
        assert_eq!(
            expand_braces("src/{a,{b,c}}.ts"),
            vec!["src/a.ts", "src/b.ts", "src/c.ts"]
        );
    }

    #[test]
    fn test_expand_braces_single_alt_is_literal() {
        // `{a}` has no comma — not real alternation, keep literal.
        assert_eq!(expand_braces("foo/{a}.ts"), vec!["foo/{a}.ts"]);
    }

    #[test]
    fn test_expand_braces_unbalanced_literal() {
        assert_eq!(expand_braces("foo/{a,b.ts"), vec!["foo/{a,b.ts"]);
    }

    /// End-to-end: the brace pattern that returned "No files found" must now
    /// match real files under a temp dir tree.
    #[test]
    fn test_glob_brace_expansion_finds_files() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("senclaw-glob-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("src/components")).unwrap();
        fs::write(dir.join("src/App.tsx"), "x").unwrap();
        fs::write(dir.join("src/types.ts"), "x").unwrap();
        fs::write(dir.join("package.json"), "{}").unwrap();
        fs::write(dir.join("src/components/Btn.tsx"), "x").unwrap();
        fs::write(dir.join("README.md"), "x").unwrap();

        let dir_s = dir.to_string_lossy().to_string();
        let tool = GlobTool;
        let ctx = ToolContext {
            agent_id: "t",
            working_dir: &dir_s,
            agent_data_dir: &dir_s,
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt
            .block_on(tool.call(serde_json::json!({ "pattern": "**/*.{ts,tsx,json}" }), &ctx))
            .unwrap();
        let data = match &out[0] {
            ToolOutput::Result { data, .. } => data,
            _ => panic!("expected result"),
        };
        let num = data.get("numFiles").and_then(|v| v.as_u64()).unwrap();
        assert_eq!(num, 4, "should match the 4 ts/tsx/json files, not the .md");

        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod relative_path_tests {
    use super::*;

    #[tokio::test]
    async fn a_relative_path_argument_searches_the_working_dir() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("src")).unwrap();
        // NOT "lib.rs": with the fix reverted the glob runs against the test
        // process cwd — the repo root — where `src/lib.rs` exists, and the
        // assertion would pass for entirely the wrong reason.
        std::fs::write(d.path().join("src/only_in_this_tempdir.rs"), "").unwrap();
        let wd = d.path().to_string_lossy().to_string();
        let ctx = ToolContext {
            agent_id: "t",
            working_dir: &wd,
            agent_data_dir: &wd,
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: crate::zen_core::EditFormat::default(),
        };
        // `path` defaults to the (absolute) working dir, so only an explicit
        // one was ever broken — and an explicit one is almost always relative,
        // because that is the shape Glob's own results come back in.
        let outs = GlobTool
            .call(serde_json::json!({"pattern": "*.rs", "path": "src"}), &ctx)
            .await
            .unwrap();
        let text = match &outs[0] {
            ToolOutput::Result { result_for_assistant, .. } => result_for_assistant.clone(),
            _ => panic!("unexpected output"),
        };
        assert!(text.contains("src/only_in_this_tempdir.rs"), "{text}");
    }
}
