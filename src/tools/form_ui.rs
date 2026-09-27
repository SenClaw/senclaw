//! FormUI tool — declarative interactive form rendered by the UI.
//!
//! Port of TS `sema-core/dist/tools/FormUI/`. The agent describes a form
//! (typed fields, options, defaults); the tool emits `EngineEvent::FormRequest`
//! and suspends until the user submits (or skips) via the paired
//! `FormResponse` delivered through the [`ResponseRegistry`].

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::Value;

use crate::zen_core::{
    EngineEvent, FormField, FormRequestData, Tool, ToolContext, ToolOutput, ToolResultMessage,
};

const DESCRIPTION: &str = r#"# Interactive form tool
Render a rich form for the user to fill in, then receive structured values back.
Use when you need multiple typed inputs at once (text, numbers, sliders, selects,
multi-select, checkboxes, dates, an editable table) — richer than AskUserQuestion's
option buttons.

Usage notes:
- surface:'inline' for short forms (renders as a chat card); 'dock' for large forms
  (renders in the right-side workbench panel). Both block until the user submits.
- Every field needs a unique `key`; the returned `values` object is keyed by it.
- Pre-fill from conversation context via each field's `default` — the form shows up
  already filled, and the user edits/confirms.
- Mark fields `required: true` to block submission until they are filled.
- Prefer AskUserQuestion when you only need a single multiple-choice decision."#;

pub struct FormUITool;

/// Parse the raw tool input into a typed request. Applies the schema defaults
/// (`surface: "inline"`, `submitLabel: "Submit"`) that serde can't express.
fn parse_input(input: &Value, agent_id: &str) -> std::result::Result<FormRequestData, String> {
    let title = input
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or("title is required")?
        .to_string();
    let surface = match input.get("surface").and_then(|v| v.as_str()) {
        None => "inline".to_string(),
        Some(s @ ("inline" | "dock")) => s.to_string(),
        Some(other) => return Err(format!("Invalid surface: \"{other}\"")),
    };
    let submit_label = input
        .get("submitLabel")
        .and_then(|v| v.as_str())
        .unwrap_or("Submit")
        .to_string();
    let fields_val = input.get("fields").ok_or("fields array is required")?;
    let fields: Vec<FormField> =
        serde_json::from_value(fields_val.clone()).map_err(|e| format!("Invalid fields: {e}"))?;
    if fields.is_empty() || fields.len() > 20 {
        return Err("fields must contain 1-20 items".to_string());
    }
    Ok(FormRequestData {
        agent_id: agent_id.to_string(),
        title,
        surface,
        submit_label,
        fields,
    })
}

/// Cross-field constraint the schema can't express: value-bearing keys unique.
fn find_duplicate_key(fields: &[FormField]) -> Option<&str> {
    let mut seen = std::collections::HashSet::new();
    fields
        .iter()
        .filter_map(|f| f.key())
        .find(|k| !seen.insert(*k))
}

#[async_trait]
impl Tool for FormUITool {
    fn name(&self) -> &str {
        "FormUI"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        let field_base = |extra: Value| -> Value {
            let mut props = serde_json::json!({
                "key": {"type": "string", "description": "Unique key for this field; becomes a key in the returned values object."},
                "label": {"type": "string", "description": "Human-readable label shown above the control."},
                "required": {"type": "boolean", "default": false, "description": "If true, submit is blocked until this field has a value."},
                "help": {"type": "string", "description": "Optional one-line helper text shown under the control."}
            });
            if let (Some(base), Some(add)) = (props.as_object_mut(), extra.as_object()) {
                for (k, v) in add {
                    base.insert(k.clone(), v.clone());
                }
            }
            props
        };
        let option_item = serde_json::json!({
            "type": "object",
            "properties": {
                "label": {"type": "string"},
                "value": {"type": "string", "description": "The value returned when this option is chosen."}
            },
            "required": ["label", "value"]
        });
        serde_json::json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "Title shown at the top of the form."},
                "surface": {
                    "type": "string",
                    "enum": ["inline", "dock"],
                    "default": "inline",
                    "description": "inline = card in chat stream (small forms); dock = right-side workbench panel (large forms). Both block until the user submits."
                },
                "submitLabel": {"type": "string", "default": "Submit", "description": "Label for the submit button."},
                "fields": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 20,
                    "description": "Ordered list of form fields, rendered top to bottom.",
                    "items": {
                        "type": "object",
                        "oneOf": [
                            {"properties": field_base(serde_json::json!({
                                "type": {"const": "text"},
                                "placeholder": {"type": "string"},
                                "maxLength": {"type": "integer", "minimum": 1},
                                "default": {"type": "string"}
                            })), "required": ["type", "key", "label"]},
                            {"properties": field_base(serde_json::json!({
                                "type": {"const": "textarea"},
                                "placeholder": {"type": "string"},
                                "maxLength": {"type": "integer", "minimum": 1},
                                "rows": {"type": "integer", "minimum": 2, "maximum": 20, "default": 4},
                                "default": {"type": "string"}
                            })), "required": ["type", "key", "label"]},
                            {"properties": field_base(serde_json::json!({
                                "type": {"const": "number"},
                                "min": {"type": "number"},
                                "max": {"type": "number"},
                                "step": {"type": "number", "exclusiveMinimum": 0},
                                "default": {"type": "number"}
                            })), "required": ["type", "key", "label"]},
                            {"properties": field_base(serde_json::json!({
                                "type": {"const": "slider"},
                                "min": {"type": "number"},
                                "max": {"type": "number"},
                                "step": {"type": "number", "exclusiveMinimum": 0, "default": 1},
                                "default": {"type": "number"}
                            })), "required": ["type", "key", "label", "min", "max"]},
                            {"properties": field_base(serde_json::json!({
                                "type": {"const": "select"},
                                "options": {"type": "array", "minItems": 1, "items": option_item},
                                "default": {"type": "string"}
                            })), "required": ["type", "key", "label", "options"]},
                            {"properties": field_base(serde_json::json!({
                                "type": {"const": "radio"},
                                "options": {"type": "array", "minItems": 2, "items": option_item},
                                "default": {"type": "string"}
                            })), "required": ["type", "key", "label", "options"]},
                            {"properties": field_base(serde_json::json!({
                                "type": {"const": "multiselect"},
                                "options": {"type": "array", "minItems": 1, "items": option_item},
                                "default": {"type": "array", "items": {"type": "string"}}
                            })), "required": ["type", "key", "label", "options"]},
                            {"properties": field_base(serde_json::json!({
                                "type": {"const": "checkbox"},
                                "default": {"type": "boolean", "default": false}
                            })), "required": ["type", "key", "label"]},
                            {"properties": field_base(serde_json::json!({
                                "type": {"const": "date"},
                                "min": {"type": "string"},
                                "max": {"type": "string"},
                                "default": {"type": "string"}
                            })), "required": ["type", "key", "label"]},
                            {"properties": {
                                "type": {"const": "static_text"},
                                "text": {"type": "string"},
                                "variant": {"type": "string", "enum": ["heading", "body", "divider"], "default": "body"}
                            }, "required": ["type", "text"]},
                            {"properties": field_base(serde_json::json!({
                                "type": {"const": "editable_table"},
                                "columns": {
                                    "type": "array",
                                    "minItems": 1,
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "key": {"type": "string"},
                                            "label": {"type": "string"},
                                            "type": {"type": "string", "enum": ["text", "number"], "default": "text"}
                                        },
                                        "required": ["key", "label"]
                                    }
                                },
                                "rows": {"type": "array", "items": {"type": "object"}, "default": []},
                                "allowAddRow": {"type": "boolean", "default": true}
                            })), "required": ["type", "key", "label", "columns"]}
                        ]
                    }
                }
            },
            "required": ["title", "fields"]
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
        let request = parse_input(input, ctx.agent_id)?;
        if let Some(dup) = find_duplicate_key(&request.fields) {
            return Err(format!("Duplicate field key: \"{dup}\""));
        }
        Ok(())
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let request = parse_input(&input, ctx.agent_id).map_err(|e| anyhow::anyhow!(e))?;

        let event_bus = ctx
            .event_bus
            .ok_or_else(|| anyhow::anyhow!("EventBus not available"))?;
        let response_registry = ctx
            .response_registry
            .ok_or_else(|| anyhow::anyhow!("ResponseRegistry not available"))?;

        let rx = response_registry.register_form(ctx.agent_id);
        event_bus.emit(EngineEvent::FormRequest(request.clone()));

        let response = tokio::select! {
            result = rx => {
                match result {
                    Ok(response) => response,
                    Err(_) => bail!("Response channel closed"),
                }
            }
            _ = ctx.abort.cancelled() => {
                bail!("User cancelled the form");
            }
        };

        let result_for_assistant = format_response(&response.values, response.submitted);
        Ok(vec![ToolOutput::Result {
            data: serde_json::json!({
                "title": request.title,
                "surface": request.surface,
                "submitLabel": request.submit_label,
                "fields": request.fields,
                "values": response.values,
                "submitted": response.submitted,
            }),
            result_for_assistant,
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let submitted = data
            .get("submitted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !submitted {
            return ToolResultMessage {
                title: "Form Skipped".into(),
                summary: "User skipped the form".into(),
                content: serde_json::json!(""),
            };
        }
        let empty = serde_json::Map::new();
        let values = data
            .get("values")
            .and_then(|v| v.as_object())
            .unwrap_or(&empty);
        let content = values
            .iter()
            .map(|(k, v)| format!("· {k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n");
        ToolResultMessage {
            title: "Form Submitted".into(),
            summary: format!(
                "Got {} field{}",
                values.len(),
                if values.len() == 1 { "" } else { "s" }
            ),
            content: serde_json::json!(content),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        match input.get("title").and_then(|t| t.as_str()) {
            Some(title) => format!("Form: {title}"),
            None => "Interactive form".into(),
        }
    }
}

fn format_response(values: &std::collections::HashMap<String, Value>, submitted: bool) -> String {
    if !submitted {
        return "User skipped the form. No values were provided; proceed with sensible defaults or ask again.".to_string();
    }
    let parts: Vec<String> = values.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!(
        "User submitted the form: {}. You can now continue with these values.",
        parts.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zen_core::{EventBus, FormResponseData, ResponseRegistry};

    fn sample_input() -> Value {
        serde_json::json!({
            "title": "Deploy settings",
            "fields": [
                {"type": "static_text", "text": "Configure the deploy", "variant": "heading"},
                {"type": "text", "key": "env", "label": "Environment", "required": true, "default": "staging"},
                {"type": "slider", "key": "replicas", "label": "Replicas", "min": 1, "max": 10, "default": 3},
                {"type": "multiselect", "key": "regions", "label": "Regions",
                 "options": [{"label": "US", "value": "us"}, {"label": "EU", "value": "eu"}]},
                {"type": "editable_table", "key": "envvars", "label": "Env vars",
                 "columns": [{"key": "name", "label": "Name"}, {"key": "value", "label": "Value"}]}
            ]
        })
    }

    fn test_ctx<'a>(
        bus: &'a EventBus,
        registry: &'a ResponseRegistry,
        abort: &tokio_util::sync::CancellationToken,
    ) -> ToolContext<'a> {
        ToolContext {
            agent_id: "main",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: abort.clone(),
            event_bus: Some(bus),
            response_registry: Some(registry),
            edit_format: Default::default(),
        }
    }

    #[test]
    fn parse_input_applies_defaults() {
        let request = parse_input(&sample_input(), "main").unwrap();
        assert_eq!(request.surface, "inline");
        assert_eq!(request.submit_label, "Submit");
        assert_eq!(request.fields.len(), 5);
        assert!(matches!(request.fields[0], FormField::StaticText { .. }));
        assert_eq!(request.fields[1].key(), Some("env"));
        assert!(request.fields[1].required());
    }

    #[test]
    fn parse_input_rejects_bad_surface_and_unknown_type() {
        let mut input = sample_input();
        input["surface"] = serde_json::json!("modal");
        assert!(parse_input(&input, "main").is_err());

        let bad = serde_json::json!({
            "title": "t",
            "fields": [{"type": "color_picker", "key": "c", "label": "C"}]
        });
        assert!(parse_input(&bad, "main")
            .unwrap_err()
            .contains("Invalid fields"));
    }

    #[test]
    fn form_field_serde_round_trip() {
        let request = parse_input(&sample_input(), "main").unwrap();
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["submitLabel"], "Submit");
        assert_eq!(json["fields"][1]["type"], "text");
        assert_eq!(json["fields"][4]["type"], "editable_table");
        let back: FormRequestData = serde_json::from_value(json).unwrap();
        assert_eq!(back.fields, request.fields);
    }

    #[tokio::test]
    async fn validate_rejects_duplicate_keys() {
        let input = serde_json::json!({
            "title": "t",
            "fields": [
                {"type": "text", "key": "name", "label": "A"},
                {"type": "number", "key": "name", "label": "B"}
            ]
        });
        let bus = EventBus::new();
        let registry = ResponseRegistry::new();
        let abort = tokio_util::sync::CancellationToken::new();
        let ctx = test_ctx(&bus, &registry, &abort);
        let err = FormUITool.validate_input(&input, &ctx).await.unwrap_err();
        assert!(err.contains("Duplicate field key"), "{err}");
    }

    #[tokio::test]
    async fn call_blocks_until_response_delivered() {
        let bus = EventBus::new();
        let registry = ResponseRegistry::new();
        let abort = tokio_util::sync::CancellationToken::new();
        let mut rx = bus.subscribe();

        let ctx = test_ctx(&bus, &registry, &abort);
        let call = FormUITool.call(sample_input(), &ctx);
        tokio::pin!(call);

        // Drive the call until it emits the request, then answer it.
        let outputs = tokio::select! {
            biased;
            outputs = &mut call => outputs, // should NOT complete yet
            _ = tokio::task::yield_now() => {
                let event = rx.recv().await.unwrap();
                let EngineEvent::FormRequest(request) = event else {
                    panic!("expected FormRequest");
                };
                assert_eq!(request.title, "Deploy settings");
                let mut values = std::collections::HashMap::new();
                values.insert("env".to_string(), serde_json::json!("prod"));
                assert!(registry.deliver_form(FormResponseData {
                    agent_id: request.agent_id,
                    values,
                    submitted: true,
                }));
                call.await
            }
        };

        let outputs = outputs.unwrap();
        let ToolOutput::Result {
            data,
            result_for_assistant,
        } = &outputs[0]
        else {
            panic!("expected Result output");
        };
        assert_eq!(data["submitted"], serde_json::json!(true));
        assert_eq!(data["values"]["env"], serde_json::json!("prod"));
        assert!(result_for_assistant.contains("env=\"prod\""));
    }

    #[tokio::test]
    async fn call_cancelled_by_abort() {
        let bus = EventBus::new();
        let registry = ResponseRegistry::new();
        let abort = tokio_util::sync::CancellationToken::new();
        abort.cancel();
        let ctx = test_ctx(&bus, &registry, &abort);
        let err = FormUITool.call(sample_input(), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("cancelled"));
    }

    #[test]
    fn skipped_result_message() {
        let data = serde_json::json!({"submitted": false, "values": {}});
        let msg = FormUITool.gen_tool_result_message(&data, &serde_json::json!({}));
        assert_eq!(msg.title, "Form Skipped");
    }
}
