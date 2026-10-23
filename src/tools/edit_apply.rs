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

/// The text of the file that looks most like what the model sent, for the
/// error that tells it its `old_string` did not match.
#[derive(Debug, Clone, PartialEq)]
pub struct NearMiss {
    /// 1-based, inclusive.
    pub first_line: usize,
    pub last_line: usize,
    pub ratio: f32,
    /// The file's own lines, numbered, ready to show.
    pub text: String,
}

/// Below this a window is a coincidence, not what the model meant.
pub const NEAR_MISS_MIN_RATIO: f32 = 0.6;

/// Most lines shown in a near miss.
const NEAR_MISS_MAX_LINES: usize = 12;

/// The window of `content` most similar to `old`, or `None` when nothing is
/// close or the file is too large to scan quickly.
///
/// The model's `old_string` that "did not appear verbatim" is almost always
/// *almost* right — one indentation, one renamed variable, a line that moved.
/// Telling it only "not found" costs a re-read of the file and often a second
/// miss; showing the lines it was probably after (as Aider does) lets it fix
/// the next attempt without a tool call. SWE-agent measured edit success
/// falling from 90.5 % to 57.2 % after one failed edit, so the first retry is
/// the one worth helping.
pub fn nearest_block(content: &str, old: &str) -> Option<NearMiss> {
    let old_lines: Vec<&str> = old.lines().collect();
    let content_lines: Vec<&str> = content.lines().collect();
    let n = old_lines.len();
    if n == 0 || n > 60 || content_lines.len() < n || content_lines.len() > 20_000 {
        return None;
    }
    let old_norm: Vec<String> = old_lines.iter().map(|l| norm_line(l)).collect();
    let content_norm: Vec<String> = content_lines.iter().map(|l| norm_line(l)).collect();
    let old_joined = old_norm.join("\n");

    // Cheap filter first: a window that shares fewer than a third of `old`'s
    // words cannot be close, and diffing every window of a 10,000-line file
    // is not free. `covered[i]` = how many of `old`'s words window `i` holds.
    let words = |l: &str| -> Vec<String> {
        l.split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|w| w.chars().count() >= 3)
            .map(str::to_lowercase)
            .collect()
    };
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for l in &old_norm {
        for w in words(l) {
            let next = index.len();
            index.entry(w).or_insert(next);
        }
    }
    let per_line: Vec<Vec<usize>> = content_norm
        .iter()
        .map(|l| {
            let mut v: Vec<usize> = words(l).iter().filter_map(|w| index.get(w).copied()).collect();
            v.sort_unstable();
            v.dedup();
            v
        })
        .collect();
    let mut counts = vec![0usize; index.len()];
    let mut covered = 0usize;
    let add = |line: &Vec<usize>, delta: isize, counts: &mut Vec<usize>, covered: &mut usize| {
        for &w in line {
            if delta > 0 {
                counts[w] += 1;
                if counts[w] == 1 {
                    *covered += 1;
                }
            } else {
                counts[w] -= 1;
                if counts[w] == 0 {
                    *covered -= 1;
                }
            }
        }
    };

    let mut best: Option<(f32, usize)> = None;
    for i in 0..=(content_norm.len() - n) {
        if i == 0 {
            for line in &per_line[..n] {
                add(line, 1, &mut counts, &mut covered);
            }
        } else {
            add(&per_line[i - 1], -1, &mut counts, &mut covered);
            add(&per_line[i + n - 1], 1, &mut counts, &mut covered);
        }
        if !index.is_empty() && covered * 3 < index.len() {
            continue;
        }
        let window = content_norm[i..i + n].join("\n");
        let ratio = TextDiff::from_chars(old_joined.as_str(), window.as_str()).ratio();
        if best.map_or(true, |(b, _)| ratio > b) {
            best = Some((ratio, i));
        }
    }
    let (ratio, i) = best?;
    if ratio < NEAR_MISS_MIN_RATIO {
        return None;
    }
    let last = (i + n).min(i + NEAR_MISS_MAX_LINES);
    let text = content_lines[i..last]
        .iter()
        .enumerate()
        .map(|(k, l)| {
            let l: String = l.chars().take(200).collect();
            format!("{:>5}\t{}", i + k + 1, l)
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(NearMiss { first_line: i + 1, last_line: last, ratio, text })
}

/// 1-based line numbers where `needle` starts in `content`, at most `limit`.
pub fn occurrence_lines(content: &str, needle: &str, limit: usize) -> Vec<usize> {
    content
        .match_indices(needle)
        .take(limit)
        .map(|(pos, _)| content[..pos].bytes().filter(|b| *b == b'\n').count() + 1)
        .collect()
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

    #[test]
    fn a_near_miss_shows_the_files_own_lines_numbered() {
        let content = "fn a() {}\n\nfn handle(req: Request) -> Response {\n    let body = req.body();\n    respond(body)\n}\n";
        // The model remembered `request` and a different local name.
        let old = "fn handle(request: Request) -> Response {\n    let data = request.body();\n    respond(data)\n}";
        let miss = nearest_block(content, old).expect("close enough to show");
        assert_eq!((miss.first_line, miss.last_line), (3, 6));
        assert!(miss.text.contains("    3\tfn handle(req: Request)"), "{}", miss.text);
        assert!(miss.ratio >= NEAR_MISS_MIN_RATIO);
    }

    #[test]
    fn nothing_close_shows_nothing() {
        assert_eq!(nearest_block("alpha\nbeta\ngamma\n", "completely unrelated text here\nand more"), None);
        assert_eq!(nearest_block("", "x"), None);
        assert_eq!(nearest_block("a\nb\n", ""), None);
    }

    #[test]
    fn a_huge_file_is_not_scanned() {
        let big = "line\n".repeat(25_000);
        assert_eq!(nearest_block(&big, "line\nline"), None);
    }

    #[test]
    fn occurrences_are_reported_by_line() {
        assert_eq!(occurrence_lines("a\nx\nb\nx\nc x\n", "x", 2), vec![2, 4]);
        assert_eq!(occurrence_lines("a\nx\nb\nx\nc x\n", "x", 10), vec![2, 4, 5]);
        assert!(occurrence_lines("abc", "zzz", 3).is_empty());
    }
}
