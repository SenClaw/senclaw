//! Agent Client Protocol — SenClaw as an agent inside Zed, JetBrains, Kiro
//! and any other ACP client.
//!
//! `senclaw acp` speaks ACP (JSON-RPC 2.0, one message per line on stdio) to
//! the editor and translates to the daemon's WebSocket gateway on the other
//! side. There is no second engine: `session/new` registers a code chat pinned
//! to the editor's `cwd`, `session/prompt` is a chat message, and the chat's
//! `agent:delta` / `tool:execution` / `permission:request` frames become
//! `session/update` and `session/request_permission`. Written against ACP
//! protocol version 1 by hand — the surface used here is small and stable,
//! and a typed SDK crate would pin the daemon to its release cadence.
//!
//! Not translated in v1: `fs/*` and `terminal/*` client capabilities (the
//! daemon reads and writes files itself), `session/load` (a chat can be
//! resumed by prompting it again), AskUserQuestion / FormUI prompts (they
//! show as text and the turn waits on them in the web UI).

pub mod jsonrpc;
pub mod ws;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use jsonrpc::{Inbound, Outbound};
use ws::DaemonWs;

pub const PROTOCOL_VERSION: u64 = 1;

/// ACP tool kind for a SenClaw tool name.
pub fn tool_kind(tool_name: &str) -> &'static str {
    let n = tool_name.rsplit("__").next().unwrap_or(tool_name);
    match n {
        "Read" | "read_file" | "symbol_body" | "NotebookRead" => "read",
        "Edit" | "Write" | "NotebookEdit" | "MultiEdit" => "edit",
        "Bash" | "bash" | "run_command" => "execute",
        "Glob" | "Grep" | "find_symbol" | "find_references" | "repo_map" | "ToolSearch" => "search",
        "WebFetch" | "WebSearch" | "browser_navigate" => "fetch",
        "TodoWrite" | "EnterPlanMode" | "ExitPlanMode" | "Task" => "think",
        _ => "other",
    }
}

/// Map a daemon `tool:execution` frame to an ACP `tool_call` update.
pub fn tool_call_update(frame: &Value, call_id: &str) -> Value {
    let tool = frame.get("toolName").and_then(|v| v.as_str()).unwrap_or("tool");
    let title = frame.get("title").and_then(|v| v.as_str()).filter(|t| !t.is_empty()).unwrap_or(tool);
    let ok = frame.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
    let content = frame.get("content").cloned().unwrap_or(Value::Null);
    let mut blocks: Vec<Value> = Vec::new();
    let mut locations: Vec<Value> = Vec::new();
    if let Some(path) = content.get("path").and_then(|v| v.as_str()) {
        locations.push(json!({ "path": path }));
    }
    if let Some(diff) = content.get("diff").and_then(|v| v.as_str()) {
        // A unified diff is shown as text; ACP's `diff` block wants old/new
        // texts, which the daemon does not keep after the edit.
        blocks.push(json!({ "type": "content", "content": { "type": "text", "text": diff } }));
    } else if let Some(summary) = frame.get("summary").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        blocks.push(json!({ "type": "content", "content": { "type": "text", "text": summary } }));
    } else if let Some(s) = content.as_str() {
        blocks.push(json!({ "type": "content", "content": { "type": "text", "text": s } }));
    }
    json!({
        "sessionUpdate": "tool_call",
        "toolCallId": call_id,
        "title": title,
        "kind": tool_kind(tool),
        "status": if ok { "completed" } else { "failed" },
        "content": blocks,
        "locations": locations,
        "rawOutput": content,
    })
}

/// Map SenClaw permission options (`key`/`label`) to ACP option kinds.
pub fn permission_options(frame: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(opts) = frame.get("options").and_then(|v| v.as_array()) {
        for o in opts {
            let key = o.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let label = o.get("label").and_then(|v| v.as_str()).unwrap_or(key);
            let lk = key.to_ascii_lowercase();
            let ll = label.to_ascii_lowercase();
            let kind = if lk.contains("always") || ll.contains("always") || ll.contains("don't ask") {
                if lk.contains("no") || lk.contains("reject") || lk.contains("deny") || ll.starts_with("no") {
                    "reject_always"
                } else {
                    "allow_always"
                }
            } else if lk.contains("no") || lk.contains("reject") || lk.contains("deny") || ll.starts_with("no") || ll.starts_with("deny") {
                "reject_once"
            } else {
                "allow_once"
            };
            out.push(json!({ "optionId": key, "name": label, "kind": kind }));
        }
    }
    if out.is_empty() {
        out.push(json!({ "optionId": "yes", "name": "Allow", "kind": "allow_once" }));
        out.push(json!({ "optionId": "no", "name": "Deny", "kind": "reject_once" }));
    }
    out
}

/// Text of a prompt: text blocks joined, embedded resources appended as
/// fenced file contents so the model sees what the editor attached.
pub fn prompt_text(blocks: &Value) -> String {
    let mut out = String::new();
    let mut files = String::new();
    if let Some(arr) = blocks.as_array() {
        for b in arr {
            match b.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(t);
                    }
                }
                Some("resource") => {
                    let r = b.get("resource").cloned().unwrap_or(Value::Null);
                    let uri = r.get("uri").and_then(|u| u.as_str()).unwrap_or("");
                    if let Some(t) = r.get("text").and_then(|t| t.as_str()) {
                        files.push_str(&format!("\n\n<file uri=\"{uri}\">\n{t}\n</file>"));
                    } else {
                        files.push_str(&format!("\n\n(attached: {uri})"));
                    }
                }
                Some("resource_link") => {
                    let uri = b.get("uri").and_then(|u| u.as_str()).unwrap_or("");
                    files.push_str(&format!("\n\n(see file: {uri})"));
                }
                _ => {}
            }
        }
    }
    out.push_str(&files);
    out
}

struct Session {
    jid: String,
    events: mpsc::UnboundedSender<Value>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

struct State {
    sessions: HashMap<String, Session>,
}

fn session_id_for(cwd: &str) -> String {
    let short: String = uuid::Uuid::new_v4().to_string().chars().take(8).collect();
    let base = std::path::Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "session".into());
    format!("acp:{}:{short}", crate::worktree::sanitize_name(&base))
}

/// Run the ACP agent on stdin/stdout against the daemon at `ws_url`.
pub async fn run(ws_url: &str, token: Option<&str>) -> Result<()> {
    let (daemon, mut daemon_rx) = DaemonWs::connect(ws_url, token).await?;
    let (out, out_rx) = Outbound::new();
    tokio::spawn(jsonrpc::write_loop(out_rx));
    let (in_tx, mut in_rx) = mpsc::unbounded_channel();
    tokio::spawn(jsonrpc::read_loop(in_tx));

    let state = Arc::new(Mutex::new(State { sessions: HashMap::new() }));

    // Daemon frames → the session they belong to.
    {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            while let Some(frame) = daemon_rx.recv().await {
                let jid = frame.get("groupJid").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if jid.is_empty() {
                    continue;
                }
                let tx = state.lock().unwrap().sessions.get(&jid).map(|s| s.events.clone());
                if let Some(tx) = tx {
                    let _ = tx.send(frame);
                }
            }
        });
    }

    while let Some(msg) = in_rx.recv().await {
        match msg {
            Inbound::Response(v) => {
                out.resolve(&v);
            }
            Inbound::Notification { method, params } => {
                if method == "session/cancel" {
                    let sid = params.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
                    let st = state.lock().unwrap();
                    if let Some(s) = st.sessions.get(sid) {
                        s.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
                        let _ = daemon.stop(&s.jid);
                    }
                }
            }
            Inbound::Request { id, method, params } => match method.as_str() {
                "initialize" => {
                    out.respond(
                        &id,
                        json!({
                            "protocolVersion": PROTOCOL_VERSION,
                            "agentCapabilities": {
                                "loadSession": false,
                                "promptCapabilities": { "image": false, "audio": false, "embeddedContext": true }
                            },
                            "authMethods": [],
                            "agentInfo": { "name": "senclaw", "version": env!("CARGO_PKG_VERSION") }
                        }),
                    )?;
                }
                "authenticate" => {
                    out.respond(&id, json!({}))?;
                }
                "session/new" => {
                    let cwd = params.get("cwd").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if cwd.is_empty() {
                        out.respond_error(&id, -32602, "cwd is required")?;
                        continue;
                    }
                    let sid = session_id_for(&cwd);
                    let name = format!(
                        "ACP · {}",
                        std::path::Path::new(&cwd).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
                    );
                    daemon.register_code_group(&sid, &name, &cwd)?;
                    daemon.subscribe(&sid)?;
                    let (etx, erx) = mpsc::unbounded_channel();
                    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    state.lock().unwrap().sessions.insert(
                        sid.clone(),
                        Session { jid: sid.clone(), events: etx, cancelled: Arc::clone(&cancelled) },
                    );
                    // The receiver lives with the prompt loop of this session.
                    tokio::spawn(hold_events(erx, sid.clone(), Arc::clone(&state)));
                    out.respond(&id, json!({ "sessionId": sid, "modes": Value::Null }))?;
                }
                "session/load" => {
                    out.respond_error(&id, -32601, "session/load is not supported; open a new session")?;
                }
                "session/set_mode" => {
                    out.respond(&id, json!({}))?;
                }
                "session/prompt" => {
                    let sid = params.get("sessionId").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let text = prompt_text(params.get("prompt").unwrap_or(&Value::Null));
                    let (jid, cancelled, erx) = {
                        let mut st = state.lock().unwrap();
                        let Some(s) = st.sessions.get_mut(&sid) else {
                            drop(st);
                            out.respond_error(&id, -32602, "unknown sessionId")?;
                            continue;
                        };
                        s.cancelled.store(false, std::sync::atomic::Ordering::SeqCst);
                        // Take over the event stream for the duration of the turn.
                        let (etx, erx) = mpsc::unbounded_channel();
                        s.events = etx;
                        (s.jid.clone(), Arc::clone(&s.cancelled), erx)
                    };
                    if text.trim().is_empty() {
                        out.respond_error(&id, -32602, "empty prompt")?;
                        continue;
                    }
                    let daemon = daemon.clone();
                    let out2 = out.clone();
                    let state2 = Arc::clone(&state);
                    tokio::spawn(async move {
                        let stop = run_turn(&daemon, &out2, &sid, &jid, &text, erx, cancelled).await;
                        // Park the stream again until the next prompt.
                        let (etx, erx) = mpsc::unbounded_channel();
                        if let Some(s) = state2.lock().unwrap().sessions.get_mut(&sid) {
                            s.events = etx;
                        }
                        tokio::spawn(hold_events(erx, sid.clone(), Arc::clone(&state2)));
                        let _ = out2.respond(&id, json!({ "stopReason": stop }));
                    });
                }
                other => {
                    out.respond_error(&id, -32601, &format!("method not found: {other}"))?;
                }
            },
        }
    }
    Ok(())
}

/// Drain frames that arrive between prompts (state changes, late deltas) so
/// the channel never fills; nothing outside a turn is sent to the editor.
async fn hold_events(mut rx: mpsc::UnboundedReceiver<Value>, _sid: String, _state: Arc<Mutex<State>>) {
    while rx.recv().await.is_some() {}
}

/// One prompt turn: send the message, translate frames until the reply.
async fn run_turn(
    daemon: &DaemonWs,
    out: &Outbound,
    sid: &str,
    jid: &str,
    text: &str,
    mut events: mpsc::UnboundedReceiver<Value>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> &'static str {
    if daemon.send_message(jid, text).is_err() {
        let _ = out.notify(
            "session/update",
            json!({ "sessionId": sid, "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "SenClaw daemon connection lost." } } }),
        );
        return "refusal";
    }
    let mut streamed = false;
    let mut call_seq = 0u64;
    loop {
        let Some(frame) = events.recv().await else { return "end_turn" };
        let kind = frame.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "agent:delta" => {
                if let Some(d) = frame.get("delta").and_then(|v| v.as_str()) {
                    streamed = true;
                    let _ = out.notify(
                        "session/update",
                        json!({ "sessionId": sid, "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": d } } }),
                    );
                }
            }
            "tool:execution" => {
                call_seq += 1;
                let call_id = format!("{sid}-{call_seq}");
                let _ = out.notify("session/update", json!({ "sessionId": sid, "update": tool_call_update(&frame, &call_id) }));
            }
            "permission:request" => {
                let request_id = frame.get("requestId").and_then(|v| v.as_str()).unwrap_or("").to_string();
                call_seq += 1;
                let call_id = format!("{sid}-perm-{call_seq}");
                let tool = frame.get("toolName").and_then(|v| v.as_str()).unwrap_or("tool");
                let title = frame.get("title").and_then(|v| v.as_str()).unwrap_or(tool);
                let content = frame.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let options = permission_options(&frame);
                let req = json!({
                    "sessionId": sid,
                    "toolCall": {
                        "toolCallId": call_id,
                        "title": title,
                        "kind": tool_kind(tool),
                        "status": "pending",
                        "content": if content.is_empty() { json!([]) } else { json!([{ "type": "content", "content": { "type": "text", "text": content } }]) },
                        "rawInput": frame.get("input").cloned().unwrap_or(Value::Null),
                    },
                    "options": options,
                });
                let answer = out.request("session/request_permission", req).await;
                let key = match answer {
                    Ok(v) => {
                        let outcome = v.get("outcome").cloned().unwrap_or(Value::Null);
                        if outcome.get("outcome").and_then(|o| o.as_str()) == Some("selected") {
                            outcome.get("optionId").and_then(|o| o.as_str()).unwrap_or("no").to_string()
                        } else {
                            // cancelled → deny this call
                            first_reject_key(&frame)
                        }
                    }
                    Err(_) => first_reject_key(&frame),
                };
                let _ = daemon.permission_response(&request_id, &key);
            }
            "agent:reply" => {
                if !streamed {
                    if let Some(t) = frame.get("text").and_then(|v| v.as_str()) {
                        let _ = out.notify(
                            "session/update",
                            json!({ "sessionId": sid, "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": t } } }),
                        );
                    }
                }
                return if cancelled.load(std::sync::atomic::Ordering::SeqCst) { "cancelled" } else { "end_turn" };
            }
            "agent:state" => {
                let st = frame.get("state").and_then(|v| v.as_str()).unwrap_or("");
                if st == "error" {
                    return "refusal";
                }
                if st == "idle" && cancelled.load(std::sync::atomic::Ordering::SeqCst) {
                    return "cancelled";
                }
            }
            _ => {}
        }
    }
}

fn first_reject_key(frame: &Value) -> String {
    frame
        .get("options")
        .and_then(|v| v.as_array())
        .and_then(|a| {
            a.iter().find_map(|o| {
                let k = o.get("key").and_then(|v| v.as_str())?;
                let lk = k.to_ascii_lowercase();
                (lk.contains("no") || lk.contains("reject") || lk.contains("deny")).then(|| k.to_string())
            })
        })
        .unwrap_or_else(|| "no".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_kinds_and_tool_call_mapping() {
        assert_eq!(tool_kind("Edit"), "edit");
        assert_eq!(tool_kind("mcp__core__browser_navigate"), "fetch");
        assert_eq!(tool_kind("Grep"), "search");
        assert_eq!(tool_kind("kanban_list"), "other");
        let frame = json!({
            "type": "tool:execution", "toolName": "Edit", "title": "Edit main.rs", "summary": "1 replacements",
            "content": { "path": "/w/src/main.rs", "diff": "--- a\n+++ b\n-x\n+y\n" }, "ok": true
        });
        let u = tool_call_update(&frame, "c1");
        assert_eq!(u["sessionUpdate"], "tool_call");
        assert_eq!(u["kind"], "edit");
        assert_eq!(u["status"], "completed");
        assert_eq!(u["locations"][0]["path"], "/w/src/main.rs");
        assert!(u["content"][0]["content"]["text"].as_str().unwrap().contains("+y"));
        let failed = tool_call_update(&json!({"toolName": "Bash", "title": "cargo test", "content": "boom", "ok": false}), "c2");
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["kind"], "execute");
    }

    #[test]
    fn permission_options_map_to_acp_kinds() {
        let frame = json!({ "options": [
            { "key": "yes", "label": "Yes" },
            { "key": "yes_always", "label": "Yes, don't ask again" },
            { "key": "no", "label": "No" },
        ]});
        let opts = permission_options(&frame);
        assert_eq!(opts[0]["kind"], "allow_once");
        assert_eq!(opts[1]["kind"], "allow_always");
        assert_eq!(opts[2]["kind"], "reject_once");
        assert_eq!(opts[2]["optionId"], "no");
        assert_eq!(first_reject_key(&frame), "no");
        assert_eq!(permission_options(&json!({})).len(), 2);
    }

    #[test]
    fn prompt_text_joins_blocks_and_embeds_resources() {
        let p = json!([
            { "type": "text", "text": "fix this" },
            { "type": "resource", "resource": { "uri": "file:///w/a.rs", "text": "fn a() {}" } },
            { "type": "resource_link", "uri": "file:///w/b.rs" }
        ]);
        let t = prompt_text(&p);
        assert!(t.starts_with("fix this"));
        assert!(t.contains("<file uri=\"file:///w/a.rs\">\nfn a() {}\n</file>"));
        assert!(t.contains("(see file: file:///w/b.rs)"));
    }

    #[test]
    fn session_ids_are_safe_and_distinct() {
        let a = session_id_for("/Users/x/My Project");
        let b = session_id_for("/Users/x/My Project");
        assert!(a.starts_with("acp:my-project:"));
        assert_ne!(a, b);
    }
}
