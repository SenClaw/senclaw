//! Failure ledger — what failed, how often, and what happened next.
//!
//! The engine already detects tool failure well: [`crate::zen_core::conversation`]
//! counts consecutive all-error turns, nudges the model, and hard-stops with a
//! reported error instead of a fabricated success. What it never did is keep
//! any of it. The same tool fails the same way tomorrow and nothing in the
//! system is any the wiser.
//!
//! This module closes the *measurement* half of that gap and nothing else. It
//! subscribes to the same per-chat event stream as [`crate::trajectory`] and
//! writes one row per failure *episode* — a run of consecutive failures of one
//! tool — together with the outcome:
//!
//! * the tool eventually succeeded, and a user message had arrived first
//!   (`fix_source = user`: only a remembered lesson could have skipped this);
//! * it succeeded with no user message in between (`fix_source = model`: the
//!   existing nudge already handles it);
//! * it never succeeded (`gave_up`).
//!
//! **It only records.** Nothing here is read back into a prompt, and no lesson
//! is distilled from it. That is deliberate: a store of lessons is only worth
//! building if failures actually repeat, and this table is how we find out —
//! see `/api/failures/summary` and [docs/failure-ledger.md].
//!
//! Note what is *absent* from the ledger by design: argument values (only the
//! shape, name -> JSON type, is kept — values are user data), and permission
//! denials, which never emit `ToolExecutionError` because a user saying no is
//! not a tool failure.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock, RwLock};

use tokio::sync::mpsc;

use crate::db::Db;
use crate::util::text::truncate_on_char_boundary;
use crate::zen_core::EngineEvent;

/// Error text kept per episode. Enough for the message and its first detail
/// line; the full text stays in the chat's own tool-execution row.
const MAX_ERROR_BYTES: usize = 600;

/// Buffered ops. Failures are rare; a full buffer means something pathological
/// is happening and dropping is better than stalling the event bus.
const BUFFER_CAP: usize = 512;

/// One ledger mutation, in the order it happened on the chat's event bus.
#[derive(Debug)]
enum Op {
    Failed {
        jid: String,
        agent_id: String,
        tool: String,
        args_shape: String,
        error: String,
        class: String,
        at: String,
    },
    Succeeded {
        jid: String,
        tool: String,
        at: String,
    },
    UserInput {
        jid: String,
    },
    TurnEnded {
        jid: String,
        at: String,
    },
    LoopStopped {
        jid: String,
        sig: String,
    },
}

static SENDER: OnceLock<mpsc::Sender<Op>> = OnceLock::new();

/// `jid\u{1}tool` keys with an episode still open. Kept in memory so the hot
/// path — every *successful* tool call — costs a read lock on a usually-empty
/// set instead of an UPDATE that would match no rows.
static OPEN: RwLock<Option<HashSet<String>>> = RwLock::new(None);

fn open_key(jid: &str, tool: &str) -> String {
    format!("{jid}\u{1}{tool}")
}

fn any_open() -> bool {
    OPEN.read()
        .ok()
        .and_then(|g| g.as_ref().map(|s| !s.is_empty()))
        .unwrap_or(false)
}

fn is_open(jid: &str, tool: &str) -> bool {
    let key = open_key(jid, tool);
    OPEN.read()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.contains(&key)))
        .unwrap_or(false)
}

fn set_open(key: String, open: bool) {
    if let Ok(mut g) = OPEN.write() {
        let set = g.get_or_insert_with(HashSet::new);
        if open {
            set.insert(key);
        } else {
            set.remove(&key);
        }
    }
}

fn clear_open_for_chat(jid: &str) {
    let prefix = format!("{jid}\u{1}");
    if let Ok(mut g) = OPEN.write() {
        if let Some(set) = g.as_mut() {
            set.retain(|k| !k.starts_with(&prefix));
        }
    }
}

/// Start the ledger writer. Call once from `run_daemon` after the DB is open;
/// without it [`record`] is a no-op (unit tests, CLI one-shots).
pub fn start(db: Arc<Db>) {
    let (tx, rx) = mpsc::channel(BUFFER_CAP);
    if SENDER.set(tx).is_err() {
        return; // already started
    }
    // Seed the open set from the DB: a daemon that restarted mid-episode still
    // has rows sitting 'open', and without these keys their turn-end and
    // success events would be filtered out and the rows would never close.
    if let Ok(rows) = db.list_failure_episodes(None, Some("open"), 500) {
        for r in rows {
            set_open(open_key(&r.chat_jid, &r.tool_name), true);
        }
    }
    tokio::spawn(worker(db, rx));
}

/// Feed one engine event for `jid` into the ledger. A no-op for every event
/// that is not a tool failure or part of its outcome, and for every event at
/// all until [`start`] has run.
pub fn record(jid: &str, event: &EngineEvent) {
    let Some(tx) = SENDER.get() else {
        return;
    };
    let Some(op) = op_for(jid, event) else {
        return;
    };
    if let Op::Failed { tool, .. } = &op {
        // Marked open here, not in the worker: the very next event may be this
        // tool succeeding, and that event is filtered on the open set. Setting
        // it a queue-length later would drop the success and record a failure
        // that was actually fixed as given up on.
        set_open(open_key(jid, tool), true);
    }
    if tx.try_send(op).is_err() {
        tracing::debug!("[failures] buffer full — dropping ledger op");
    }
}

/// Which ledger op, if any, one engine event calls for. Pure, so the filters
/// that are easy to get wrong — whose turn ended, which loop stop names a tool
/// — are testable without a database.
fn op_for(jid: &str, event: &EngineEvent) -> Option<Op> {
    let op = match event {
        EngineEvent::ToolExecutionError(d) => {
            Op::Failed {
                jid: jid.to_string(),
                agent_id: d.agent_id.clone(),
                tool: d.tool_name.clone(),
                args_shape: serde_json::to_string(&d.args_shape)
                    .unwrap_or_else(|_| "{}".to_string()),
                error: truncate_on_char_boundary(d.content.trim(), MAX_ERROR_BYTES).to_string(),
                class: error_class(&d.content),
                at: now(),
            }
        }
        // Everything below only matters while some episode is open, and these
        // events (a successful tool call above all) are the common case.
        EngineEvent::ToolExecutionComplete(d) if is_open(jid, &d.tool_name) => Op::Succeeded {
            jid: jid.to_string(),
            tool: d.tool_name.clone(),
            at: now(),
        },
        EngineEvent::InputReceived(_) if any_open() => Op::UserInput {
            jid: jid.to_string(),
        },
        // A final answer ends the turn. An episode that lives through two of
        // these without the tool ever working has been given up on. Subagents
        // finish on the same bus, and counting their last message would retire
        // the parent chat's episodes a turn or two early.
        EngineEvent::MessageComplete(d)
            if !d.has_tool_calls && d.agent_id == crate::zen_core::MAIN_AGENT_ID && any_open() =>
        {
            Op::TurnEnded {
                jid: jid.to_string(),
                at: now(),
            }
        }
        EngineEvent::SessionError(d) if d.error_type == "tool_error_loop" => {
            let sig = d
                .error
                .details
                .as_ref()
                .and_then(|v| v.get("toolSig"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if sig.is_empty() {
                return None;
            }
            Op::LoopStopped {
                jid: jid.to_string(),
                sig,
            }
        }
        _ => return None,
    };
    Some(op)
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

async fn worker(db: Arc<Db>, mut rx: mpsc::Receiver<Op>) {
    // Applied inline rather than spawned per op: the ledger is a state machine
    // (fail -> user speaks -> succeed) and out-of-order writes would misattribute
    // the fix. Each op is one or two short statements on a rare event.
    while let Some(op) = rx.recv().await {
        if let Err(e) = apply(&db, op) {
            tracing::debug!(error = %e, "[failures] ledger write failed");
        }
    }
}

fn apply(db: &Db, op: Op) -> anyhow::Result<()> {
    match op {
        Op::Failed {
            jid,
            agent_id,
            tool,
            args_shape,
            error,
            class,
            at,
        } => {
            db.record_tool_failure(&jid, &agent_id, &tool, &args_shape, &error, &class, &at)?;
            set_open(open_key(&jid, &tool), true);
        }
        Op::Succeeded { jid, tool, at } => {
            db.resolve_tool_failure(&jid, &tool, &at)?;
            set_open(open_key(&jid, &tool), false);
        }
        Op::UserInput { jid } => {
            db.mark_failures_user_input(&jid)?;
        }
        Op::TurnEnded { jid, at } => {
            if db.end_turn_for_failures(&jid, &at)? > 0 {
                // Some episodes in this chat were given up on. Which ones the
                // SQL decided is not worth a second query — drop the chat's
                // keys and let the next failure re-open what it needs.
                clear_open_for_chat(&jid);
                if let Ok(rows) = db.list_failure_episodes(Some(&jid), Some("open"), 100) {
                    for r in rows {
                        set_open(open_key(&r.chat_jid, &r.tool_name), true);
                    }
                }
            }
        }
        Op::LoopStopped { jid, sig } => {
            // `tool_names_sig` joins the turn's tool names with commas.
            for tool in sig.split(',').filter(|t| !t.is_empty()) {
                db.mark_failure_loop_stopped(&jid, tool)?;
            }
        }
    }
    Ok(())
}

/// A stable name for "the same failure again", so counting repeats is a
/// `GROUP BY` and not a judgement call.
///
/// Keeps the first non-empty line and folds away the parts that vary between
/// two instances of one failure: paths and URLs, bare numbers, and surrounding
/// punctuation. It deliberately does *not* fold identifiers — "No such tool:
/// browser_click" and "No such tool: wiki_write" are two different problems.
pub fn error_class(text: &str) -> String {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let norm: Vec<String> = first
        .split_whitespace()
        .take(24)
        .map(normalize_token)
        .filter(|t| !t.is_empty())
        .collect();
    truncate_on_char_boundary(norm.join(" ").trim(), 120).to_string()
}

fn normalize_token(t: &str) -> String {
    let trimmed = t.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | '.' | ':' | ';' | '!'
        )
    });
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.starts_with("http") {
        return "<path>".to_string();
    }
    let has_digit = trimmed.chars().any(|c| c.is_ascii_digit());
    if has_digit
        && trimmed
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '_' | ':' | '%' | 'x'))
    {
        return "#".to_string();
    }
    trimmed.to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_failure_with_different_values_shares_a_class() {
        let a = error_class("Edit failed: old_string not found in \"/Users/a/src/lib.rs\"");
        let b = error_class("Edit failed: old_string not found in \"/Users/b/other/main.rs\"");
        assert_eq!(a, b);
        assert!(a.contains("<path>"), "{a}");
    }

    #[test]
    fn numbers_and_ports_fold_but_identifiers_do_not() {
        assert_eq!(
            error_class("Connection refused on port 18789"),
            error_class("Connection refused on port 4500")
        );
        assert_ne!(
            error_class("No such tool available: browser_click"),
            error_class("No such tool available: wiki_write")
        );
    }

    #[test]
    fn only_the_first_line_and_a_bounded_length_are_kept() {
        let cls = error_class("Missing host, port, or user\n  at ssh_start_connect (x.rs:12)\n");
        assert_eq!(cls, "missing host port or user");
        let long = "word ".repeat(200);
        assert!(error_class(&long).len() <= 120);
    }

    #[test]
    fn vietnamese_error_text_is_not_split_mid_character() {
        let cls = error_class("Không tìm thấy tệp cấu hình");
        assert!(cls.starts_with("không tìm thấy"), "{cls}");
    }

    fn err_event(tool: &str, content: &str) -> EngineEvent {
        let mut shape = std::collections::BTreeMap::new();
        shape.insert("host".to_string(), "string:empty".to_string());
        EngineEvent::ToolExecutionError(crate::zen_core::ToolExecutionErrorData {
            agent_id: "main".into(),
            tool_name: tool.into(),
            title: tool.into(),
            description: String::new(),
            content: content.into(),
            args_shape: shape,
        })
    }

    fn final_message(agent_id: &str) -> EngineEvent {
        EngineEvent::MessageComplete(crate::zen_core::MessageCompleteData {
            agent_id: agent_id.into(),
            reasoning: String::new(),
            content: "done".into(),
            has_tool_calls: false,
            tool_calls: None,
            output_tokens: 0,
        })
    }

    #[test]
    fn a_tool_error_becomes_a_failed_op_carrying_shape_and_class() {
        let op = op_for("web:a", &err_event("ssh_start_connect", "Missing host, port, or user"))
            .expect("failed op");
        match op {
            Op::Failed {
                tool,
                args_shape,
                class,
                ..
            } => {
                assert_eq!(tool, "ssh_start_connect");
                assert_eq!(args_shape, r#"{"host":"string:empty"}"#);
                assert_eq!(class, "missing host port or user");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn only_the_main_agent_ending_its_turn_counts() {
        // A subagent finishing is not the chat's turn ending; counting it
        // would retire the chat's episodes one or two turns early.
        set_open(open_key("web:b", "some_tool"), true);
        assert!(op_for("web:b", &final_message("task-7")).is_none());
        assert!(matches!(
            op_for("web:b", &final_message("main")),
            Some(Op::TurnEnded { .. })
        ));
        set_open(open_key("web:b", "some_tool"), false);
    }

    #[test]
    fn a_success_for_a_tool_with_nothing_open_is_ignored() {
        let ev = EngineEvent::ToolExecutionComplete(crate::zen_core::ToolExecutionCompleteData {
            agent_id: "main".into(),
            tool_name: "never_failed_here".into(),
            title: String::new(),
            summary: String::new(),
            description: String::new(),
            content: serde_json::json!({}),
        });
        assert!(op_for("web:c", &ev).is_none());
    }

    #[test]
    fn a_loop_stop_is_read_from_the_structured_details_only() {
        let bare = EngineEvent::SessionError(crate::zen_core::SessionErrorData {
            error_type: "tool_error_loop".into(),
            error: crate::zen_core::SessionErrorDetail {
                code: "TOOL_ERROR_LOOP".into(),
                message: "Tool 'ssh_start_connect' failed on 8 consecutive turns".into(),
                details: None,
            },
        });
        // The sentence is for a person; parsing it back would break the first
        // time it is reworded.
        assert!(op_for("web:d", &bare).is_none());

        let structured = EngineEvent::SessionError(crate::zen_core::SessionErrorData {
            error_type: "tool_error_loop".into(),
            error: crate::zen_core::SessionErrorDetail {
                code: "TOOL_ERROR_LOOP".into(),
                message: String::new(),
                details: Some(serde_json::json!({"toolSig": "a_tool,b_tool", "streak": 8})),
            },
        });
        match op_for("web:d", &structured).expect("loop stop") {
            Op::LoopStopped { sig, .. } => assert_eq!(sig, "a_tool,b_tool"),
            other => panic!("expected LoopStopped, got {other:?}"),
        }
    }

    #[test]
    fn recording_without_start_is_a_noop() {
        // No SENDER set in unit tests: `record` must not panic or block.
        record(
            "web:main",
            &EngineEvent::SessionError(crate::zen_core::SessionErrorData {
                error_type: "tool_error_loop".into(),
                error: crate::zen_core::SessionErrorDetail {
                    code: "TOOL_ERROR_LOOP".into(),
                    message: "x".into(),
                    details: None,
                },
            }),
        );
    }
}
