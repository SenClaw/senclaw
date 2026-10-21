//! Skill usage ledger — which skills get loaded, and what the turn did next.
//!
//! The history ([`super::history`]) says what a skill *is*; this says how it
//! *behaves*: for each time its instructions were loaded into a chat, whether
//! the turn then finished, errored or was interrupted, and how many tool calls
//! failed meanwhile. Counted per content hash, so a change to a skill and its
//! effect on those numbers can be read side by side.
//!
//! **It only records.** Nothing here is read back into a prompt, and no lesson
//! or proposal comes out of it. It exists for the same reason the failure
//! ledger does: whether skills misbehave often enough, and repeatedly enough,
//! to be worth any automatic change is a question for these numbers, not for a
//! guess. See [docs/skill-history.md].
//!
//! Two inputs feed it:
//! - [`note_load`], called by the engine where a skill is loaded — the `Skill`
//!   tool, or a forced load (`#name`, `/name`, pre-trigger, the router). Skills
//!   that are always on are not loads and are not recorded. The engine's
//!   `instance_id` is the chat's jid.
//! - [`record`], called with every engine event of a chat (next to
//!   [`crate::failures::record`]). It only does anything while the chat has an
//!   open row.
//!
//! What a row never holds: the prompt, tool arguments, model output.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

use tokio::sync::mpsc;

use crate::db::skill_usage::outcome;
use crate::db::Db;
use crate::skills::history::read_snapshot;
use crate::skills::Skill;
use crate::zen_core::{EngineEvent, MAIN_AGENT_ID};

/// Buffered ops. A full buffer drops: this is a measurement, and stalling the
/// engine for it would be worse than a missing row.
const BUFFER_CAP: usize = 512;

/// How a skill's instructions reached the chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Load {
    /// The model called the `Skill` tool.
    Tool,
    /// The engine loaded it: `#name`, `/name`, pre-trigger, or the router.
    Forced,
}

impl Load {
    fn as_str(self) -> &'static str {
        match self {
            Load::Tool => "tool",
            Load::Forced => "forced",
        }
    }
}

#[derive(Debug, PartialEq)]
enum Op {
    Loaded {
        jid: String,
        name: String,
        base_dir: String,
        source: String,
        how: Load,
        at: i64,
    },
    ToolFailed {
        jid: String,
        tool: String,
    },
    Ended {
        jid: String,
        outcome: &'static str,
        loop_stopped: bool,
        at: i64,
    },
}

static SENDER: OnceLock<mpsc::Sender<Op>> = OnceLock::new();

/// Chats with an open row. Every engine event of every chat passes through
/// [`record`]; this keeps the common case to one read lock on a usually-empty
/// set.
static OPEN: RwLock<Option<HashSet<String>>> = RwLock::new(None);

fn is_open(jid: &str) -> bool {
    OPEN.read()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.contains(jid)))
        .unwrap_or(false)
}

/// More open chats than this means rows that never close (isolated runs have no
/// event loop); drop the filter rather than let it grow.
const MAX_OPEN_CHATS: usize = 4096;

fn set_open(jid: &str, open: bool) {
    if let Ok(mut g) = OPEN.write() {
        let set = g.get_or_insert_with(HashSet::new);
        if open {
            if set.len() >= MAX_OPEN_CHATS {
                set.clear();
            }
            set.insert(jid.to_string());
        } else {
            set.remove(jid);
        }
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Start the writer. Call once from `run_daemon` after the database is open;
/// without it [`note_load`] and [`record`] do nothing (unit tests, CLI runs).
pub fn start(db: Arc<Db>) {
    let (tx, rx) = mpsc::channel(BUFFER_CAP);
    if SENDER.set(tx).is_err() {
        return; // already started
    }
    // Rows a stopped daemon left open will never see their turn end.
    if let Err(e) = db.close_stale_skill_usage(now_ms()) {
        tracing::debug!(error = %e, "[skill-usage] could not close stale rows");
    }
    tokio::spawn(worker(db, rx));
}

/// Every load noted in this test process, started ledger or not, so a test of
/// the engine can check that a path notes one without a database.
#[cfg(test)]
pub(crate) static TEST_LOADS: std::sync::Mutex<Vec<(String, String, Load)>> = std::sync::Mutex::new(Vec::new());

/// A skill's instructions were just loaded into chat `jid`.
pub fn note_load(jid: &str, skill: &Skill, how: Load) {
    #[cfg(test)]
    TEST_LOADS
        .lock()
        .unwrap()
        .push((jid.to_string(), skill.metadata.name.clone(), how));
    let Some(tx) = SENDER.get() else {
        return;
    };
    // Open before the send: the next event of this chat may be the one that
    // ends the turn, and it is filtered on this set.
    set_open(jid, true);
    let op = Op::Loaded {
        jid: jid.to_string(),
        name: skill.metadata.name.clone(),
        base_dir: skill.base_dir.clone(),
        source: skill.locate.clone(),
        how,
        at: now_ms(),
    };
    if tx.try_send(op).is_err() {
        tracing::debug!("[skill-usage] buffer full — dropping a load");
    }
}

/// Feed one engine event of chat `jid` into the ledger.
pub fn record(jid: &str, event: &EngineEvent) {
    let Some(tx) = SENDER.get() else {
        return;
    };
    if !is_open(jid) {
        return;
    }
    if let Some(op) = op_for(jid, event) {
        if matches!(op, Op::Ended { .. }) {
            set_open(jid, false);
        }
        if tx.try_send(op).is_err() {
            tracing::debug!("[skill-usage] buffer full — dropping an op");
        }
    }
}

/// Which op, if any, one event of a chat with an open row calls for.
///
/// A new input from the person is deliberately not one: it is fired on a
/// different path than a forced load and can reach this ledger after the load
/// it belongs with, so treating it as "the previous turn is over" would close
/// the very row it started. A turn ends with a final answer, an error or an
/// interruption; those are what close rows.
fn op_for(jid: &str, event: &EngineEvent) -> Option<Op> {
    match event {
        EngineEvent::ToolExecutionError(d) => Some(Op::ToolFailed {
            jid: jid.to_string(),
            tool: d.tool_name.clone(),
        }),
        // Subagents finish on the same bus; only the main agent's last message
        // ends the person's turn.
        EngineEvent::MessageComplete(d) if !d.has_tool_calls && d.agent_id == MAIN_AGENT_ID => {
            Some(Op::Ended {
                jid: jid.to_string(),
                outcome: outcome::COMPLETED,
                loop_stopped: false,
                at: now_ms(),
            })
        }
        EngineEvent::SessionError(d) => Some(Op::Ended {
            jid: jid.to_string(),
            outcome: outcome::ERRORED,
            loop_stopped: d.error_type == "tool_error_loop",
            at: now_ms(),
        }),
        EngineEvent::SessionInterrupted(d) if d.agent_id == MAIN_AGENT_ID => Some(Op::Ended {
            jid: jid.to_string(),
            outcome: outcome::INTERRUPTED,
            loop_stopped: false,
            at: now_ms(),
        }),
        _ => None,
    }
}

async fn worker(db: Arc<Db>, mut rx: mpsc::Receiver<Op>) {
    // Sequential on purpose: a row is opened, counted on, then closed, and
    // out-of-order writes would close a row before it exists.
    while let Some(op) = rx.recv().await {
        if let Err(e) = apply(&db, op) {
            tracing::debug!(error = %e, "[skill-usage] ledger write failed");
        }
    }
}

/// The directory name a skill lives under.
fn dir_key(base_dir: &str, fallback: &str) -> String {
    Path::new(base_dir)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(fallback)
        .to_string()
}

fn apply(db: &Db, op: Op) -> anyhow::Result<()> {
    match op {
        Op::Loaded {
            jid,
            name,
            base_dir,
            source,
            how,
            at,
        } => {
            // The hash is read here, on the writer, not on the engine's path.
            // A directory that cannot be read is recorded without one.
            let hash = read_snapshot(Path::new(&base_dir)).ok().map(|s| s.hash);
            let key = dir_key(&base_dir, &name);
            db.insert_skill_usage(
                at,
                &jid,
                &name,
                &key,
                &source,
                hash.as_deref(),
                how.as_str(),
            )?;
        }
        Op::ToolFailed { jid, tool } => {
            db.skill_usage_tool_failed(&jid, &tool)?;
        }
        Op::Ended {
            jid,
            outcome,
            loop_stopped,
            at,
        } => {
            db.close_skill_usage(&jid, outcome, loop_stopped, at)?;
        }
    }
    Ok(())
}

// ─── Summary ─────────────────────────────────────────────────────────────────

use crate::db::skill_usage::UsageGroup;
use serde_json::{json, Value};

#[derive(Default)]
struct Counts {
    loads: i64,
    completed: i64,
    errored: i64,
    interrupted: i64,
    unfinished: i64,
    with_tool_failures: i64,
    loop_stopped: i64,
}

impl Counts {
    fn add(&mut self, g: &UsageGroup) {
        self.loads += g.loads;
        self.completed += g.completed;
        self.errored += g.errored;
        self.interrupted += g.interrupted;
        self.unfinished += g.unfinished;
        self.with_tool_failures += g.with_tool_failures;
        self.loop_stopped += g.loop_stopped;
    }

    fn ratio(num: i64, den: i64) -> Value {
        if den > 0 {
            json!(((num as f64 / den as f64) * 1000.0).round() / 1000.0)
        } else {
            Value::Null
        }
    }

    /// `fallbackRate`: of the loads whose turn ended, how many ended in an
    /// error or an interruption. `toolFailureRate`: how many saw a tool fail.
    /// Both are null until a load has ended.
    fn to_json(&self) -> Value {
        let ended = self.completed + self.errored + self.interrupted;
        json!({
            "loads": self.loads,
            "completed": self.completed,
            "errored": self.errored,
            "interrupted": self.interrupted,
            "unfinished": self.unfinished,
            "withToolFailures": self.with_tool_failures,
            "loopStopped": self.loop_stopped,
            "fallbackRate": Self::ratio(self.errored + self.interrupted, ended),
            "toolFailureRate": Self::ratio(self.with_tool_failures, self.loads - self.unfinished),
        })
    }
}

fn merge(base: Value, extra: Value) -> Value {
    let (Value::Object(mut a), Value::Object(b)) = (base, extra) else {
        return Value::Null;
    };
    a.extend(b);
    Value::Object(a)
}

/// Per skill and per content version, for loads since `since_ms`. A version is
/// named by the newest revision that holds its content, when there is one.
pub fn summary(db: &Db, since_ms: i64) -> anyhow::Result<Value> {
    let groups = db.skill_usage_groups(since_ms)?;
    let mut totals = Counts::default();
    let mut skills: Vec<(i64, Value)> = Vec::new();
    let mut i = 0;
    while i < groups.len() {
        // `skill_usage_groups` is ordered by source and key, so one skill's
        // versions are adjacent.
        let (source, key) = (&groups[i].source, &groups[i].skill_key);
        let mut j = i;
        while j < groups.len() && groups[j].source == *source && groups[j].skill_key == *key {
            j += 1;
        }
        let mut counts = Counts::default();
        let mut versions = Vec::new();
        for g in &groups[i..j] {
            let mut c = Counts::default();
            c.add(g);
            counts.add(g);
            let revision = match g.content_hash.as_deref() {
                Some(h) => db.revision_for_hash(&g.source, &g.skill_key, h)?,
                None => None,
            };
            versions.push(merge(
                c.to_json(),
                json!({
                    "contentHash": g.content_hash,
                    "revision": revision.map(|(id, origin, generation)| json!({"id": id, "origin": origin, "generation": generation})),
                    "byTool": g.by_tool,
                    "forced": g.forced,
                    "firstAt": g.first_at,
                    "lastAt": g.last_at,
                }),
            ));
        }
        totals.add_counts(&counts);
        skills.push((
            counts.loads,
            merge(
                counts.to_json(),
                json!({
                    "source": source,
                    "key": key,
                    "name": groups[i].skill_name,
                    "versions": versions,
                }),
            ),
        ));
        i = j;
    }
    skills.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(json!({
        "since": since_ms,
        "totals": totals.to_json(),
        "skills": skills.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
    }))
}

impl Counts {
    fn add_counts(&mut self, o: &Counts) {
        self.loads += o.loads;
        self.completed += o.completed;
        self.errored += o.errored;
        self.interrupted += o.interrupted;
        self.unfinished += o.unfinished;
        self.with_tool_failures += o.with_tool_failures;
        self.loop_stopped += o.loop_stopped;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zen_core::{
        InputReceivedData, MessageCompleteData, SessionErrorData, SessionErrorDetail,
        SessionInterruptedData, ToolExecutionErrorData,
    };

    fn complete(agent: &str, has_tool_calls: bool) -> EngineEvent {
        EngineEvent::MessageComplete(MessageCompleteData {
            agent_id: agent.into(),
            reasoning: String::new(),
            content: "done".into(),
            has_tool_calls,
            tool_calls: None,
            output_tokens: 0,
        })
    }

    fn tool_error(tool: &str) -> EngineEvent {
        EngineEvent::ToolExecutionError(ToolExecutionErrorData {
            agent_id: "main".into(),
            tool_name: tool.into(),
            title: tool.into(),
            description: String::new(),
            content: "boom".into(),
            args_shape: Default::default(),
        })
    }

    fn session_error(kind: &str) -> EngineEvent {
        EngineEvent::SessionError(SessionErrorData {
            error_type: kind.into(),
            error: SessionErrorDetail {
                code: kind.into(),
                message: "x".into(),
                details: None,
            },
        })
    }

    fn ended(op: Option<Op>) -> Option<(&'static str, bool)> {
        match op {
            Some(Op::Ended {
                outcome,
                loop_stopped,
                ..
            }) => Some((outcome, loop_stopped)),
            _ => None,
        }
    }

    #[test]
    fn only_the_main_agents_final_answer_completes_a_turn() {
        assert_eq!(
            ended(op_for("c", &complete("main", false))),
            Some((outcome::COMPLETED, false))
        );
        assert!(
            op_for("c", &complete("main", true)).is_none(),
            "a message that calls tools is not the end"
        );
        assert!(
            op_for("c", &complete("sub-1", false)).is_none(),
            "a subagent finishing is not the turn ending"
        );
    }

    #[test]
    fn errors_and_interruptions_close_a_turn_and_a_loop_stop_is_marked() {
        assert_eq!(
            ended(op_for("c", &session_error("tool_error_loop"))),
            Some((outcome::ERRORED, true))
        );
        assert_eq!(
            ended(op_for("c", &session_error("api_error"))),
            Some((outcome::ERRORED, false))
        );
        let interrupted = |agent: &str| {
            EngineEvent::SessionInterrupted(SessionInterruptedData {
                agent_id: agent.into(),
                content: String::new(),
            })
        };
        assert_eq!(
            ended(op_for("c", &interrupted("main"))),
            Some((outcome::INTERRUPTED, false))
        );
        assert!(
            op_for("c", &interrupted("sub-1")).is_none(),
            "a subagent stopping does not end the turn"
        );
    }

    #[test]
    fn a_tool_failure_is_counted_and_a_new_input_changes_nothing() {
        assert_eq!(
            op_for("c", &tool_error("Bash")),
            Some(Op::ToolFailed {
                jid: "c".into(),
                tool: "Bash".into()
            })
        );
        let input = EngineEvent::InputReceived(InputReceivedData {
            input: "hi".into(),
            queued: false,
            inject: false,
            queue_length: 0,
        });
        assert!(op_for("c", &input).is_none());
    }

    #[test]
    fn nothing_is_recorded_before_the_ledger_starts_or_for_a_chat_with_no_open_row() {
        // No sender in a unit-test process: both entry points are inert.
        record("never-opened", &complete("main", false));
        assert!(!is_open("never-opened"));
    }

    fn db() -> Db {
        Db::open_in_memory(&crate::config::Config::from_env()).unwrap()
    }

    #[test]
    fn the_writer_opens_counts_and_closes_a_row_with_the_skills_content_hash() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: d\n---\n\nHi\n",
        )
        .unwrap();
        let expected = read_snapshot(&skill_dir).unwrap().hash;
        let db = db();

        apply(
            &db,
            Op::Loaded {
                jid: "web:a".into(),
                name: "My Skill".into(),
                base_dir: skill_dir.to_string_lossy().to_string(),
                source: "clawhub-managed".into(),
                how: Load::Forced,
                at: 5,
            },
        )
        .unwrap();
        apply(
            &db,
            Op::ToolFailed {
                jid: "web:a".into(),
                tool: "Bash".into(),
            },
        )
        .unwrap();
        apply(
            &db,
            Op::Ended {
                jid: "web:a".into(),
                outcome: outcome::COMPLETED,
                loop_stopped: false,
                at: 9,
            },
        )
        .unwrap();

        let r = &db.list_skill_usage(None, 5).unwrap()[0];
        assert_eq!(
            (r.skill_key.as_str(), r.skill_name.as_str(), r.how.as_str()),
            ("my-skill", "My Skill", "forced")
        );
        assert_eq!(r.content_hash.as_deref(), Some(expected.as_str()));
        assert_eq!(
            (r.outcome.as_str(), r.tool_failures, r.ended_at),
            (outcome::COMPLETED, 1, Some(9))
        );
    }

    #[test]
    fn a_skill_directory_that_cannot_be_read_is_still_counted_without_a_hash() {
        let db = db();
        apply(
            &db,
            Op::Loaded {
                jid: "web:a".into(),
                name: "ghost".into(),
                base_dir: "/definitely/not/here/ghost".into(),
                source: "bundled".into(),
                how: Load::Tool,
                at: 1,
            },
        )
        .unwrap();
        let r = &db.list_skill_usage(None, 1).unwrap()[0];
        assert_eq!(
            (r.skill_key.as_str(), r.content_hash.as_deref()),
            ("ghost", None)
        );
    }

    #[test]
    fn the_summary_rolls_versions_up_to_skills_and_names_each_by_its_revision() {
        use crate::db::skill_history::{origin, NewSkillRevision};
        let db = db();
        let rev = NewSkillRevision {
            id: "r1",
            skill_key: "wiki",
            skill_name: "wiki",
            source: "clawhub-managed",
            origin: origin::MANUAL,
            actor: "t",
            content_hash: "h1",
            file_count: 1,
            bytes: 1,
            truncated: false,
            has_snapshot: true,
            restored_from: None,
            change_summary: "",
            diff: "",
            shadows: None,
            scan: None,
        };
        db.insert_skill_revision(&rev).unwrap();
        // h1: three loads, one errored; h2: one load that is still open.
        for (i, out) in [outcome::COMPLETED, outcome::COMPLETED, outcome::ERRORED]
            .iter()
            .enumerate()
        {
            db.insert_skill_usage(
                10 + i as i64,
                "web:a",
                "wiki",
                "wiki",
                "clawhub-managed",
                Some("h1"),
                "tool",
            )
            .unwrap();
            db.close_skill_usage("web:a", out, false, 20).unwrap();
        }
        db.insert_skill_usage(
            40,
            "web:a",
            "wiki",
            "wiki",
            "clawhub-managed",
            Some("h2"),
            "forced",
        )
        .unwrap();
        db.insert_skill_usage(41, "web:a", "code", "code", "bundled", None, "forced")
            .unwrap();

        let out = summary(&db, 0).unwrap();
        assert_eq!(out["totals"]["loads"], 5);
        let skills = out["skills"].as_array().unwrap();
        assert_eq!(skills[0]["key"], "wiki", "busiest first");
        assert_eq!(
            (skills[0]["loads"].as_i64(), skills[0]["errored"].as_i64()),
            (Some(4), Some(1))
        );
        assert_eq!(skills[0]["fallbackRate"], 0.333);
        let v = skills[0]["versions"].as_array().unwrap();
        let h1 = v.iter().find(|x| x["contentHash"] == "h1").unwrap();
        assert_eq!(h1["revision"]["id"], "r1");
        assert_eq!(h1["fallbackRate"], 0.333);
        let h2 = v.iter().find(|x| x["contentHash"] == "h2").unwrap();
        assert!(
            h2["revision"].is_null(),
            "a version with no revision row is still counted"
        );
        assert!(h2["fallbackRate"].is_null(), "no load of it has ended yet");
        assert_eq!(skills[1]["key"], "code");
    }
}
