//! LaunchUI tool — surface a deliverable in the WebUI workbench panel.
//!
//! Port of `code-old/SenClaw/vendor/package/dist/tools/LaunchUI/`. The agent
//! calls this after producing a viewable artifact (a rendered `.html`/`.md`
//! file, a running web app, an exposed API endpoint). It hands the artifact to
//! the engine [`WorkbenchService`], which persists it and emits `workbench:new`
//! — the [`crate::agent::workbench_bridge::WorkbenchBridge`] then broadcasts it
//! to the WebUI. Display-only: returns immediately, no user response comes back.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::zen_core::workbench::{WorkbenchMode, WorkbenchService};
use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolResultMessage};

const DESCRIPTION: &str = r#"# LaunchUI — display deliverables in a workbench panel

When you produce a deliverable that the user should *view* rather than read as chat text — a rendered HTML report, a Markdown document, a deployed web page, an exposed API endpoint — call LaunchUI to surface it in the workbench panel.

This is a **display-only** tool. It hands the deliverable off to the UI and returns immediately. No user response comes back. (For asking the user something or collecting structured input, use AskUser.)

## When to use

**Rule of thumb: if your task work writes a `.md` or `.html` file to disk, call LaunchUI on it.** Don't just say "I wrote the file to X" in chat — surface it in the panel.

Specifically:
- You wrote one or more `.md` / `.html` files (reports, docs, dashboards, slides, charts, notes, READMEs…) → LaunchUI them. Pass every related file in **one** call so the panel renders them as tabs (e.g. a `.md` source together with its `.html` render, or multi-chapter docs).
- You started a frontend or full-stack service → LaunchUI with the served URL.
- You exposed an HTTP API endpoint → LaunchUI with the URL + usage card.

## When NOT to use
- Short conversational reply (a sentence, a code snippet, a paragraph of explanation) → just reply in chat. The trigger is **a file you wrote**, not formatted prose in chat.
- The `.md`/`.html` you wrote is purely intermediate scratch (e.g. a temp file you immediately re-read and discarded) → no need.
- Asking the user a question or getting input → use AskUser, not LaunchUI.

**Common anti-pattern to avoid:** writing a `.md` report file and then pasting its contents back into chat instead of (or in addition to) calling LaunchUI. The panel is the right surface; chat is for narration of what you did.

## Modes
- **static** — render local HTML / Markdown files
  - `files`: one or more paths. Supported: `.html` `.htm` `.md` `.markdown`
  - **Pass ALL related deliverables in a single call** — multiple files automatically render as tabs in one panel. Don't make multiple LaunchUI calls when the artifacts belong together (e.g. a research `.md` report and the `.html` presentation built from it; a dashboard `.html` and its underlying data `.md`; multiple chapter `.md` files of the same doc).
  - HTML must be **self-contained** (inline CSS / JS / base64 images). External `./style.css` style refs will NOT resolve.
- **web** — show a running frontend or full-stack service
  - `url`: the served page URL. Start the service with Bash first (the tool does not start it for you).
- **backend** — show an API endpoint as an info card
  - `url`: the endpoint
  - `usage`: markdown with call examples (curl, SDK snippet, request/response shape)

## Workflow
LaunchUI does NOT decide what to build — it only surfaces deliverables that your actual task work has already produced. DO NOT invent or fabricate artifacts solely to have something to display.

When your work *does* yield a viewable result:
1. Finish producing the artifact(s) as part of the task (write the file(s), start the service, expose the API)
2. Collect EVERY file/URL that belongs to this deliverable into a single LaunchUI call. If the task produced both a Markdown source/report AND a rendered HTML page, pass both paths together as `files` — the panel renders them as tabs so the user can switch between them.
3. Continue with your next step — the panel opens for the user; no response comes back

## File locations
Usually LaunchUI does NOT decide where files live — by the time you call it, the file already exists on disk and you pass its actual path. The question is only where *you* chose to write it earlier:

- If the user named a destination, write there.
- Otherwise write it under the current working directory like any other generated output.
- For transient deliverables with no natural home in the project tree, `<cwd>/workbench/` is the conventional landing spot.

Do NOT paste rendered output into chat as a substitute — the panel gives the user copy-path, tabs, and history affordances that chat can't."#;

pub struct LaunchUITool {
    workbench: Arc<WorkbenchService>,
}

impl LaunchUITool {
    pub fn new(workbench: Arc<WorkbenchService>) -> Self {
        Self { workbench }
    }

    /// Display-only assistant-facing summary (mirrors `genResultForAssistant`).
    fn result_for_assistant(output: &Value) -> String {
        if let Some(err) = output.get("error").and_then(|v| v.as_str()) {
            return format!("LaunchUI failed: {err}");
        }
        let mut parts = Vec::new();
        let id = output
            .get("artifact_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let mode = output.get("mode").and_then(|v| v.as_str()).unwrap_or("");
        parts.push(format!("Workbench opened (id={id}, mode={mode})."));
        if let Some(files) = output.get("files_resolved").and_then(|v| v.as_array()) {
            if !files.is_empty() {
                let list: Vec<&str> = files.iter().filter_map(|v| v.as_str()).collect();
                parts.push(format!("Files: {}", list.join(", ")));
            }
        }
        if let Some(url) = output.get("url").and_then(|v| v.as_str()) {
            parts.push(format!("URL: {url}"));
        }
        parts.push(
            "The deliverable is now visible to the user in the workbench panel. This is \
             display-only — no response will come back. Continue with the next step."
                .to_string(),
        );
        parts.join("\n")
    }
}

#[async_trait]
impl Tool for LaunchUITool {
    fn name(&self) -> &str {
        "LaunchUI"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["static", "web", "backend"],
                    "description": "Display mode. \"static\": render local html/md files. \"web\": iframe a running service URL. \"backend\": show an API endpoint card with usage examples."
                },
                "title": {
                    "type": "string",
                    "description": "Panel title shown in the workbench header. Keep it short and descriptive (e.g. \"Q3 Report\", \"Auth API\"). Defaults to filename or URL when omitted."
                },
                "files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "mode=static only. One or more html/markdown file paths (absolute or relative to cwd). Files must already exist. Multiple files automatically render as tabs."
                },
                "url": {
                    "type": "string",
                    "description": "mode=web / mode=backend. The URL to display. For \"web\" this is iframed and must be reachable (start the server with Bash first). For \"backend\" this is shown as the API endpoint to call. Must include protocol (http:// or https://)."
                },
                "usage": {
                    "type": "string",
                    "description": "mode=backend only. Markdown showing how to call the API — curl examples, SDK snippets, request/response shape. Rendered with markdown formatting in the panel."
                }
            },
            "required": ["mode"]
        })
    }

    fn is_read_only(&self) -> bool {
        // Writes the workbench manifest; not read-only.
        false
    }

    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolContext<'_>,
    ) -> std::result::Result<(), String> {
        let mode = input.get("mode").and_then(|v| v.as_str()).unwrap_or("");
        match mode {
            "static" => {
                let has_files = input
                    .get("files")
                    .and_then(|v| v.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false);
                if !has_files {
                    return Err("mode=static requires non-empty `files`.".to_string());
                }
            }
            "web" | "backend" => {
                let has_url = input
                    .get("url")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false);
                if !has_url {
                    return Err(format!("mode={mode} requires `url`."));
                }
            }
            other => return Err(format!("unknown mode: {other}")),
        }
        Ok(())
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let mode = input.get("mode").and_then(|v| v.as_str()).unwrap_or("");
        let title = input
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let agent_id = ctx.agent_id;

        let output: Value = match mode {
            "static" => {
                let files: Vec<String> = input
                    .get("files")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                match self.workbench.create_static(&files, title, agent_id) {
                    Ok(artifact) => {
                        let resolved: Vec<String> =
                            artifact.files.iter().map(|f| f.path.clone()).collect();
                        serde_json::json!({
                            "artifact_id": artifact.id,
                            "mode": "static",
                            "opened": true,
                            "files_resolved": resolved,
                        })
                    }
                    Err(e) => serde_json::json!({
                        "artifact_id": "",
                        "mode": "static",
                        "opened": false,
                        "error": e,
                    }),
                }
            }
            "web" | "backend" => {
                let wb_mode = if mode == "web" {
                    WorkbenchMode::Web
                } else {
                    WorkbenchMode::Backend
                };
                let url = input
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let usage = input
                    .get("usage")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let artifact = self
                    .workbench
                    .create_service(wb_mode, url, title, usage, agent_id, None, None);
                serde_json::json!({
                    "artifact_id": artifact.id,
                    "mode": mode,
                    "opened": true,
                    "url": artifact.url,
                })
            }
            other => serde_json::json!({
                "artifact_id": "",
                "mode": other,
                "opened": false,
                "error": format!("unknown mode: {other}"),
            }),
        };

        let result_for_assistant = Self::result_for_assistant(&output);
        Ok(vec![ToolOutput::Result {
            data: output,
            result_for_assistant,
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, input: &Value) -> ToolResultMessage {
        let mode = data.get("mode").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(err) = data.get("error").and_then(|v| v.as_str()) {
            return ToolResultMessage {
                title: "LaunchUI Failed".into(),
                summary: err.to_string(),
                content: serde_json::json!({ "error": err }),
            };
        }
        let title = input
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| mode.to_string());
        let summary = if mode == "static" {
            let n = data
                .get("files_resolved")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("{n} file(s)")
        } else {
            data.get("url")
                .and_then(|v| v.as_str())
                .unwrap_or(mode)
                .to_string()
        };
        ToolResultMessage {
            title: format!("Workbench: {title}"),
            summary,
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        if let Some(t) = input.get("title").and_then(|v| v.as_str()) {
            return format!("LaunchUI: {t}");
        }
        let mode = input.get("mode").and_then(|v| v.as_str()).unwrap_or("");
        if mode == "static" {
            if let Some(files) = input.get("files").and_then(|v| v.as_array()) {
                if let Some(first) = files.first().and_then(|v| v.as_str()) {
                    let base = std::path::Path::new(first)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| first.to_string());
                    let extra = if files.len() > 1 {
                        format!(" (+{})", files.len() - 1)
                    } else {
                        String::new()
                    };
                    return format!("LaunchUI: {base}{extra}");
                }
            }
        }
        if let Some(url) = input.get("url").and_then(|v| v.as_str()) {
            return format!("LaunchUI: {url}");
        }
        format!("LaunchUI: {mode}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zen_core::EventBus;

    fn tmp_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("launchui-test-{}", uuid::Uuid::new_v4()))
    }

    fn ctx<'a>(working_dir: &'a str) -> ToolContext<'a> {
        ToolContext {
            agent_id: "agent-1",
            working_dir,
            agent_data_dir: working_dir,
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        }
    }

    #[tokio::test]
    async fn static_call_creates_artifact() {
        let dir = tmp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("report.md");
        std::fs::write(&file, "# hi").unwrap();
        let wd = dir.to_string_lossy().to_string();

        let svc = Arc::new(WorkbenchService::new(EventBus::new(), "inst1", dir.clone()));
        let tool = LaunchUITool::new(svc.clone());

        let input = serde_json::json!({
            "mode": "static",
            "files": [file.to_string_lossy()],
        });
        tool.validate_input(&input, &ctx(&wd)).await.unwrap();
        let out = tool.call(input, &ctx(&wd)).await.unwrap();
        let ToolOutput::Result {
            data,
            result_for_assistant,
        } = &out[0]
        else {
            panic!("expected Result");
        };
        assert_eq!(data.get("opened").and_then(|v| v.as_bool()), Some(true));
        assert!(data
            .get("artifact_id")
            .unwrap()
            .as_str()
            .unwrap()
            .starts_with("wb_"));
        assert!(result_for_assistant.contains("Workbench opened"));
        assert_eq!(svc.list().len(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn static_missing_file_returns_error_output() {
        let dir = tmp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let wd = dir.to_string_lossy().to_string();
        let svc = Arc::new(WorkbenchService::new(EventBus::new(), "inst1", dir.clone()));
        let tool = LaunchUITool::new(svc);

        let input = serde_json::json!({ "mode": "static", "files": ["ghost.md"] });
        let out = tool.call(input, &ctx(&wd)).await.unwrap();
        let ToolOutput::Result {
            data,
            result_for_assistant,
        } = &out[0]
        else {
            panic!("expected Result");
        };
        assert_eq!(data.get("opened").and_then(|v| v.as_bool()), Some(false));
        assert!(result_for_assistant.starts_with("LaunchUI failed"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn validate_web_requires_url() {
        let dir = tmp_dir();
        let wd = dir.to_string_lossy().to_string();
        let svc = Arc::new(WorkbenchService::new(EventBus::new(), "inst1", dir));
        let tool = LaunchUITool::new(svc);
        let input = serde_json::json!({ "mode": "web" });
        assert!(tool.validate_input(&input, &ctx(&wd)).await.is_err());
    }
}
