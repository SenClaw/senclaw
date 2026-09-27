//! Session-window auto-reflection (P14 v2).
//!
//! Instead of firing one cognify call per user message, turns are buffered
//! per agent folder into a conversation *window* and flushed as ONE
//! extraction call when either:
//!   * the buffered text reaches `reflect_max_chars` (size flush), or
//!   * the chat goes quiet for `reflect_window_idle_ms` (idle flush), or
//!   * `reflect_window_idle_ms == 0` → flush on every push (legacy
//!     per-message behavior).
//!
//! Why a window instead of per-message calls:
//!   * Facts that span turns ("SemaClaw deadline khi nào?" → "tháng 8")
//!     arrive in one prompt, so the extractor has enough context to emit
//!     the triplet — the old per-message path lost them (the question was
//!     skipped as a pure question, the answer as too short).
//!   * Speaker prefixes ("User: …" / "Assistant: …") plus the transcript
//!     guidance in the cognify SYSTEM_PROMPT let the LLM resolve pronouns
//!     across turns.
//!   * One system prompt amortises over several turns — fewer total tokens
//!     than the same content extracted message-by-message.
//!
//! `reflect_cooldown_ms` is enforced here as the minimum gap between two
//! flushes of the same window: a flush that comes due too early is
//! deferred (the task sleeps out the remainder), never dropped.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::pool::should_reflect;

#[derive(Default)]
struct Window {
    /// Transcript lines ("speaker: text").
    parts: Vec<String>,
    chars: usize,
    /// Bumped on every push. An idle timer captures the generation it was
    /// armed with and only flushes if it still matches — a later push
    /// supersedes older timers without cancellation plumbing.
    generation: u64,
    last_flush: Option<Instant>,
}

fn windows() -> &'static Mutex<HashMap<String, Window>> {
    static WINDOWS: OnceLock<Mutex<HashMap<String, Window>>> = OnceLock::new();
    WINDOWS.get_or_init(Default::default)
}

/// Buffer one conversation turn for the group's reflection window and
/// schedule the appropriate flush. Cheap and non-blocking — cognify runs
/// on a spawned task, never on the caller's path. Callers gate on the
/// `memory.cognitive_reflection` toggle; this function gates on the
/// cognitive master switch + size knobs.
pub(crate) fn reflect_push(group_folder: &str, speaker: &str, text: &str) {
    // Outside a tokio runtime (unit tests, shutdown) there is nowhere to
    // run the flush — drop silently, reflection is best-effort by design.
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let cfg = crate::config::Config::from_env();
    if !cfg.cognitive.enabled {
        return;
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    // One turn is capped at the window budget: a huge paste gets truncated
    // instead of dropped (the old path dropped it entirely — the user can
    // still CogAdd the full text).
    let max_chars = cfg.cognitive.reflect_max_chars.max(100);
    let turn: String = trimmed.chars().take(max_chars).collect();
    // Channel envelopes already carry per-message senders; sanitize turns
    // them into speaker lines. Only bare text needs our prefix.
    let line = if turn.contains("<message") {
        turn
    } else {
        format!("{speaker}: {turn}")
    };

    let (size_flush, generation) = {
        let mut map = windows().lock().unwrap();
        let w = map.entry(group_folder.to_string()).or_default();
        w.chars += line.chars().count() + 1;
        w.parts.push(line);
        w.generation += 1;
        (w.chars >= max_chars, w.generation)
    };

    let folder = group_folder.to_string();
    let idle = Duration::from_millis(cfg.cognitive.reflect_window_idle_ms);
    let cooldown = Duration::from_millis(cfg.cognitive.reflect_cooldown_ms);
    let forced = size_flush || idle.is_zero();
    handle.spawn(async move {
        if !forced {
            tokio::time::sleep(idle).await;
        }
        flush_window(&folder, generation, forced, cooldown).await;
    });
}

/// Flush the window if this task is still the one responsible for it.
/// `forced` (size trigger / zero idle) flushes regardless of newer pushes;
/// an idle timer whose generation was superseded simply retires — the
/// newer push armed a newer timer.
async fn flush_window(folder: &str, generation: u64, forced: bool, cooldown: Duration) {
    // Enforce the cooldown by sleeping out the remainder, re-checking
    // afterwards (another task may have flushed meanwhile).
    loop {
        let wait = {
            let map = windows().lock().unwrap();
            let Some(w) = map.get(folder) else { return };
            if !forced && w.generation != generation {
                return;
            }
            match w.last_flush {
                Some(t) if t.elapsed() < cooldown => Some(cooldown - t.elapsed()),
                _ => None,
            }
        };
        match wait {
            Some(d) => tokio::time::sleep(d).await,
            None => break,
        }
    }

    let text = {
        let mut map = windows().lock().unwrap();
        let Some(w) = map.get_mut(folder) else { return };
        if !forced && w.generation != generation {
            return;
        }
        if w.parts.is_empty() {
            return;
        }
        w.last_flush = Some(Instant::now());
        w.chars = 0;
        w.parts.drain(..).collect::<Vec<_>>().join("\n")
    };
    reflect_window_text(text, folder.to_string()).await;
}

/// Cognify one flushed conversation window. Mirrors the old per-message
/// `cognitive_reflect`, with the size gates applied to the whole window:
/// enough substance overall, and not a lone unanswered question.
async fn reflect_window_text(text: String, group_folder: String) {
    let cfg = crate::config::Config::from_env();
    if !cfg.cognitive.enabled {
        return;
    }
    if !should_reflect(&text, cfg.cognitive.reflect_min_chars, usize::MAX) {
        return;
    }
    let Some(sys) = crate::memory::cognitive::try_get_instance() else {
        return;
    };
    if !sys.is_enabled() {
        return;
    }
    let opts = crate::memory::cognitive::CognifyOptions {
        node_sets: vec![crate::memory::cognitive::NodeSet::group(
            &group_folder,
            "default_memory",
        )],
        ..Default::default()
    };
    match sys.cognify(&text, "reflection", &opts).await {
        Ok(r) => {
            if r.entities_added > 0 || r.edges_added > 0 {
                tracing::info!(
                    chunks_added = r.chunks_added,
                    entities_added = r.entities_added,
                    edges_added = r.edges_added,
                    "[reflection] auto-cognified conversation window"
                );
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "[reflection] window cognify failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(folder: &str) -> Option<(String, u64)> {
        let mut map = windows().lock().unwrap();
        map.get_mut(folder).map(|w| {
            let text = w.parts.drain(..).collect::<Vec<_>>().join("\n");
            w.chars = 0;
            (text, w.generation)
        })
    }

    /// Pushes buffer as speaker-prefixed transcript lines; envelope text
    /// keeps its own senders. Uses the window map directly — reflect_push
    /// requires a runtime for the flush timer, so this exercises the same
    /// buffering code path via a runtime.
    #[tokio::test]
    async fn push_buffers_turns_as_transcript() {
        let folder = "test-buffering-folder";
        reflect_push(folder, "User", "SemaClaw deadline khi nào vậy nhỉ?");
        reflect_push(folder, "Assistant", "Theo kế hoạch là tháng 8.");
        reflect_push(
            folder,
            "User",
            r#"<message sender="an">đồng ý nhé</message>"#,
        );

        let (text, generation) = drain(folder).expect("window exists");
        assert_eq!(generation, 3);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "User: SemaClaw deadline khi nào vậy nhỉ?");
        assert_eq!(lines[1], "Assistant: Theo kế hoạch là tháng 8.");
        // Envelope line kept raw for sanitize to speakerize downstream.
        assert!(lines[2].contains("<message sender=\"an\""));
    }

    #[tokio::test]
    async fn empty_and_whitespace_turns_are_ignored() {
        let folder = "test-empty-folder";
        reflect_push(folder, "User", "   ");
        reflect_push(folder, "User", "");
        assert!(
            drain(folder).map(|(t, _)| t.is_empty()).unwrap_or(true),
            "nothing should be buffered"
        );
    }
}
