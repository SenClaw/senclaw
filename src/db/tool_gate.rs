//! Persistence for the tool-call gate's audit log (`tool_gate_log`).
//!
//! Every command the gate judged, what it concluded, whether that skipped the
//! prompt, and — when a prompt was shown — what the person answered. The last
//! part is the point of shadow mode: before the gate approves anything on its
//! own, the log shows how often a person agreed with it, and above all how
//! often the gate would have approved something the person refused.

use anyhow::Result;
use rusqlite::params;
use serde::Serialize;

/// Rows kept: months of prompts on a busy machine, still bounded.
const MAX_ROWS: i64 = 5_000;

/// The longest command stored; the rest is cut. A command is an audit record,
/// not a replay.
const MAX_COMMAND_CHARS: usize = 2_000;

/// One judgment, as the gate records it.
pub struct GateLogEntry<'a> {
    pub chat_jid: &'a str,
    pub tool: &'a str,
    pub command: &'a str,
    /// `shadow` or `on`.
    pub mode: &'a str,
    /// `allow` or `ask`.
    pub outcome: &'a str,
    /// The prompt was skipped because of this judgment.
    pub applied: bool,
    /// `risky`, `engine` or `error`.
    pub stage: &'a str,
    pub reason: &'a str,
    pub p: Option<f64>,
    pub choice: Option<&'a str>,
    pub questions: &'a str,
    pub model: Option<&'a str>,
    pub engine: Option<&'a str>,
    pub latency_ms: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GateLogRow {
    pub id: i64,
    /// Unix millis.
    pub at: i64,
    pub chat_jid: String,
    pub tool: String,
    pub command: String,
    pub mode: String,
    pub outcome: String,
    pub applied: bool,
    pub stage: String,
    pub reason: String,
    pub p: Option<f64>,
    pub choice: Option<String>,
    pub questions: String,
    pub model: Option<String>,
    pub engine: Option<String>,
    pub latency_ms: Option<f64>,
    pub human: Option<String>,
    pub human_at: Option<i64>,
}

/// Counts over the whole log.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GateStats {
    pub total: i64,
    /// The gate concluded "allow" (whether or not that skipped a prompt).
    pub would_allow: i64,
    /// Prompts actually skipped.
    pub applied: i64,
    /// Kept on the prompt by the risk list, never asked.
    pub risky: i64,
    /// The engine failed or timed out; the prompt showed.
    pub errors: i64,
    /// Prompts a person answered.
    pub answered: i64,
    /// "Allow", and the person approved too.
    pub allow_agreed: i64,
    /// "Allow", and the person refused — the number that matters.
    pub allow_refused: i64,
    /// "Ask", and the person approved: a prompt the gate could have saved.
    pub ask_approved: i64,
    pub ask_refused: i64,
}

fn approved(answer: &str) -> bool {
    matches!(answer, "agree" | "allow")
}

impl super::Db {
    /// Record one judgment. Returns the row id, for the person's answer later.
    pub fn insert_gate_log(&self, e: &GateLogEntry<'_>) -> Result<i64> {
        let command: String = e.command.chars().take(MAX_COMMAND_CHARS).collect();
        let at = chrono::Utc::now().timestamp_millis();
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO tool_gate_log \
                   (at, chat_jid, tool, command, mode, outcome, applied, stage, reason, p, choice, \
                    questions, model, engine, latency_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    at,
                    e.chat_jid,
                    e.tool,
                    command,
                    e.mode,
                    e.outcome,
                    e.applied as i64,
                    e.stage,
                    e.reason,
                    e.p,
                    e.choice,
                    e.questions,
                    e.model,
                    e.engine,
                    e.latency_ms
                ],
            )?;
            let id = c.last_insert_rowid();
            c.execute(
                "DELETE FROM tool_gate_log WHERE id <= (SELECT MAX(id) FROM tool_gate_log) - ?1",
                params![MAX_ROWS],
            )?;
            Ok(id)
        })
    }

    /// What the person answered the prompt a judged command showed. Only the
    /// first answer counts; returns whether a row changed.
    pub fn set_gate_human(&self, id: i64, answer: &str) -> Result<bool> {
        let answer = match answer {
            "agree" | "allow" | "refuse" => answer,
            _ => "other",
        };
        let at = chrono::Utc::now().timestamp_millis();
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE tool_gate_log SET human = ?2, human_at = ?3 WHERE id = ?1 AND human IS NULL",
                params![id, answer, at],
            )?;
            Ok(n > 0)
        })
    }

    /// The newest judgments first.
    pub fn list_gate_log(&self, limit: usize) -> Result<Vec<GateLogRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, at, chat_jid, tool, command, mode, outcome, applied, stage, reason, p, choice, \
                        questions, model, engine, latency_ms, human, human_at \
                 FROM tool_gate_log ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = stmt
                .query_map(params![limit as i64], |r| {
                    Ok(GateLogRow {
                        id: r.get(0)?,
                        at: r.get(1)?,
                        chat_jid: r.get(2)?,
                        tool: r.get(3)?,
                        command: r.get(4)?,
                        mode: r.get(5)?,
                        outcome: r.get(6)?,
                        applied: r.get::<_, i64>(7)? != 0,
                        stage: r.get(8)?,
                        reason: r.get(9)?,
                        p: r.get(10)?,
                        choice: r.get(11)?,
                        questions: r.get(12)?,
                        model: r.get(13)?,
                        engine: r.get(14)?,
                        latency_ms: r.get(15)?,
                        human: r.get(16)?,
                        human_at: r.get(17)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn gate_stats(&self) -> Result<GateStats> {
        self.with_conn(|c| {
            let mut stats = GateStats::default();
            let mut stmt = c.prepare("SELECT outcome, applied, stage, human FROM tool_gate_log")?;
            let mut rows = stmt.query([])?;
            while let Some(r) = rows.next()? {
                let outcome: String = r.get(0)?;
                let applied: i64 = r.get(1)?;
                let stage: String = r.get(2)?;
                let human: Option<String> = r.get(3)?;
                stats.total += 1;
                let allow = outcome == "allow";
                stats.would_allow += allow as i64;
                stats.applied += (applied != 0) as i64;
                stats.risky += (stage == "risky") as i64;
                stats.errors += (stage == "error") as i64;
                if let Some(h) = human.as_deref() {
                    stats.answered += 1;
                    match (allow, approved(h), h == "refuse") {
                        (true, true, _) => stats.allow_agreed += 1,
                        (true, _, true) => stats.allow_refused += 1,
                        (false, true, _) => stats.ask_approved += 1,
                        (false, _, true) => stats.ask_refused += 1,
                        _ => {}
                    }
                }
            }
            Ok(stats)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> super::super::Db {
        let config = crate::config::Config::from_env();
        super::super::Db::open_in_memory(&config).unwrap()
    }

    fn entry<'a>(command: &'a str, outcome: &'a str, applied: bool, stage: &'a str) -> GateLogEntry<'a> {
        GateLogEntry {
            chat_jid: "web:main",
            tool: "Bash",
            command,
            mode: if applied { "on" } else { "shadow" },
            outcome,
            applied,
            stage,
            reason: "",
            p: Some(0.95),
            choice: Some("test"),
            questions: "laya",
            model: Some("multilingual"),
            engine: Some("laya-onnx"),
            latency_ms: Some(40.0),
        }
    }

    #[test]
    fn shadow_agreement_and_refused_allows_are_counted() {
        let db = db();
        let a = db.insert_gate_log(&entry("bun test", "allow", false, "engine")).unwrap();
        let b = db.insert_gate_log(&entry("bun add x", "allow", false, "engine")).unwrap();
        let c = db.insert_gate_log(&entry("rm -rf x", "ask", false, "risky")).unwrap();
        db.insert_gate_log(&entry("npm run lint", "allow", true, "engine")).unwrap();
        assert!(db.set_gate_human(a, "agree").unwrap());
        assert!(db.set_gate_human(b, "refuse").unwrap());
        assert!(db.set_gate_human(c, "allow").unwrap());
        assert!(!db.set_gate_human(c, "refuse").unwrap(), "only the first answer counts");

        let s = db.gate_stats().unwrap();
        assert_eq!((s.total, s.would_allow, s.applied, s.risky), (4, 3, 1, 1));
        assert_eq!((s.answered, s.allow_agreed, s.allow_refused, s.ask_approved), (3, 1, 1, 1));
        let rows = db.list_gate_log(10).unwrap();
        assert_eq!(rows[0].command, "npm run lint", "newest first");
        assert_eq!(rows.iter().find(|r| r.id == c).unwrap().human.as_deref(), Some("allow"));
    }

    #[test]
    fn long_commands_are_cut_and_odd_answers_are_other() {
        let db = db();
        let long = "x".repeat(5_000);
        let id = db.insert_gate_log(&entry(&long, "ask", false, "engine")).unwrap();
        db.set_gate_human(id, "feedback: try again").unwrap();
        let row = &db.list_gate_log(1).unwrap()[0];
        assert_eq!(row.command.chars().count(), MAX_COMMAND_CHARS);
        assert_eq!(row.human.as_deref(), Some("other"));
    }
}
