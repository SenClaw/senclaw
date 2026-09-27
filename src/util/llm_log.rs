//! Best-effort file logging of LLM requests/responses for prompt analysis.
//!
//! Port of the request/response side of TS `util/logLLM.ts`, which the Rust
//! daemon never carried over — so until now the full system prompt + message
//! history sent to the model was nowhere on disk, making prompt debugging
//! (e.g. an agent stuck re-invoking a skill in a loop) impossible after the
//! fact.
//!
//! Writes one JSON line per request and per response to
//! `<dir>/<YYYY-MM-DD>.log`, matching the TS `[HH:MM:SS]{json}` format so the
//! same tooling (`scripts/dump_llm_prompt.py`) reads both TS and Rust logs.
//!
//! - Enabled by default. Disable with `SENCLAW_LLM_LOG=0` (or `false`).
//! - Directory override: `SENCLAW_LLM_LOG_DIR` (default `~/.senclaw/llm_logs`).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use chrono::{Local, Timelike};
use serde_json::{json, Value};

use crate::zen_core::{ContentBlock, Message};

/// Whether file logging is enabled (cheap env check; not cached so tests/runs
/// can toggle it without a restart).
fn enabled() -> bool {
    match std::env::var("SENCLAW_LLM_LOG") {
        Ok(v) => {
            let v = v.trim().to_lowercase();
            v != "0" && v != "false" && v != "off" && v != "no"
        }
        Err(_) => true,
    }
}

fn logs_dir() -> PathBuf {
    if let Ok(d) = std::env::var("SENCLAW_LLM_LOG_DIR") {
        if !d.trim().is_empty() {
            return PathBuf::from(d);
        }
    }
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".senclaw").join("llm_logs")
}

/// `HH:MM:SS` in local time, matching the TS `getTimeString()`.
fn time_str() -> String {
    let now = Local::now();
    format!("{:02}:{:02}:{:02}", now.hour(), now.minute(), now.second())
}

/// Keep at most this many `.log` files (matches TS
/// `LLM_LOG_FILES_RETAIN_COUNT`); older files are deleted.
const LOG_FILES_RETAIN_COUNT: usize = 30;
/// Throttle cleanup scans to once per interval per process.
const CLEANUP_INTERVAL_SECS: u64 = 3600;

static LAST_CLEANUP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Delete all but the newest [`LOG_FILES_RETAIN_COUNT`] `.log` files (by
/// mtime). Runs at most once per [`CLEANUP_INTERVAL_SECS`]; without this the
/// daily log files accumulate forever (the TS side archives after 30 files).
fn maybe_cleanup(dir: &std::path::Path) {
    use std::sync::atomic::Ordering;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST_CLEANUP.load(Ordering::Relaxed);
    if now.saturating_sub(last) < CLEANUP_INTERVAL_SECS {
        return;
    }
    if LAST_CLEANUP
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return; // another thread is on it
    }

    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "log").unwrap_or(false))
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .collect();
    if files.len() <= LOG_FILES_RETAIN_COUNT {
        return;
    }
    // Newest first; delete the tail.
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in files.into_iter().skip(LOG_FILES_RETAIN_COUNT) {
        let _ = fs::remove_file(path);
    }
}

/// Append a `[HH:MM:SS]{json}` line to today's log file. Never panics.
fn append_line(payload: &Value) {
    if !enabled() {
        return;
    }
    let dir = logs_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let file = dir.join(format!(
        "{}.log",
        crate::util::local_time::local_date_string_now()
    ));
    let line = format!("[{}]{}\n", time_str(), payload);
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&file) {
        let _ = f.write_all(line.as_bytes());
    }
    maybe_cleanup(&dir);
}

/// Log an outgoing LLM request. `system_prompt` is wrapped as a single text
/// block (`[{"type":"text","text":...}]`) to mirror the Anthropic-shaped TS
/// log and keep the analysis tooling's system-block logic working.
pub fn log_request(
    model: &str,
    system_prompt: &str,
    messages: &[Message],
    tool_names: &[(String, String)],
    thinking: bool,
) {
    if !enabled() {
        return;
    }
    // API-shaped messages: role at top level, content as Anthropic blocks.
    let api_messages: Vec<Value> = messages
        .iter()
        .map(|m| json!({ "role": m.message.role, "content": m.message.content }))
        .collect();
    let tools: Vec<Value> = tool_names
        .iter()
        .map(|(name, desc)| json!({ "name": name, "description": desc }))
        .collect();
    let payload = json!({
        "model": model,
        "messages": api_messages,
        "system": [{ "type": "text", "text": system_prompt }],
        "tools": tools,
        "thinking": thinking,
    });
    append_line(&payload);
}

/// Log an assistant response, extracting text / thinking / tool calls — the
/// same compact shape the TS `logLLMResponse` produced.
pub fn log_response(msg: &Message) {
    if !enabled() {
        return;
    }
    let mut content = String::new();
    let mut thinking = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for block in &msg.message.content {
        match block {
            ContentBlock::Text { text } => content.push_str(text),
            ContentBlock::Thinking { thinking: t } => thinking.push_str(t),
            ContentBlock::ToolUse { name, input, .. } => {
                tool_calls.push(json!({ "name": name, "args": input }));
            }
            _ => {}
        }
    }
    let mut payload = serde_json::Map::new();
    if !thinking.is_empty() {
        payload.insert("thinking".into(), Value::String(thinking));
    }
    payload.insert("content".into(), Value::String(content));
    if !tool_calls.is_empty() {
        payload.insert("toolCalls".into(), Value::Array(tool_calls));
    }
    // Token usage — makes the audit log a per-call historical cost record.
    if let Some(u) = msg.usage.as_ref().filter(|u| !u.is_empty()) {
        if let Ok(v) = serde_json::to_value(u) {
            payload.insert("usage".into(), v);
        }
    }
    append_line(&Value::Object(payload));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zen_core::MessagePayload;

    fn msg(role: &str, blocks: Vec<ContentBlock>) -> Message {
        Message {
            msg_type: if role == "user" { "user" } else { "assistant" }.into(),
            message: MessagePayload {
                role: role.into(),
                content: blocks,
            },
            uuid: "test".into(),
            usage: None,
        }
    }

    #[test]
    fn writes_request_and_response_in_ts_format() {
        let dir = std::env::temp_dir().join(format!("senclaw_llm_log_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        std::env::set_var("SENCLAW_LLM_LOG_DIR", &dir);
        std::env::set_var("SENCLAW_LLM_LOG", "1");

        log_request(
            "test-model",
            "You are a test.",
            &[msg("user", vec![ContentBlock::Text { text: "hi".into() }])],
            &[("Bash".into(), "Run a command".into())],
            true,
        );
        log_response(&msg(
            "assistant",
            vec![
                ContentBlock::Thinking {
                    thinking: "pondering".into(),
                },
                ContentBlock::ToolUse {
                    id: "1".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({ "command": "ls" }),
                },
            ],
        ));

        let file = dir.join(format!(
            "{}.log",
            crate::util::local_time::local_date_string_now()
        ));
        let body = fs::read_to_string(&file).expect("log file written");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "one request + one response line");

        // [HH:MM:SS]{json} prefix
        assert!(lines[0].starts_with('['));
        let req_json = &lines[0][lines[0].find(']').unwrap() + 1..];
        let req: Value = serde_json::from_str(req_json).unwrap();
        assert_eq!(req["model"], "test-model");
        assert_eq!(req["system"][0]["text"], "You are a test.");
        assert_eq!(req["messages"][0]["role"], "user");
        assert_eq!(req["tools"][0]["name"], "Bash");

        let resp_json = &lines[1][lines[1].find(']').unwrap() + 1..];
        let resp: Value = serde_json::from_str(resp_json).unwrap();
        assert_eq!(resp["thinking"], "pondering");
        assert_eq!(resp["toolCalls"][0]["name"], "Bash");
        assert_eq!(resp["toolCalls"][0]["args"]["command"], "ls");

        let _ = fs::remove_dir_all(&dir);
        std::env::remove_var("SENCLAW_LLM_LOG_DIR");
    }
}
