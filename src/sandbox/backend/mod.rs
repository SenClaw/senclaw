//! Backend abstraction: one shape of "run this script", two implementations.
//!
//! * `direct` — the script runs on this machine, confined by the OS
//!   (macOS Seatbelt / Linux bubblewrap). Fast, no daemon required.
//! * `docker` — the script runs inside a container. Stronger boundary, needs a
//!   working Docker daemon.
//!
//! Two rules hold across both, and they are enforced here rather than in each
//! backend so neither can forget:
//!
//! 1. **The script is fed on stdin, never interpolated into a command line.**
//!    Every `sh -c "…"` in an executor is a quoting bug waiting to happen: a
//!    single apostrophe in a user's Python snippet turns into a syntax error at
//!    best and a different command at worst. `sh -s` reads the program from
//!    stdin, so no amount of quoting in the payload can change the command.
//! 2. **The environment is built, never inherited.** This process holds the
//!    daemon's environment — `SENCLAW_*`, API keys, tokens. Handing that to
//!    sandboxed code would defeat the point of the sandbox, so the child gets
//!    an explicitly constructed environment and nothing else.

pub mod direct;
#[cfg(windows)]
pub mod direct_windows;
pub mod docker;

use std::collections::BTreeMap;

use serde::Serialize;

use crate::sandbox::db::Sandbox;

/// Output kept per stream. Beyond this the run is marked `truncated`; the cap
/// exists because a `yes` loop otherwise fills the caller's memory.
pub const MAX_OUTPUT: usize = 100_000;

/// Ceiling on any single run regardless of what the caller asks for.
pub const MAX_TIMEOUT_MS: u64 = 10 * 60 * 1000;

#[derive(Debug, Clone)]
pub struct ExecSpec {
    /// The program text, handed to `sh` on stdin.
    pub script: String,
    pub timeout_ms: u64,
    /// Extra environment for this run, on top of the sandbox's own.
    pub extra_env: BTreeMap<String, String>,
    /// Pre-resolved program and arguments. Read on Windows only.
    #[cfg_attr(not(windows), allow(dead_code))]
    ///
    /// Unix ignores this and feeds `script` to `sh -s`. Windows has no
    /// `/bin/sh`, so it needs the program resolved up front — which also tells
    /// the AppContainer which interpreter directory to grant.
    pub argv: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
    pub duration_ms: i64,
    /// What actually confined this run — `seatbelt`, `bubblewrap`, `container`
    /// or `degraded`. Measured at run time, never copied from the sandbox row.
    pub isolation: String,
}

/// Truncate to `MAX_OUTPUT` **bytes without splitting a UTF-8 character**.
///
/// `&s[..n]` panics when `n` lands inside a multi-byte character, and command
/// output is exactly where that happens: a Vietnamese error message or any
/// emoji in a stack trace is multi-byte, so a naive cap turns a long-output run
/// into a panic in the executor.
pub fn clamp(s: String) -> (String, bool) {
    if s.len() <= MAX_OUTPUT {
        return (s, false);
    }
    let mut end = MAX_OUTPUT;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str("\n…[truncated]");
    (out, true)
}

/// The environment a sandboxed process gets. Built from nothing.
///
/// `HOME` points at the sandbox's own directory: a lot of tooling writes there
/// by default (pip's cache, npm's config), and pointing it inside the sandbox
/// means those writes land in the one place the sandbox is allowed to write
/// instead of failing or escaping.
pub fn build_env(
    sb: &Sandbox,
    extra: &BTreeMap<String, String>,
    home: &str,
) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    env.insert(
        "PATH".into(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin:/usr/sbin:/sbin".into()),
    );
    env.insert("HOME".into(), home.to_string());
    env.insert("TMPDIR".into(), format!("{home}/.tmp"));
    env.insert("LANG".into(), "en_US.UTF-8".into());
    env.insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
    // Unbuffered stdout, so a run that times out still shows what it printed
    // before the kill instead of losing it in libc's buffer.
    env.insert("PYTHONUNBUFFERED".into(), "1".into());
    env.insert("SENCLAW_SANDBOX".into(), "1".into());

    if let Some(obj) = sb.env.as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                env.insert(k.clone(), s.to_string());
            }
        }
    }
    for (k, v) in extra {
        env.insert(k.clone(), v.clone());
    }
    env.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sb(env: serde_json::Value) -> Sandbox {
        Sandbox {
            id: "id".into(),
            name: "n".into(),
            backend: "direct".into(),
            image: None,
            workdir: "/w".into(),
            network: false,
            cpus: 1.0,
            memory_mb: 512,
            pids_limit: 256,
            timeout_ms: 1000,
            env,
            mounts: Vec::new(),
            fs_mode: crate::sandbox::fsmode::FsMode::Strict,
            trace_enabled: false,
            ports: Default::default(),
            status: "stopped".into(),
            container_id: None,
            last_error: None,
            created_at: 0,
            updated_at: 0,
            last_used_at: None,
        }
    }

    #[test]
    fn clamp_leaves_short_output_untouched() {
        let (s, t) = clamp("hello".into());
        assert_eq!(s, "hello");
        assert!(!t);
    }

    #[test]
    fn clamp_never_splits_a_multibyte_char() {
        // Every char is 3 bytes, so the cap lands mid-character unless the
        // boundary walk-back works. Without it this test panics.
        let big = "ế".repeat(MAX_OUTPUT);
        let (s, t) = clamp(big);
        assert!(t);
        assert!(s.ends_with("[truncated]"));
        // Round-tripping proves no partial character survived.
        assert_eq!(s, String::from_utf8(s.clone().into_bytes()).unwrap());
    }

    #[test]
    fn env_is_built_not_inherited() {
        // A secret in this process's environment must not reach the child.
        std::env::set_var("SENCLAW_TEST_SECRET_KEY", "super-secret");
        let env = build_env(&sb(json!({})), &BTreeMap::new(), "/w");
        assert!(
            !env.iter().any(|(k, _)| k == "SENCLAW_TEST_SECRET_KEY"),
            "daemon environment leaked into the sandbox"
        );
        assert!(env.iter().any(|(k, v)| k == "HOME" && v == "/w"));
        std::env::remove_var("SENCLAW_TEST_SECRET_KEY");
    }

    #[test]
    fn per_run_env_overrides_the_sandbox_env() {
        let mut extra = BTreeMap::new();
        extra.insert("MODE".to_string(), "run".to_string());
        let env = build_env(
            &sb(json!({ "MODE": "sandbox", "KEEP": "yes" })),
            &extra,
            "/w",
        );
        let get = |k: &str| env.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        assert_eq!(get("MODE").as_deref(), Some("run"));
        assert_eq!(get("KEEP").as_deref(), Some("yes"));
    }

    #[test]
    fn non_string_env_values_are_skipped_rather_than_stringified() {
        // `{"PORT": 8080}` must not become the literal "8080" silently — JSON
        // numbers here are a caller mistake, and inventing a value hides it.
        let env = build_env(&sb(json!({ "PORT": 8080 })), &BTreeMap::new(), "/w");
        assert!(!env.iter().any(|(k, _)| k == "PORT"));
    }
}
