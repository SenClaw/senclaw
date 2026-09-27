//! Trajectory log — every turn of a chat as an append-only JSONL file in the
//! shape `agentevals` reads (OpenAI-style messages), so a turn can be
//! replayed step by step in the UI and scored by an evals runner without
//! any change to `run_one_shot` or the engine (OpenHands' event-log idea,
//! kept as a side channel off `EngineEvent`).
//!
//! Layout: `~/.senclaw/trajectories/<chat>/<turn-id>.jsonl`, one JSON object
//! per line:
//!
//! ```json
//! {"role":"user","content":"…","ts":…}
//! {"role":"assistant","content":"…","tool_calls":[{"id":"c1","type":"function","function":{"name":"Edit","arguments":"{…}"}}],"reasoning":"…","ts":…}
//! {"role":"tool","tool_call_id":"c1","name":"Edit","content":"…","ok":true,"ts":…}
//! {"role":"meta","usage":{…},"model":"…","ts":…}
//! ```
//!
//! Off by default. Enabled per chat (`~/.senclaw/trajectories/enabled.json`,
//! toggled from the API) or globally with `SENCLAW_TRAJECTORY=1`. Files are
//! written 0600 in a 0700 directory: they carry the contents of files the
//! agent read.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::zen_core::EngineEvent;

/// Largest tool result kept per line; the rest is cut and flagged.
const MAX_TOOL_CONTENT: usize = 32 * 1024;

fn root() -> PathBuf {
    std::env::var("SENCLAW_TRAJECTORIES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".senclaw").join("trajectories"))
}

fn safe(jid: &str) -> String {
    jid.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

pub fn chat_dir(jid: &str) -> PathBuf {
    root().join(safe(jid))
}

// ----- enablement -----

struct Enabled {
    jids: HashSet<String>,
    /// Enabled for this process only, never written to `enabled.json`: a
    /// one-shot run (`senclaw agent-task --trajectory …`, the evals runner)
    /// says so on its command line and must not leave recording switched on
    /// for a chat with that id afterwards.
    forced: HashSet<String>,
    loaded: bool,
}

static ENABLED: OnceLock<Mutex<Enabled>> = OnceLock::new();

fn enabled_file() -> PathBuf {
    root().join("enabled.json")
}

fn enabled() -> &'static Mutex<Enabled> {
    ENABLED.get_or_init(|| Mutex::new(Enabled { jids: HashSet::new(), forced: HashSet::new(), loaded: false }))
}

fn ensure_loaded(e: &mut Enabled) {
    if e.loaded {
        return;
    }
    e.loaded = true;
    if let Ok(raw) = std::fs::read(enabled_file()) {
        if let Ok(list) = serde_json::from_slice::<Vec<String>>(&raw) {
            e.jids = list.into_iter().collect();
        }
    }
}

pub fn is_enabled(jid: &str) -> bool {
    if std::env::var("SENCLAW_TRAJECTORY").map(|v| v == "1").unwrap_or(false) {
        return true;
    }
    let mut e = enabled().lock().unwrap();
    if e.forced.contains(jid) {
        return true;
    }
    ensure_loaded(&mut e);
    e.jids.contains(jid)
}

/// Record `jid` for the lifetime of this process without persisting the
/// choice. The opt-in is the caller's own flag, so nothing needs to be turned
/// off afterwards — and an isolated run cannot flip recording on for a real
/// chat that happens to share the id.
pub fn force_enable(jid: &str) {
    let mut e = enabled().lock().unwrap();
    e.forced.insert(jid.to_string());
}

pub fn set_enabled(jid: &str, on: bool) -> std::io::Result<()> {
    let mut e = enabled().lock().unwrap();
    ensure_loaded(&mut e);
    if on {
        e.jids.insert(jid.to_string());
    } else {
        e.jids.remove(jid);
    }
    let mut list: Vec<&String> = e.jids.iter().collect();
    list.sort();
    let dir = root();
    std::fs::create_dir_all(&dir)?;
    restrict_dir(&dir);
    std::fs::write(enabled_file(), serde_json::to_vec_pretty(&list)?)?;
    Ok(())
}

#[cfg(unix)]
fn restrict_dir(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
}
#[cfg(not(unix))]
fn restrict_dir(_p: &Path) {}

#[cfg(unix)]
fn restrict_file(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn restrict_file(_p: &Path) {}

// ----- writer -----

struct TurnState {
    file: PathBuf,
    /// Tool calls announced by the last assistant message, in order; each
    /// tool result takes the next id so results link to their call.
    pending_calls: VecDeque<(String, String)>,
    seq: u64,
}

static TURNS: OnceLock<Mutex<HashMap<String, TurnState>>> = OnceLock::new();

fn turns() -> &'static Mutex<HashMap<String, TurnState>> {
    TURNS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn append(file: &Path, line: &Value) {
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
        restrict_dir(parent);
    }
    let fresh = !file.exists();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(file) {
        let _ = writeln!(f, "{}", line);
        if fresh {
            restrict_file(file);
        }
    }
}

fn cut(s: &str) -> (String, bool) {
    if s.len() > MAX_TOOL_CONTENT {
        (crate::util::text::truncate_on_char_boundary(s, MAX_TOOL_CONTENT).to_string(), true)
    } else {
        (s.to_string(), false)
    }
}

/// Record one engine event for `jid`. Cheap no-op when the chat is not
/// enabled; otherwise appends to the current turn's file. Never blocks the
/// engine: file I/O is small and local.
pub fn record(jid: &str, ev: &EngineEvent) {
    // A new user input opens a turn; everything else needs an open turn and
    // is dropped otherwise (events from before enabling).
    if let EngineEvent::InputReceived(d) = ev {
        if !is_enabled(jid) {
            return;
        }
        if d.queued {
            // Mid-turn injections belong to the running turn.
            let mut t = turns().lock().unwrap();
            if let Some(ts) = t.get_mut(jid) {
                ts.seq += 1;
                append(&ts.file, &json!({ "role": "user", "content": d.input, "injected": true, "ts": now_ms() }));
            }
            return;
        }
        let turn_id = format!("{}-{}", chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ"), &uuid::Uuid::new_v4().to_string()[..6]);
        let file = chat_dir(jid).join(format!("{turn_id}.jsonl"));
        append(&file, &json!({ "role": "user", "content": d.input, "ts": now_ms(), "turnId": turn_id, "chatJid": jid }));
        turns().lock().unwrap().insert(jid.to_string(), TurnState { file, pending_calls: VecDeque::new(), seq: 1 });
        return;
    }
    let mut t = turns().lock().unwrap();
    let Some(ts) = t.get_mut(jid) else { return };
    match ev {
        EngineEvent::MessageComplete(d) => {
            ts.seq += 1;
            let mut calls = Vec::new();
            ts.pending_calls.clear();
            if let Some(tcs) = &d.tool_calls {
                for tc in tcs {
                    let id = format!("call_{}", ts.seq * 100 + calls.len() as u64);
                    ts.pending_calls.push_back((id.clone(), tc.name.clone()));
                    calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": { "name": tc.name, "arguments": tc.args.to_string() }
                    }));
                }
            }
            let mut line = json!({ "role": "assistant", "content": d.content, "ts": now_ms() });
            if !calls.is_empty() {
                line["tool_calls"] = Value::Array(calls);
            }
            if !d.reasoning.is_empty() {
                line["reasoning"] = Value::String(d.reasoning.clone());
            }
            append(&ts.file, &line);
        }
        EngineEvent::ToolExecutionComplete(d) => {
            ts.seq += 1;
            let (id, _) = ts.pending_calls.pop_front().unwrap_or_else(|| (format!("call_{}", ts.seq), d.tool_name.clone()));
            let text = match &d.content {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let (content, truncated) = cut(&text);
            let mut line = json!({ "role": "tool", "tool_call_id": id, "name": d.tool_name, "title": d.title, "summary": d.summary, "content": content, "ok": true, "ts": now_ms() });
            if truncated {
                line["truncated"] = Value::Bool(true);
            }
            append(&ts.file, &line);
        }
        EngineEvent::ToolExecutionError(d) => {
            ts.seq += 1;
            let (id, _) = ts.pending_calls.pop_front().unwrap_or_else(|| (format!("call_{}", ts.seq), d.tool_name.clone()));
            let (content, _) = cut(&d.content);
            append(&ts.file, &json!({ "role": "tool", "tool_call_id": id, "name": d.tool_name, "title": d.title, "content": content, "ok": false, "ts": now_ms() }));
        }
        EngineEvent::LlmUsage(d) => {
            append(&ts.file, &json!({ "role": "meta", "kind": "usage", "source": d.source, "model": d.model, "provider": d.provider, "profile": d.profile, "usage": d.usage, "ts": now_ms() }));
        }
        EngineEvent::SessionInterrupted(_) => {
            append(&ts.file, &json!({ "role": "meta", "kind": "interrupted", "ts": now_ms() }));
        }
        EngineEvent::SessionError(d) => {
            append(&ts.file, &json!({ "role": "meta", "kind": "error", "detail": serde_json::to_value(d).unwrap_or(Value::Null), "ts": now_ms() }));
        }
        _ => {}
    }
}

// ----- reading -----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnSummary {
    pub turn_id: String,
    pub started_at: Option<i64>,
    pub lines: usize,
    pub tool_calls: usize,
    pub first_user_line: String,
    pub bytes: u64,
}

pub fn list_turns(jid: &str) -> Vec<TurnSummary> {
    let dir = chat_dir(jid);
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&p) else { continue };
        let mut lines = 0;
        let mut calls = 0;
        let mut first = String::new();
        let mut started = None;
        for l in raw.lines() {
            let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
            lines += 1;
            if started.is_none() {
                started = v.get("ts").and_then(|t| t.as_i64());
            }
            if first.is_empty() && v.get("role").and_then(|r| r.as_str()) == Some("user") {
                first = v.get("content").and_then(|c| c.as_str()).unwrap_or("").chars().take(120).collect();
            }
            if let Some(tc) = v.get("tool_calls").and_then(|t| t.as_array()) {
                calls += tc.len();
            }
        }
        out.push(TurnSummary {
            turn_id: p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
            started_at: started,
            lines,
            tool_calls: calls,
            first_user_line: first,
            bytes: e.metadata().map(|m| m.len()).unwrap_or(0),
        });
    }
    out.sort_by(|a, b| b.turn_id.cmp(&a.turn_id));
    out
}

pub fn read_turn(jid: &str, turn_id: &str) -> Option<Vec<Value>> {
    if turn_id.contains('/') || turn_id.contains("..") {
        return None;
    }
    let p = chat_dir(jid).join(format!("{turn_id}.jsonl"));
    let raw = std::fs::read_to_string(p).ok()?;
    Some(raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
}

pub fn delete_all(jid: &str) -> std::io::Result<usize> {
    let dir = chat_dir(jid);
    if !dir.exists() {
        return Ok(0);
    }
    let n = std::fs::read_dir(&dir)?.count();
    std::fs::remove_dir_all(&dir)?;
    turns().lock().unwrap().remove(jid);
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zen_core::{InputReceivedData, MessageCompleteData, ToolCallInfo, ToolExecutionCompleteData, ToolExecutionErrorData};

    #[test]
    fn a_turn_becomes_agentevals_style_lines() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("SENCLAW_TRAJECTORIES_DIR", tmp.path());
        let jid = "web:traj:1";
        set_enabled(jid, true).unwrap();
        assert!(is_enabled(jid));
        assert!(!is_enabled("web:other"));

        record(jid, &EngineEvent::InputReceived(InputReceivedData { input: "fix the bug".into(), queued: false, inject: false, queue_length: 0 }));
        record(
            jid,
            &EngineEvent::MessageComplete(MessageCompleteData {
                agent_id: "main".into(),
                reasoning: String::new(),
                content: "Let me look.".into(),
                has_tool_calls: true,
                tool_calls: Some(vec![
                    ToolCallInfo { name: "Read".into(), args: json!({"file_path": "/w/a.rs"}) },
                    ToolCallInfo { name: "Edit".into(), args: json!({"file_path": "/w/a.rs"}) },
                ]),
                output_tokens: 0,
            }),
        );
        record(jid, &EngineEvent::ToolExecutionComplete(ToolExecutionCompleteData { agent_id: "main".into(), tool_name: "Read".into(), title: "a.rs".into(), summary: "12 lines".into(), description: String::new(), content: json!({"text": "fn a(){}"}) }));
        record(jid, &EngineEvent::ToolExecutionError(ToolExecutionErrorData { agent_id: "main".into(), tool_name: "Edit".into(), title: "a.rs".into(), description: String::new(), content: "old_string not found".into(), args_shape: Default::default() }));
        record(jid, &EngineEvent::MessageComplete(MessageCompleteData { agent_id: "main".into(), reasoning: String::new(), content: "Done.".into(), has_tool_calls: false, tool_calls: None, output_tokens: 0 }));

        let turns = list_turns(jid);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].tool_calls, 2);
        assert_eq!(turns[0].first_user_line, "fix the bug");
        let lines = read_turn(jid, &turns[0].turn_id).unwrap();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0]["role"], "user");
        assert_eq!(lines[1]["role"], "assistant");
        let call_id = lines[1]["tool_calls"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(lines[1]["tool_calls"][0]["function"]["name"], "Read");
        assert_eq!(lines[2]["role"], "tool");
        assert_eq!(lines[2]["tool_call_id"], call_id, "results link to their call in order");
        assert_eq!(lines[3]["ok"], false);
        assert_eq!(lines[3]["tool_call_id"], lines[1]["tool_calls"][1]["id"]);
        assert_eq!(lines[4]["content"], "Done.");

        // A second input opens a second turn; disabled chats write nothing.
        record(jid, &EngineEvent::InputReceived(InputReceivedData { input: "again".into(), queued: false, inject: false, queue_length: 0 }));
        assert_eq!(list_turns(jid).len(), 2);
        record("web:other", &EngineEvent::InputReceived(InputReceivedData { input: "x".into(), queued: false, inject: false, queue_length: 0 }));
        assert!(list_turns("web:other").is_empty());
        assert!(read_turn(jid, "../etc").is_none());
        assert_eq!(delete_all(jid).unwrap(), 2);
    }
}
