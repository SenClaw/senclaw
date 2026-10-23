//! Shrinking a long build / test / service log — the part of Headroom's
//! LogCompressor that survives without a model: keep the first and last lines,
//! every line that names a failure with a little of what surrounds it, the
//! summary lines, the first of each distinct warning, and say how many lines
//! were left out.
//!
//! It only ever runs on text that reads like a log (see [`looks_like_a_log`]).
//! A listing, a source file or a page of prose has none of the signals, and cutting its
//! middle would cut the part the model asked for.

use std::collections::{BTreeSet, HashMap};

/// Below these the saving does not repay the loss of a line.
pub const MIN_LINES: usize = 60;
const HEAD: usize = 8;
const TAIL: usize = 12;
const MAX_ERRORS: usize = 30;
/// Lines kept either side of a failure, and after it for a stack trace.
const CONTEXT: usize = 2;
const TRACE_LINES: usize = 10;
const MAX_WARNINGS: usize = 8;
const MAX_SUMMARY: usize = 12;
/// A failure that repeats is kept this many times (the first ones), plus the last.
const REPEATS_KEPT: usize = 2;
/// Share of non-empty lines that must carry a log signal.
const LOG_SHARE: f64 = 0.3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    Error,
    Warn,
    Summary,
    Plain,
}

pub struct Shrunk {
    pub text: String,
    pub kept: usize,
    pub total: usize,
}

/// `None` when the text is not a log, is short, or would not get meaningfully
/// smaller.
pub fn shrink(text: &str) -> Option<Shrunk> {
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    if n < MIN_LINES || !looks_like_a_log(&lines) {
        return None;
    }
    let levels: Vec<Level> = lines.iter().map(|l| level_of(l)).collect();

    let mut keep: BTreeSet<usize> = (0..HEAD.min(n)).chain(n.saturating_sub(TAIL)..n).collect();

    // Failures: the first few of each distinct kind, and the last of it.
    let mut by_kind: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, level) in levels.iter().enumerate() {
        if *level == Level::Error {
            by_kind.entry(kind_of(lines[i])).or_default().push(i);
        }
    }
    let mut chosen: Vec<usize> = by_kind
        .values()
        .flat_map(|at| {
            let mut v: Vec<usize> = at.iter().take(REPEATS_KEPT).copied().collect();
            if let Some(&last) = at.last() {
                v.push(last);
            }
            v
        })
        .collect();
    chosen.sort_unstable();
    chosen.dedup();
    if chosen.len() > MAX_ERRORS {
        // Keep the earliest — the cause — and the final one.
        let last = *chosen.last().unwrap();
        chosen.truncate(MAX_ERRORS - 1);
        chosen.push(last);
    }
    for &i in &chosen {
        let from = i.saturating_sub(CONTEXT);
        keep.extend(from..(i + 1 + CONTEXT).min(n));
        // A stack trace or a wrapped message follows indented.
        let mut j = i + 1;
        while j < n
            && j <= i + TRACE_LINES
            && (lines[j].starts_with(' ') || lines[j].starts_with('\t'))
        {
            keep.insert(j);
            j += 1;
        }
    }

    // Warnings: the first of each distinct one.
    let mut seen = BTreeSet::new();
    let mut warnings = 0;
    for (i, level) in levels.iter().enumerate() {
        if *level == Level::Warn && warnings < MAX_WARNINGS && seen.insert(kind_of(lines[i])) {
            keep.insert(i);
            warnings += 1;
        }
    }

    // Summaries: the last ones, which are the totals.
    let summaries: Vec<usize> = levels
        .iter()
        .enumerate()
        .filter(|(_, l)| **l == Level::Summary)
        .map(|(i, _)| i)
        .collect();
    keep.extend(summaries.iter().rev().take(MAX_SUMMARY));

    let omitted = n - keep.len();
    if omitted < MIN_LINES / 2 {
        return None;
    }
    let mut out = String::new();
    let mut next = 0;
    for &i in &keep {
        if i > next {
            out.push_str(&format!("[… {} lines omitted …]\n", i - next));
        }
        out.push_str(lines[i]);
        out.push('\n');
        next = i + 1;
    }
    if next < n {
        out.push_str(&format!("[… {} lines omitted …]\n", n - next));
    }
    // Under half of what it was, or it is not worth losing a line.
    if out.len() * 2 > text.len() {
        return None;
    }
    Some(Shrunk {
        text: out.trim_end().to_string(),
        kept: keep.len(),
        total: n,
    })
}

/// A log has levels, timestamps, or test-runner verdicts on a good share of its
/// lines.
pub fn looks_like_a_log(lines: &[&str]) -> bool {
    let non_empty = lines.iter().filter(|l| !l.trim().is_empty()).count();
    if non_empty == 0 {
        return false;
    }
    let signalled = lines.iter().filter(|l| has_log_signal(l)).count();
    signalled as f64 >= LOG_SHARE * non_empty as f64
}

fn has_log_signal(line: &str) -> bool {
    let t = line.trim_start();
    if starts_with_timestamp(t) {
        return true;
    }
    let lower = t.to_lowercase();
    const LEVELS: &[&str] = &["error", "warn", "info", "debug", "trace", "fatal", "notice"];
    if LEVELS.iter().any(|l| word_at_start_or_bracketed(&lower, l)) {
        return true;
    }
    const VERDICTS: &[&str] = &[
        " ... ok",
        " ... failed",
        " ... ignored",
        "passed",
        "failed",
        "✓",
        "✔",
        "✗",
        "✘",
        "ok  \t",
        "--- pass",
        "--- fail",
        "compiling ",
        "downloading ",
        "running ",
        "finished ",
    ];
    VERDICTS.iter().any(|v| lower.contains(v))
}

fn word_at_start_or_bracketed(lower: &str, word: &str) -> bool {
    let rest = lower.trim_start_matches(|c: char| c == '[' || c == '(' || c == '<');
    rest.starts_with(word)
        && rest[word.len()..]
            .chars()
            .next()
            .map_or(true, |c| !c.is_alphanumeric())
}

/// `2026-10-23`, `10:32:01`, `[2026…`, `Oct 23 10:32`.
fn starts_with_timestamp(t: &str) -> bool {
    let t = t.trim_start_matches('[');
    let b = t.as_bytes();
    let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
    (digit(0) && digit(1) && digit(2) && digit(3) && b.get(4) == Some(&b'-'))
        || (digit(0) && digit(1) && b.get(2) == Some(&b':') && digit(3) && digit(4))
}

fn level_of(line: &str) -> Level {
    let lower = line.to_lowercase();
    const ERRORS: &[&str] = &[
        "error",
        "exception",
        "fail",
        "fatal",
        "panic",
        "traceback",
        "denied",
        "timed out",
        "timeout",
        "✗",
        "✘",
        "❌",
        "lỗi",
        "thất bại",
    ];
    // A zero count is a verdict, not a failure ("0 failed", "errors: 0").
    if ERRORS.iter().any(|w| mentions(&lower, w)) {
        return if is_summary(&lower) {
            Level::Summary
        } else {
            Level::Error
        };
    }
    if is_summary(&lower) {
        return Level::Summary;
    }
    if ["warn", "deprecat"].iter().any(|w| mentions(&lower, w)) {
        return Level::Warn;
    }
    Level::Plain
}

/// The word starts somewhere in the line, not preceded by a letter, and is not
/// followed by a zero count.
fn mentions(lower: &str, word: &str) -> bool {
    lower.match_indices(word).any(|(at, _)| {
        let starts = lower[..at]
            .chars()
            .next_back()
            .map_or(true, |c| !c.is_alphanumeric());
        starts && !is_zero_count(lower, at, word.len())
    })
}

/// `0 failed`, `failed: 0`, `errors=0`, `0 errors`.
fn is_zero_count(lower: &str, at: usize, len: usize) -> bool {
    let before = lower[..at].trim_end();
    if before.ends_with(" 0") || before == "0" {
        return true;
    }
    let after = lower[at + len..].trim_start_matches(|c: char| c.is_alphabetic());
    let after = after.trim_start_matches(|c: char| c == ':' || c == '=' || c == ' ');
    after.starts_with('0') && !after[1..].starts_with(|c: char| c.is_ascii_digit())
}

fn is_summary(lower: &str) -> bool {
    let t = lower.trim_start();
    t.starts_with("===")
        || t.starts_with("test result")
        || t.starts_with("tests:")
        || t.starts_with("test suites:")
        || t.starts_with("summary")
        || t.starts_with("total")
        || t.starts_with("finished ")
        || t.starts_with("ran ")
        || (t.contains(" passed")
            && (t.contains(" failed") || t.contains(" skipped") || t.contains(';')))
}

/// A line with its numbers, addresses and ids blanked, so the same failure at
/// a different time or address is the same kind.
fn kind_of(line: &str) -> String {
    let mut out = String::with_capacity(line.len().min(120));
    let mut in_number = false;
    for c in line.trim().chars().take(160) {
        if c.is_ascii_digit() {
            if !in_number {
                out.push('N');
            }
            in_number = true;
        } else {
            in_number = false;
            out.push(c.to_ascii_lowercase());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_log(lines: usize, failing: &[usize]) -> String {
        let mut out = Vec::new();
        for i in 0..lines {
            if failing.contains(&i) {
                out.push(format!("2026-10-23 10:{:02}:{:02} ERROR worker-{} request {} failed: connection refused", i / 60 % 60, i % 60, i % 4, i));
                out.push("    at connect (net.js:1:1)".to_string());
                out.push("    at retry (pool.js:9:3)".to_string());
            } else {
                out.push(format!(
                    "2026-10-23 10:{:02}:{:02} INFO worker-{} request {} ok in {}ms",
                    i / 60 % 60,
                    i % 60,
                    i % 4,
                    i,
                    10 + i % 9
                ));
            }
        }
        out.push("Summary: 1 failed, 1990 passed".to_string());
        out.join("\n")
    }

    #[test]
    fn a_long_log_keeps_its_failures_with_context_and_its_summary() {
        let log = build_log(2000, &[700]);
        let shrunk = shrink(&log).expect("a 2000-line log shrinks");
        assert!(
            shrunk.text.len() * 10 < log.len(),
            "{} vs {}",
            shrunk.text.len(),
            log.len()
        );
        assert!(shrunk
            .text
            .contains("request 700 failed: connection refused"));
        assert!(
            shrunk.text.contains("at connect (net.js:1:1)"),
            "the trace under the failure"
        );
        assert!(
            shrunk.text.contains("request 699 ok"),
            "a line of context before"
        );
        assert!(shrunk.text.contains("Summary: 1 failed, 1990 passed"));
        assert!(shrunk.text.contains("request 0 ok"), "the start");
        assert!(shrunk.text.contains("lines omitted"));
        assert!(
            !shrunk.text.contains("request 350 ok"),
            "the quiet middle is gone"
        );
    }

    #[test]
    fn a_failure_that_repeats_is_kept_at_its_start_and_its_end_not_hundreds_of_times() {
        let failing: Vec<usize> = (100..1500).step_by(3).collect();
        let log = build_log(2000, &failing);
        let shrunk = shrink(&log).unwrap();
        // `request N failed` differs only in a number: one kind.
        let kept_failures = shrunk
            .text
            .lines()
            .filter(|l| l.contains("failed: connection refused"))
            .count();
        assert!(kept_failures <= 6, "kept {kept_failures}");
        assert!(shrunk.text.contains("request 100 failed"));
        assert!(
            shrunk.text.contains("request 1498 failed"),
            "the last of the kind"
        );
    }

    #[test]
    fn text_that_is_not_a_log_is_never_cut() {
        let prose: String = (0..300)
            .map(|i| format!("Paragraph {i} talks about gardens and the weather.\n"))
            .collect();
        assert!(shrink(&prose).is_none());
        let code: String = (0..300)
            .map(|i| format!("    let value_{i} = compute({i});\n"))
            .collect();
        assert!(shrink(&code).is_none());
    }

    #[test]
    fn a_short_log_is_left_alone() {
        assert!(shrink(&build_log(30, &[10])).is_none());
    }

    #[test]
    fn test_runner_output_is_recognised_and_its_one_failure_survives() {
        let mut out = String::from("running 900 tests\n");
        for i in 0..900 {
            if i == 421 {
                out.push_str("test billing::refund_rounds_up ... FAILED\n");
            } else {
                out.push_str(&format!("test module_{}::case_{i} ... ok\n", i % 20));
            }
        }
        out.push_str("\nfailures:\n    billing::refund_rounds_up\n\ntest result: FAILED. 899 passed; 1 failed; 0 ignored\n");
        let shrunk = shrink(&out).expect("test output shrinks");
        assert!(shrunk.text.contains("billing::refund_rounds_up ... FAILED"));
        assert!(shrunk
            .text
            .contains("test result: FAILED. 899 passed; 1 failed"));
        assert!(shrunk.text.len() * 5 < out.len());
    }

    #[test]
    fn a_zero_count_is_not_a_failure() {
        assert_eq!(
            level_of("test result: ok. 12 passed; 0 failed; 0 ignored"),
            Level::Summary
        );
        assert_eq!(level_of("errors: 0"), Level::Plain);
        assert_eq!(level_of("0 errors, 2 warnings"), Level::Warn);
        assert_eq!(level_of("error[E0308]: mismatched types"), Level::Error);
        assert_eq!(level_of("2 failed"), Level::Error);
    }

    #[test]
    fn the_same_failure_at_another_number_is_one_kind() {
        assert_eq!(
            kind_of("ERROR request 17 failed after 300ms"),
            kind_of("ERROR request 9021 failed after 12ms")
        );
        assert_ne!(
            kind_of("ERROR disk full"),
            kind_of("ERROR connection refused")
        );
    }
}
