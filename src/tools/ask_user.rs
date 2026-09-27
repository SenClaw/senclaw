//! AskUser tool — present questions to the user and wait for answers.
//!
//! Port of TS `node_modules/sema-core/dist/tools/AskUser/`.

use std::collections::HashMap;

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde_json::Value;

use crate::zen_core::{
    AskQuestionItem, AskQuestionOption, AskQuestionRequestData, EngineEvent, Tool, ToolContext,
    ToolOutput, ToolResultMessage,
};

pub struct AskUserTool;

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "AskUser"
    }

    fn description(&self) -> &str {
        "Ask the user questions and wait for their responses"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "description": "1-4 related questions to ask in a single turn",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {
                                "type": "string",
                                "description": "The complete question to ask the user"
                            },
                            "header": {
                                "type": "string",
                                "maxLength": 500,
                                "description": "Short label displayed as a chip/tag (max 500 chars)"
                            },
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string", "description": "Short selectable label (1-5 words)"},
                                        "description": {"type": "string", "description": "One-sentence explanation of what this option means"}
                                    },
                                    "required": ["label", "description"]
                                }
                            },
                            "multiSelect": {
                                "type": "boolean",
                                "default": false,
                                "description": "Allow multiple selections"
                            }
                        },
                        "required": ["question", "header", "options", "multiSelect"]
                    }
                }
            },
            "required": ["questions"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolContext<'_>,
    ) -> std::result::Result<(), String> {
        let questions = input.get("questions").and_then(|v| v.as_array());
        let Some(questions) = questions else {
            return Err("questions array is required".to_string());
        };
        for q in questions {
            let header = q.get("header").and_then(|h| h.as_str()).unwrap_or("");
            if header.len() > 500 {
                return Err(format!(
                    "Header \"{header}\" exceeds maximum length of 500 characters"
                ));
            }
        }
        Ok(())
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let questions_arr = input
            .get("questions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let questions: Vec<AskQuestionItem> = questions_arr
            .iter()
            .map(|q| AskQuestionItem {
                question: q
                    .get("question")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                header: q
                    .get("header")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                options: q
                    .get("options")
                    .and_then(|a| a.as_array())
                    .map(|arr| {
                        arr.iter()
                            .map(|o| AskQuestionOption {
                                label: o
                                    .get("label")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                description: o
                                    .get("description")
                                    .and_then(|s| s.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                multi_select: q
                    .get("multiSelect")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(false),
            })
            .collect();

        let event_bus = ctx
            .event_bus
            .ok_or_else(|| anyhow::anyhow!("EventBus not available"))?;
        let response_registry = ctx
            .response_registry
            .ok_or_else(|| anyhow::anyhow!("ResponseRegistry not available"))?;

        let request_data = AskQuestionRequestData {
            agent_id: ctx.agent_id.to_string(),
            questions: questions.clone(),
            metadata: None,
        };

        // Register response waiter BEFORE emitting
        let rx = response_registry.register_ask_question(ctx.agent_id);

        // Emit the question request
        event_bus.emit(EngineEvent::AskQuestionRequest(request_data));

        // Wait for response (with abort support)
        let answers = tokio::select! {
            result = rx => {
                match result {
                    Ok(response) => response.answers,
                    Err(_) => bail!("Response channel closed"),
                }
            }
            _ = ctx.abort.cancelled() => {
                bail!("Question cancelled by user");
            }
        };

        let _answers_json: Value = serde_json::to_value(&answers).unwrap_or_default();

        Ok(vec![ToolOutput::Result {
            data: serde_json::json!({
                "questions": questions,
                "answers": answers,
            }),
            result_for_assistant: format_answers(&questions, &answers),
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let answers = data.get("answers");
        if let Some(answers) = answers.and_then(|a| a.as_object()) {
            if !answers.is_empty() {
                let count = answers.len();
                let content = answers
                    .iter()
                    .map(|(q, a)| format!("  {}: -> {}", q, a.as_str().unwrap_or("")))
                    .collect::<Vec<_>>()
                    .join("\n");
                return ToolResultMessage {
                    title: "User Response".into(),
                    summary: if count == 1 {
                        "Got 1 answer".into()
                    } else {
                        format!("Got {count} answers")
                    },
                    content: serde_json::json!(content),
                };
            }
        }
        let questions = data
            .get("questions")
            .and_then(|q| q.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        ToolResultMessage {
            title: "Asking User".into(),
            summary: format!(
                "Asked {questions} question{}",
                if questions == 1 { "" } else { "s" }
            ),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        let questions = input.get("questions").and_then(|v| v.as_array());
        match questions
            .and_then(|q| q.first())
            .and_then(|q| q.get("header"))
            .and_then(|h| h.as_str())
        {
            Some(header) => format!("Ask: {header}"),
            None => "Ask user".into(),
        }
    }
}

fn format_answers(questions: &[AskQuestionItem], answers: &HashMap<String, String>) -> String {
    let parts: Vec<String> = answers
        .iter()
        .map(|(q, a)| format!("\"{}\"=\"{}\"", q, a))
        .collect();
    if parts.is_empty() {
        let qlist: Vec<String> = questions
            .iter()
            .map(|q| format!("- {}", q.question))
            .collect();
        format!("Waiting for user to answer:\n{}", qlist.join("\n"))
    } else {
        format!(
            "User's answers: {}. Continue with this information.",
            parts.join(", ")
        )
    }
}
