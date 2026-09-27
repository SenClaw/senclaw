//! Trace neutral-v1 (§16) — one JSON file per turn, metadata only: which
//! specs decided what, token/cache counts per LLM call, which tools ran and
//! how they failed, never a message or a file's content. On by default,
//! bounded by retention, 0600 in a 0700 directory — the same posture
//! [`crate::trajectory`] uses for full message content, minus the content.
//!
//! Fed from the same per-chat event seam trajectory/failures already use
//! (`agent_pool/engine.rs`'s event-bus subscriber, `isolated_runner.rs`'s
//! one-shot loop) plus one extra input those don't have: explicit
//! [`record_decision`] calls from the gate, the router and the new shadow
//! specs, since a Jev decision is not an `EngineEvent` at all.
//!
//! [`record_decision_input`] is the one opt-in exception to "metadata only":
//! with `controlPlane.recordDecisionInputs` on, the exact `state`/`questions`
//! sent to a spec are written to a *separate* file, never merged into the
//! trace itself, so the trace's own "no message content" guarantee holds
//! regardless of that switch.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use serde::Serialize;

use crate::decision::json::Json;
use crate::zen_core::{EngineEvent, MAIN_AGENT_ID};

use super::ladder::Decision;
use super::settings::ControlPlaneSettings;
use super::{restrict_dir, restrict_file, safe_id, senclaw_home};

/// Files kept per chat before the oldest are pruned. A trace is small
/// (kilobytes), so this bounds disk, not information density.
const MAX_TRACES_PER_CHAT: usize = 200;

fn root() -> PathBuf {
    std::env::var("SENCLAW_TRACES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| senclaw_home().join("control-plane").join("traces"))
}

fn decision_inputs_root() -> PathBuf {
    std::env::var("SENCLAW_DECISION_INPUTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| senclaw_home().join("control-plane").join("decision-inputs"))
}

pub fn chat_dir(jid: &str) -> PathBuf {
    root().join(safe_id(jid))
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct LlmCallRecord {
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read: u64,
    pub compact: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ToolCallRecord {
    pub id: String,
    pub name: String,
    pub fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Trace {
    pub trace_id: String,
    pub lang: String,
    pub format: &'static str,
    pub decisions: Vec<Decision>,
    pub llm_calls: Vec<LlmCallRecord>,
    pub tool_calls: Vec<ToolCallRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution: Option<serde_json::Value>,
    pub synthetic_placeholders: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<serde_json::Value>,
}

impl Trace {
    fn new(trace_id: String) -> Trace {
        Trace {
            trace_id,
            lang: String::new(),
            format: "neutral-v1",
            decisions: Vec::new(),
            llm_calls: Vec::new(),
            tool_calls: Vec::new(),
            verdict: None,
            attribution: None,
            synthetic_placeholders: 0,
            outcome: None,
        }
    }
}

fn gen_trace_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let rand: u32 = rand::random();
    format!("t_{millis:x}{:04x}", rand & 0xFFFF)
}

static OPEN: Lazy<Mutex<HashMap<String, Trace>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Feed one engine event for `jid` into the trace. Always on — like the
/// failure ledger, this only ever writes metadata, so there is no cost
/// argument for gating it the way shadow specs are gated.
pub fn record(jid: &str, event: &EngineEvent) {
    let mut guard = OPEN.lock().unwrap();
    match event {
        EngineEvent::InputReceived(_) => {
            guard.insert(jid.to_string(), Trace::new(gen_trace_id()));
        }
        EngineEvent::LlmUsage(d) => {
            let t = guard.entry(jid.to_string()).or_insert_with(|| Trace::new(gen_trace_id()));
            let u = &d.usage;
            t.llm_calls.push(LlmCallRecord {
                tokens_in: u.input_tokens.or(u.prompt_tokens).unwrap_or(0),
                tokens_out: u.output_tokens.or(u.completion_tokens).unwrap_or(0),
                cache_read: u.cache_read_input_tokens.unwrap_or(0),
                compact: d.source == "compact",
            });
        }
        EngineEvent::ToolExecutionComplete(d) => {
            let t = guard.entry(jid.to_string()).or_insert_with(|| Trace::new(gen_trace_id()));
            let id = format!("c_{}", t.tool_calls.len() + 1);
            t.tool_calls.push(ToolCallRecord {
                fingerprint: super::loop_controller::fingerprint(&d.tool_name, &d.content),
                id,
                name: d.tool_name.clone(),
                error_type: None,
                artifact: None,
            });
        }
        EngineEvent::ToolExecutionError(d) => {
            let t = guard.entry(jid.to_string()).or_insert_with(|| Trace::new(gen_trace_id()));
            let id = format!("c_{}", t.tool_calls.len() + 1);
            let shape = serde_json::to_value(&d.args_shape).unwrap_or_default();
            t.tool_calls.push(ToolCallRecord {
                fingerprint: super::loop_controller::fingerprint(&d.tool_name, &shape),
                id,
                name: d.tool_name.clone(),
                error_type: Some(crate::failures::error_class(&d.content)),
                artifact: None,
            });
        }
        EngineEvent::SessionError(d) => {
            if let Some(t) = guard.get_mut(jid) {
                t.attribution = Some(serde_json::json!({"error_type": d.error_type}));
            }
        }
        EngineEvent::MessageComplete(d) if d.agent_id == MAIN_AGENT_ID && !d.has_tool_calls => {
            if let Some(mut t) = guard.remove(jid) {
                t.outcome = Some(serde_json::json!({"signal": "completed"}));
                drop(guard);
                write(jid, &t);
            }
        }
        _ => {}
    }
}

/// Push one Jev decision into the currently open trace for `jid`, opening one
/// if none exists yet — a pre-turn decision (skill routing, input guard) can
/// run before `InputReceived` is observed on this side channel.
pub fn record_decision(jid: &str, decision: Decision) {
    let mut guard = OPEN.lock().unwrap();
    let t = guard.entry(jid.to_string()).or_insert_with(|| Trace::new(gen_trace_id()));
    t.decisions.push(decision);
}

fn write(jid: &str, trace: &Trace) {
    let dir = chat_dir(jid);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    restrict_dir(&dir);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("{millis:013}-{}.json", trace.trace_id));
    let Ok(text) = serde_json::to_vec_pretty(trace) else {
        return;
    };
    if std::fs::write(&path, text).is_ok() {
        restrict_file(&path);
    }
    prune(&dir);
}

/// Delete the oldest files beyond the retention cap. File names are
/// timestamp-prefixed, so a lexicographic sort is a chronological one.
fn prune(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    if files.len() <= MAX_TRACES_PER_CHAT {
        return;
    }
    files.sort();
    for stale in &files[..files.len() - MAX_TRACES_PER_CHAT] {
        let _ = std::fs::remove_file(stale);
    }
}

/// List every trace file across all chats, newest first, each entry a
/// summary (id, chat, decision/tool/llm counts) cheap enough to build for a
/// listing endpoint without reading every file's full body twice.
pub fn list_all(limit: usize) -> Vec<serde_json::Value> {
    let base = root();
    let Ok(chats) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    for chat in chats.flatten() {
        let jid_dir = chat.path();
        let Ok(turns) = std::fs::read_dir(&jid_dir) else { continue };
        let chat_name = chat.file_name().to_string_lossy().to_string();
        for t in turns.flatten() {
            let p = t.path();
            if p.extension().is_some_and(|e| e == "json") {
                files.push((p, chat_name.clone()));
            }
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0)); // filename timestamp prefix, newest first
    files
        .into_iter()
        .take(limit)
        .filter_map(|(p, chat)| {
            let raw = std::fs::read(&p).ok()?;
            let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
            let stem = p.file_stem()?.to_string_lossy().to_string();
            Some(serde_json::json!({
                // `chat` is already a `safe_id`-mangled directory name (no
                // colons survive that mangling), so `chat:stem` round-trips
                // unambiguously through `read_by_id`.
                "id": format!("{chat}:{stem}"),
                "chat": chat,
                "traceId": v.get("trace_id"),
                "decisions": v.get("decisions").and_then(|d| d.as_array()).map(|a| a.len()).unwrap_or(0),
                "toolCalls": v.get("tool_calls").and_then(|d| d.as_array()).map(|a| a.len()).unwrap_or(0),
                "llmCalls": v.get("llm_calls").and_then(|d| d.as_array()).map(|a| a.len()).unwrap_or(0),
            }))
        })
        .collect()
}

/// Read one trace file back by the composite id [`list_all`] hands out.
///
/// `id` is the raw axum path param — attacker-controlled, not something
/// `list_all` already produced. It used to be split on `:` and joined
/// straight onto `root()` with no validation, so `id = "..:../config"`
/// resolved to `~/.senclaw/config.json` (provider API keys) for any loopback
/// peer. Both halves are now required to already be in `safe_id`'s mangled
/// form — the exact shape every id `list_all` hands out satisfies by
/// construction — and the resolved path is re-checked against `root()` after
/// canonicalizing, so a future change to the character-set check alone could
/// not reopen this.
pub fn read_by_id(id: &str) -> Option<serde_json::Value> {
    let (chat, stem) = id.split_once(':')?;
    if !super::is_safe_path_component(chat) || !super::is_safe_path_component(stem) {
        return None;
    }
    let base = root();
    let path = base.join(chat).join(format!("{stem}.json"));
    if !super::path_stays_under(&base, &path) {
        return None;
    }
    let raw = std::fs::read(path).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// The G1 opt-in: record exactly what a spec was asked, for later replay
/// against a newer registry. Never called unless the caller already checked
/// `controlPlane.recordDecisionInputs` — see module docs.
pub fn record_decision_input(
    settings: &ControlPlaneSettings,
    jid: &str,
    spec_id: &str,
    version: u32,
    state: &Json,
    questions: &Json,
) {
    if !settings.record_decision_inputs {
        return;
    }
    let dir = decision_inputs_root().join(safe_id(jid));
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    restrict_dir(&dir);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let safe_spec = safe_id(spec_id);
    let path = dir.join(format!("{millis:013}-{safe_spec}.json"));
    let body = serde_json::json!({
        "spec": spec_id,
        "version": version,
        "state": state,
        "questions": questions,
    });
    if std::fs::write(&path, serde_json::to_vec_pretty(&body).unwrap_or_default()).is_ok() {
        restrict_file(&path);
    }
}

/// Drop any state accumulated for a jid without writing it — used by tests so
/// one test's open trace never leaks into the next.
#[cfg(test)]
pub fn reset_for_test(jid: &str) {
    OPEN.lock().unwrap().remove(jid);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zen_core::{
        InputReceivedData, LlmUsageData, MessageCompleteData, RawUsage, ToolExecutionCompleteData,
        ToolExecutionErrorData,
    };

    fn with_tmp_dir<F: FnOnce()>(f: F) {
        // Held for the whole closure, not just the set/remove — see
        // `control_plane::env_test_guard`'s docs. Every test below (directly
        // or via this helper) touches a process-global `SENCLAW_*` path env
        // var, so one shared guard serializes all of them.
        let _guard = super::super::env_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("SENCLAW_TRACES_DIR", tmp.path());
        f();
        std::env::remove_var("SENCLAW_TRACES_DIR");
    }

    #[test]
    fn a_full_turn_writes_one_metadata_only_file() {
        with_tmp_dir(|| {
            let jid = "trace-test-1";
            reset_for_test(jid);
            record(
                jid,
                &EngineEvent::InputReceived(InputReceivedData {
                    input: "hello".into(),
                    queued: false,
                    inject: false,
                    queue_length: 0,
                }),
            );
            record_decision(
                jid,
                Decision {
                    spec: "route.skill".into(),
                    version: 1,
                    by: super::super::ladder::DecidedBy::Jev,
                    answer: "clock-timer".into(),
                    confidence: 0.9,
                    band: "act",
                    latency_ms: 12.3,
                    shadow: false,
                },
            );
            record(
                jid,
                &EngineEvent::LlmUsage(LlmUsageData {
                    source: "agent".into(),
                    agent_id: "main".into(),
                    session_id: "s".into(),
                    profile: "p".into(),
                    provider: "prov".into(),
                    model: "m".into(),
                    usage: RawUsage {
                        input_tokens: Some(1000),
                        output_tokens: Some(50),
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: Some(800),
                        prompt_tokens: None,
                        completion_tokens: None,
                    },
                    latency_ms: 500,
                    ok: true,
                }),
            );
            record(
                jid,
                &EngineEvent::ToolExecutionError(ToolExecutionErrorData {
                    agent_id: "main".into(),
                    tool_name: "Bash".into(),
                    title: "run".into(),
                    description: String::new(),
                    content: "permission denied: /Users/alice/.ssh/id_rsa".into(),
                    args_shape: Default::default(),
                }),
            );
            record(
                jid,
                &EngineEvent::MessageComplete(MessageCompleteData {
                    agent_id: MAIN_AGENT_ID.to_string(),
                    reasoning: String::new(),
                    content: "the secret is 42".into(),
                    has_tool_calls: false,
                    tool_calls: None,
                    output_tokens: 50,
                }),
            );

            let files: Vec<_> = std::fs::read_dir(chat_dir(jid)).unwrap().flatten().collect();
            assert_eq!(files.len(), 1, "exactly one turn was written");
            let raw = std::fs::read_to_string(files[0].path()).unwrap();
            assert!(!raw.contains("the secret is 42"), "no message content in the trace");
            assert!(!raw.contains("/Users/alice/.ssh/id_rsa"), "no raw path from a tool error in the trace");
            assert!(!raw.contains("id_rsa"), "the folded error class must not leak the path either");
            let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(v["format"], "neutral-v1");
            assert_eq!(v["decisions"][0]["spec"], "route.skill");
            assert_eq!(v["llm_calls"][0]["cache_read"], 800);
            assert_eq!(v["tool_calls"][0]["error_type"], "permission denied <path>");
        });
    }

    #[test]
    fn list_all_and_read_by_id_round_trip_a_written_trace() {
        with_tmp_dir(|| {
            let jid = "trace-test-list";
            reset_for_test(jid);
            record(
                jid,
                &EngineEvent::InputReceived(InputReceivedData { input: "hi".into(), queued: false, inject: false, queue_length: 0 }),
            );
            record(
                jid,
                &EngineEvent::MessageComplete(MessageCompleteData {
                    agent_id: MAIN_AGENT_ID.to_string(),
                    reasoning: String::new(),
                    content: "done".into(),
                    has_tool_calls: false,
                    tool_calls: None,
                    output_tokens: 1,
                }),
            );
            let listed = list_all(10);
            let entry = listed.iter().find(|v| v["chat"] == safe_id(jid)).expect("the written trace is listed");
            let id = entry["id"].as_str().unwrap().to_string();
            let fetched = read_by_id(&id).expect("read_by_id resolves the listed id");
            assert_eq!(fetched["format"], "neutral-v1");
            assert!(read_by_id("nonexistent-chat:nonexistent-file").is_none());
        });
    }

    /// `read_by_id` used to split on `:` and join both halves onto
    /// `root()` with no validation at all — a raw axum path param straight
    /// into a filesystem join. This proves the fix actually blocks reading a
    /// file outside the traces root, not just that the string check exists.
    #[test]
    fn read_by_id_rejects_path_traversal_and_cannot_read_outside_the_traces_root() {
        let _guard = super::super::env_test_guard();
        let tmp = tempfile::tempdir().unwrap();
        // `root()` will be `tmp/traces`; a "secret" file sits next to it,
        // exactly the `~/.senclaw/config.json`-vs-`~/.senclaw/control-plane/traces`
        // relationship from the real report.
        let traces_root = tmp.path().join("traces");
        std::fs::create_dir_all(&traces_root).unwrap();
        std::fs::write(tmp.path().join("config.json"), r#"{"apiKey":"sk-super-secret"}"#).unwrap();
        std::env::set_var("SENCLAW_TRACES_DIR", &traces_root);

        for attempt in [
            "..:../config",     // resolved to ~/.senclaw/config.json before the fix
            "a/b:c",            // a separator smuggled into a "chat" component
            "c:a/b",            // a separator smuggled into a "stem" component
            "/etc:passwd",      // an absolute-looking chat component
            "etc:/passwd",      // an absolute-looking stem component
            "..:config",
            "..%2f..:config",   // the raw (undecoded) percent-escape does not help either
        ] {
            assert!(read_by_id(attempt).is_none(), "{attempt:?} must not resolve to any file");
        }

        // Confirm the file really is reachable by *legitimate* means, so a
        // bug that made every lookup fail could not hide as a false pass above.
        let real = std::fs::read_to_string(tmp.path().join("config.json")).unwrap();
        assert!(real.contains("sk-super-secret"));

        std::env::remove_var("SENCLAW_TRACES_DIR");
    }

    #[test]
    fn record_decision_input_is_a_strict_opt_in() {
        with_tmp_dir(|| {
            let tmp = tempfile::tempdir().unwrap();
            std::env::set_var("SENCLAW_DECISION_INPUTS_DIR", tmp.path());
            let off = ControlPlaneSettings::default();
            record_decision_input(&off, "jid-a", "clarify.needed", 1, &Json::String("s".into()), &Json::Null);
            assert!(std::fs::read_dir(tmp.path().join(safe_id("jid-a"))).is_err(), "off means nothing is written");

            let mut on = off.clone();
            on.record_decision_inputs = true;
            record_decision_input(&on, "jid-b", "clarify.needed", 1, &Json::String("the real query".into()), &Json::Null);
            let dir = tmp.path().join(safe_id("jid-b"));
            let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
            assert_eq!(files.len(), 1);
            let raw = std::fs::read_to_string(files[0].path()).unwrap();
            assert!(raw.contains("the real query"), "on purpose: this file, unlike the trace, does hold state");
            std::env::remove_var("SENCLAW_DECISION_INPUTS_DIR");
        });
    }

    #[test]
    fn tool_execution_complete_never_records_the_result_content_verbatim() {
        with_tmp_dir(|| {
            let jid = "trace-test-2";
            reset_for_test(jid);
            record(
                jid,
                &EngineEvent::InputReceived(InputReceivedData {
                    input: "hi".into(),
                    queued: false,
                    inject: false,
                    queue_length: 0,
                }),
            );
            record(
                jid,
                &EngineEvent::ToolExecutionComplete(ToolExecutionCompleteData {
                    agent_id: "main".into(),
                    tool_name: "Read".into(),
                    title: "Read".into(),
                    summary: String::new(),
                    description: String::new(),
                    content: serde_json::json!({"text": "super secret file contents"}),
                }),
            );
            record(
                jid,
                &EngineEvent::MessageComplete(MessageCompleteData {
                    agent_id: MAIN_AGENT_ID.to_string(),
                    reasoning: String::new(),
                    content: "done".into(),
                    has_tool_calls: false,
                    tool_calls: None,
                    output_tokens: 1,
                }),
            );
            let files: Vec<_> = std::fs::read_dir(chat_dir(jid)).unwrap().flatten().collect();
            let raw = std::fs::read_to_string(files[0].path()).unwrap();
            assert!(!raw.contains("super secret"));
            assert!(raw.contains("\"name\": \"Read\""));
        });
    }
}
