//! Tolerant ways to apply an edit, for models that cannot reproduce
//! `old_string` byte for byte (Aider's observation: small local models fail
//! exact search/replace far more than they fail to describe the change).
//!
//! - [`find_fuzzy`]: locate `old` in `content` when the exact match fails —
//!   first by comparing with runs of whitespace collapsed and line ends
//!   trimmed, then by the best line window with a high similarity ratio.
//!   Returns the byte span of the *original* text so the caller replaces
//!   exactly what is there.
//! - [`apply_udiff`]: apply unified-diff hunks without trusting their line
//!   numbers: each hunk's "before" lines are searched for (exact, then
//!   whitespace-normalized), and replaced with its "after" lines.
//!
//! Both are read-only over strings; the caller writes the file and produces
//! the diff shown to the model.

use similar::TextDiff;

/// Minimum similarity for a line-window fuzzy match. Below this the edit is
/// refused rather than applied somewhere merely resembling the target.
pub const FUZZY_MIN_RATIO: f32 = 0.9;

fn norm_line(l: &str) -> String {
    l.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Byte offsets of each line start in `content` (plus the end).
fn line_starts(content: &str) -> Vec<usize> {
    let mut v = vec![0];
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' {
            v.push(i + 1);
        }
    }
    if *v.last().unwrap() != content.len() {
        v.push(content.len());
    }
    v
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyHit {
    pub start: usize,
    pub end: usize,
    /// `whitespace` or `similar`.
    pub how: &'static str,
}

/// Locate `old` in `content` tolerating whitespace differences, then near
/// misses. `None` when nothing is close enough or the match is ambiguous.
pub fn find_fuzzy(content: &str, old: &str) -> Option<FuzzyHit> {
    let old_lines: Vec<&str> = old.lines().collect();
    if old_lines.is_empty() {
        return None;
    }
    let content_lines: Vec<&str> = content.lines().collect();
    if content_lines.len() < old_lines.len() {
        return None;
    }
    let starts = line_starts(content);
    let n = old_lines.len();

    // 1. Whitespace-insensitive window match (must be unique).
    let old_norm: Vec<String> = old_lines.iter().map(|l| norm_line(l)).collect();
    let content_norm: Vec<String> = content_lines.iter().map(|l| norm_line(l)).collect();
    let mut ws_hits: Vec<usize> = Vec::new();
    for i in 0..=(content_norm.len() - n) {
        if content_norm[i..i + n] == old_norm[..] {
            ws_hits.push(i);
            if ws_hits.len() > 1 {
                break;
            }
        }
    }
    if ws_hits.len() == 1 {
        let i = ws_hits[0];
        return Some(FuzzyHit { start: starts[i], end: line_end(&starts, content, i + n - 1), how: "whitespace" });
    }
    if ws_hits.len() > 1 {
        return None;
    }

    // 2. Best line window by similarity ratio.
    let old_joined = old_norm.join("\n");
    let mut best: Option<(f32, usize)> = None;
    let mut second: f32 = 0.0;
    for i in 0..=(content_norm.len() - n) {
        let window = content_norm[i..i + n].join("\n");
        let ratio = TextDiff::from_chars(old_joined.as_str(), window.as_str()).ratio();
        match best {
            Some((b, _)) if ratio <= b => {
                if ratio > second {
                    second = ratio;
                }
            }
            Some((b, _)) => {
                second = b;
                best = Some((ratio, i));
            }
            None => best = Some((ratio, i)),
        }
    }
    let (ratio, i) = best?;
    if ratio < FUZZY_MIN_RATIO || (second >= FUZZY_MIN_RATIO && (ratio - second).abs() < 0.01) {
        return None;
    }
    Some(FuzzyHit { start: starts[i], end: line_end(&starts, content, i + n - 1), how: "similar" })
}

/// Byte offset just past line `idx` (its newline excluded).
fn line_end(starts: &[usize], content: &str, idx: usize) -> usize {
    let next = starts.get(idx + 1).copied().unwrap_or(content.len());
    let mut end = next;
    if end > 0 && content.as_bytes().get(end - 1) == Some(&b'\n') && next != content.len() {
        end -= 1;
    } else if next == content.len() && content.ends_with('\n') && end > 0 {
        end -= 1;
    }
    end
}

#[derive(Debug, Clone)]
struct Hunk {
    before: Vec<String>,
    after: Vec<String>,
}

/// Parse a unified diff (with or without `---`/`+++`/`@@` headers) into
/// hunks of before/after lines. Context lines belong to both.
fn parse_udiff(patch: &str) -> Result<Vec<Hunk>, String> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut cur: Option<Hunk> = None;
    for raw in patch.lines() {
        if raw.starts_with("--- ") || raw.starts_with("+++ ") || raw.starts_with("diff ") || raw.starts_with("index ") {
            continue;
        }
        if raw.starts_with("@@") {
            if let Some(h) = cur.take() {
                hunks.push(h);
            }
            cur = Some(Hunk { before: vec![], after: vec![] });
            continue;
        }
        if raw == "\\ No newline at end of file" {
            continue;
        }
        let h = cur.get_or_insert_with(|| Hunk { before: vec![], after: vec![] });
        if let Some(l) = raw.strip_prefix('+') {
            h.after.push(l.to_string());
        } else if let Some(l) = raw.strip_prefix('-') {
            h.before.push(l.to_string());
        } else {
            let l = raw.strip_prefix(' ').unwrap_or(raw);
            h.before.push(l.to_string());
            h.after.push(l.to_string());
        }
    }
    if let Some(h) = cur.take() {
        hunks.push(h);
    }
    let hunks: Vec<Hunk> = hunks.into_iter().filter(|h| !(h.before.is_empty() && h.after.is_empty())).collect();
    if hunks.is_empty() {
        return Err("the patch contains no hunks (lines must start with ' ', '+' or '-')".into());
    }
    Ok(hunks)
}

/// Byte spans of every window of whole lines equal to `lines`.
fn exact_line_windows(content: &str, lines: &[String]) -> Vec<(usize, usize)> {
    let content_lines: Vec<&str> = content.lines().collect();
    let n = lines.len();
    if n == 0 || content_lines.len() < n {
        return Vec::new();
    }
    let starts = line_starts(content);
    let mut hits = Vec::new();
    for i in 0..=(content_lines.len() - n) {
        if content_lines[i..i + n].iter().zip(lines).all(|(a, b)| *a == b.as_str()) {
            hits.push((starts[i], line_end(&starts, content, i + n - 1)));
        }
    }
    hits
}

#[derive(Debug)]
pub struct UdiffOutcome {
    pub content: String,
    pub hunks_applied: usize,
    /// How each hunk matched: `exact` / `whitespace` / `similar`.
    pub how: Vec<&'static str>,
}

/// Apply `patch` to `content`. Every hunk must match somewhere (uniquely);
/// otherwise nothing is applied and the error says which hunk failed.
pub fn apply_udiff(content: &str, patch: &str) -> Result<UdiffOutcome, String> {
    let hunks = parse_udiff(patch)?;
    let mut out = content.to_string();
    let mut how = Vec::new();
    for (n, h) in hunks.iter().enumerate() {
        let before = h.before.join("\n");
        let after = h.after.join("\n");
        if h.before.is_empty() {
            // Pure insertion with no context: append.
            if !out.ends_with('\n') && !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&after);
            out.push('\n');
            how.push("append");
            continue;
        }
        // Whole-line windows only: a hunk's "before" text must line up with
        // complete lines, never with the tail of one (`b = 2` inside
        // `sub = 2`). Exact first, then the tolerant matchers.
        let exact_hits = exact_line_windows(&out, &h.before);
        let (start, end, kind) = if exact_hits.len() == 1 {
            let (s, e) = exact_hits[0];
            (s, e, "exact")
        } else if exact_hits.len() > 1 {
            return Err(format!("hunk {} matches {} places; add more context lines", n + 1, exact_hits.len()));
        } else {
            match find_fuzzy(&out, &before) {
                Some(hit) => (hit.start, hit.end, hit.how),
                None => {
                    return Err(format!(
                        "hunk {} does not match the file (first line: {:?}); re-read the file and include exact context",
                        n + 1,
                        h.before.first().cloned().unwrap_or_default()
                    ))
                }
            }
        };
        out.replace_range(start..end, &after);
        how.push(kind);
    }
    Ok(UdiffOutcome { content: out, hunks_applied: hunks.len(), how })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "fn main() {\n    let a = 1;\n    let b = 2;\n    println!(\"{}\", a + b);\n}\n";

    #[test]
    fn fuzzy_matches_whitespace_drift_and_refuses_ambiguity() {
        let hit = find_fuzzy(SRC, "let a = 1;\n  let b   = 2;").unwrap();
        assert_eq!(hit.how, "whitespace");
        assert_eq!(&SRC[hit.start..hit.end], "    let a = 1;\n    let b = 2;");
        // One-character drift: similarity path.
        let hit = find_fuzzy(SRC, "    let a = 1;\n    let b = 3;").unwrap();
        assert_eq!(hit.how, "similar");
        // Nothing alike.
        assert!(find_fuzzy(SRC, "completely different\ntext here").is_none());
        // Ambiguous window → refused.
        let dup = "x\ny\nx\ny\n";
        assert!(find_fuzzy(dup, "x\n y").is_none());
    }

    #[test]
    fn udiff_applies_hunks_by_content_not_line_numbers() {
        let patch = "@@ -100,3 +100,3 @@\n     let a = 1;\n-    let b = 2;\n+    let b = 20;\n     println!(\"{}\", a + b);\n";
        let out = apply_udiff(SRC, patch).unwrap();
        assert!(out.content.contains("let b = 20;"));
        assert_eq!(out.hunks_applied, 1);
        assert_eq!(out.how, vec!["exact"]);
        // Headers optional; whitespace drift tolerated.
        let patch2 = "-let b = 2;\n+let b = 30;\n";
        let out = apply_udiff(SRC, patch2).unwrap();
        assert!(out.content.contains("let b = 30;"));
        assert_eq!(out.how, vec!["whitespace"]);
    }

    #[test]
    fn udiff_failures_are_named_and_atomic() {
        let patch = "@@\n-    let a = 1;\n+    let a = 10;\n@@\n-    nope();\n+    yes();\n";
        let err = apply_udiff(SRC, patch).unwrap_err();
        assert!(err.contains("hunk 2"), "{err}");
        assert!(parse_udiff("").is_err());
    }
}
