//! Context compression for tool results — fewer tokens in, same answers out.
//!
//! A tool result enters the chat history once and is re-sent with **every**
//! later call of the conversation, so a 60 KB list an app returned on turn 3 is
//! paid for again on turns 4 to 40. This module shrinks such a result at the
//! single point it is created (`zen_core::run_tools`), and the shrunk text is
//! what the history keeps: nothing is recompressed later, so the request prefix
//! stays byte-identical from call to call — which a provider's prompt cache and
//! a local engine's KV cache both depend on.
//!
//! The ideas are Headroom's (Apache-2.0, <https://github.com/headroomlabs-ai/headroom>):
//! route by what the content is, shrink structure-aware instead of cutting the
//! middle, keep what is unusual, and make the cut reversible. The
//! implementation is ours and has no model and no extra process; the research
//! behind each choice is in `plans/reports/research-261023-1209-headroom-context-compression.md`.
//!
//! Two tiers, and only the first is applied without leaving a way back:
//!
//! 1. **Same data, fewer bytes.** An MCP result is printed as pretty JSON around
//!    a text block that is itself JSON: the envelope, the indentation and the
//!    escaping carry no meaning. They are removed and nothing else changes.
//! 2. **Rows left out, original saved.** A long array keeps its start, its end,
//!    its unusual rows and a spread (`array`); a long build log keeps its
//!    failures, summary and ends (`log`). The full text is saved under the
//!    chat's workspace and the footer names the file — the model reads it with
//!    `Read`/`Grep`, the same tools it already has. If the file cannot be
//!    written, tier 1 is used and nothing is lost.
//!
//! What it never touches: `Read`, `Edit`, `Write` and every other tool whose
//! output a later edit is matched against (only an allow-list is processed, see
//! [`eligible`]), errors, and anything that does not shrink by a clear margin.
//! The user's own view of the tool call (the card in the chat) is the full
//! `data`, not this text.

pub mod array;
pub mod json;
pub mod log;

use std::path::Path;

use json::Envelope;

/// Below this a result is not worth looking at.
const MIN_BYTES: usize = 800;
/// A tier-1 change must save at least this much, absolutely and as a share.
const MIN_SAVED_BYTES: usize = 200;
const MIN_SAVED_SHARE: f64 = 0.08;
/// Rows are only left out of a document this large (about 1,000 tokens).
const LOSSY_MIN_BYTES: usize = 4096;
/// Above this the scan is not worth the time; tier 1 still applies.
const LOSSY_MAX_BYTES: usize = 8 * 1024 * 1024;
/// A cut array must come out at most this share of what it was, and the whole
/// document at most `DOC_SHARE`.
const ARRAY_SHARE: f64 = 0.5;
const DOC_SHARE: f64 = 0.7;

/// What [`compress`] decided.
pub enum Outcome {
    Unchanged,
    /// The same data, fewer bytes.
    Lossless(String),
    /// Rows left out; see [`Lossy::with_footer`].
    Lossy(Lossy),
}

pub struct Lossy {
    /// What the model sees, before the footer.
    pub text: String,
    /// Everything, one row per line — the file the footer points at.
    pub full: String,
    /// The tier-1 answer, for when `full` cannot be saved.
    pub lossless: Option<String>,
    pub kept: usize,
    pub total: usize,
    pub unit: &'static str,
}

impl Lossy {
    pub fn with_footer(&self, saved_at: &Path) -> String {
        format!(
            "{}\n[context-compress: kept {} of {} {}. If the task needs the others, Read or Grep {} (the full {}) before answering.]",
            self.text,
            self.kept,
            self.total,
            self.unit,
            saved_at.display(),
            human_size(self.full.len())
        )
    }
}

fn human_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

/// The tools whose output is data or a log and never the text of a file that is
/// edited next. An unknown tool is left alone: skipping costs tokens, a wrong
/// guess costs an edit that no longer matches.
pub fn eligible(tool_name: &str) -> bool {
    // A browser snapshot lists the elements the model clicks next by index:
    // leaving some out would leave them unclickable.
    if tool_name.contains("__browser_") {
        return false;
    }
    tool_name.starts_with("mcp__") || matches!(tool_name, "WebFetch" | "Bash" | "PeekBgJob")
}

/// Bash and background-job output is text a person might be reading line by
/// line; only the log shrinker, which needs the text to read like a log, may
/// touch it. The JSON tier would also reformat a `cat package.json` the model
/// is about to edit.
fn is_shell(tool_name: &str) -> bool {
    matches!(tool_name, "Bash" | "PeekBgJob")
}

pub fn compress(tool_name: &str, text: &str) -> Outcome {
    if !eligible(tool_name) || text.len() < MIN_BYTES {
        return Outcome::Unchanged;
    }
    if is_shell(tool_name) {
        return match log::shrink(text) {
            Some(s) => Outcome::Lossy(Lossy {
                text: s.text,
                full: text.to_string(),
                lossless: None,
                kept: s.kept,
                total: s.total,
                unit: "lines",
            }),
            None => Outcome::Unchanged,
        };
    }

    let trimmed = text.trim();
    let unwrapped;
    let payload = match json::classify_envelope(trimmed) {
        Envelope::Leave => return Outcome::Unchanged,
        Envelope::Text(inner) => {
            unwrapped = inner;
            unwrapped.trim()
        }
        Envelope::NotOne => trimmed,
    };

    if !json::is_json_container(payload) {
        // Prose inside an MCP envelope: the envelope still goes.
        return lossless(text, payload.to_string());
    }
    let compact = json::minify(payload);
    let tier1 = lossless_text(text, &compact);

    if compact.len() >= LOSSY_MIN_BYTES && compact.len() <= LOSSY_MAX_BYTES {
        if let Some(cut) = cut_arrays(&compact) {
            return Outcome::Lossy(Lossy {
                text: cut.text,
                full: cut.full,
                lossless: tier1,
                kept: cut.kept,
                total: cut.total,
                unit: "items",
            });
        }
    }
    match tier1 {
        Some(t) => Outcome::Lossless(t),
        None => Outcome::Unchanged,
    }
}

/// `smaller` is the same data in other bytes; use it only when it is clearly less.
fn lossless_text(original: &str, smaller: &str) -> Option<String> {
    let saved = original.len().saturating_sub(smaller.len());
    (saved >= MIN_SAVED_BYTES && saved as f64 >= MIN_SAVED_SHARE * original.len() as f64)
        .then(|| smaller.to_string())
}

fn lossless(original: &str, smaller: String) -> Outcome {
    match lossless_text(original, &smaller) {
        Some(t) => Outcome::Lossless(t),
        None => Outcome::Unchanged,
    }
}

struct Cut {
    text: String,
    full: String,
    kept: usize,
    total: usize,
}

/// Cut every long array of a minified document that shrinks enough, outermost
/// first. `text` has the arrays' markers in place of the omitted rows; `full`
/// has every row on a line of its own.
fn cut_arrays(compact: &str) -> Option<Cut> {
    let mut arrays = json::scan_arrays(compact);
    arrays.retain(|a| a.elems.len() >= array::MIN_ITEMS);
    arrays.sort_by_key(|a| a.open);

    let mut accepted: Vec<(usize, usize, Vec<usize>)> = Vec::new();
    let mut covered_until = 0;
    for a in &arrays {
        if a.open < covered_until {
            continue;
        }
        let keep = array::select(compact, &a.elems);
        let old_len = a.close + 1 - a.open;
        if render_array(compact, a, &keep).len() as f64 > ARRAY_SHARE * old_len as f64 {
            continue;
        }
        covered_until = a.close + 1;
        accepted.push((a.open, a.close, keep));
    }
    if accepted.is_empty() {
        return None;
    }

    let by_open: std::collections::HashMap<usize, &json::ArrayInfo> =
        arrays.iter().map(|a| (a.open, a)).collect();
    let (mut text, mut full) = (String::new(), String::new());
    let (mut cursor, mut kept, mut total) = (0, 0, 0);
    for (open, close, keep) in &accepted {
        let a = by_open[open];
        let between = &compact[cursor..*open];
        text.push_str(between);
        full.push_str(between);
        text.push_str(&render_array(compact, a, keep));
        full.push_str("[\n");
        full.push_str(
            &a.elems
                .iter()
                .map(|&(s, e)| &compact[s..e])
                .collect::<Vec<_>>()
                .join(",\n"),
        );
        full.push_str("\n]");
        cursor = close + 1;
        kept += keep.len();
        total += a.elems.len();
    }
    text.push_str(&compact[cursor..]);
    full.push_str(&compact[cursor..]);

    (text.len() as f64 <= DOC_SHARE * compact.len() as f64).then_some(Cut {
        text,
        full,
        kept,
        total,
    })
}

/// The array with only `keep` rows, a marker standing for each run left out.
fn render_array(compact: &str, a: &json::ArrayInfo, keep: &[usize]) -> String {
    let mut out = String::from("[");
    let mut next = 0;
    let mut first = true;
    for &i in keep {
        if !first {
            out.push(',');
        }
        first = false;
        if i > next {
            out.push_str(&format!("\"[… {} items omitted …]\",", i - next));
        }
        let (s, e) = a.elems[i];
        out.push_str(&compact[s..e]);
        next = i + 1;
    }
    if next < a.elems.len() {
        out.push_str(&format!(
            ",\"[… {} items omitted …]\"",
            a.elems.len() - next
        ));
    }
    out.push(']');
    out
}

/// First 4 bytes of the SHA-256 of `text`, hex.
fn content_tag(text: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(&Sha256::digest(text.as_bytes())[..4])
}

/// The seam `zen_core::run_tools` calls with a finished tool result. Returns the
/// text the model should be shown.
///
/// The setting is read only after the cheap checks pass, so the ordinary small
/// result never touches `config.json`.
pub fn apply(tool_name: &str, tool_id: &str, agent_data_dir: &str, text: String) -> String {
    if !eligible(tool_name) || text.len() < MIN_BYTES {
        return text;
    }
    let config_path = crate::control_plane::default_config_path();
    if !crate::gateway::group_manager::get_context_compression_enabled(&config_path) {
        return text;
    }
    let before = text.len();
    let shown = match compress(tool_name, &text) {
        Outcome::Unchanged => return text,
        Outcome::Lossless(t) => t,
        Outcome::Lossy(l) => {
            let workspace = crate::control_plane::workspace::Workspace::for_chat(agent_data_dir);
            // Named by what it holds, not only by the call id: a provider that
            // numbers calls per response (`ag_call_0` every turn) reuses an id
            // within a chat, and a second result would overwrite the file an
            // earlier footer in the history still points at. The same content
            // gives the same name, so the text stays a pure function of the
            // output. `-full` keeps it apart from the L1 offload's file.
            let name = format!("{tool_id}-{}-full", content_tag(&l.full));
            match workspace.save_artifact(&name, &l.full) {
                Ok(path) => l.with_footer(&path),
                Err(e) => {
                    tracing::debug!(
                        "[context-compress] could not save the full {tool_name} result: {e}"
                    );
                    match l.lossless {
                        Some(t) => t,
                        None => return text,
                    }
                }
            }
        }
    };
    tracing::info!(
        target: "context_compress",
        "{tool_name}: {before} -> {} bytes (~{} tokens saved)",
        shown.len(),
        before.saturating_sub(shown.len()) / 4
    );
    shown
}

#[cfg(test)]
mod tests;
