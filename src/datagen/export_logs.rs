//! `export_logs`: turn recorded chat turns into a training/eval dataset.
//!
//! Reads the trajectory JSONL files ([`crate::trajectory`]) of the chats named
//! (or `*` for every chat that has any) and writes one record per turn in one
//! of three shapes:
//! - `sharegpt` — `{"conversations": [{"from": "human"|"gpt"|"function_call"|"observation", "value"}]}`,
//!   tool calls kept, the shape CAMEL's ShareGPT collector writes;
//! - `alpaca` — `{"instruction", "input": "", "output"}` from the turn's first
//!   user message and its last answer;
//! - `messages` — OpenAI-style `{"messages": [...]}` without the trajectory's
//!   bookkeeping fields.
//!
//! No model is called. A turn is skipped when it was interrupted or ended in
//! an error (its last answer is not one to learn from), when it has no answer,
//! or when it repeats one already written.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::generator::AcceptLoop;
use super::jobs::{JobCtx, Outcome};

pub const MAX_RECORDS: u32 = 100_000;
const DEFAULT_MAX_RECORDS: u32 = 10_000;
/// Publish progress every this many turns.
const PUBLISH_EVERY: u32 = 50;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Sharegpt,
    Alpaca,
    Messages,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Params {
    /// Chat jids, or `["*"]` for every chat with recorded turns.
    pub chats: Vec<String>,
    #[serde(default)]
    pub format: Format,
    #[serde(default)]
    pub max_records: Option<u32>,
}

impl Params {
    pub fn validate(&self) -> Result<(), String> {
        if self.chats.iter().all(|c| c.trim().is_empty()) {
            return Err("chats is empty — name the chats to export, or \"*\" for all".into());
        }
        if self.max_records.is_some_and(|n| n == 0 || n > MAX_RECORDS) {
            return Err(format!("maxRecords must be between 1 and {MAX_RECORDS}"));
        }
        Ok(())
    }

    fn all_chats(&self) -> bool {
        self.chats.iter().any(|c| c.trim() == "*")
    }
}

pub async fn run(params: Params, root: &Path, ctx: &mut JobCtx) -> Result<Outcome, String> {
    params.validate()?;
    // Directory walks and file reads are blocking I/O over what may be
    // thousands of files; keep them off the async workers.
    let (p, r) = (params.clone(), root.to_path_buf());
    let files = tokio::task::spawn_blocking(move || turn_files(&p, &r)).await.map_err(|e| e.to_string())?;
    let cap = params.max_records.unwrap_or(DEFAULT_MAX_RECORDS);
    let mut l = AcceptLoop::new(cap.min(files.len() as u32), 0);
    ctx.set_target(l.target, 0);
    if files.is_empty() {
        ctx.set_note("no recorded turns were found for these chats — is the trajectory turned on for them?");
        return Ok(Outcome::Finished);
    }

    // Duplicates are found by a hash of the record, not the record itself:
    // a turn can be tens of KB, and an export may hold 100k of them.
    let mut seen: HashSet<u64> = HashSet::new();
    for (chat, path) in files {
        if ctx.cancel.is_cancelled() {
            ctx.publish(&l);
            return Ok(Outcome::Cancelled);
        }
        if l.accepted >= l.target {
            break;
        }
        l.attempts += 1;
        let turn = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let format = params.format;
        let converted = tokio::task::spawn_blocking(move || load_turn(&path, format)).await.map_err(|e| e.to_string())?;
        match converted {
            Err(reason) => l.reject(reason),
            Ok((mut rec, hash)) => {
                if !seen.insert(hash) {
                    l.reject("duplicate");
                    continue;
                }
                rec["source"] = json!({ "chat": chat, "turn": turn });
                ctx.write(&rec).map_err(|e| format!("cannot write the dataset: {e}"))?;
                l.accept();
            }
        }
        if l.attempts.is_multiple_of(PUBLISH_EVERY) {
            ctx.publish(&l);
        }
    }
    ctx.publish(&l);
    Ok(Outcome::Finished)
}

/// Every turn file of the chats asked for, as `(chat folder, path)`.
fn turn_files(params: &Params, root: &Path) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    for dir in chat_dirs(params, root) {
        let chat = dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut turns: Vec<PathBuf> =
            rd.flatten().map(|e| e.path()).filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl")).collect();
        // Turn ids start with a UTC timestamp, so name order is time order.
        turns.sort();
        files.extend(turns.into_iter().map(|p| (chat.clone(), p)));
    }
    files
}

/// One turn file as a record plus the hash its duplicates share.
fn load_turn(path: &Path, format: Format) -> Result<(Value, u64), &'static str> {
    let raw = std::fs::read_to_string(path).map_err(|_| "unreadable")?;
    let lines: Vec<Value> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    let rec = convert(&lines, format)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    rec.to_string().hash(&mut h);
    Ok((rec, h.finish()))
}

fn chat_dirs(params: &Params, root: &Path) -> Vec<PathBuf> {
    if params.all_chats() {
        let Ok(rd) = std::fs::read_dir(root) else { return Vec::new() };
        let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        return dirs;
    }
    let mut dirs: Vec<PathBuf> = params
        .chats
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .map(|jid| root.join(crate::trajectory::dir_name(jid)))
        .filter(|p| p.is_dir())
        .collect();
    dirs.dedup();
    dirs
}

/// One recorded turn as a record of `format`, or the reason it is skipped.
pub fn convert(lines: &[Value], format: Format) -> Result<Value, &'static str> {
    if lines.is_empty() {
        return Err("empty_turn");
    }
    for l in lines {
        if role(l) == "meta" {
            match l.get("kind").and_then(Value::as_str) {
                Some("interrupted") => return Err("interrupted"),
                Some("error") => return Err("errored"),
                _ => {}
            }
        }
    }
    let answer = lines
        .iter()
        .rev()
        .find(|l| role(l) == "assistant" && !text(l).trim().is_empty())
        .map(text);
    let question = lines.iter().find(|l| role(l) == "user").map(text).filter(|q| !q.trim().is_empty());
    let (Some(question), Some(answer)) = (question, answer) else {
        return Err("no_answer");
    };
    // The last thing said must be the answer; a turn that ends on a tool call
    // or a tool result stopped mid-work.
    let last = lines.iter().rev().find(|l| matches!(role(l), "user" | "assistant" | "tool"));
    if last.is_none_or(|l| role(l) != "assistant" || l.get("tool_calls").is_some()) {
        return Err("no_answer");
    }

    Ok(match format {
        Format::Alpaca => json!({ "instruction": question, "input": "", "output": answer }),
        Format::Messages => json!({ "messages": lines.iter().filter_map(message).collect::<Vec<_>>() }),
        Format::Sharegpt => json!({ "conversations": lines.iter().flat_map(sharegpt_turns).collect::<Vec<_>>() }),
    })
}

fn role(l: &Value) -> &str {
    l.get("role").and_then(Value::as_str).unwrap_or("")
}

fn text(l: &Value) -> String {
    match l.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn message(l: &Value) -> Option<Value> {
    match role(l) {
        "user" => Some(json!({ "role": "user", "content": text(l) })),
        "assistant" => {
            let mut m = json!({ "role": "assistant", "content": text(l) });
            if let Some(calls) = l.get("tool_calls") {
                m["tool_calls"] = calls.clone();
            }
            Some(m)
        }
        "tool" => Some(json!({
            "role": "tool",
            "tool_call_id": l.get("tool_call_id").cloned().unwrap_or(Value::Null),
            "name": l.get("name").cloned().unwrap_or(Value::Null),
            "content": text(l),
        })),
        _ => None,
    }
}

fn sharegpt_turns(l: &Value) -> Vec<Value> {
    match role(l) {
        "user" => vec![json!({ "from": "human", "value": text(l) })],
        "tool" => vec![json!({ "from": "observation", "value": text(l) })],
        "assistant" => {
            let mut out = Vec::new();
            let t = text(l);
            if !t.trim().is_empty() {
                out.push(json!({ "from": "gpt", "value": t }));
            }
            for call in l.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                let f = &call["function"];
                let args = f["arguments"]
                    .as_str()
                    .and_then(|a| serde_json::from_str::<Value>(a).ok())
                    .unwrap_or_else(|| f["arguments"].clone());
                out.push(json!({ "from": "function_call", "value": json!({ "name": f["name"], "arguments": args }).to_string() }));
            }
            out
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datagen::store::DATA_FILE;

    fn turn() -> Vec<Value> {
        vec![
            json!({"role": "user", "content": "thời tiết Hà Nội hôm nay?", "ts": 1, "turnId": "t1", "chatJid": "web:a"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "web_search", "arguments": "{\"q\":\"thời tiết Hà Nội\"}"}}], "reasoning": "need data", "ts": 2}),
            json!({"role": "tool", "tool_call_id": "call_1", "name": "web_search", "title": "t", "summary": "s", "content": "28°C, mưa rào", "ok": true, "ts": 3}),
            json!({"role": "meta", "kind": "usage", "usage": {"input_tokens": 5}, "ts": 4}),
            json!({"role": "assistant", "content": "Hà Nội hôm nay 28°C, có mưa rào.", "ts": 5}),
        ]
    }

    #[test]
    fn sharegpt_keeps_tool_calls_and_observations() {
        let r = convert(&turn(), Format::Sharegpt).unwrap();
        let froms: Vec<&str> = r["conversations"].as_array().unwrap().iter().map(|c| c["from"].as_str().unwrap()).collect();
        assert_eq!(froms, vec!["human", "function_call", "observation", "gpt"]);
        let call: Value = serde_json::from_str(r["conversations"][1]["value"].as_str().unwrap()).unwrap();
        assert_eq!(call, json!({"name": "web_search", "arguments": {"q": "thời tiết Hà Nội"}}));
    }

    #[test]
    fn alpaca_and_messages_drop_bookkeeping() {
        let a = convert(&turn(), Format::Alpaca).unwrap();
        assert_eq!(a, json!({"instruction": "thời tiết Hà Nội hôm nay?", "input": "", "output": "Hà Nội hôm nay 28°C, có mưa rào."}));
        let m = convert(&turn(), Format::Messages).unwrap();
        let msgs = m["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4, "meta line dropped");
        assert!(msgs.iter().all(|m| m.get("ts").is_none() && m.get("reasoning").is_none() && m.get("summary").is_none()));
        assert_eq!(msgs[2]["tool_call_id"], "call_1");
    }

    #[test]
    fn skips_turns_that_did_not_end_in_an_answer() {
        let mut interrupted = turn();
        interrupted.push(json!({"role": "meta", "kind": "interrupted"}));
        assert_eq!(convert(&interrupted, Format::Alpaca), Err("interrupted"));
        let mut errored = turn();
        errored.push(json!({"role": "meta", "kind": "error"}));
        assert_eq!(convert(&errored, Format::Alpaca), Err("errored"));
        let mid_work = turn()[..3].to_vec();
        assert_eq!(convert(&mid_work, Format::Sharegpt), Err("no_answer"));
        assert_eq!(convert(&[], Format::Sharegpt), Err("empty_turn"));
    }

    fn write_turn(root: &Path, jid: &str, id: &str, lines: &[Value]) {
        let dir = root.join(crate::trajectory::dir_name(jid));
        std::fs::create_dir_all(&dir).unwrap();
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(dir.join(format!("{id}.jsonl")), body).unwrap();
    }

    #[tokio::test]
    async fn exports_named_chats_dedupes_and_tags_the_source() {
        let tmp = tempfile::tempdir().unwrap();
        let traj = tmp.path().join("traj");
        write_turn(&traj, "web:a", "20261012T010000.000Z-aaaaaa", &turn());
        write_turn(&traj, "web:a", "20261012T020000.000Z-bbbbbb", &turn());
        write_turn(&traj, "web:a", "20261012T030000.000Z-cccccc", &turn()[..2]);
        let mut other = turn();
        other[4]["content"] = json!("Trời nắng, 31°C.");
        write_turn(&traj, "web:other", "20261012T010000.000Z-dddddd", &other);

        let mut ctx = JobCtx::for_test(&tmp.path().join("out"));
        let p = Params { chats: vec!["web:a".into()], format: Format::Alpaca, max_records: None };
        run(p, &traj, &mut ctx).await.unwrap();

        let out: Vec<Value> = std::fs::read_to_string(ctx.dir().join(DATA_FILE))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(out.len(), 1, "the second turn repeats the first; the third has no answer");
        assert_eq!(out[0]["source"]["chat"], "web_a");
        assert_eq!(out[0]["source"]["turn"], "20261012T010000.000Z-aaaaaa");
        let st = ctx.snapshot();
        assert_eq!((st.target, st.attempts), (3, 3));
        assert_eq!(st.rejected.get("duplicate"), Some(&1));
        assert_eq!(st.rejected.get("no_answer"), Some(&1));

        let mut ctx = JobCtx::for_test(&tmp.path().join("out2"));
        run(Params { chats: vec!["*".into()], format: Format::Messages, max_records: Some(2) }, &traj, &mut ctx)
            .await
            .unwrap();
        assert_eq!(ctx.snapshot().accepted, 2, "capped by maxRecords");
    }

    #[test]
    fn validation_requires_chats() {
        let p = Params { chats: vec![" ".into()], format: Format::default(), max_records: None };
        assert!(p.validate().is_err());
        let p = Params { chats: vec!["web:a".into()], format: Format::default(), max_records: Some(0) };
        assert!(p.validate().is_err());
    }
}
