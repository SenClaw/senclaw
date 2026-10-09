//! In-memory registry of live and recent Claude Code runs. Each run owns an
//! ephemeral bearer token the Stop hook and the MCP bridge use to find the run
//! (its working directory, verify command, chat scope). It scopes what those
//! two endpoints do for the run; it is not a sandbox — see the trust boundary
//! in `docs/claude-code-cowork.md`. Nothing here is persisted: a daemon
//! restart ends every run anyway (children are killed on drop).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde::Serialize;

/// How many finished runs stay listed for `/api/claude-code/runs`.
const KEEP_FINISHED: usize = 50;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunEntry {
    pub run_id: String,
    /// Source-scoped work item id (kanban card id for the built-in board).
    pub item_id: String,
    pub session_id: Option<String>,
    pub cwd: String,
    /// `running` | `verifying` | `reviewing` | `done` | `blocked` | `failed`.
    pub phase: String,
    pub verify_command: Option<String>,
    #[serde(skip)]
    pub verify_timeout_secs: u64,
    pub stop_blocks: u32,
    #[serde(skip)]
    pub max_stop_blocks: u32,
    /// Chat whose SenClaw tools this run's MCP bridge sessions resolve to.
    #[serde(skip)]
    pub mcp_chat_jid: Option<String>,
    pub fix_round: u32,
    pub last_activity: String,
    pub cost_usd: f64,
    pub started_at: String,
    pub finished_at: Option<String>,
    #[serde(skip)]
    token: String,
}

#[derive(Default)]
struct Registry {
    runs: HashMap<String, RunEntry>,
    /// Finished run ids, oldest first, for pruning.
    finished: Vec<String>,
}

fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry::default()))
}

/// A fresh random token, `ccr_` + 64 hex.
pub fn new_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    format!("ccr_{}", hex::encode(b))
}

pub struct NewRun {
    pub item_id: String,
    pub cwd: String,
    pub verify_command: Option<String>,
    pub verify_timeout_secs: u64,
    pub max_stop_blocks: u32,
    pub mcp_chat_jid: Option<String>,
}

/// Register a run; returns `(run_id, token)`.
pub fn register(n: NewRun) -> (String, String) {
    let run_id = uuid::Uuid::new_v4().to_string();
    let token = new_token();
    let now = chrono::Utc::now().to_rfc3339();
    let entry = RunEntry {
        run_id: run_id.clone(),
        item_id: n.item_id,
        session_id: None,
        cwd: n.cwd,
        phase: "running".into(),
        verify_command: n.verify_command,
        verify_timeout_secs: n.verify_timeout_secs,
        stop_blocks: 0,
        max_stop_blocks: n.max_stop_blocks,
        mcp_chat_jid: n.mcp_chat_jid,
        fix_round: 0,
        last_activity: String::new(),
        cost_usd: 0.0,
        started_at: now,
        finished_at: None,
        token: token.clone(),
    };
    registry().lock().unwrap().runs.insert(run_id.clone(), entry);
    (run_id, token)
}

/// Mutate a run in place. No-op for an unknown id.
pub fn update(run_id: &str, f: impl FnOnce(&mut RunEntry)) {
    if let Some(e) = registry().lock().unwrap().runs.get_mut(run_id) {
        f(e);
    }
}

/// Mark a run finished. Its token stops authenticating immediately.
pub fn finish(run_id: &str, phase: &str) {
    let mut r = registry().lock().unwrap();
    if let Some(e) = r.runs.get_mut(run_id) {
        e.phase = phase.to_string();
        e.finished_at = Some(chrono::Utc::now().to_rfc3339());
        e.token.clear();
        r.finished.push(run_id.to_string());
    }
    while r.finished.len() > KEEP_FINISHED {
        let old = r.finished.remove(0);
        r.runs.remove(&old);
    }
}

/// The live run a bearer token belongs to (constant-time compare).
pub fn by_token(token: &str) -> Option<RunEntry> {
    if token.is_empty() {
        return None;
    }
    let r = registry().lock().unwrap();
    r.runs
        .values()
        .find(|e| !e.token.is_empty() && crate::gateway::ui_server::auth::ct_eq(&e.token, token))
        .cloned()
}

/// Every listed run, newest first.
pub fn list() -> Vec<RunEntry> {
    let mut v: Vec<RunEntry> = registry().lock().unwrap().runs.values().cloned().collect();
    v.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_run(item: &str) -> NewRun {
        NewRun {
            item_id: item.into(),
            cwd: "/tmp".into(),
            verify_command: None,
            verify_timeout_secs: 10,
            max_stop_blocks: 1,
            mcp_chat_jid: None,
        }
    }

    #[test]
    fn a_token_authenticates_only_until_the_run_finishes() {
        let (id, tok) = register(new_run("reg-1"));
        assert!(tok.starts_with("ccr_") && tok.len() == 68);
        assert_eq!(by_token(&tok).unwrap().run_id, id);
        assert!(by_token("ccr_wrong").is_none());
        update(&id, |e| e.stop_blocks = 2);
        assert_eq!(by_token(&tok).unwrap().stop_blocks, 2);
        finish(&id, "done");
        assert!(by_token(&tok).is_none());
        assert!(list().iter().any(|e| e.run_id == id && e.phase == "done"));
    }

    #[test]
    fn the_empty_token_never_matches() {
        let (id, _) = register(new_run("reg-2"));
        finish(&id, "failed");
        assert!(by_token("").is_none());
    }
}
