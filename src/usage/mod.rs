//! Token accounting for every LLM call the daemon makes or brokers.
//!
//! One [`UsageEvent`] per LLM call, funnelled through [`UsageRecorder`]
//! (non-blocking `try_send` into an MPSC buffer, batch-flushed to SQLite).
//! Design: docs/token-usage-tracking-design.md.
//!
//! Ground rules:
//! * Metadata only — never prompt/response content, never api keys.
//! * The recorder must never slow the agent loop: a full buffer drops the
//!   event (with a debug log) instead of blocking.
//! * A call is recorded where it *happens*: the zen_core agent path records
//!   `agent`/`subagent`/`compact`/`hook`; the bridge records `bridge` for
//!   `llm.request` it executes itself; `agent.run` does NOT add rows (the
//!   underlying agent calls are already recorded) — it only reports totals
//!   back to the app.
//! * `estimated = true` marks chars/4-style numbers so they are never
//!   mistaken for provider-reported counts.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::db::Db;
use crate::zen_core::RawUsage;

pub mod aggregate;

/// Where a recorded LLM call originated. Serialized as the `source` column
/// (snake_case) of `llm_usage_log`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    /// Main-agent turn in a group conversation.
    Agent,
    /// Task-tool subagent turn (`agent_id` = task id).
    Subagent,
    /// History-compaction summary call (the real cost, not the fabricated
    /// gauge value).
    Compact,
    /// Prompt-type hook execution.
    Hook,
    /// Space-App `llm.request` brokered by the daemon bridge.
    Bridge,
    /// Cognify triplet-extraction call (cognitive memory stack).
    Cognitive,
    /// Embedding request (output_tokens is always 0).
    Embedding,
    /// Usage a Space App reported for a direct provider call it made itself
    /// (`usage.report` bridge action).
    AppDirect,
}

impl UsageSource {
    /// Map the string carried by `zen_core::LlmUsageData` back to the enum.
    /// Unknown strings fall back to `Agent` (better misfiled than dropped).
    pub fn from_zen(source: &str) -> UsageSource {
        match source {
            "subagent" => UsageSource::Subagent,
            "compact" => UsageSource::Compact,
            "hook" => UsageSource::Hook,
            _ => UsageSource::Agent,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            UsageSource::Agent => "agent",
            UsageSource::Subagent => "subagent",
            UsageSource::Compact => "compact",
            UsageSource::Hook => "hook",
            UsageSource::Bridge => "bridge",
            UsageSource::Cognitive => "cognitive",
            UsageSource::Embedding => "embedding",
            UsageSource::AppDirect => "app_direct",
        }
    }
}

/// One LLM call's accounting record. Field semantics follow the provider
/// split: `input_tokens` is the non-cached prompt count for Anthropic and the
/// full prompt count for OpenAI-style providers (which fold cache into it);
/// the `cache_*` fields are only non-zero for providers that report them.
/// Total billed input = `input_tokens + cache_creation_tokens +
/// cache_read_tokens` — matching `RawUsage::input()`.
#[derive(Debug, Clone, Serialize)]
pub struct UsageEvent {
    /// Unix millis.
    pub ts: i64,
    pub source: UsageSource,
    pub jid: String,
    pub agent_id: String,
    pub session_id: String,
    pub app_id: String,
    pub profile: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub latency_ms: u64,
    pub ok: bool,
    pub estimated: bool,
}

impl UsageEvent {
    /// Empty event for `source` stamped with the current wall clock. Fill in
    /// dimensions with struct-update syntax and token counts with
    /// [`UsageEvent::with_tokens`].
    pub fn new(source: UsageSource) -> Self {
        UsageEvent {
            ts: chrono::Utc::now().timestamp_millis(),
            source,
            jid: String::new(),
            agent_id: String::new(),
            session_id: String::new(),
            app_id: String::new(),
            profile: String::new(),
            provider: String::new(),
            model: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            latency_ms: 0,
            ok: true,
            estimated: false,
        }
    }

    /// Copy token counts out of a provider-reported [`RawUsage`].
    pub fn with_tokens(mut self, u: &RawUsage) -> Self {
        self.input_tokens = u.prompt_tokens.or(u.input_tokens).unwrap_or(0);
        self.output_tokens = u.output();
        self.cache_creation_tokens = u.cache_creation_input_tokens.unwrap_or(0);
        self.cache_read_tokens = u.cache_read_input_tokens.unwrap_or(0);
        self
    }

    /// Total billed input tokens (prompt + cache creation + cache read).
    pub fn total_input(&self) -> u64 {
        self.input_tokens + self.cache_creation_tokens + self.cache_read_tokens
    }
}

/// Cheap cloneable handle. Call [`UsageRecorder::record`] from anywhere; the
/// background task owns the DB writes.
#[derive(Clone)]
pub struct UsageRecorder {
    tx: mpsc::Sender<UsageEvent>,
}

/// Process-global handle for call sites too deep to thread the recorder into
/// (virtual worker pool, isolated runner, cognitive stack, embeddings). Set
/// once at daemon boot; `None` in unit tests and CLI one-shots, where
/// recording silently no-ops. Explicitly-wired handles (AgentPool, UiState)
/// stay preferred where the plumbing is short.
static GLOBAL: std::sync::OnceLock<Arc<UsageRecorder>> = std::sync::OnceLock::new();

pub fn set_global(rec: Arc<UsageRecorder>) {
    let _ = GLOBAL.set(rec);
}

pub fn global() -> Option<Arc<UsageRecorder>> {
    GLOBAL.get().cloned()
}

const BUFFER_CAP: usize = 10_000;
const FLUSH_EVERY: Duration = Duration::from_secs(5);
const FLUSH_AT: usize = 100;

impl UsageRecorder {
    /// Spawn the flush task and return the shared handle. Call once from
    /// `run_daemon` after the DB is open.
    pub fn start(db: Arc<Db>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(BUFFER_CAP);
        tokio::spawn(flush_loop(db, rx));
        Arc::new(UsageRecorder { tx })
    }

    /// Record one LLM call. Never blocks: a full buffer drops the event.
    pub fn record(&self, ev: UsageEvent) {
        if ev.total_input() == 0 && ev.output_tokens == 0 {
            return; // nothing measured, nothing to store
        }
        if self.tx.try_send(ev).is_err() {
            tracing::debug!("[usage] buffer full — dropping usage event");
        }
    }

    /// Test-only recorder wired to a buffer nobody drains.
    #[cfg(test)]
    pub fn dangling() -> Arc<Self> {
        let (tx, _rx) = mpsc::channel(4);
        Arc::new(UsageRecorder { tx })
    }
}

async fn flush_loop(db: Arc<Db>, mut rx: mpsc::Receiver<UsageEvent>) {
    let mut buf: Vec<UsageEvent> = Vec::with_capacity(FLUSH_AT);
    let mut tick = tokio::time::interval(FLUSH_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            got = rx.recv() => match got {
                Some(ev) => {
                    buf.push(ev);
                    if buf.len() >= FLUSH_AT {
                        flush(&db, &mut buf);
                    }
                }
                None => {
                    flush(&db, &mut buf);
                    break;
                }
            },
            _ = tick.tick() => flush(&db, &mut buf),
        }
    }
}

fn flush(db: &Db, buf: &mut Vec<UsageEvent>) {
    if buf.is_empty() {
        return;
    }
    if let Err(e) = db.insert_usage_events(buf) {
        tracing::warn!(error = %e, dropped = buf.len(), "[usage] flush failed");
    }
    buf.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(
        input: Option<u64>,
        output: Option<u64>,
        cc: Option<u64>,
        cr: Option<u64>,
        prompt: Option<u64>,
        completion: Option<u64>,
    ) -> RawUsage {
        RawUsage {
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: cc,
            cache_read_input_tokens: cr,
            prompt_tokens: prompt,
            completion_tokens: completion,
        }
    }

    #[test]
    fn with_tokens_maps_anthropic_fields() {
        let ev = UsageEvent::new(UsageSource::Agent).with_tokens(&raw(
            Some(120),
            Some(30),
            Some(1000),
            Some(9000),
            None,
            None,
        ));
        assert_eq!(ev.input_tokens, 120);
        assert_eq!(ev.output_tokens, 30);
        assert_eq!(ev.cache_creation_tokens, 1000);
        assert_eq!(ev.cache_read_tokens, 9000);
        assert_eq!(ev.total_input(), 10_120);
    }

    #[test]
    fn with_tokens_maps_openai_fields() {
        let ev = UsageEvent::new(UsageSource::Bridge).with_tokens(&raw(
            None,
            None,
            None,
            None,
            Some(500),
            Some(80),
        ));
        assert_eq!(ev.input_tokens, 500);
        assert_eq!(ev.output_tokens, 80);
        assert_eq!(ev.cache_creation_tokens, 0);
        assert_eq!(ev.cache_read_tokens, 0);
        assert_eq!(ev.total_input(), 500);
    }

    #[test]
    fn source_round_trips_as_snake_case() {
        for (s, want) in [
            (UsageSource::Agent, "agent"),
            (UsageSource::AppDirect, "app_direct"),
            (UsageSource::Cognitive, "cognitive"),
        ] {
            assert_eq!(s.as_str(), want);
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(json, format!("\"{want}\""));
        }
    }
}
