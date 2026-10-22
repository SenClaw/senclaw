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
/// The peer asked for knowledge this daemon has not shared with it.
pub const KNOWLEDGE_NOT_SHARED: i64 = -32010;
/// Too many requests of one kind in a minute.
pub const TOO_MANY_REQUESTS: i64 = -32011;
/// The app is not installed here, does not sync, is not running, or is not safe to sync.
pub const APP_UNAVAILABLE: i64 = -32012;
/// The app's live schema is not the one its manifest declares, or the two sides' schemas differ.
pub const APP_SCHEMA_MISMATCH: i64 = -32013;

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

/// Extension URI under which the Agent Card carries what pairing needs to
/// know about this daemon (`params.daemonId`, `params.pairable`).
pub const SLINK_EXTENSION_URI: &str = "https://github.com/SenClaw/senclaw/sen-link/v1";

pub struct AgentCardInfo<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub url: Option<&'a str>,
    pub version: &'a str,
    /// This daemon's id (`PeerStore::daemon_id`).
    pub daemon_id: &'a str,
    /// A live invite exists, so a peer that has the code can pair now.
    pub pairable: bool,
}

/// What an Agent Card says about the SLink behind it, or `None` when it is
/// not a SenClaw card (a probe or scan hit some other A2A agent or web page).
pub struct CardIdentity {
    pub name: String,
    pub version: String,
    pub daemon_id: String,
    pub pairable: bool,
    /// Version of the knowledge methods it answers; 0 when it has none.
    pub knowledge: u32,
    /// Version of the app-sync methods it answers; 0 when it has none.
    pub app_sync: u32,
}

pub fn card_identity(card: &Value) -> Option<CardIdentity> {
    if card["provider"]["organization"].as_str() != Some("SenClaw") {
        return None;
    }
    let ext = card["capabilities"]["extensions"].as_array()?.iter().find(|e| e["uri"] == SLINK_EXTENSION_URI)?;
    Some(CardIdentity {
        name: card["name"].as_str()?.chars().take(80).collect(),
        version: card["version"].as_str().unwrap_or_default().chars().take(40).collect(),
        daemon_id: super::store::clean_daemon_id(ext["params"]["daemonId"].as_str()?)?,
        pairable: ext["params"]["pairable"].as_bool().unwrap_or(false),
        knowledge: ext["params"]["knowledge"].as_u64().unwrap_or(0).min(1000) as u32,
        app_sync: ext["params"]["appSync"].as_u64().unwrap_or(0).min(1000) as u32,
    })
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
        "capabilities": {
            "streaming": false,
            "pushNotifications": false,
            "extensions": [{
                "uri": SLINK_EXTENSION_URI,
                "description": "SLink: a SenClaw reachable on the local network",
                "required": false,
                "params": { "daemonId": i.daemon_id, "pairable": i.pairable, "knowledge": super::knowledge::KNOWLEDGE_VERSION, "appSync": super::app_sync::APP_SYNC_VERSION },
            }],
        },
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": [{
            "id": "delegate-task",
            "name": "Delegate a task",
            "description": "Queue a task on this SenClaw's kanban board; a worker (SenClaw persona or Claude Code) runs it and the result comes back as the task's artifact.",
            "tags": ["kanban", "code", "research"],
        }],
        "securitySchemes": {
            "bearer": { "httpAuthSecurityScheme": { "scheme": "Bearer", "description": "Requests are sealed under the key made when the peers paired (SLink invite); the token issued then travels inside the envelope." } }
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

    fn info<'a>(url: Option<&'a str>, pairable: bool) -> AgentCardInfo<'a> {
        AgentCardInfo { name: "A", description: "d", url, version: "0", daemon_id: "6f1c-0a", pairable }
    }

    #[test]
    fn the_card_advertises_jsonrpc_only_when_reachable() {
        let c = agent_card(&info(Some("http://h:1/"), false));
        assert_eq!(c["supportedInterfaces"][0]["url"], "http://h:1/a2a");
        let c = agent_card(&info(None, false));
        assert!(c["supportedInterfaces"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_card_tells_pairing_who_it_is_and_foreign_cards_are_not_ours() {
        let id = card_identity(&agent_card(&info(None, true))).unwrap();
        assert_eq!((id.name.as_str(), id.daemon_id.as_str(), id.pairable), ("A", "6f1c-0a", true));
        assert!(!card_identity(&agent_card(&info(None, false))).unwrap().pairable);

        // Another A2A agent, a page that is not a card, and a SenClaw card without the extension.
        let mut other = agent_card(&info(None, false));
        other["provider"]["organization"] = json!("Someone");
        assert!(card_identity(&other).is_none());
        assert!(card_identity(&json!({ "name": "x" })).is_none());
        let mut bare = agent_card(&info(None, false));
        bare["capabilities"] = json!({ "streaming": false });
        assert!(card_identity(&bare).is_none());
        // The card says which knowledge methods this daemon answers; an older card says nothing.
        assert_eq!(card_identity(&agent_card(&info(None, false))).unwrap().knowledge, super::super::knowledge::KNOWLEDGE_VERSION);
        let mut old_card = agent_card(&info(None, false));
        old_card["capabilities"]["extensions"][0]["params"].as_object_mut().unwrap().remove("knowledge");
        assert_eq!(card_identity(&old_card).unwrap().knowledge, 0);
        // A daemon id that is not printable is refused rather than shown.
        let mut bad = agent_card(&info(None, false));
        bad["capabilities"]["extensions"][0]["params"]["daemonId"] = json!("<b>x</b>");
        assert!(card_identity(&bad).is_none());
    }
}
