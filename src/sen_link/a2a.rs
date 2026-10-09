//! A2A v1.0 wire helpers (JSON-RPC binding). Only what sen-link speaks:
//! `SendMessage`, `GetTask`, `ListTasks`, `CancelTask`, plus the Agent Card.
//! Pre-1.0 slash method names are accepted as aliases for interop.

use serde_json::{json, Value};

pub const PROTOCOL_VERSION: &str = "1.0";

// JSON-RPC / A2A error codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
pub const TASK_NOT_FOUND: i64 = -32001;
pub const TASK_NOT_CANCELABLE: i64 = -32002;

/// v1.0 method name for `method` (pre-1.0 aliases mapped).
pub fn canonical_method(method: &str) -> &str {
    match method {
        "message/send" => "SendMessage",
        "tasks/get" => "GetTask",
        "tasks/list" => "ListTasks",
        "tasks/cancel" => "CancelTask",
        m => m,
    }
}

/// Task states (ProtoJSON enum names).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Submitted,
    Working,
    InputRequired,
    Completed,
    Failed,
    Canceled,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Submitted => "TASK_STATE_SUBMITTED",
            Self::Working => "TASK_STATE_WORKING",
            Self::InputRequired => "TASK_STATE_INPUT_REQUIRED",
            Self::Completed => "TASK_STATE_COMPLETED",
            Self::Failed => "TASK_STATE_FAILED",
            Self::Canceled => "TASK_STATE_CANCELED",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }
}

pub fn rpc_result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// All text parts of a `Message`, joined. Accepts v1.0 parts (`{"text": …}`)
/// and pre-1.0 ones (`{"kind": "text", "text": …}`).
pub fn message_text(message: &Value) -> String {
    message["parts"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// A user `Message` carrying `text`.
pub fn user_message(text: &str, task_id: Option<&str>, metadata: Value) -> Value {
    let mut m = json!({
        "messageId": uuid::Uuid::new_v4().to_string(),
        "role": "ROLE_USER",
        "parts": [{ "text": text }],
        "metadata": metadata,
    });
    if let Some(t) = task_id {
        m["taskId"] = json!(t);
    }
    m
}

pub struct AgentCardInfo<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub url: Option<&'a str>,
    pub version: &'a str,
}

/// This daemon's Agent Card (`/.well-known/agent-card.json`).
pub fn agent_card(i: &AgentCardInfo<'_>) -> Value {
    let interfaces: Vec<Value> = i
        .url
        .map(|u| {
            vec![json!({
                "url": format!("{}/a2a", u.trim_end_matches('/')),
                "protocolBinding": "JSONRPC",
                "protocolVersion": PROTOCOL_VERSION,
            })]
        })
        .unwrap_or_default();
    json!({
        "name": i.name,
        "description": i.description,
        "version": i.version,
        "supportedInterfaces": interfaces,
        "provider": { "organization": "SenClaw", "url": "https://github.com/SenClaw" },
        "capabilities": { "streaming": false, "pushNotifications": false },
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": [{
            "id": "delegate-task",
            "name": "Delegate a task",
            "description": "Queue a task on this SenClaw's kanban board; a worker (SenClaw persona or Claude Code) runs it and the result comes back as the task's artifact.",
            "tags": ["kanban", "code", "research"],
        }],
        "securitySchemes": {
            "bearer": { "httpAuthSecurityScheme": { "scheme": "Bearer", "description": "Token issued when the peers paired (sen-link invite)." } }
        },
        "securityRequirements": [{ "schemes": { "bearer": { "list": [] } } }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_method_names_map_to_v1() {
        assert_eq!(canonical_method("message/send"), "SendMessage");
        assert_eq!(canonical_method("GetTask"), "GetTask");
    }

    #[test]
    fn text_is_read_from_both_part_shapes() {
        let m = json!({"parts": [{"text": "a"}, {"kind": "text", "text": "b"}, {"url": "x"}]});
        assert_eq!(message_text(&m), "a\nb");
        assert_eq!(message_text(&user_message("hi", Some("7"), json!({}))), "hi");
    }

    #[test]
    fn the_card_advertises_jsonrpc_only_when_reachable() {
        let c = agent_card(&AgentCardInfo { name: "A", description: "d", url: Some("http://h:1/"), version: "0" });
        assert_eq!(c["supportedInterfaces"][0]["url"], "http://h:1/a2a");
        let c = agent_card(&AgentCardInfo { name: "A", description: "d", url: None, version: "0" });
        assert!(c["supportedInterfaces"].as_array().unwrap().is_empty());
    }
}
