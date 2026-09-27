//! Does this machine have what the app said it needs?
//!
//! An app that needs `ffmpeg`, or Node 18, or Python 3.10 used to discover that
//! at its first launch — as a non-zero exit code in a log file nobody was
//! looking at, minutes after the install said "done". Declaring it in the
//! manifest (`requires`) turns that into a sentence at install time, and into a
//! refusal to launch with a reason attached, which is the difference between
//! "the app is broken" and "install ffmpeg".
//!
//! Checks are deliberately cheap and cached for a few seconds: `which`, and
//! `--version` on the two runtimes. Nothing is installed on the user's behalf —
//! we report, they decide.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use super::manifest::{Requirement, RequirementKind, Requires};

/// The outcome for one declared requirement.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CheckResult {
    pub name: String,
    /// `node` | `python` | `bin` | `env`
    pub kind: String,
    /// The declared range, when one was asked for.
    pub range: Option<String>,
    /// What was found on this machine — a version, a path, or nothing.
    pub found: Option<String>,
    pub ok: bool,
    pub optional: bool,
    /// What the user should do about it. Empty when `ok`.
    pub hint: String,
}

/// The whole report for one app.
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RequirementsReport {
    pub items: Vec<CheckResult>,
    /// Every non-optional requirement is satisfied — the app may be launched.
    pub satisfied: bool,
    /// One line naming what is missing, ready to put in a log or a toast.
    pub summary: String,
}

impl RequirementsReport {
    /// The requirements that are both unmet and not optional.
    pub fn blocking(&self) -> Vec<&CheckResult> {
        self.items.iter().filter(|i| !i.ok && !i.optional).collect()
    }
}

/// Check every declared requirement. Never fails: an unknown answer is reported
/// as unmet with a hint, because "we could not tell" and "it is there" must not
/// look the same.
pub async fn check(requires: &Requires) -> RequirementsReport {
    let mut items = Vec::new();

    if !requires.os.is_empty() {
        let here = current_os();
        let ok = requires.os.iter().any(|o| normalise_os(o) == here);
        items.push(CheckResult {
            name: "os".to_string(),
            kind: "os".to_string(),
            range: Some(requires.os.join(", ")),
            found: Some(here.to_string()),
            ok,
            optional: false,
            hint: if ok {
                String::new()
            } else {
                format!(
                    "This app supports {} — this machine is {here}.",
                    requires.os.join(" / ")
                )
            },
        });
    }

    for req in &requires.items {
        items.push(check_one(req).await);
    }

    let satisfied = items.iter().all(|i| i.ok || i.optional);
    let missing: Vec<String> = items
        .iter()
        .filter(|i| !i.ok)
        .map(|i| match (&i.range, &i.found) {
            (Some(r), Some(f)) => format!("{} {r} (found {f})", i.name),
            (Some(r), None) => format!("{} {r} (missing)", i.name),
            (None, _) => format!("{} (missing)", i.name),
        })
        .collect();
    let summary = if missing.is_empty() {
        "all requirements satisfied".to_string()
    } else {
        format!("missing: {}", missing.join(", "))
    };

    RequirementsReport { items, satisfied, summary }
}

async fn check_one(req: &Requirement) -> CheckResult {
    let mut out = CheckResult {
        name: req.name.clone(),
        kind: req.kind.as_str().to_string(),
        range: req.range.clone(),
        found: None,
        ok: false,
        optional: req.optional,
        hint: String::new(),
    };

    match req.kind {
        RequirementKind::Env => {
            let v = std::env::var(&req.name).unwrap_or_default();
            out.ok = !v.trim().is_empty();
            // Never the value: this is a report the Web UI renders.
            out.found = if out.ok { Some("set".into()) } else { None };
            if !out.ok {
                out.hint = format!("Set the environment variable `{}` for the daemon.", req.name);
            }
        }
        RequirementKind::Bin => {
            match which(&req.name).await {
                Some(path) => {
                    out.ok = true;
                    out.found = Some(path);
                }
                None => {
                    out.hint = format!(
                        "`{}` is not on PATH. Install it (macOS: `brew install {}`, \
                         Debian/Ubuntu: `apt install {}`).",
                        req.name, req.name, req.name
                    );
                }
            }
        }
        RequirementKind::Node | RequirementKind::Python => {
            let candidates: &[&str] = if req.kind == RequirementKind::Node {
                &["node"]
            } else {
                // `python` alone is Python 2 on some machines and absent on
                // others, so `python3` is tried first.
                &["python3", "python"]
            };
            let mut found_version = None;
            for c in candidates {
                if let Some(v) = version_of(c).await {
                    found_version = Some(v);
                    break;
                }
            }
            match found_version {
                Some(v) => {
                    out.found = Some(v.clone());
                    out.ok = match &req.range {
                        Some(range) => version_matches(&v, range),
                        None => true,
                    };
                    if !out.ok {
                        out.hint = format!(
                            "{} {} is required; this machine has {v}.",
                            req.name,
                            req.range.clone().unwrap_or_default()
                        );
                    }
                }
                None => {
                    out.hint = format!(
                        "{} is not installed (or not on the daemon's PATH).",
                        req.name
                    );
                }
            }
        }
    }
    out
}

fn normalise_os(s: &str) -> &'static str {
    match s.trim().to_ascii_lowercase().as_str() {
        "macos" | "mac" | "darwin" | "osx" => "macos",
        "linux" => "linux",
        "windows" | "win" | "win32" => "windows",
        _ => "other",
    }
}

pub fn current_os() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "other"
    }
}

// ---------------------------------------------------------------------------
// Probing the machine
// ---------------------------------------------------------------------------

/// `which` results, cached briefly. The install path and the launch path both
/// ask the same questions seconds apart, and a per-app launch must not turn
/// into a dozen process spawns.
static PROBE_CACHE: Mutex<Option<HashMap<String, (Instant, Option<String>)>>> = Mutex::new(None);
const PROBE_TTL: Duration = Duration::from_secs(20);

fn cache_get(key: &str) -> Option<Option<String>> {
    let guard = PROBE_CACHE.lock().ok()?;
    let map = guard.as_ref()?;
    let (at, v) = map.get(key)?;
    (at.elapsed() < PROBE_TTL).then(|| v.clone())
}

fn cache_put(key: &str, value: Option<String>) {
    if let Ok(mut guard) = PROBE_CACHE.lock() {
        guard
            .get_or_insert_with(HashMap::new)
            .insert(key.to_string(), (Instant::now(), value));
    }
}

/// Absolute path of `bin` on the daemon's PATH, or `None`.
pub async fn which(bin: &str) -> Option<String> {
    let key = format!("which:{bin}");
    if let Some(hit) = cache_get(&key) {
        return hit;
    }
    let found = which_uncached(bin).await;
    cache_put(&key, found.clone());
    found
}

async fn which_uncached(bin: &str) -> Option<String> {
    // Do it ourselves rather than shelling out to `which`/`where`: one fewer
    // process, and it behaves the same on Windows.
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
            .split(';')
            .map(|e| e.to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate: PathBuf = dir.join(format!("{bin}{ext}"));
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().to_string());
            }
        }
    }
    None
}

/// `<bin> --version`, reduced to the bare version number. `None` when the
/// program is absent or does not answer within a couple of seconds.
pub async fn version_of(bin: &str) -> Option<String> {
    let key = format!("version:{bin}");
    if let Some(hit) = cache_get(&key) {
        return hit;
    }
    let v = version_uncached(bin).await;
    cache_put(&key, v.clone());
    v
}

async fn version_uncached(bin: &str) -> Option<String> {
    which(bin).await?;
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(bin)
            .arg("--version")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    // Python 2 wrote its version to stderr; Node writes to stdout. Read both.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    extract_version(&text)
}

/// Pull `3.11.6` out of `Python 3.11.6`, `v18.20.2`, `ffmpeg version 6.1.1-…`.
pub fn extract_version(text: &str) -> Option<String> {
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == '.') {
                i += 1;
            }
            let candidate: String = bytes[start..i].iter().collect();
            let candidate = candidate.trim_end_matches('.').to_string();
            if candidate.contains('.') || candidate.len() >= 2 {
                return Some(candidate);
            }
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Version ranges
// ---------------------------------------------------------------------------

fn parts(v: &str) -> [u64; 3] {
    let mut out = [0u64; 3];
    for (i, seg) in v
        .trim()
        .trim_start_matches('v')
        .split(['.', '-', '+'])
        .take(3)
        .enumerate()
    {
        out[i] = seg
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0);
    }
    out
}

/// Does `version` satisfy `range`?
///
/// Supports what manifests actually contain: `>=18`, `>18.1`, `<=3.12`,
/// `=1.2.3`, `18`, `18.x`, `^18`, `~3.10`, and space/comma-separated
/// conjunctions like `>=3.10 <4`. Anything unparseable is treated as satisfied
/// — refusing to launch an app because we could not read its own range string
/// would be our bug punishing the user.
pub fn version_matches(version: &str, range: &str) -> bool {
    let v = parts(version);
    let range = range.trim();
    if range.is_empty() || range == "*" {
        return true;
    }
    for clause in range.split([',', '|']).flat_map(|c| c.split_whitespace()) {
        let clause = clause.trim();
        if clause.is_empty() {
            continue;
        }
        if !clause_matches(v, clause) {
            return false;
        }
    }
    true
}

fn clause_matches(v: [u64; 3], clause: &str) -> bool {
    let (op, rest) = if let Some(r) = clause.strip_prefix(">=") {
        (">=", r)
    } else if let Some(r) = clause.strip_prefix("<=") {
        ("<=", r)
    } else if let Some(r) = clause.strip_prefix('>') {
        (">", r)
    } else if let Some(r) = clause.strip_prefix('<') {
        ("<", r)
    } else if let Some(r) = clause.strip_prefix("==") {
        ("=", r)
    } else if let Some(r) = clause.strip_prefix('=') {
        ("=", r)
    } else if let Some(r) = clause.strip_prefix('^') {
        ("^", r)
    } else if let Some(r) = clause.strip_prefix('~') {
        ("~", r)
    } else {
        ("=", clause)
    };
    let rest = rest.trim();
    // Nothing numeric to compare against — a tag like `latest`, or a range
    // syntax we do not speak. Refusing to launch an app because we could not
    // read its own range string would be our bug punishing the user.
    if rest.is_empty() || !rest.chars().any(|c| c.is_ascii_digit()) {
        return true;
    }
    // `18.x` / `3.10.*` — a prefix match on the segments that were given.
    let wildcard = rest.contains('x') || rest.contains('X') || rest.contains('*');
    let given: Vec<&str> = rest.trim_start_matches('v').split('.').collect();
    let w = parts(rest);

    match op {
        ">=" => v >= w,
        ">" => v > w,
        "<=" => v <= w,
        "<" => v < w,
        // `^18` → >=18.0.0 <19.0.0; `^0.5` → >=0.5 <0.6 (npm's rule: the
        // leftmost non-zero segment is the one that may not change).
        "^" => {
            if v < w {
                return false;
            }
            if w[0] != 0 {
                v[0] == w[0]
            } else if w[1] != 0 {
                v[0] == 0 && v[1] == w[1]
            } else {
                v[0] == 0 && v[1] == 0
            }
        }
        // `~3.10` → >=3.10 <3.11
        "~" => v >= w && v[0] == w[0] && (given.len() < 2 || v[1] == w[1]),
        // `=`, with wildcards and short forms behaving as a prefix match.
        _ => {
            let depth = if wildcard {
                given.iter().take_while(|g| !g.contains(['x', 'X', '*'])).count()
            } else {
                given.len().min(3)
            };
            (0..depth).all(|i| v[i] == w[i])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::manifest::Requires;
    use serde_json::json;

    #[test]
    fn a_version_string_is_found_in_whatever_the_tool_printed() {
        assert_eq!(extract_version("v18.20.2\n").as_deref(), Some("18.20.2"));
        assert_eq!(extract_version("Python 3.11.6").as_deref(), Some("3.11.6"));
        assert_eq!(
            extract_version("ffmpeg version 6.1.1-tessus  https://x").as_deref(),
            Some("6.1.1")
        );
        assert_eq!(extract_version("no numbers here"), None);
    }

    #[test]
    fn the_ranges_manifests_actually_contain() {
        assert!(version_matches("18.20.2", ">=18"));
        assert!(version_matches("20.0.0", ">=18"));
        assert!(!version_matches("16.9.0", ">=18"));
        assert!(version_matches("3.11.6", ">=3.10"));
        // The classic string-compare bug: "3.9" > "3.10" as text, not as a version.
        assert!(!version_matches("3.9.18", ">=3.10"));
        assert!(version_matches("3.11.6", ">=3.10 <4"));
        assert!(!version_matches("4.0.0", ">=3.10 <4"));
        assert!(version_matches("18.1.0", "^18"));
        assert!(!version_matches("19.0.0", "^18"));
        assert!(version_matches("3.10.9", "~3.10"));
        assert!(!version_matches("3.11.0", "~3.10"));
        assert!(version_matches("18.20.2", "18.x"));
        assert!(!version_matches("20.1.0", "18.x"));
        assert!(version_matches("1.2.3", "1.2.3"));
        assert!(version_matches("22.0.1", ""));
        // A range we cannot read must not block a launch.
        assert!(version_matches("1.0.0", "latest-stable"));
    }

    #[tokio::test]
    async fn a_missing_binary_is_reported_with_something_to_do_about_it() {
        let m = json!({"requires": {"bin": ["definitely-not-installed-xyzzy"]}});
        let report = check(&Requires::parse(&m)).await;
        assert!(!report.satisfied);
        assert_eq!(report.blocking().len(), 1);
        assert!(report.items[0].hint.contains("PATH"), "{}", report.items[0].hint);
        assert!(report.summary.contains("definitely-not-installed-xyzzy"));
    }

    #[tokio::test]
    async fn an_optional_requirement_never_blocks() {
        let m = json!({"requires": {"optionalBin": ["definitely-not-installed-xyzzy"]}});
        let report = check(&Requires::parse(&m)).await;
        assert!(report.satisfied, "optional misses are reported, not enforced");
        assert!(!report.items[0].ok);
    }

    #[tokio::test]
    async fn the_wrong_platform_is_a_blocking_answer() {
        let other = if current_os() == "windows" { "linux" } else { "windows" };
        let m = json!({"requires": {"os": [other]}});
        let report = check(&Requires::parse(&m)).await;
        assert!(!report.satisfied);
        assert!(report.items[0].hint.contains("this machine is"));
    }

    #[tokio::test]
    async fn nothing_declared_is_satisfied() {
        let report = check(&Requires::default()).await;
        assert!(report.satisfied && report.items.is_empty());
        assert_eq!(report.summary, "all requirements satisfied");
    }

    #[tokio::test]
    async fn a_binary_that_is_certainly_here_resolves_to_a_path() {
        // `sh` on unix, `cmd` on windows — if this fails, PATH probing is broken.
        let bin = if cfg!(windows) { "cmd" } else { "sh" };
        let found = which(bin).await;
        assert!(found.is_some(), "{bin} must be on PATH");
        // …and the answer is cached, so the second call cannot disagree.
        assert_eq!(which(bin).await, found);
    }
}
