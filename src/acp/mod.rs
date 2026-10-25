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

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Map a daemon `tool:execution` frame to an ACP `tool_call` update — a new
/// entry in the editor.
pub fn tool_call_update(frame: &Value, call_id: &str) -> Value {
    tool_result(frame, call_id, "tool_call")
}

/// The same result as a `tool_call_update` of an entry the editor already
/// shows: the call a permission request announced.
pub fn tool_call_finished(frame: &Value, call_id: &str) -> Value {
    tool_result(frame, call_id, "tool_call_update")
}

fn tool_result(frame: &Value, call_id: &str, session_update: &str) -> Value {
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
        "sessionUpdate": session_update,
        "toolCallId": call_id,
        "title": title,
        "kind": tool_kind(tool),
        "status": if ok { "completed" } else { "failed" },
        "content": blocks,
        "locations": locations,
        "rawOutput": content,
    })
}

/// ACP option kind for a SenClaw permission option. The daemon's own options
/// are `agree` "Confirm", `allow` "Confirm, never ask … again" and `refuse`
/// "Reject" (`zen_core::permissions`); missing `refuse` once offered Reject
/// as an `allow_once`, which a client choosing by kind picks to approve.
fn option_kind(key: &str, label: &str) -> &'static str {
    let lk = key.to_ascii_lowercase();
    let ll = label.to_ascii_lowercase();
    let rejects = ["no", "reject", "deny", "refuse"].iter().any(|w| lk.contains(w))
        || ["no", "deny", "reject"].iter().any(|w| ll.starts_with(w));
    let always = lk.contains("always") || ["always", "don't ask", "never ask"].iter().any(|w| ll.contains(w));
    match (rejects, always) {
        (true, true) => "reject_always",
        (true, false) => "reject_once",
        (false, true) => "allow_always",
        (false, false) => "allow_once",
    }
}

/// Map SenClaw permission options (`key`/`label`) to ACP options, allowing
/// first — the daemon sends them in hash-map order.
pub fn permission_options(frame: &Value) -> Vec<Value> {
    const ORDER: [&str; 4] = ["allow_once", "allow_always", "reject_once", "reject_always"];
    let mut out = Vec::new();
    if let Some(opts) = frame.get("options").and_then(|v| v.as_array()) {
        for o in opts {
            let key = o.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let label = o.get("label").and_then(|v| v.as_str()).unwrap_or(key);
            out.push(json!({ "optionId": key, "name": label, "kind": option_kind(key, label) }));
        }
    }
    if out.is_empty() {
        out.push(json!({ "optionId": "yes", "name": "Allow", "kind": "allow_once" }));
        out.push(json!({ "optionId": "no", "name": "Deny", "kind": "reject_once" }));
    }
    out.sort_by_key(|o| ORDER.iter().position(|k| o["kind"] == *k));
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

/// Split an `agent:reply` text into the model's thinking and what it said.
/// The daemon puts reasoning in a leading `<think>` block
/// (`merge_assistant_reasoning_for_web_ui`) — the whole reply when the model
/// only thought before calling a tool. ACP carries it as
/// `agent_thought_chunk`, not as message text.
pub fn split_reasoning(text: &str) -> (Option<&str>, &str) {
    const BLOCKS: [(&str, &str); 3] = [
        (concat!("<", "think", ">"), concat!("</", "think", ">")),
        (concat!("<", "redacted_", "reasoning", ">"), concat!("</", "redacted_", "reasoning", ">")),
        (concat!("<", "redacted_", "thinking", ">"), concat!("</", "redacted_", "thinking", ">")),
    ];
    let body = text.trim_start();
    for (open, close) in BLOCKS {
        let Some(rest) = body.strip_prefix(open) else { continue };
        let Some(end) = rest.find(close) else { continue };
        let thought = rest[..end].trim();
        return ((!thought.is_empty()).then_some(thought), rest[end + close.len()..].trim_start());
    }
    (None, text)
}

struct Session {
    jid: String,
    /// Every daemon frame for this chat, for the session's translator task.
    events: mpsc::UnboundedSender<Value>,
    /// `session/prompt` requests, for the same task.
    prompts: mpsc::UnboundedSender<Prompt>,
    cancelled: Arc<AtomicBool>,
}

struct Prompt {
    id: Value,
    text: String,
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
                        s.cancelled.store(true, Ordering::SeqCst);
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
                    let (ptx, prx) = mpsc::unbounded_channel();
                    let cancelled = Arc::new(AtomicBool::new(false));
                    state.lock().unwrap().sessions.insert(
                        sid.clone(),
                        Session { jid: sid.clone(), events: etx, prompts: ptx, cancelled: Arc::clone(&cancelled) },
                    );
                    tokio::spawn(session_loop(daemon.clone(), out.clone(), sid.clone(), sid.clone(), erx, prx, cancelled));
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
                    let prompts = {
                        let st = state.lock().unwrap();
                        let Some(s) = st.sessions.get(&sid) else {
                            drop(st);
                            out.respond_error(&id, -32602, "unknown sessionId")?;
                            continue;
                        };
                        s.cancelled.store(false, Ordering::SeqCst);
                        s.prompts.clone()
                    };
                    if text.trim().is_empty() {
                        out.respond_error(&id, -32602, "empty prompt")?;
                        continue;
                    }
                    if let Err(refused) = prompts.send(Prompt { id, text }) {
                        out.respond_error(&refused.0.id, -32603, "session ended")?;
                    }
                }
                other => {
                    out.respond_error(&id, -32601, &format!("method not found: {other}"))?;
                }
            },
        }
    }
    Ok(())
}

/// One session's translator, for its whole life: prompt turns, and the
/// frames between them.
///
/// Between prompts the chat can still run a turn nobody typed in the editor —
/// a `schedule_watch` resuming it, a message sent from the web UI. Its output
/// is forwarded as `session/update` (ACP: "Session updates are not limited to
/// active prompt turns"; Zed applies them to the thread whatever its state),
/// and its permission requests are asked in the editor too. They used to be
/// drained and dropped, so a watch's answer never reached the editor and its
/// permission request waited for the web UI.
async fn session_loop(
    daemon: DaemonWs,
    out: Outbound,
    sid: String,
    jid: String,
    mut events: mpsc::UnboundedReceiver<Value>,
    mut prompts: mpsc::UnboundedReceiver<Prompt>,
    cancelled: Arc<AtomicBool>,
) {
    let mut tr = Translator::new(&sid);
    loop {
        // Frames set aside while a permission was pending come first.
        let frame = match tr.backlog.pop_front() {
            Some(f) => f,
            None => tokio::select! {
                // Frames already queued are older than a prompt that arrives
                // with them.
                biased;
                f = events.recv() => match f {
                    Some(f) => f,
                    None => break,
                },
                p = prompts.recv() => match p {
                    Some(p) => {
                        let stop = run_turn(&daemon, &out, &mut tr, &jid, &p.text, &mut events, &cancelled).await;
                        let _ = out.respond(&p.id, json!({ "stopReason": stop }));
                        continue;
                    }
                    None => break,
                },
            },
        };
        tr.translate(&daemon, &out, frame, &mut events).await;
    }
}

/// One prompt turn: send the message, translate frames until the agent's
/// turn ends.
///
/// The end is `agent:state idle`, not `agent:reply`: the daemon sends a reply
/// per assistant *message*, and a turn that calls tools has several — the
/// first often only thinking before a tool call. Ending on it answered the
/// prompt before any tool ran, and the frames after it went to the drain
/// loop: permission requests nobody was asked, tool calls the editor never
/// saw, and the actual answer.
async fn run_turn(
    daemon: &DaemonWs,
    out: &Outbound,
    tr: &mut Translator,
    jid: &str,
    text: &str,
    events: &mut mpsc::UnboundedReceiver<Value>,
    cancelled: &AtomicBool,
) -> &'static str {
    if daemon.send_message(jid, text).is_err() {
        tr.chunk(out, "agent_message_chunk", "SenClaw daemon connection lost.");
        return "refusal";
    }
    let stop = || if cancelled.load(Ordering::SeqCst) { "cancelled" } else { "end_turn" };
    // Whether the agent run for this prompt has been seen. A chat's engine
    // reports `idle` once when it is created — ahead of the prompt's own
    // `processing` — and that idle must not end the turn.
    let mut running = false;
    loop {
        let Some(frame) = tr.next_frame(events).await else { return "end_turn" };
        match tr.translate(daemon, out, frame, events).await {
            Seen::Running => running = true,
            // A reply with no agent run before it is a command the gateway
            // answered itself (`/reset`, `/plugin …`): no turn follows.
            Seen::Reply if !running => return stop(),
            Seen::Idle if running || cancelled.load(Ordering::SeqCst) => return stop(),
            Seen::Failed => return "refusal",
            _ => {}
        }
    }
}

/// What a daemon frame meant for the prompt turn it arrived in.
#[derive(Debug, PartialEq)]
enum Seen {
    /// Says nothing about whether a turn is running.
    Other,
    /// The agent is working: a state change, streamed text, a tool, a
    /// permission request.
    Running,
    /// An assistant message ended.
    Reply,
    /// `agent:state idle`.
    Idle,
    /// `agent:state error`.
    Failed,
}

/// Translation state of one session.
///
/// Neither `permission:request` nor `tool:execution` carries the model's
/// tool-use id, so a permission is paired with the execution that follows it
/// by tool name, oldest first. That is all the daemon itself can tell apart:
/// it asks permission only for tools that are not read-only, runs those one at
/// a time, and routes the answer to the waiting call by tool name.
struct Translator {
    sid: String,
    /// Tool-call ids are numbered per session, not per prompt — the editor
    /// treats a repeated id as an update of the earlier call.
    call_seq: u64,
    /// Approved calls waiting for their `tool:execution`: (tool name, id).
    approved: VecDeque<(String, String)>,
    /// Whether the current assistant message streamed as deltas; its reply
    /// then repeats text the editor already has.
    streamed: bool,
    /// Frames that arrived while a permission request was pending.
    backlog: VecDeque<Value>,
}

impl Translator {
    fn new(sid: &str) -> Self {
        Self { sid: sid.to_string(), call_seq: 0, approved: VecDeque::new(), streamed: false, backlog: VecDeque::new() }
    }

    fn update(&self, out: &Outbound, update: Value) {
        let _ = out.notify("session/update", json!({ "sessionId": self.sid, "update": update }));
    }

    fn chunk(&self, out: &Outbound, kind: &str, text: &str) {
        self.update(out, json!({ "sessionUpdate": kind, "content": { "type": "text", "text": text } }));
    }

    fn set_status(&self, out: &Outbound, call_id: &str, status: &str) {
        self.update(out, json!({ "sessionUpdate": "tool_call_update", "toolCallId": call_id, "status": status }));
    }

    fn next_call_id(&mut self) -> String {
        self.call_seq += 1;
        format!("{}-{}", self.sid, self.call_seq)
    }

    async fn next_frame(&mut self, events: &mut mpsc::UnboundedReceiver<Value>) -> Option<Value> {
        match self.backlog.pop_front() {
            Some(f) => Some(f),
            None => events.recv().await,
        }
    }

    async fn translate(
        &mut self,
        daemon: &DaemonWs,
        out: &Outbound,
        frame: Value,
        events: &mut mpsc::UnboundedReceiver<Value>,
    ) -> Seen {
        match frame.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "agent:delta" => {
                if let Some(d) = frame.get("delta").and_then(|v| v.as_str()) {
                    self.streamed = true;
                    self.chunk(out, "agent_message_chunk", d);
                }
                Seen::Running
            }
            "tool:execution" => {
                self.tool_finished(out, &frame);
                Seen::Running
            }
            "permission:request" => {
                self.ask_permission(daemon, out, &frame, events).await;
                Seen::Running
            }
            "agent:reply" => {
                if !self.streamed {
                    if let Some(t) = frame.get("text").and_then(|v| v.as_str()) {
                        let (thought, said) = split_reasoning(t);
                        if let Some(th) = thought {
                            self.chunk(out, "agent_thought_chunk", th);
                        }
                        if !said.trim().is_empty() {
                            self.chunk(out, "agent_message_chunk", said);
                        }
                    }
                }
                self.streamed = false;
                Seen::Reply
            }
            "agent:state" => match frame.get("state").and_then(|v| v.as_str()).unwrap_or("") {
                "error" => {
                    self.close_open_calls(out);
                    Seen::Failed
                }
                "idle" => {
                    self.close_open_calls(out);
                    Seen::Idle
                }
                _ => Seen::Running,
            },
            _ => Seen::Other,
        }
    }

    /// A `tool:execution` completes the approved call of the same tool, or is
    /// a call of its own (a tool that needed no permission).
    fn tool_finished(&mut self, out: &Outbound, frame: &Value) {
        let tool = frame.get("toolName").and_then(|v| v.as_str()).unwrap_or("tool");
        let update = match self.approved.iter().position(|(t, _)| t == tool) {
            Some(i) => {
                let (_, id) = self.approved.remove(i).expect("position is in range");
                tool_call_finished(frame, &id)
            }
            None => {
                let id = self.next_call_id();
                tool_call_update(frame, &id)
            }
        };
        self.update(out, update);
    }

    /// Show the call, ask the editor about it, and pass the answer on.
    ///
    /// The call goes out as a `pending` `tool_call` first and the request
    /// names it, so the editor keeps one entry: the answer moves it to
    /// `in_progress` (or `failed` when refused — a refused call never runs and
    /// sends no `tool:execution`), and the tool's result completes it.
    /// Answered in the web UI instead, the daemon says so with
    /// `permission:resolved` and the editor is no longer waited on.
    async fn ask_permission(
        &mut self,
        daemon: &DaemonWs,
        out: &Outbound,
        frame: &Value,
        events: &mut mpsc::UnboundedReceiver<Value>,
    ) {
        let request_id = frame.get("requestId").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let tool = frame.get("toolName").and_then(|v| v.as_str()).unwrap_or("tool").to_string();
        let title = frame.get("title").and_then(|v| v.as_str()).unwrap_or(&tool).to_string();
        let content = frame.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let options = permission_options(frame);
        let call_id = self.next_call_id();
        let call = json!({
            "toolCallId": call_id,
            "title": title,
            "kind": tool_kind(&tool),
            "status": "pending",
            "content": if content.is_empty() { json!([]) } else { json!([{ "type": "content", "content": { "type": "text", "text": content } }]) },
            "rawInput": frame.get("input").cloned().unwrap_or(Value::Null),
        });
        let mut announce = call.clone();
        announce["sessionUpdate"] = json!("tool_call");
        self.update(out, announce);

        let ask = out.request("session/request_permission", json!({ "sessionId": self.sid, "toolCall": call, "options": options }));
        tokio::pin!(ask);
        let key = loop {
            tokio::select! {
                answer = &mut ask => {
                    let key = editor_choice(answer, frame);
                    let _ = daemon.permission_response(&request_id, &key);
                    break key;
                }
                next = events.recv() => match next {
                    Some(f) if is_resolution_of(&f, &request_id) => {
                        break f.get("optionKey").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    }
                    Some(f) => self.backlog.push_back(f),
                    None => return,
                },
            }
        };
        let allowed = options
            .iter()
            .any(|o| o["optionId"] == key.as_str() && o["kind"].as_str().is_some_and(|k| k.starts_with("allow")));
        if allowed {
            self.approved.push_back((tool, call_id.clone()));
            self.set_status(out, &call_id, "in_progress");
        } else {
            self.set_status(out, &call_id, "failed");
        }
    }

    /// An approved call the turn ended without reporting — stopped mid-run —
    /// is not running any more.
    fn close_open_calls(&mut self, out: &Outbound) {
        for (_, id) in std::mem::take(&mut self.approved) {
            self.set_status(out, &id, "failed");
        }
    }
}

/// The option key for the editor's answer to a permission request.
fn editor_choice(answer: Result<Value>, frame: &Value) -> String {
    let Ok(v) = answer else { return first_reject_key(frame) };
    let outcome = v.get("outcome").cloned().unwrap_or(Value::Null);
    if outcome.get("outcome").and_then(|o| o.as_str()) == Some("selected") {
        outcome.get("optionId").and_then(|o| o.as_str()).unwrap_or("no").to_string()
    } else {
        // cancelled → deny this call
        first_reject_key(frame)
    }
}

fn is_resolution_of(frame: &Value, request_id: &str) -> bool {
    frame.get("type").and_then(|v| v.as_str()) == Some("permission:resolved")
        && frame.get("requestId").and_then(|v| v.as_str()) == Some(request_id)
}

/// The option a cancelled or failed permission request answers with: a
/// one-time refusal, never a standing one. A key the daemon does not know
/// also refuses the call.
fn first_reject_key(frame: &Value) -> String {
    permission_options(frame)
        .iter()
        .find(|o| o["kind"] == "reject_once")
        .and_then(|o| o["optionId"].as_str())
        .unwrap_or("no")
        .to_string()
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
        // The result of a call the editor already shows updates that entry.
        let done = tool_call_finished(&frame, "c1");
        assert_eq!(done["sessionUpdate"], "tool_call_update");
        assert_eq!(done["toolCallId"], "c1");
        assert_eq!(done["status"], "completed");
        assert_eq!(done["locations"], u["locations"]);
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

        // The daemon's own option set (`zen_core::permissions`), in the
        // hash-map order a real request arrived in on 2026-10-16.
        let daemon = json!({ "options": [
            { "key": "refuse", "label": "Reject" },
            { "key": "allow", "label": "Confirm, never ask for mcp__core__datagen_start again (all chats)" },
            { "key": "agree", "label": "Confirm" },
        ]});
        let kinds: Vec<(String, String)> = permission_options(&daemon)
            .iter()
            .map(|o| (o["optionId"].as_str().unwrap().into(), o["kind"].as_str().unwrap().into()))
            .collect();
        let want = [("agree", "allow_once"), ("allow", "allow_always"), ("refuse", "reject_once")];
        assert_eq!(kinds, want.map(|(k, v)| (k.to_string(), v.to_string())));
        assert_eq!(first_reject_key(&daemon), "refuse");
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
    fn reasoning_splits_from_the_reply() {
        let think = |s: &str| format!("{}{s}{}", concat!("<", "think", ">"), concat!("</", "think", ">"));
        assert_eq!(split_reasoning(&think("\npick a tool\n")), (Some("pick a tool"), ""));
        assert_eq!(split_reasoning(&format!("{}\n\nDone.", think("\nok\n"))), (Some("ok"), "Done."));
        assert_eq!(split_reasoning("Plain answer"), (None, "Plain answer"));
        let unclosed = concat!("<", "think", ">", "never closed");
        assert_eq!(split_reasoning(unclosed), (None, unclosed));
    }

    fn line(rx: &mut mpsc::UnboundedReceiver<String>) -> Value {
        serde_json::from_str(&rx.try_recv().expect("an outbound line")).unwrap()
    }

    /// One session's translator task with no daemon or editor behind it:
    /// frames go in on `frames`, what the daemon would be sent comes out of
    /// `sent`, what the editor would read comes out of `lines`.
    struct Rig {
        sid: &'static str,
        frames: mpsc::UnboundedSender<Value>,
        prompts: mpsc::UnboundedSender<Prompt>,
        sent: mpsc::UnboundedReceiver<Value>,
        out: Outbound,
        lines: mpsc::UnboundedReceiver<String>,
        cancelled: Arc<AtomicBool>,
    }

    impl Rig {
        fn start(sid: &'static str) -> Self {
            let (daemon, sent) = DaemonWs::detached();
            let (out, lines) = Outbound::new();
            let (frames, events) = mpsc::unbounded_channel();
            let (prompts, prompt_rx) = mpsc::unbounded_channel();
            let cancelled = Arc::new(AtomicBool::new(false));
            tokio::spawn(session_loop(daemon, out.clone(), sid.into(), sid.into(), events, prompt_rx, Arc::clone(&cancelled)));
            Self { sid, frames, prompts, sent, out, lines, cancelled }
        }

        async fn prompt(&mut self, id: u64, text: &str) {
            self.prompts.send(Prompt { id: json!(id), text: text.into() }).unwrap();
            settle().await;
            assert_eq!(self.sent.try_recv().unwrap()["type"], "message");
        }

        fn frame(&self, mut v: Value) {
            v["groupJid"] = json!(self.sid);
            self.frames.send(v).unwrap();
        }

        fn line(&mut self) -> Value {
            line(&mut self.lines)
        }

        fn update(&mut self) -> Value {
            let l = self.line();
            assert_eq!(l["method"], "session/update", "{l}");
            l["params"]["update"].clone()
        }

        fn quiet(&mut self) -> bool {
            self.lines.try_recv().is_err()
        }

        fn answer(&self, ask: &Value, option_id: &str) {
            self.out.resolve(&json!({
                "jsonrpc": "2.0", "id": ask["id"], "result": { "outcome": { "outcome": "selected", "optionId": option_id } }
            }));
        }
    }

    /// Lets the spawned translator task drain what it can.
    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    fn permission(request_id: &str, tool: &str) -> Value {
        json!({
            "type": "permission:request", "requestId": request_id, "toolName": tool, "title": format!("Run {tool}"),
            "content": "{\"command\":\"rm -rf build\"}",
            "options": [
                { "key": "refuse", "label": "Reject" },
                { "key": "allow", "label": format!("Confirm, never ask for {tool} again (all chats)") },
                { "key": "agree", "label": "Confirm" },
            ]
        })
    }

    // gemma-4-e4b on 2026-10-16: the first assistant message was only
    // thinking plus a tool call. Its `agent:reply` used to answer the prompt,
    // and the permission request and tool call after it never reached the
    // editor.
    #[tokio::test]
    async fn prompt_ends_at_idle_not_at_the_first_reply() {
        let mut rig = Rig::start("acp:w:1");
        rig.prompt(7, "make a dataset").await;
        // The engine's own idle from being created, ahead of the prompt.
        rig.frame(json!({ "type": "agent:state", "state": "idle" }));
        rig.frame(json!({ "type": "agent:state", "state": "processing" }));
        // message_complete with empty content: thinking, then a tool call.
        let thinking = format!("{}\nuse datagen_start\n{}", concat!("<", "think", ">"), concat!("</", "think", ">"));
        rig.frame(json!({ "type": "agent:reply", "text": thinking, "tokens": 40 }));
        rig.frame(json!({
            "type": "permission:request", "requestId": "req-1",
            "toolName": "mcp__core__datagen_start", "title": "Start datagen job",
            "options": [{ "key": "agree", "label": "Yes" }, { "key": "disagree", "label": "No" }]
        }));
        settle().await;

        let thought = rig.update();
        assert_eq!(thought["sessionUpdate"], "agent_thought_chunk");
        assert_eq!(thought["content"]["text"], "use datagen_start");
        let shown = rig.update();
        assert_eq!(shown["sessionUpdate"], "tool_call");
        assert_eq!(shown["status"], "pending");
        assert_eq!(shown["title"], "Start datagen job");
        let ask = rig.line();
        assert_eq!(ask["method"], "session/request_permission");
        assert_eq!(ask["params"]["toolCall"]["toolCallId"], shown["toolCallId"]);
        assert_eq!(ask["params"]["toolCall"]["status"], "pending");
        assert!(rig.quiet(), "nothing else before the permission is answered");

        rig.answer(&ask, "agree");
        settle().await;
        let answer = rig.sent.try_recv().unwrap();
        assert_eq!(answer["type"], "permission:response");
        assert_eq!(answer["requestId"], "req-1");
        assert_eq!(answer["optionKey"], "agree");
        let running = rig.update();
        assert_eq!(running["sessionUpdate"], "tool_call_update");
        assert_eq!(running["toolCallId"], shown["toolCallId"]);
        assert_eq!(running["status"], "in_progress");

        rig.frame(json!({
            "type": "tool:execution", "toolName": "mcp__core__datagen_start",
            "title": "Start datagen job", "summary": "job dg-1 started", "content": { "jobId": "dg-1" }, "ok": true
        }));
        rig.frame(json!({ "type": "agent:delta", "delta": "Started job dg-1." }));
        rig.frame(json!({ "type": "agent:reply", "text": "Started job dg-1.", "tokens": 6 }));
        settle().await;
        let done_call = rig.update();
        assert_eq!(done_call["sessionUpdate"], "tool_call_update", "the approved call completes, no second entry");
        assert_eq!(done_call["toolCallId"], shown["toolCallId"]);
        assert_eq!(done_call["status"], "completed");
        let said = rig.update();
        assert_eq!(said["sessionUpdate"], "agent_message_chunk");
        assert_eq!(said["content"]["text"], "Started job dg-1.");
        assert!(rig.quiet(), "a streamed reply is not sent twice, and the prompt is still open");

        rig.frame(json!({ "type": "agent:state", "state": "idle" }));
        settle().await;
        let done = rig.line();
        assert_eq!(done["id"], 7);
        assert_eq!(done["result"]["stopReason"], "end_turn");
        assert!(rig.quiet(), "exactly one prompt response");
    }

    #[tokio::test]
    async fn a_command_reply_with_no_agent_run_ends_the_prompt() {
        let mut rig = Rig::start("acp:w:2");
        rig.prompt(3, "/reset").await;
        rig.frame(json!({ "type": "agent:reply", "text": "Context cleared." }));
        settle().await;
        assert_eq!(rig.update()["content"]["text"], "Context cleared.");
        assert_eq!(rig.line()["result"]["stopReason"], "end_turn");
    }

    #[tokio::test]
    async fn a_refused_call_fails_and_call_ids_stay_unique_across_prompts() {
        let mut rig = Rig::start("acp:w:3");
        rig.prompt(1, "clean the build").await;
        rig.frame(json!({ "type": "agent:state", "state": "processing" }));
        rig.frame(permission("req-1", "Bash"));
        settle().await;
        let shown = rig.update();
        let ask = rig.line();
        // Allow first, whatever order the daemon's hash map sent them in.
        let kinds: Vec<&str> = ask["params"]["options"].as_array().unwrap().iter().map(|o| o["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["allow_once", "allow_always", "reject_once"]);
        assert_eq!(shown["kind"], "execute");
        assert_eq!(shown["content"][0]["content"]["text"], "{\"command\":\"rm -rf build\"}");

        rig.answer(&ask, "refuse");
        settle().await;
        assert_eq!(rig.sent.try_recv().unwrap()["optionKey"], "refuse");
        // A refused call never runs, so nothing else will close it.
        let refused = rig.update();
        assert_eq!(refused["toolCallId"], shown["toolCallId"]);
        assert_eq!(refused["status"], "failed");
        rig.frame(json!({ "type": "agent:delta", "delta": "Not cleaned." }));
        rig.frame(json!({ "type": "agent:state", "state": "idle" }));
        settle().await;
        assert_eq!(rig.update()["content"]["text"], "Not cleaned.");
        assert_eq!(rig.line()["result"]["stopReason"], "end_turn");
        assert!(rig.quiet(), "the refused call is not closed a second time at idle");

        rig.prompt(2, "read it").await;
        rig.frame(json!({ "type": "agent:state", "state": "processing" }));
        rig.frame(json!({ "type": "tool:execution", "toolName": "Read", "title": "Cargo.toml", "content": "x", "ok": true }));
        rig.frame(json!({ "type": "agent:state", "state": "idle" }));
        settle().await;
        let read = rig.update();
        assert_eq!(read["sessionUpdate"], "tool_call", "a tool that needed no permission is an entry of its own");
        assert_ne!(read["toolCallId"], shown["toolCallId"], "an id from an earlier prompt would update that call");
        assert_eq!(rig.line()["result"]["stopReason"], "end_turn");
    }

    #[tokio::test]
    async fn a_cancelled_turn_closes_the_call_it_approved() {
        let mut rig = Rig::start("acp:w:4");
        rig.prompt(1, "clean the build").await;
        rig.frame(json!({ "type": "agent:state", "state": "processing" }));
        rig.frame(permission("req-1", "Bash"));
        settle().await;
        let shown = rig.update();
        let ask = rig.line();
        rig.answer(&ask, "agree");
        settle().await;
        assert_eq!(rig.update()["status"], "in_progress");

        // session/cancel: the daemon stops the run mid-tool, which reports
        // no `tool:execution`.
        rig.cancelled.store(true, Ordering::SeqCst);
        rig.frame(json!({ "type": "agent:state", "state": "idle" }));
        settle().await;
        let closed = rig.update();
        assert_eq!(closed["toolCallId"], shown["toolCallId"]);
        assert_eq!(closed["status"], "failed");
        assert_eq!(rig.line()["result"]["stopReason"], "cancelled");
    }

    // A `schedule_watch` resume turn runs with no prompt open. Its frames used
    // to be drained and dropped: the answer never reached the editor, and its
    // permission request waited for the web UI.
    #[tokio::test]
    async fn a_turn_the_editor_did_not_start_is_forwarded() {
        let mut rig = Rig::start("acp:w:5");
        rig.frame(json!({ "type": "agent:state", "state": "processing" }));
        rig.frame(json!({ "type": "agent:delta", "delta": "Job dg-1 finished: 120 records." }));
        rig.frame(json!({ "type": "agent:reply", "text": "Job dg-1 finished: 120 records." }));
        rig.frame(permission("req-9", "mcp__core__datagen_export"));
        settle().await;
        assert_eq!(rig.update()["content"]["text"], "Job dg-1 finished: 120 records.");
        let shown = rig.update();
        assert_eq!(shown["sessionUpdate"], "tool_call");
        let ask = rig.line();
        assert_eq!(ask["method"], "session/request_permission");

        rig.answer(&ask, "agree");
        settle().await;
        assert_eq!(rig.sent.try_recv().unwrap()["requestId"], "req-9");
        assert_eq!(rig.update()["status"], "in_progress");
        rig.frame(json!({ "type": "tool:execution", "toolName": "mcp__core__datagen_export", "title": "Export", "content": {}, "ok": true }));
        rig.frame(json!({ "type": "agent:state", "state": "idle" }));
        settle().await;
        let done = rig.update();
        assert_eq!(done["toolCallId"], shown["toolCallId"]);
        assert_eq!(done["status"], "completed");
        assert!(rig.quiet(), "no prompt was open, so nothing is answered");

        // The next prompt is a normal turn.
        rig.prompt(4, "thanks").await;
        rig.frame(json!({ "type": "agent:state", "state": "processing" }));
        rig.frame(json!({ "type": "agent:state", "state": "idle" }));
        settle().await;
        assert_eq!(rig.line()["id"], 4);
    }

    #[tokio::test]
    async fn a_permission_answered_in_the_web_ui_stops_waiting_on_the_editor() {
        let mut rig = Rig::start("acp:w:6");
        rig.prompt(1, "clean the build").await;
        rig.frame(json!({ "type": "agent:state", "state": "processing" }));
        rig.frame(permission("req-1", "Bash"));
        settle().await;
        let shown = rig.update();
        let ask = rig.line();

        // Another chat's answer is not this one.
        rig.frame(json!({ "type": "permission:resolved", "requestId": "req-0", "optionKey": "refuse" }));
        rig.frame(json!({ "type": "permission:resolved", "requestId": "req-1", "optionKey": "agree" }));
        rig.frame(json!({ "type": "tool:execution", "toolName": "Bash", "title": "rm -rf build", "content": "", "ok": true }));
        settle().await;
        assert!(rig.sent.try_recv().is_err(), "the daemon already has its answer");
        let running = rig.update();
        assert_eq!(running["toolCallId"], shown["toolCallId"]);
        assert_eq!(running["status"], "in_progress");
        let done = rig.update();
        assert_eq!(done["toolCallId"], shown["toolCallId"]);
        assert_eq!(done["status"], "completed");

        // The editor answering late sends nothing to the daemon.
        rig.answer(&ask, "agree");
        rig.frame(json!({ "type": "agent:state", "state": "idle" }));
        settle().await;
        assert!(rig.sent.try_recv().is_err());
        assert_eq!(rig.line()["result"]["stopReason"], "end_turn");
    }

    #[test]
    fn session_ids_are_safe_and_distinct() {
        let a = session_id_for("/Users/x/My Project");
        let b = session_id_for("/Users/x/My Project");
        assert!(a.starts_with("acp:my-project:"));
        assert_ne!(a, b);
    }
}
