//! Watch mode: wait for a long-running job, then resume the chat with its result.
//!
//! The gap this fills. An agent that hands work to something slow — an AI Office
//! task, a DAG, a Space App job, a build — had two options, and both lose the
//! thread:
//!
//! - `sleep` + one poll inside the turn, then give up and tell the user to ask
//!   again later. The turn ends; nothing ever comes back on its own.
//! - `background_*`, which runs autonomously but **cannot reply to a chat** by
//!   design (see [`crate::background`]: "no reply to anybody"). Its only reach
//!   is an OS notification.
//!
//! A watch is the missing third thing: a row in `scheduled_tasks` that re-checks
//! a condition on an interval and, the moment it holds, dispatches a prompt into
//! the *originating chat* — the same delivery path [`ContextMode::Group`] uses.
//! So the conversation continues by itself instead of waiting on the user to
//! poke it.
//!
//! **The check is a tool call, not an agent turn.** Each tick re-invokes one MCP
//! tool and evaluates [`DoneWhen`] against its result — no LLM, so a watch that
//! polls for an hour costs nothing but the tool calls. An agent turn is spent
//! only when the condition finally holds. A watch with no `tool` degrades to
//! waking the agent every interval, which is what jobs that can't be checked in
//! a single call need.
//!
//! Bounded on purpose: `deadline_at` and `max_checks` both terminate the watch,
//! and both terminate it *by telling the chat* — a watch that quietly stops is
//! indistinguishable to the user from the giving-up behaviour it replaces.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::util::text::truncate_on_char_boundary;

/// How much of a probe result travels into the resume prompt. A job's payload
/// can be a whole report; the agent gets the head of it plus the tool to fetch
/// the rest itself.
pub const MAX_RESULT_CHARS: usize = 4000;

/// Consecutive probe errors tolerated before the watch gives up. A stopped
/// Space App, a restarting daemon and a transient network blip all look like an
/// error here, and all resolve on their own — so a single failure must not end
/// a watch that has been running for an hour.
pub const MAX_ERROR_STREAK: i64 = 5;

/// Consecutive *tool-level* rejections tolerated. Far lower than
/// [`MAX_ERROR_STREAK`] because these are deterministic: a tool answering
/// `isError` because the arguments are wrong will answer that way forever.
/// Tolerating one blip is worth it; tolerating five hides a broken watch for
/// five minutes, and ignoring them entirely hides it until the deadline.
pub const MAX_TOOL_ERROR_STREAK: i64 = 2;

/// The comparison applied to the probe result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoneOp {
    /// The path resolves to anything non-null.
    Exists,
    Equals,
    NotEquals,
    /// Substring, case-insensitive — the common shape for a status blob.
    Contains,
    NotContains,
    /// Value is one of `values`.
    In,
    NotIn,
}

impl DoneOp {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "equals" | "eq" | "==" => Self::Equals,
            "not_equals" | "ne" | "!=" => Self::NotEquals,
            "contains" => Self::Contains,
            "not_contains" => Self::NotContains,
            "in" => Self::In,
            "not_in" => Self::NotIn,
            _ => Self::Exists,
        }
    }
}

/// The done-condition. Deliberately declarative: it is evaluated in Rust, so a
/// watch never needs a model to decide whether it is finished.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoneWhen {
    /// Dot path into the probe result (`data.status`, `items.0.state`). Empty
    /// applies the op to the whole result rendered as text.
    #[serde(default)]
    pub path: String,
    #[serde(default = "default_op")]
    pub op: DoneOp,
    /// Single comparison value. Ignored by `exists`.
    #[serde(default)]
    pub value: Option<String>,
    /// Comparison set for `in` / `not_in`.
    #[serde(default)]
    pub values: Vec<String>,
}

fn default_op() -> DoneOp {
    DoneOp::Exists
}

impl Default for DoneWhen {
    fn default() -> Self {
        Self {
            path: String::new(),
            op: DoneOp::Exists,
            value: None,
            values: Vec::new(),
        }
    }
}

/// Everything a watch needs, serialised into `scheduled_tasks.watch_json`.
///
/// One JSON column rather than eight sparse ones: every field here is
/// meaningless outside watch mode, and the column is written and read by this
/// module alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchConfig {
    /// Full `mcp__<server>__<tool>` name to re-invoke each tick. Absent or empty
    /// selects the agent-turn fallback: no probe, just wake the chat.
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub args: serde_json::Value,
    #[serde(default)]
    pub done_when: DoneWhen,
    /// RFC3339. The watch stops here whatever the condition says.
    pub deadline_at: String,
    /// Hard ceiling on ticks, independent of the deadline.
    #[serde(default = "default_max_checks")]
    pub max_checks: i64,
    #[serde(default)]
    pub checks: i64,
    #[serde(default)]
    pub error_streak: i64,
    #[serde(default)]
    pub last_error: Option<String>,
    /// What the chat is told when the condition holds. `{{result}}` is replaced
    /// with the probe payload; unknown placeholders survive verbatim, the same
    /// rule [`crate::scaffold`] and [`crate::patterns`] follow.
    pub resume_prompt: String,
    /// What the chat is told when the deadline or check ceiling is hit instead.
    /// Absent falls back to a generated line — a watch must never end silently.
    #[serde(default)]
    pub timeout_prompt: Option<String>,
    /// Human label for logs and the run record.
    #[serde(default)]
    pub label: Option<String>,
}

fn default_max_checks() -> i64 {
    120
}

/// Why a tick ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchOutcome {
    /// Condition held. Dispatch `prompt` into the chat and retire the watch.
    Done { prompt: String },
    /// Not yet. Persist the updated config and let the interval re-arm.
    Pending { checks: i64 },
    /// Deadline, check ceiling, or too many consecutive probe errors. Dispatch
    /// `prompt` into the chat and retire the watch.
    GaveUp { prompt: String, reason: String },
}

impl WatchConfig {
    pub fn parse(raw: &str) -> Result<Self> {
        serde_json::from_str(raw).context("invalid watch config")
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("serialise watch config")
    }

    /// True when no probe is declared — the watch wakes the agent each interval
    /// and lets it decide, which is the fallback for jobs that cannot be checked
    /// with a single tool call.
    pub fn is_agent_fallback(&self) -> bool {
        self.tool.as_deref().map(str::trim).unwrap_or("").is_empty()
    }

    /// Has the watch run out of road? Checked *before* probing, so an expired
    /// watch costs no tool call.
    pub fn exhausted(&self, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
        if self.checks >= self.max_checks {
            return Some(format!("reached the {} check ceiling", self.max_checks));
        }
        match chrono::DateTime::parse_from_rfc3339(&self.deadline_at) {
            Ok(deadline) => {
                if now >= deadline.with_timezone(&chrono::Utc) {
                    Some(format!("passed its deadline ({})", self.deadline_at))
                } else {
                    None
                }
            }
            // An unparseable deadline is a broken watch. Ending it is right:
            // the alternative is polling forever on a row nobody can fix.
            Err(_) => Some(format!("has an unreadable deadline ({})", self.deadline_at)),
        }
    }

    /// Fold a probe error into the config. `Some(reason)` once the streak is
    /// past tolerance and the watch should stop.
    pub fn record_error(&mut self, err: &str) -> Option<String> {
        self.record_error_kind(err, false)
    }

    /// Fold an error into the config. `deterministic` marks a tool-level
    /// rejection — wrong arguments, unknown id — which will not fix itself, so
    /// it burns the much shorter [`MAX_TOOL_ERROR_STREAK`].
    pub fn record_error_kind(&mut self, err: &str, deterministic: bool) -> Option<String> {
        self.checks += 1;
        self.error_streak += 1;
        self.last_error = Some(truncate_on_char_boundary(err, 500).to_string());
        let ceiling = if deterministic {
            MAX_TOOL_ERROR_STREAK
        } else {
            MAX_ERROR_STREAK
        };
        if self.error_streak >= ceiling {
            Some(format!(
                "failed {} checks in a row (last error: {err})",
                self.error_streak
            ))
        } else {
            None
        }
    }

    /// Evaluate one successful probe.
    pub fn record_probe(&mut self, result: &serde_json::Value) -> WatchOutcome {
        self.checks += 1;
        self.error_streak = 0;
        self.last_error = None;

        if self.done_when.evaluate(result) {
            WatchOutcome::Done {
                prompt: self.render_resume(result),
            }
        } else {
            WatchOutcome::Pending {
                checks: self.checks,
            }
        }
    }

    /// The prompt dispatched into the chat when the condition holds.
    pub fn render_resume(&self, result: &serde_json::Value) -> String {
        let rendered = render_value(result);
        let payload = truncate_on_char_boundary(&rendered, MAX_RESULT_CHARS);
        self.resume_prompt.replace("{{result}}", payload)
    }

    /// The prompt dispatched when the watch gives up. Never empty — the user
    /// must hear about a watch that ended without an answer.
    pub fn render_timeout(&self, reason: &str) -> String {
        let label = self
            .label
            .as_deref()
            .unwrap_or("the job you were waiting on");
        match &self.timeout_prompt {
            Some(p) if !p.trim().is_empty() => p.replace("{{reason}}", reason),
            _ => format!(
                "The watch on {label} stopped without a result: it {reason}. \
                 Tell the user plainly that it has not finished, say what was \
                 last seen{}, and offer to keep waiting or to check another way. \
                 Do not invent a result.",
                match &self.last_error {
                    Some(e) => format!(" (last error: {e})"),
                    None => String::new(),
                }
            ),
        }
    }
}

impl DoneWhen {
    /// Apply the condition to a probe result.
    pub fn evaluate(&self, result: &serde_json::Value) -> bool {
        let resolved = resolve_path(result, &self.path);

        // A path that resolves to nothing means "the job has not reported that
        // field yet", never "the job is finished". Without this the negated
        // operators invert an absent value into a match and a mistyped path
        // finishes the watch on its very first probe — which is exactly how a
        // watch on `status` (the real field being `task.status`) woke the chat
        // every 60 s, each wake spending a full agent turn to say "still
        // running" and arm another identically-wrong watch.
        //
        // `Exists` is excluded because absence *is* its answer.
        if self.op != DoneOp::Exists && resolved.map(|v| v.is_null()).unwrap_or(true) {
            return false;
        }

        match self.op {
            DoneOp::Exists => resolved.map(|v| !v.is_null()).unwrap_or(false),
            DoneOp::Equals => self.text_of(resolved).as_deref() == self.value.as_deref(),
            DoneOp::NotEquals => self.text_of(resolved).as_deref() != self.value.as_deref(),
            DoneOp::Contains => self.haystack(resolved).contains(&self.needle()),
            DoneOp::NotContains => !self.haystack(resolved).contains(&self.needle()),
            DoneOp::In => self.in_set(resolved),
            DoneOp::NotIn => !self.in_set(resolved),
        }
    }

    fn text_of(&self, v: Option<&serde_json::Value>) -> Option<String> {
        v.map(render_value)
    }

    fn haystack(&self, v: Option<&serde_json::Value>) -> String {
        v.map(render_value).unwrap_or_default().to_lowercase()
    }

    fn needle(&self) -> String {
        self.value.clone().unwrap_or_default().to_lowercase()
    }

    fn in_set(&self, v: Option<&serde_json::Value>) -> bool {
        let Some(actual) = v.map(render_value) else {
            return false;
        };
        let actual = actual.to_lowercase();
        self.values.iter().any(|c| c.to_lowercase() == actual)
    }
}

/// Render a JSON value the way a comparison should see it: a string is its own
/// text (not a quoted literal), everything else is compact JSON. Without this a
/// `"done"` status compares against `"\"done\""` and never matches.
pub fn render_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Walk a dot path, indexing arrays by number. An empty path is the whole value.
pub fn resolve_path<'a>(root: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let path = path.trim();
    if path.is_empty() {
        return Some(root);
    }
    let mut cur = root;
    for seg in path.split('.').filter(|s| !s.is_empty()) {
        cur = match cur {
            serde_json::Value::Object(map) => map.get(seg)?,
            serde_json::Value::Array(items) => items.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// The tool's own error text when it answered `isError: true`.
///
/// A JSON-RPC call that *succeeds* can still carry a tool-level failure, and
/// this one is invisible without the flag: the envelope unwraps to an error
/// message like "thiếu 'id' nhiệm vụ", no declared path resolves on it, and the
/// watch reads that as "the job has not finished yet". It then re-asks a
/// question the tool has already refused — 240 times, silently, until the
/// deadline. Treating it as an error is what turns that into a report.
pub fn tool_error_text(raw: &serde_json::Value) -> Option<String> {
    if raw.get("isError").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let text = raw
        .get("content")
        .and_then(|c| c.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    Some(if text.is_empty() {
        "tool reported an error".to_string()
    } else {
        text
    })
}

/// Unwrap an MCP tool result into the value a condition should be tested
/// against.
///
/// An MCP call answers `{"content":[{"type":"text","text":"..."}]}`, and that
/// text is itself usually JSON. Testing `done_when` against the envelope makes
/// every sane path — `status`, `data.state` — resolve to nothing, so the watch
/// polls until its deadline against a job that finished on the first tick. So:
/// concatenate the text parts, and hand back the parsed JSON when it parses.
pub fn normalize_tool_result(raw: &serde_json::Value) -> serde_json::Value {
    let Some(items) = raw.get("content").and_then(|c| c.as_array()) else {
        return raw.clone();
    };
    let text = items
        .iter()
        .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return raw.clone();
    }
    serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(done_when: DoneWhen) -> WatchConfig {
        WatchConfig {
            tool: Some("mcp__ai-office-mcp__office_get_task".into()),
            args: json!({"id": 27}),
            done_when,
            deadline_at: "2999-01-01T00:00:00Z".into(),
            max_checks: 10,
            checks: 0,
            error_streak: 0,
            last_error: None,
            resume_prompt: "Xong rồi. Kết quả: {{result}}".into(),
            timeout_prompt: None,
            label: Some("AI Office #27".into()),
        }
    }

    #[test]
    fn mcp_envelope_is_unwrapped_to_the_json_inside_it() {
        // The whole point: a condition on `status` must see the inner payload,
        // not the {"content":[...]} envelope wrapping it.
        let raw = json!({"content":[{"type":"text","text":"{\"status\":\"done\",\"n\":3}"}]});
        let norm = normalize_tool_result(&raw);
        assert_eq!(norm, json!({"status":"done","n":3}));
        assert_eq!(
            resolve_path(&norm, "status").map(render_value).as_deref(),
            Some("done")
        );
    }

    #[test]
    fn envelope_holding_plain_text_becomes_a_string_not_a_parse_error() {
        let raw = json!({"content":[{"type":"text","text":"still running"}]});
        assert_eq!(
            normalize_tool_result(&raw),
            json!("still running"),
            "non-JSON text is a legitimate answer, not a failure"
        );
    }

    #[test]
    fn a_result_without_an_envelope_passes_through() {
        let raw = json!({"status":"queued"});
        assert_eq!(normalize_tool_result(&raw), raw);
    }

    #[test]
    fn string_values_compare_as_text_not_as_quoted_json() {
        // render_value's reason for existing: `"done"` must not compare as
        // `"\"done\""`, which would never match anything a user writes.
        let done = DoneWhen {
            path: "status".into(),
            op: DoneOp::Equals,
            value: Some("done".into()),
            values: vec![],
        };
        assert!(done.evaluate(&json!({"status":"done"})));
        assert!(!done.evaluate(&json!({"status":"running"})));
    }

    #[test]
    fn in_set_matches_case_insensitively_across_terminal_states() {
        let done = DoneWhen {
            path: "state".into(),
            op: DoneOp::In,
            value: None,
            values: vec!["done".into(), "failed".into(), "cancelled".into()],
        };
        assert!(done.evaluate(&json!({"state":"DONE"})));
        assert!(done.evaluate(&json!({"state":"failed"})));
        assert!(!done.evaluate(&json!({"state":"in_progress"})));
    }

    #[test]
    fn a_mistyped_path_never_finishes_a_negated_watch() {
        // The field failure: `office_get_task` answers {"task":{"status":…}},
        // so a watch on `status` resolves to nothing. Before the guard,
        // `not_in` inverted that absence into "done" on the first probe — the
        // chat was woken every 60 s, each wake costing an agent turn that said
        // "still running" and armed another watch with the same wrong path.
        let done = DoneWhen {
            path: "status".into(),
            op: DoneOp::NotIn,
            value: None,
            values: vec!["pending".into(), "running".into(), "review".into()],
        };
        let payload = json!({"task": {"status": "running"}});
        assert!(
            !done.evaluate(&payload),
            "a path that resolves to nothing means 'not reported yet', not 'finished'"
        );

        // ...and the correct path still behaves.
        let ok = DoneWhen {
            path: "task.status".into(),
            ..done.clone()
        };
        assert!(!ok.evaluate(&payload));
        assert!(ok.evaluate(&json!({"task": {"status": "done"}})));
    }

    #[test]
    fn every_negated_op_treats_absence_as_not_ready() {
        for op in [DoneOp::NotEquals, DoneOp::NotContains, DoneOp::NotIn] {
            let d = DoneWhen {
                path: "nope".into(),
                op,
                value: Some("x".into()),
                values: vec!["x".into()],
            };
            assert!(
                !d.evaluate(&json!({"other": 1})),
                "{op:?} matched on absence"
            );
        }
    }

    #[test]
    fn exists_is_false_for_a_missing_path_and_for_an_explicit_null() {
        let done = DoneWhen {
            path: "result".into(),
            op: DoneOp::Exists,
            value: None,
            values: vec![],
        };
        assert!(!done.evaluate(&json!({"status":"running"})));
        assert!(
            !done.evaluate(&json!({"result": null})),
            "a null result is 'not ready', which is the whole reason to keep waiting"
        );
        assert!(done.evaluate(&json!({"result": {"text":"ok"}})));
    }

    #[test]
    fn array_indices_walk_the_path() {
        let v = json!({"items":[{"state":"done"}]});
        assert_eq!(
            resolve_path(&v, "items.0.state")
                .map(render_value)
                .as_deref(),
            Some("done")
        );
        assert!(resolve_path(&v, "items.7.state").is_none());
    }

    #[test]
    fn an_empty_path_tests_the_whole_payload() {
        let done = DoneWhen {
            path: String::new(),
            op: DoneOp::Contains,
            value: Some("COMPLETED".into()),
            values: vec![],
        };
        assert!(
            done.evaluate(&json!("job 27 completed successfully")),
            "contains folds case so a status blob matches however it is cased"
        );
    }

    #[test]
    fn a_holding_condition_retires_the_watch_with_the_result_interpolated() {
        let mut c = cfg(DoneWhen {
            path: "status".into(),
            op: DoneOp::Equals,
            value: Some("done".into()),
            values: vec![],
        });
        let out = c.record_probe(&json!({"status":"done"}));
        match out {
            WatchOutcome::Done { prompt } => {
                assert!(prompt.contains("\"status\":\"done\""), "got: {prompt}")
            }
            other => panic!("expected Done, got {other:?}"),
        }
        assert_eq!(c.checks, 1);
    }

    #[test]
    fn unknown_placeholders_survive_the_resume_render() {
        // Same rule as scaffold/patterns: blanking silently deletes an
        // instruction the author meant to keep.
        let c = cfg(DoneWhen::default());
        let rendered = WatchConfig {
            resume_prompt: "{{result}} then {{unknown}}".into(),
            ..c
        }
        .render_resume(&json!("ok"));
        assert_eq!(rendered, "ok then {{unknown}}");
    }

    #[test]
    fn a_pending_probe_only_advances_the_counter() {
        let mut c = cfg(DoneWhen {
            path: "status".into(),
            op: DoneOp::Equals,
            value: Some("done".into()),
            values: vec![],
        });
        assert_eq!(
            c.record_probe(&json!({"status":"running"})),
            WatchOutcome::Pending { checks: 1 }
        );
        assert_eq!(c.checks, 1);
    }

    #[test]
    fn a_tool_level_rejection_is_an_error_not_a_pending_result() {
        // The field failure, verbatim: the agent armed a watch with no `id`, so
        // `office_get_task` answered isError with "thiếu 'id' nhiệm vụ". The
        // envelope unwraps to that string, `task.status` resolves on nothing,
        // and the watch read it as "not finished yet" — 240 silent checks of a
        // question already refused.
        let raw = json!({
            "content": [{"type": "text", "text": "thiếu 'id' nhiệm vụ"}],
            "isError": true
        });
        assert_eq!(
            tool_error_text(&raw).as_deref(),
            Some("thiếu 'id' nhiệm vụ")
        );

        // A normal answer carries no flag and must not be mistaken for one.
        assert!(tool_error_text(&json!({"content":[{"text":"{\"task\":{}}"}]})).is_none());
        assert!(tool_error_text(&json!({"isError": false, "content": []})).is_none());
    }

    #[test]
    fn a_deterministic_rejection_gives_up_far_sooner_than_a_transport_blip() {
        // Wrong arguments will be wrong on every retry, so tolerating them as
        // long as a network hiccup just hides a broken watch.
        let mut det = cfg(DoneWhen::default());
        assert!(
            det.record_error_kind("bad args", true).is_none(),
            "one blip tolerated"
        );
        assert!(
            det.record_error_kind("bad args", true).is_some(),
            "second ends it"
        );

        let mut transient = cfg(DoneWhen::default());
        for _ in 0..(MAX_ERROR_STREAK - 1) {
            assert!(transient
                .record_error_kind("connection reset", false)
                .is_none());
        }
        assert!(transient
            .record_error_kind("connection reset", false)
            .is_some());
    }

    #[test]
    fn a_single_probe_error_does_not_end_a_long_running_watch() {
        // A stopped session Space App is the resting state, not a fault.
        let mut c = cfg(DoneWhen::default());
        assert!(c.record_error("MCP server not connected").is_none());
        assert_eq!(c.error_streak, 1);
        // ...and a later success clears the streak.
        c.record_probe(&json!({"status":"running"}));
        assert_eq!(c.error_streak, 0);
        assert!(c.last_error.is_none());
    }

    #[test]
    fn a_sustained_error_streak_ends_the_watch() {
        let mut c = cfg(DoneWhen::default());
        let mut reason = None;
        for _ in 0..MAX_ERROR_STREAK {
            reason = c.record_error("boom");
        }
        assert!(
            reason.is_some(),
            "must stop rather than poll a dead tool forever"
        );
    }

    #[test]
    fn the_check_ceiling_and_the_deadline_both_stop_the_watch() {
        let now = chrono::Utc::now();
        let mut c = cfg(DoneWhen::default());
        c.checks = c.max_checks;
        assert!(c.exhausted(now).is_some());

        let expired = WatchConfig {
            deadline_at: "2000-01-01T00:00:00Z".into(),
            ..cfg(DoneWhen::default())
        };
        assert!(expired.exhausted(now).is_some());
    }

    #[test]
    fn an_unreadable_deadline_stops_the_watch_rather_than_polling_forever() {
        let bad = WatchConfig {
            deadline_at: "not a date".into(),
            ..cfg(DoneWhen::default())
        };
        assert!(bad.exhausted(chrono::Utc::now()).is_some());
    }

    #[test]
    fn giving_up_still_produces_a_prompt_so_the_chat_is_told() {
        let c = cfg(DoneWhen::default());
        let msg = c.render_timeout("passed its deadline");
        assert!(msg.contains("AI Office #27"));
        assert!(
            msg.contains("Do not invent"),
            "a timed-out watch must not invite a fabricated result"
        );
    }

    #[test]
    fn a_missing_tool_selects_the_agent_fallback() {
        let mut c = cfg(DoneWhen::default());
        c.tool = None;
        assert!(c.is_agent_fallback());
        c.tool = Some("   ".into());
        assert!(c.is_agent_fallback(), "whitespace is not a tool name");
        c.tool = Some("mcp__x__y".into());
        assert!(!c.is_agent_fallback());
    }

    #[test]
    fn config_round_trips_through_the_db_column() {
        let c = cfg(DoneWhen {
            path: "status".into(),
            op: DoneOp::In,
            value: None,
            values: vec!["done".into()],
        });
        let back = WatchConfig::parse(&c.to_json().unwrap()).unwrap();
        assert_eq!(back.done_when.op, DoneOp::In);
        assert_eq!(back.max_checks, 10);
        assert_eq!(back.args, json!({"id": 27}));
    }

    #[test]
    fn a_sparse_config_fills_its_own_defaults() {
        let c =
            WatchConfig::parse(r#"{"deadline_at":"2999-01-01T00:00:00Z","resume_prompt":"go"}"#)
                .unwrap();
        assert!(c.is_agent_fallback());
        assert_eq!(c.max_checks, 120);
        assert_eq!(c.done_when.op, DoneOp::Exists);
    }
}
