//! `widget_list` tool — the discovery half of the chat-widget feature.
//!
//! Returns the widget catalog usable from the chat box: the built-in
//! `emit_widget` template kinds plus every enabled Space-App / plugin widget
//! whose `surfaces` include `"chat"` (with id, description and params schema).
//! Deferred by default — the `widget` skill and ToolSearch surface it when the
//! agent needs an app widget, so it costs nothing in the base prompt.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolResultMessage};

const DESCRIPTION: &str = "List the widgets available for emit_widget: built-in kinds \
(chart/image/clock/weather/video/audio) plus widgets provided by installed Space Apps and \
plugins (id, description, params schema). Call this before emitting a kind \"app\" widget.";

pub struct WidgetListTool;

#[async_trait]
impl Tool for WidgetListTool {
    fn name(&self) -> &str {
        "widget_list"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn should_defer(&self) -> bool {
        // Only needed when the agent wants an app widget; ToolSearch + the
        // `widget` skill point here. Keeps the base prompt slim.
        true
    }

    fn search_hint(&self) -> String {
        "list available chat widgets from apps and plugins".to_string()
    }

    async fn call(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let catalog: Vec<crate::widgets::WidgetDef> = match crate::widgets::global() {
            Some(reg) => {
                // The catalog reads SQLite + config + plugin dirs — keep it
                // off the async runtime thread.
                tokio::task::spawn_blocking(move || reg.catalog()).await?
            }
            None => Vec::new(),
        };
        let chat_widgets: Vec<&crate::widgets::WidgetDef> = catalog
            .iter()
            .filter(|d| d.enabled && d.surfaces.iter().any(|s| s == "chat"))
            .collect();

        let mut lines: Vec<String> = Vec::new();
        for d in &chat_widgets {
            if d.source == "builtin" {
                continue; // built-ins are already documented in emit_widget's description
            }
            let params = d
                .params
                .as_ref()
                .and_then(|p| p.get("properties"))
                .and_then(|p| p.as_object())
                .map(|p| p.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            let mut line = format!("- {} — {}: {}", d.id, d.name, d.description);
            if !params.is_empty() {
                line.push_str(&format!(" (params: {params})"));
            }
            lines.push(line);
        }
        let result_for_assistant = if lines.is_empty() {
            "No app/plugin widgets are installed. Built-in emit_widget kinds remain available: \
             chart | image | clock | weather | video | audio."
                .to_string()
        } else {
            format!(
                "App widgets available via emit_widget kind \"app\" (pass `widget` + `params`):\n{}\n\
                 Built-in kinds also available: chart | image | clock | weather | video | audio.",
                lines.join("\n")
            )
        };

        let data = serde_json::json!({
            "widgets": chat_widgets.iter().map(|d| serde_json::to_value(d).unwrap_or_default()).collect::<Vec<_>>(),
        });
        Ok(vec![ToolOutput::Result {
            data,
            result_for_assistant,
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let n = data
            .get("widgets")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        ToolResultMessage {
            title: "Widgets".into(),
            summary: format!("{n} widget khả dụng cho chat"),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, _input: &Value) -> String {
        "Widget list".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_builtins_only_without_registry() {
        // No global registry in the test process → empty catalog, and the
        // assistant text must still point at the built-in kinds.
        let bus = crate::zen_core::EventBus::new();
        let abort = tokio_util::sync::CancellationToken::new();
        let ctx = ToolContext {
            agent_id: "main",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: abort.clone(),
            event_bus: Some(&bus),
            response_registry: None,
            edit_format: Default::default(),
        };
        let out = WidgetListTool
            .call(serde_json::json!({}), &ctx)
            .await
            .unwrap();
        let ToolOutput::Result {
            result_for_assistant,
            ..
        } = &out[0]
        else {
            panic!("expected Result");
        };
        assert!(result_for_assistant.contains("chart"), "{result_for_assistant}");
    }
}
