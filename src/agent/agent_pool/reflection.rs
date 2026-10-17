//! Session-window auto-reflection: the conversation becomes memory.
//!
//! Turns are buffered per agent folder into a conversation *window* and
//! flushed as ONE fact-extraction call (`memory::engine::facts`) when either:
//!   * the buffered text reaches `reflect_max_chars` (size flush), or
//!   * the chat goes quiet for `reflect_window_idle_ms` (idle flush), or
//!   * `reflect_window_idle_ms == 0` → flush on every push (legacy
//!     per-message behavior).
//!
//! Why a window instead of per-message calls:
//!   * Facts that span turns ("SemaClaw deadline khi nào?" → "tháng 8")
//!     arrive in one prompt, so the extractor has enough context to state
//!     them — extracting message by message lost them (the question was
//!     skipped as a pure question, the answer as too short).
//!   * Speaker prefixes ("User: …" / "Assistant: …") plus the extraction
//!     prompt's speaker rules let the model resolve pronouns across turns.
//!   * One system prompt amortises over several turns — fewer total tokens
//!     than the same content extracted message-by-message.
//!
//! This used to feed the knowledge graph with triplets and, when the memory
//! engine was on, ran a second extraction for facts. There is one extraction
//! now: facts. The graph is still filled by uploads, `cog_add` and SOUL
//! ingest; conversations no longer go into it.
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
    /// The agent folder the facts will belong to.
    folder: String,
    /// The chat they will be tagged with; empty when chats share one window.
    chat: String,
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

/// Which window a turn goes to: one per chat, so a window never mixes what
/// different chats of the same agent said and every fact can say where it came
/// from. With per-chat scoping off the agent's chats share one window.
fn window_key(folder: &str, chat_jid: &str, by_chat: bool) -> String {
    if by_chat {
        format!("{folder}\u{1f}{chat_jid}")
    } else {
        folder.to_string()
    }
}

/// Buffer one conversation turn for the chat's reflection window and
/// schedule the appropriate flush. Cheap and non-blocking — extraction runs
/// on a spawned task, never on the caller's path. Callers gate on the
/// `memory.cognitive_reflection` toggle; the size and idle knobs come from
/// `cognitive.reflect_*`.
pub(crate) fn reflect_push(group_folder: &str, chat_jid: &str, speaker: &str, text: &str) {
    // Outside a tokio runtime (unit tests, shutdown) there is nowhere to
    // run the flush — drop silently, reflection is best-effort by design.
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let cfg = crate::config::Config::from_env();
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

    let by_chat = cfg.memory.engine_facts_by_chat && !chat_jid.is_empty();
    let key = window_key(group_folder, chat_jid, by_chat);
    let (size_flush, generation) = {
        let mut map = windows().lock().unwrap();
        let w = map.entry(key.clone()).or_default();
        if w.folder.is_empty() {
            w.folder = group_folder.to_string();
            w.chat = if by_chat {
                chat_jid.to_string()
            } else {
                String::new()
            };
        }
        w.chars += line.chars().count() + 1;
        w.parts.push(line);
        w.generation += 1;
        (w.chars >= max_chars, w.generation)
    };

    let idle = Duration::from_millis(cfg.cognitive.reflect_window_idle_ms);
    let cooldown = Duration::from_millis(cfg.cognitive.reflect_cooldown_ms);
    let forced = size_flush || idle.is_zero();
    handle.spawn(async move {
        if !forced {
            tokio::time::sleep(idle).await;
        }
        flush_window(&key, generation, forced, cooldown).await;
    });
}

/// Flush the window if this task is still the one responsible for it.
/// `forced` (size trigger / zero idle) flushes regardless of newer pushes;
/// an idle timer whose generation was superseded simply retires — the
/// newer push armed a newer timer.
async fn flush_window(key: &str, generation: u64, forced: bool, cooldown: Duration) {
    // Enforce the cooldown by sleeping out the remainder, re-checking
    // afterwards (another task may have flushed meanwhile).
    loop {
        let wait = {
            let map = windows().lock().unwrap();
            let Some(w) = map.get(key) else { return };
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

    let (text, folder, chat) = {
        let mut map = windows().lock().unwrap();
        let Some(w) = map.get_mut(key) else { return };
        if !forced && w.generation != generation {
            return;
        }
        if w.parts.is_empty() {
            return;
        }
        w.last_flush = Some(Instant::now());
        w.chars = 0;
        (
            w.parts.drain(..).collect::<Vec<_>>().join("\n"),
            w.folder.clone(),
            w.chat.clone(),
        )
    };
    reflect_window_text(text, folder, chat).await;
}

/// Retain one flushed conversation window as facts, if it has enough
/// substance: not a lone unanswered question, and long enough overall. The
/// secret scrub happens inside `retain`, before the text reaches the model.
async fn reflect_window_text(text: String, group_folder: String, chat_jid: String) {
    let cfg = crate::config::Config::from_env();
    if !should_reflect(&text, cfg.cognitive.reflect_min_chars, usize::MAX) {
        return;
    }
    crate::memory::engine::facts::retain_text(&cfg, &group_folder, &chat_jid, &text, "reflection")
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAT: &str = "web:reflection-test";

    fn drain(folder: &str) -> Option<(String, u64)> {
        let mut map = windows().lock().unwrap();
        map.get_mut(&window_key(folder, CHAT, true)).map(|w| {
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
        reflect_push(folder, CHAT, "User", "SemaClaw deadline khi nào vậy nhỉ?");
        reflect_push(folder, CHAT, "Assistant", "Theo kế hoạch là tháng 8.");
        reflect_push(
            folder,
            CHAT,
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

    #[test]
    fn chats_of_one_agent_get_their_own_window_unless_scoping_is_off() {
        assert_ne!(
            window_key("main", "web:a", true),
            window_key("main", "web:b", true),
            "a window must not mix what different chats said"
        );
        assert_ne!(
            window_key("main", "web:a", true),
            window_key("other", "web:a", true)
        );
        assert_eq!(
            window_key("main", "web:a", false),
            window_key("main", "web:b", false),
            "with scoping off the agent's chats share one window"
        );
    }

    #[tokio::test]
    async fn two_chats_of_one_agent_are_buffered_apart_and_remember_where_from() {
        let folder = "test-two-chats-folder";
        reflect_push(
            folder,
            "web:one",
            "User",
            "Lan works at Orion, said in chat one",
        );
        reflect_push(
            folder,
            "web:two",
            "User",
            "Sen lives in Hue, said in chat two",
        );
        let map = windows().lock().unwrap();
        let one = map
            .get(&window_key(folder, "web:one", true))
            .expect("chat one window");
        let two = map
            .get(&window_key(folder, "web:two", true))
            .expect("chat two window");
        assert_eq!(
            (one.folder.as_str(), one.chat.as_str()),
            (folder, "web:one")
        );
        assert_eq!(
            (two.folder.as_str(), two.chat.as_str()),
            (folder, "web:two")
        );
        assert!(one.parts.concat().contains("Orion") && !one.parts.concat().contains("Hue"));
        assert!(two.parts.concat().contains("Hue") && !two.parts.concat().contains("Orion"));
    }

    #[tokio::test]
    async fn empty_and_whitespace_turns_are_ignored() {
        let folder = "test-empty-folder";
        reflect_push(folder, CHAT, "User", "   ");
        reflect_push(folder, CHAT, "User", "");
        assert!(
            drain(folder).map(|(t, _)| t.is_empty()).unwrap_or(true),
            "nothing should be buffered"
        );
    }
}
