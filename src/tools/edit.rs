//! Edit tool — string replacement in files, with a per-model edit format.
//!
//! Port of TS `node_modules/sema-core/dist/tools/Edit/`, extended with
//! Aider's observation that the *edit format* should follow the model:
//! `exact` (historical) for models that reproduce `old_string` verbatim,
//! `fuzzy` for ones that drift by whitespace or a character, `udiff` for
//! ones that write unified-diff hunks better than search/replace, `whole`
//! for ones that should rewrite files with `Write`. The format comes from
//! the model profile through [`ToolContext::edit_format`]; the matching
//! itself lives in [`super::edit_apply`].

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use similar::{ChangeTag, TextDiff};

use super::edit_apply::{apply_udiff, find_fuzzy};
use crate::util::paths::resolve_in_workspace;
use crate::zen_core::{EditFormat, Tool, ToolContext, ToolOutput, ToolPermissionInfo, ToolResultMessage};

/// Counters for comparing formats on a model: how often an edit was applied
/// exactly, needed a tolerant match, used a patch, or failed to apply at all.
#[derive(Default)]
pub struct EditStats {
    pub attempts: AtomicU64,
    pub exact: AtomicU64,
    pub fuzzy: AtomicU64,
    pub udiff: AtomicU64,
    pub failed: AtomicU64,
}

pub static EDIT_STATS: EditStats = EditStats {
    attempts: AtomicU64::new(0),
    exact: AtomicU64::new(0),
    fuzzy: AtomicU64::new(0),
    udiff: AtomicU64::new(0),
    failed: AtomicU64::new(0),
};

pub fn edit_stats_json() -> Value {
    serde_json::json!({
        "attempts": EDIT_STATS.attempts.load(Ordering::Relaxed),
        "exact": EDIT_STATS.exact.load(Ordering::Relaxed),
        "fuzzy": EDIT_STATS.fuzzy.load(Ordering::Relaxed),
        "udiff": EDIT_STATS.udiff.load(Ordering::Relaxed),
        "failed": EDIT_STATS.failed.load(Ordering::Relaxed),
    })
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn path_fields(&self) -> &'static [&'static str] {
        &["file_path"]
    }

    fn description(&self) -> &str {
        "Perform exact string replacements in an existing file. Alternatively pass `patch` (unified-diff hunks; line numbers are ignored, hunks are located by content) instead of old_string/new_string."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file. Absolute, or relative to the working directory."
                },
                "old_string": {
                    "type": "string",
                    "description": "The text to replace"
                },
                "new_string": {
                    "type": "string",
                    "description": "The text to replace it with (must be different from old_string)"
                },
                "replace_all": {
                    "type": "boolean",
                    "description": "Replace all occurrences of old_string (default false)"
                },
                "patch": {
                    "type": "string",
                    "description": "Unified diff to apply instead of old_string/new_string: hunks of lines prefixed ' ' (context), '-' (remove), '+' (add). @@ headers optional; each hunk is matched by its content."
                }
            },
            "required": ["file_path"]
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
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if path.is_empty() {
            return Err("file_path is required".to_string());
        }
        let patch = input.get("patch").and_then(|v| v.as_str()).unwrap_or("");
        let old = input
            .get("old_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let new = input
            .get("new_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if patch.trim().is_empty() {
            if input.get("old_string").is_none() || input.get("new_string").is_none() {
                return Err("either `patch` or both old_string and new_string are required".to_string());
            }
            if old == new {
                return Err("old_string and new_string must be different".to_string());
            }
        }

        let p = resolve_in_workspace(path, ctx.working_dir);
        // Report where we actually looked, not what the model typed.
        if !p.exists() {
            return Err(format!("File not found: {}", p.display()));
        }
        if !p.is_file() {
            return Err(format!("Not a file: {}", p.display()));
        }

        Ok(())
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        bump(&EDIT_STATS.attempts);
        let path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let p = resolve_in_workspace(path, ctx.working_dir);
        let content = std::fs::read_to_string(&p).context("Failed to read file")?;
        let fname = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());

        // ----- patch form -----
        if let Some(patch) = input.get("patch").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) {
            let outcome = match apply_udiff(&content, patch) {
                Ok(o) => o,
                Err(e) => {
                    bump(&EDIT_STATS.failed);
                    return Ok(vec![ToolOutput::Result {
                        data: serde_json::json!({"error": true, "patchFailed": true, "reason": e}),
                        result_for_assistant: format!("Error: patch not applied to {fname}: {e}. The file has not been modified."),
                    }]);
                }
            };
            if outcome.content == content {
                bail!("the patch changes nothing");
            }
            std::fs::write(&p, &outcome.content).context("Failed to write file")?;
            bump(&EDIT_STATS.udiff);
            let diff = make_unified_diff(&content, &outcome.content, path);
            let how = outcome.how.join(", ");
            return Ok(vec![ToolOutput::Result {
                data: serde_json::json!({
                    "path": path,
                    "hunks": outcome.hunks_applied,
                    "matched": outcome.how,
                    "diff": diff,
                }),
                result_for_assistant: format!(
                    "Applied {} hunk(s) to {fname} (matched: {how})\n{diff}",
                    outcome.hunks_applied
                ),
            }]);
        }

        // ----- old_string / new_string form -----
        let old_string = input
            .get("old_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let new_string = input
            .get("new_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let replace_all = input
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let occurrences = content.match_indices(old_string).count();
        let tolerant = matches!(ctx.edit_format, EditFormat::Fuzzy | EditFormat::Udiff | EditFormat::Whole);

        if occurrences == 0 {
            // Tolerant formats: find the block the model *meant*.
            if tolerant && !replace_all {
                if let Some(hit) = find_fuzzy(&content, old_string) {
                    let matched = content[hit.start..hit.end].to_string();
                    let mut new_content = String::with_capacity(content.len() + new_string.len());
                    new_content.push_str(&content[..hit.start]);
                    new_content.push_str(new_string);
                    new_content.push_str(&content[hit.end..]);
                    if new_content == content {
                        bail!("old_string and new_string are identical — no change made");
                    }
                    std::fs::write(&p, &new_content).context("Failed to write file")?;
                    bump(&EDIT_STATS.fuzzy);
                    let diff = make_unified_diff(&content, &new_content, path);
                    return Ok(vec![ToolOutput::Result {
                        data: serde_json::json!({
                            "path": path,
                            "replacements": 1,
                            "fuzzy": hit.how,
                            "matchedText": matched,
                            "diff": diff,
                        }),
                        result_for_assistant: format!(
                            "Edited {fname} (1 replacement; old_string matched by {} — the file's text differed slightly from what you sent)\n{diff}",
                            hit.how
                        ),
                    }]);
                }
            }
            bump(&EDIT_STATS.failed);
            return Ok(vec![ToolOutput::Result {
                data: serde_json::json!({"error": true, "notFound": true}),
                result_for_assistant: format!(
                    "Error: old_string not found in {fname}. The file has not been modified.{}",
                    if tolerant { " Re-read the file and copy the exact lines, or send a `patch`." } else { "" }
                ),
            }]);
        }

        if !replace_all && occurrences > 1 {
            bump(&EDIT_STATS.failed);
            return Ok(vec![ToolOutput::Result {
                data: serde_json::json!({"error": true, "multipleOccurrences": true, "count": occurrences}),
                result_for_assistant: format!(
                    "Error: old_string appears {occurrences} times in the file. \
                     Use replace_all=true to replace all, or provide a larger string \
                     with more surrounding context to make it unique."
                ),
            }]);
        }

        let new_content = if replace_all {
            content.replace(old_string, new_string)
        } else {
            let pos = content.find(old_string).unwrap();
            let mut result =
                String::with_capacity(content.len() + new_string.len() - old_string.len());
            result.push_str(&content[..pos]);
            result.push_str(new_string);
            result.push_str(&content[pos + old_string.len()..]);
            result
        };

        if new_content == content {
            bail!("old_string and new_string are identical — no change made");
        }

        std::fs::write(&p, &new_content).context("Failed to write file")?;
        bump(&EDIT_STATS.exact);

        let diff = make_unified_diff(&content, &new_content, path);
        let summary = format!(
            "Edited {fname} ({} replacement{})\n{}",
            occurrences,
            if occurrences > 1 { "s" } else { "" },
            diff,
        );

        Ok(vec![ToolOutput::Result {
            data: serde_json::json!({
                "path": path,
                "replacements": occurrences,
                "diff": diff,
            }),
            result_for_assistant: summary,
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let summary = if let Some(h) = data.get("hunks").and_then(|v| v.as_u64()) {
            format!("{h} hunk{}", if h == 1 { "" } else { "s" })
        } else {
            format!(
                "{} replacements{}",
                data.get("replacements").and_then(|v| v.as_u64()).unwrap_or(0),
                if data.get("fuzzy").is_some() { " (fuzzy)" } else { "" }
            )
        };
        ToolResultMessage {
            title: "Edit".into(),
            summary,
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
        format!("Edit {fname}")
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

    fn ctx(format: EditFormat) -> ToolContext<'static> {
        ToolContext {
            agent_id: "t",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: format,
        }
    }

    /// Like `ctx`, but rooted in a real directory so a relative `file_path`
    /// has somewhere to resolve to.
    pub(super) fn ctx_in<'a>(working_dir: &'a str) -> ToolContext<'a> {
        ToolContext {
            agent_id: "t",
            working_dir,
            agent_data_dir: working_dir,
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: EditFormat::default(),
        }
    }

    fn file(content: &str) -> (tempfile::TempDir, String) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f.rs");
        std::fs::write(&p, content).unwrap();
        let s = p.to_string_lossy().to_string();
        (d, s)
    }

    fn text(outs: &[ToolOutput]) -> String {
        match &outs[0] {
            ToolOutput::Result { result_for_assistant, .. } => result_for_assistant.clone(),
            _ => panic!("unexpected output"),
        }
    }

    #[tokio::test]
    async fn exact_format_refuses_a_drifted_old_string() {
        let (_d, p) = file("    let b = 2;\n");
        let input = serde_json::json!({"file_path": p, "old_string": "let b   = 2;", "new_string": "let b = 3;"});
        let out = EditTool.call(input, &ctx(EditFormat::Exact)).await.unwrap();
        assert!(text(&out).contains("not found"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "    let b = 2;\n");
    }

    #[tokio::test]
    async fn fuzzy_format_applies_a_drifted_old_string_and_says_so() {
        let (_d, p) = file("fn main() {\n    let b = 2;\n}\n");
        let input = serde_json::json!({"file_path": p, "old_string": "let b   = 2;", "new_string": "    let b = 3;"});
        let out = EditTool.call(input, &ctx(EditFormat::Fuzzy)).await.unwrap();
        let t = text(&out);
        assert!(t.contains("matched by whitespace"), "{t}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "fn main() {\n    let b = 3;\n}\n");
    }

    #[tokio::test]
    async fn patch_form_works_in_any_format() {
        let (_d, p) = file("a\nb\nc\n");
        let input = serde_json::json!({"file_path": p, "patch": " a\n-b\n+B\n c\n"});
        let out = EditTool.call(input, &ctx(EditFormat::Exact)).await.unwrap();
        assert!(text(&out).contains("Applied 1 hunk"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nB\nc\n");
        let bad = serde_json::json!({"file_path": p, "patch": "-zzz\n+yyy\n"});
        let out = EditTool.call(bad, &ctx(EditFormat::Udiff)).await.unwrap();
        assert!(text(&out).contains("patch not applied"));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nB\nc\n");
    }
}

#[cfg(test)]
mod relative_path_tests {
    use super::tests::*;
    use super::*;

    #[tokio::test]
    async fn a_relative_file_path_edits_the_file_in_the_working_dir() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("f.rs"), "let a = 1;\n").unwrap();
        let wd = d.path().to_string_lossy().to_string();
        let input = serde_json::json!({
            "file_path": "f.rs",
            "old_string": "let a = 1;",
            "new_string": "let a = 2;"
        });
        // Validation used to refuse this outright, before `call` ran.
        EditTool.validate_input(&input, &ctx_in(&wd)).await.unwrap();
        EditTool.call(input, &ctx_in(&wd)).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(d.path().join("f.rs")).unwrap(),
            "let a = 2;\n"
        );
    }
}
