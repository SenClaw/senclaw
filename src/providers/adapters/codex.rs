//! Codex adapter — OpenAI's **Responses** API.
//!
//! Codex does not speak `chat/completions`. It takes a flat `input` array of
//! typed items (messages, function calls, function outputs) and replies with a
//! typed SSE event stream. That is different enough from the chat API that it
//! gets its own adapter rather than a flag on the OpenAI one.
//!
//! The endpoint only streams, so there is no non-streaming branch here; the
//! caller's `stream` flag is honoured by buffering the stream when it is false.
//!
//! Shape of a request:
//! ```json
//! {
//!   "model": "gpt-5.5",
//!   "instructions": "<system prompt>",
//!   "input": [
//!     {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
//!     {"type":"function_call","call_id":"c1","name":"read","arguments":"{}"},
//!     {"type":"function_call_output","call_id":"c1","output":"..."}
//!   ],
//!   "tools": [{"type":"function","name":"read","parameters":{...}}],
//!   "stream": true
//! }
//! ```

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::zen_core::query_llm::{build_assistant_message, post_authed};
use crate::zen_core::{ContentBlock, Message, ModelProfile, RawUsage, Tool};

/// Send one turn to Codex and collect the assistant reply.
pub async fn query_codex(
    client: &Client,
    messages: &[Message],
    system_prompt: &str,
    tools: &[Arc<dyn Tool>],
    cancel: &CancellationToken,
    profile: &ModelProfile,
) -> Result<Message> {
    let url = format!("{}/responses", profile.base_url.trim_end_matches('/'));

    let mut body = serde_json::json!({
        "model": profile.model_name,
        "input": input_items(messages),
        // Codex rejects a non-streaming request outright.
        "stream": true,
        // Don't ask the backend to retain the conversation; SenClaw owns history.
        "store": false,
    });

    if !system_prompt.is_empty() {
        body["instructions"] = Value::String(system_prompt.to_string());
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(responses_tools(tools));
    }

    if cancel.is_cancelled() {
        bail!("Request cancelled before send");
    }

    debug!("[codex] POST {url}");
    let response = post_authed(client, &url, profile, &body)
        .await
        .context("Codex request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("Codex API error ({status}): {text}");
    }

    parse_stream(response, cancel).await
}

/// Convert SenClaw's message history into Responses `input` items.
///
/// One message can expand into several items: an assistant turn that both
/// spoke and called a tool becomes a `message` item plus one `function_call`
/// item per call, which is the flat shape the API expects.
pub(crate) fn input_items(messages: &[Message]) -> Vec<Value> {
    let mut items: Vec<Value> = Vec::new();

    for msg in messages {
        let role = msg.message.role.as_str();
        let is_assistant = role == "assistant";
        // Responses uses different content-part types per direction.
        let text_type = if is_assistant {
            "output_text"
        } else {
            "input_text"
        };

        let mut parts: Vec<Value> = Vec::new();

        for block in &msg.message.content {
            match block {
                ContentBlock::Text { text } => {
                    if !text.is_empty() {
                        parts.push(serde_json::json!({ "type": text_type, "text": text }));
                    }
                }
                ContentBlock::Image { source } => {
                    // Only inbound images are representable; the API takes a
                    // data URL rather than Anthropic's split source object.
                    if !is_assistant {
                        parts.push(serde_json::json!({
                            "type": "input_image",
                            "image_url": format!(
                                "data:{};base64,{}",
                                source.media_type, source.data
                            ),
                        }));
                    }
                }
                ContentBlock::ToolUse { id, name, input } => {
                    // Flush any text collected so far so ordering is preserved.
                    if !parts.is_empty() {
                        items.push(message_item(role, std::mem::take(&mut parts)));
                    }
                    items.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                    }));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    if !parts.is_empty() {
                        items.push(message_item(role, std::mem::take(&mut parts)));
                    }
                    // The Responses API has no error flag on an output item;
                    // an error is just the text the model reads back.
                    items.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": content,
                    }));
                }
                // Reasoning is opaque and server-side; replaying our own
                // rendering of it would confuse the model.
                ContentBlock::Thinking { .. } => {}
                ContentBlock::ControlSignal { .. } => {}
            }
        }

        if !parts.is_empty() {
            items.push(message_item(role, parts));
        }
    }

    items
}

fn message_item(role: &str, content: Vec<Value>) -> Value {
    serde_json::json!({
        "type": "message",
        "role": role,
        "content": content,
    })
}

/// Tool declarations in Responses shape — flat, unlike chat/completions where
/// they nest under `function`.
pub(crate) fn responses_tools(tools: &[Arc<dyn Tool>]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "name": t.name(),
                "description": t.description(),
                "parameters": t.input_schema(),
            })
        })
        .collect()
}

/// Accumulated state while reading the event stream.
#[derive(Default)]
pub(crate) struct StreamAccumulator {
    pub text: String,
    pub reasoning: String,
    /// Tool calls keyed by the API's `item_id`, so argument deltas can be
    /// appended to the right one when several are in flight.
    calls: Vec<PendingCall>,
    pub usage: Option<RawUsage>,
    pub error: Option<String>,
}

#[derive(Default, Clone)]
struct PendingCall {
    item_id: String,
    call_id: String,
    name: String,
    arguments: String,
}

impl StreamAccumulator {
    /// Fold one SSE event into the accumulator.
    ///
    /// Unknown event types are ignored rather than rejected — OpenAI adds
    /// events over time and an unrecognised one is not an error.
    pub(crate) fn apply(&mut self, event: &Value) {
        match event["type"].as_str().unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(d) = event["delta"].as_str() {
                    self.text.push_str(d);
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(d) = event["delta"].as_str() {
                    self.reasoning.push_str(d);
                }
            }
            "response.output_item.added" => {
                let item = &event["item"];
                if item["type"] == "function_call" {
                    self.calls.push(PendingCall {
                        item_id: event["item_id"]
                            .as_str()
                            .or_else(|| item["id"].as_str())
                            .unwrap_or_default()
                            .to_string(),
                        call_id: item["call_id"].as_str().unwrap_or_default().to_string(),
                        name: item["name"].as_str().unwrap_or_default().to_string(),
                        arguments: item["arguments"].as_str().unwrap_or_default().to_string(),
                    });
                }
            }
            "response.function_call_arguments.delta" => {
                let item_id = event["item_id"].as_str().unwrap_or_default();
                let delta = event["delta"].as_str().unwrap_or_default();
                match self.calls.iter_mut().find(|c| c.item_id == item_id) {
                    Some(call) => call.arguments.push_str(delta),
                    // Some streams deliver deltas before the `added` event.
                    None => self.calls.push(PendingCall {
                        item_id: item_id.to_string(),
                        arguments: delta.to_string(),
                        ..Default::default()
                    }),
                }
            }
            "response.output_item.done" => {
                let item = &event["item"];
                if item["type"] != "function_call" {
                    return;
                }
                let item_id = event["item_id"]
                    .as_str()
                    .or_else(|| item["id"].as_str())
                    .unwrap_or_default();
                let call_id = item["call_id"].as_str().unwrap_or_default();
                let name = item["name"].as_str().unwrap_or_default();
                let args = item["arguments"].as_str().unwrap_or_default();

                match self.calls.iter_mut().find(|c| c.item_id == item_id) {
                    Some(call) => {
                        // The done event carries the authoritative values.
                        if !call_id.is_empty() {
                            call.call_id = call_id.to_string();
                        }
                        if !name.is_empty() {
                            call.name = name.to_string();
                        }
                        if !args.is_empty() {
                            call.arguments = args.to_string();
                        }
                    }
                    None => self.calls.push(PendingCall {
                        item_id: item_id.to_string(),
                        call_id: call_id.to_string(),
                        name: name.to_string(),
                        arguments: args.to_string(),
                    }),
                }
            }
            "response.completed" => {
                let usage = &event["response"]["usage"];
                if !usage.is_null() {
                    self.usage = RawUsage::from_json(usage);
                }
            }
            "response.failed" | "error" => {
                let message = event["response"]["error"]["message"]
                    .as_str()
                    .or_else(|| event["error"]["message"].as_str())
                    .or_else(|| event["message"].as_str())
                    .unwrap_or("Codex stream reported an error");
                self.error = Some(message.to_string());
            }
            _ => {}
        }
    }

    /// Tool calls in the shape `build_assistant_message` expects.
    pub(crate) fn tool_calls(&self) -> Vec<Value> {
        self.calls
            .iter()
            .filter(|c| !c.name.is_empty())
            .map(|c| {
                serde_json::json!({
                    // Fall back to item_id so a call is never anonymous — the
                    // agent loop keys tool results off this.
                    "id": if c.call_id.is_empty() { &c.item_id } else { &c.call_id },
                    "name": c.name,
                    "function": {
                        "name": c.name,
                        "arguments": if c.arguments.is_empty() { "{}" } else { &c.arguments },
                    },
                })
            })
            .collect()
    }

    /// Build the assistant message this stream described.
    pub(crate) fn into_message(self) -> Result<Message> {
        if let Some(err) = &self.error {
            bail!("Codex API error: {err}");
        }
        let calls = self.tool_calls();
        build_assistant_message(&self.text, &self.reasoning, &calls, self.usage)
    }
}

async fn parse_stream(response: reqwest::Response, cancel: &CancellationToken) -> Result<Message> {
    let mut stream = response.bytes_stream();
    let mut acc = StreamAccumulator::default();
    // SSE frames can split mid-line across chunks.
    let mut pending = String::new();

    while let Some(chunk) = stream.next().await {
        if cancel.is_cancelled() {
            bail!("Stream cancelled");
        }
        let chunk = chunk.context("Codex stream chunk error")?;
        pending.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(idx) = pending.find('\n') {
            let line = pending[..idx].trim().to_string();
            pending.drain(..=idx);
            if let Some(event) = parse_sse_line(&line) {
                acc.apply(&event);
            }
        }
    }

    // Whatever is left after the stream closes, without a trailing newline.
    if let Some(event) = parse_sse_line(pending.trim()) {
        acc.apply(&event);
    }

    acc.into_message()
}

/// Extract the JSON payload from one SSE line, if it carries one.
pub(crate) fn parse_sse_line(line: &str) -> Option<Value> {
    let payload = line.strip_prefix("data:")?.trim();
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    serde_json::from_str(payload).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zen_core::{ImageSource, MessagePayload};

    fn msg(role: &str, content: Vec<ContentBlock>) -> Message {
        Message {
            msg_type: role.to_string(),
            message: MessagePayload {
                role: role.to_string(),
                content,
            },
            uuid: "u".into(),
            usage: None,
        }
    }

    fn text(t: &str) -> ContentBlock {
        ContentBlock::Text { text: t.into() }
    }

    #[test]
    fn user_text_becomes_an_input_text_message_item() {
        let items = input_items(&[msg("user", vec![text("hello")])]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[0]["content"][0]["text"], "hello");
    }

    #[test]
    fn assistant_text_uses_output_text_parts() {
        let items = input_items(&[msg("assistant", vec![text("hi back")])]);
        assert_eq!(items[0]["content"][0]["type"], "output_text");
    }

    #[test]
    fn empty_text_blocks_are_dropped() {
        let items = input_items(&[msg("user", vec![text("")])]);
        assert!(items.is_empty());
    }

    #[test]
    fn tool_use_becomes_a_flat_function_call_item() {
        let items = input_items(&[msg(
            "assistant",
            vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "read_file".into(),
                input: serde_json::json!({ "path": "a.txt" }),
            }],
        )]);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["call_id"], "call_1");
        assert_eq!(items[0]["name"], "read_file");
        // Arguments are a JSON *string*, not an object.
        let args = items[0]["arguments"].as_str().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(args).unwrap()["path"],
            "a.txt"
        );
    }

    #[test]
    fn text_before_a_tool_call_keeps_its_order() {
        let items = input_items(&[msg(
            "assistant",
            vec![
                text("let me look"),
                ContentBlock::ToolUse {
                    id: "c1".into(),
                    name: "read".into(),
                    input: serde_json::json!({}),
                },
            ],
        )]);

        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[1]["type"], "function_call");
    }

    #[test]
    fn tool_result_becomes_a_function_call_output_item() {
        let items = input_items(&[msg(
            "user",
            vec![ContentBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: "file contents".into(),
                is_error: false,
            }],
        )]);

        assert_eq!(items[0]["type"], "function_call_output");
        assert_eq!(items[0]["call_id"], "c1");
        assert_eq!(items[0]["output"], "file contents");
    }

    #[test]
    fn an_errored_tool_result_still_round_trips_its_text() {
        let items = input_items(&[msg(
            "user",
            vec![ContentBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: "boom".into(),
                is_error: true,
            }],
        )]);
        assert_eq!(items[0]["output"], "boom");
    }

    #[test]
    fn thinking_and_control_blocks_are_not_replayed() {
        let items = input_items(&[msg(
            "assistant",
            vec![
                ContentBlock::Thinking {
                    thinking: "hmm".into(),
                },
                ContentBlock::ControlSignal {
                    signal_type: "x".into(),
                    payload: Value::Null,
                },
            ],
        )]);
        assert!(items.is_empty());
    }

    #[test]
    fn images_become_data_urls_on_inbound_messages_only() {
        let image = ContentBlock::Image {
            source: ImageSource {
                source_type: "base64".into(),
                media_type: "image/png".into(),
                data: "AAAA".into(),
            },
        };
        let items = input_items(&[msg("user", vec![image.clone()])]);
        assert_eq!(items[0]["content"][0]["type"], "input_image");
        assert_eq!(
            items[0]["content"][0]["image_url"],
            "data:image/png;base64,AAAA"
        );

        // An assistant-side image has no representation and is skipped.
        assert!(input_items(&[msg("assistant", vec![image])]).is_empty());
    }

    #[test]
    fn sse_lines_yield_only_real_payloads() {
        assert!(parse_sse_line("data: {\"type\":\"x\"}").is_some());
        assert!(parse_sse_line("data: [DONE]").is_none());
        assert!(parse_sse_line("data:").is_none());
        assert!(parse_sse_line("event: response.completed").is_none());
        assert!(parse_sse_line("").is_none());
        assert!(parse_sse_line("data: not json").is_none());
    }

    fn feed(acc: &mut StreamAccumulator, events: &[Value]) {
        for e in events {
            acc.apply(e);
        }
    }

    #[test]
    fn text_deltas_accumulate_in_order() {
        let mut acc = StreamAccumulator::default();
        feed(
            &mut acc,
            &[
                serde_json::json!({"type":"response.output_text.delta","delta":"Hello"}),
                serde_json::json!({"type":"response.output_text.delta","delta":", world"}),
            ],
        );
        assert_eq!(acc.text, "Hello, world");
    }

    #[test]
    fn reasoning_deltas_are_kept_separate_from_text() {
        let mut acc = StreamAccumulator::default();
        feed(
            &mut acc,
            &[
                serde_json::json!({"type":"response.reasoning_summary_text.delta","delta":"think"}),
                serde_json::json!({"type":"response.output_text.delta","delta":"say"}),
            ],
        );
        assert_eq!(acc.reasoning, "think");
        assert_eq!(acc.text, "say");
    }

    #[test]
    fn a_streamed_tool_call_assembles_from_its_deltas() {
        let mut acc = StreamAccumulator::default();
        feed(
            &mut acc,
            &[
                serde_json::json!({
                    "type":"response.output_item.added",
                    "item_id":"i1",
                    "item":{"type":"function_call","call_id":"c1","name":"read","arguments":""}
                }),
                serde_json::json!({
                    "type":"response.function_call_arguments.delta",
                    "item_id":"i1","delta":"{\"path\":"
                }),
                serde_json::json!({
                    "type":"response.function_call_arguments.delta",
                    "item_id":"i1","delta":"\"a.txt\"}"
                }),
            ],
        );

        let calls = acc.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "c1");
        assert_eq!(calls[0]["name"], "read");
        assert_eq!(
            calls[0]["function"]["arguments"].as_str().unwrap(),
            "{\"path\":\"a.txt\"}"
        );
    }

    #[test]
    fn two_concurrent_tool_calls_do_not_mix_their_arguments() {
        let mut acc = StreamAccumulator::default();
        feed(
            &mut acc,
            &[
                serde_json::json!({"type":"response.output_item.added","item_id":"i1",
                    "item":{"type":"function_call","call_id":"c1","name":"read","arguments":""}}),
                serde_json::json!({"type":"response.output_item.added","item_id":"i2",
                    "item":{"type":"function_call","call_id":"c2","name":"write","arguments":""}}),
                serde_json::json!({"type":"response.function_call_arguments.delta",
                    "item_id":"i2","delta":"{\"w\":1}"}),
                serde_json::json!({"type":"response.function_call_arguments.delta",
                    "item_id":"i1","delta":"{\"r\":2}"}),
            ],
        );

        let calls = acc.tool_calls();
        assert_eq!(calls.len(), 2);
        let read = calls.iter().find(|c| c["name"] == "read").unwrap();
        let write = calls.iter().find(|c| c["name"] == "write").unwrap();
        assert_eq!(read["function"]["arguments"].as_str().unwrap(), "{\"r\":2}");
        assert_eq!(
            write["function"]["arguments"].as_str().unwrap(),
            "{\"w\":1}"
        );
    }

    #[test]
    fn the_done_event_overrides_partial_delta_state() {
        let mut acc = StreamAccumulator::default();
        feed(
            &mut acc,
            &[
                serde_json::json!({"type":"response.output_item.added","item_id":"i1",
                    "item":{"type":"function_call","call_id":"","name":"","arguments":""}}),
                serde_json::json!({"type":"response.function_call_arguments.delta",
                    "item_id":"i1","delta":"{\"partial\""}),
                serde_json::json!({"type":"response.output_item.done","item_id":"i1",
                    "item":{"type":"function_call","call_id":"c9","name":"final_tool",
                            "arguments":"{\"complete\":true}"}}),
            ],
        );

        let calls = acc.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "c9");
        assert_eq!(calls[0]["name"], "final_tool");
        assert_eq!(
            calls[0]["function"]["arguments"].as_str().unwrap(),
            "{\"complete\":true}"
        );
    }

    #[test]
    fn a_delta_arriving_before_its_added_event_is_not_lost() {
        let mut acc = StreamAccumulator::default();
        feed(
            &mut acc,
            &[
                serde_json::json!({"type":"response.function_call_arguments.delta",
                    "item_id":"i1","delta":"{\"a\":1}"}),
                serde_json::json!({"type":"response.output_item.done","item_id":"i1",
                    "item":{"type":"function_call","call_id":"c1","name":"t","arguments":""}}),
            ],
        );
        let calls = acc.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0]["function"]["arguments"].as_str().unwrap(),
            "{\"a\":1}"
        );
    }

    #[test]
    fn non_function_output_items_are_ignored() {
        let mut acc = StreamAccumulator::default();
        feed(
            &mut acc,
            &[
                serde_json::json!({"type":"response.output_item.added","item_id":"i1",
                "item":{"type":"message","role":"assistant"}}),
            ],
        );
        assert!(acc.tool_calls().is_empty());
    }

    #[test]
    fn usage_is_captured_from_the_completed_event() {
        let mut acc = StreamAccumulator::default();
        feed(
            &mut acc,
            &[serde_json::json!({
                "type":"response.completed",
                "response":{"usage":{"input_tokens":10,"output_tokens":5}}
            })],
        );
        let usage = acc.usage.expect("usage recorded");
        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.output_tokens, Some(5));
    }

    #[test]
    fn a_failed_response_surfaces_as_an_error() {
        let mut acc = StreamAccumulator::default();
        feed(
            &mut acc,
            &[serde_json::json!({
                "type":"response.failed",
                "response":{"error":{"message":"quota exhausted"}}
            })],
        );
        let err = acc.into_message().unwrap_err().to_string();
        assert!(err.contains("quota exhausted"), "{err}");
    }

    #[test]
    fn unknown_events_are_ignored_rather_than_failing_the_turn() {
        let mut acc = StreamAccumulator::default();
        feed(
            &mut acc,
            &[
                serde_json::json!({"type":"response.some_future_event","data":1}),
                serde_json::json!({"type":"response.output_text.delta","delta":"ok"}),
            ],
        );
        let msg = acc.into_message().unwrap();
        assert_eq!(msg.message.content.len(), 1);
    }

    #[test]
    fn a_completed_stream_builds_an_assistant_message() {
        let mut acc = StreamAccumulator::default();
        feed(
            &mut acc,
            &[
                serde_json::json!({"type":"response.reasoning_summary_text.delta","delta":"plan"}),
                serde_json::json!({"type":"response.output_text.delta","delta":"answer"}),
                serde_json::json!({"type":"response.output_item.done","item_id":"i1",
                    "item":{"type":"function_call","call_id":"c1","name":"t","arguments":"{}"}}),
                serde_json::json!({"type":"response.completed",
                    "response":{"usage":{"input_tokens":1,"output_tokens":2}}}),
            ],
        );

        let msg = acc.into_message().unwrap();
        assert_eq!(msg.message.role, "assistant");
        // thinking + text + tool_use
        assert_eq!(msg.message.content.len(), 3);
        assert!(matches!(
            msg.message.content[0],
            ContentBlock::Thinking { .. }
        ));
        assert!(matches!(msg.message.content[1], ContentBlock::Text { .. }));
        assert!(matches!(
            msg.message.content[2],
            ContentBlock::ToolUse { .. }
        ));
        assert!(msg.usage.is_some());
    }
}
