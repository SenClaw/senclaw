//! Optional activity tracing, for testing and inspection.
//!
//! Answers "what did this code actually touch?" — which files it read and
//! wrote, which processes it started, which addresses it contacted.
//!
//! # Why this traces at the language level, not the syscall level
//!
//! The obvious implementation is a syscall tracer. It is not available:
//!
//! * **macOS** — `dtrace`, `dtruss` and `ktrace` all refuse to run while System
//!   Integrity Protection is on, which is its default state. Measured on the
//!   development machine: *"DTrace requires additional privileges"*, *"ktrace
//!   must be run as root"*. The Endpoint Security framework needs an
//!   Apple-granted entitlement plus root. None of that is available to a Space
//!   App, and asking a user to disable SIP to see a file-access list is a bad
//!   trade.
//! * **Linux** — `strace` works and is genuinely better, but it is Linux-only
//!   and not always installed.
//!
//! So the primary mechanism is an **in-process hook**, injected into the
//! sandbox before the workload starts:
//!
//! | Runtime | Mechanism |
//! |---|---|
//! | Python | `sys.addaudithook` (PEP 578) via `sitecustomize.py` on `PYTHONPATH` |
//! | Node | `--require` preload patching `fs`, `child_process`, `net`, `dns` |
//! | anything else | before/after diff of the sandbox directory (writes only) |
//!
//! `sitecustomize` and `NODE_OPTIONS` are inherited by child processes, so a
//! script that shells out to another script is still covered.
//!
//! # What this is not
//!
//! **It is not a security audit.** The hook runs inside the sandbox, and the
//! event log is a file in the sandbox's own directory — code that wants to hide
//! can remove the hook, or rewrite the log, or simply call `os.write` on a raw
//! descriptor and never touch the traced APIs. It is an honest picture of what
//! ordinary code does, which is what testing needs; it is not evidence about
//! code that is actively trying to deceive. The boundary that *does* hold
//! against hostile code is the sandbox itself (`fsmode`, network, write jail),
//! and that one is enforced by the kernel.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Relative directory inside a sandbox holding the shims and the event log.
pub const DIR: &str = ".trace";
pub const LOG: &str = ".trace/events.ndjson";

/// Cap on events kept for one run. A `for` loop over ten thousand files is a
/// legitimate program and an unreadable timeline; the count is reported so the
/// truncation is never silent.
pub const MAX_EVENTS: usize = 5_000;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub ts_ms: i64,
    pub pid: i64,
    /// `python`, `node`, or `diff`.
    pub source: String,
    /// `file.read` | `file.write` | `proc.spawn` | `net.connect` | `net.dns`
    pub kind: String,
    pub target: String,
    pub detail: String,
}

/// Prefixes whose reads are the runtime loading itself, not the program doing
/// anything the user asked about.
///
/// Without this the timeline for `print(1)` is four hundred entries of CPython
/// opening its own standard library, and the one line that matters is lost in
/// it. Writes are never filtered — a write into a system directory is exactly
/// the kind of thing someone turns tracing on to find.
const NOISE_READ_PREFIXES: &[&str] = &[
    "/usr/lib",
    "/usr/local/lib",
    "/usr/share",
    "/System/",
    "/Library/",
    "/opt/homebrew/Cellar",
    "/opt/homebrew/lib",
    "/opt/homebrew/opt",
    "/nix/store",
    "/lib/",
    "/lib64/",
    "/etc/ld.so",
    "/proc/",
];

fn is_noise(kind: &str, target: &str) -> bool {
    if target.starts_with(&format!("{DIR}/")) || target.contains("/.trace/") {
        return true; // the tracer observing itself
    }
    // The app's own bookkeeping inside the sandbox: the snippet file it wrote
    // for this run, and the Seatbelt profile it generated to confine it.
    // Reporting them as things the program did is simply false.
    if target.contains("/.runs/")
        || target.starts_with(".runs/")
        || target.ends_with(".sandbox-profile.sb")
    {
        return true;
    }
    if kind != "file.read" {
        return false;
    }
    // `open(4)` on a file descriptor — a pipe from `subprocess`, not a path.
    //
    // Matched on "is entirely digits", NOT on "is not absolute": a relative
    // path is the normal way sandboxed code opens its own files
    // (`open('ket-qua.txt')`), and rejecting everything relative silently
    // dropped exactly the reads the user cares about.
    if !target.is_empty() && target.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    NOISE_READ_PREFIXES.iter().any(|p| target.starts_with(p))
        || target.ends_with(".pyc")
        || target.contains("/site-packages/")
        || target.contains("/dist-packages/")
        || target.contains("/node_modules/")
}

/// One run's worth of tracing: where the log is and how far it had been read
/// before the run started.
pub struct Session {
    log_path: PathBuf,
    start_offset: u64,
    before: Vec<(String, u64, i64)>,
    workdir: PathBuf,
}

impl Session {
    /// Write the shims and remember where the log currently ends.
    ///
    /// The offset matters because the log is append-only across runs: without
    /// it, every run would re-report every earlier run's events.
    pub fn begin(workdir: &Path) -> std::io::Result<Session> {
        let dir = workdir.join(DIR);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("sitecustomize.py"), PYTHON_SHIM)?;
        std::fs::write(dir.join("node-hook.cjs"), NODE_SHIM)?;

        let log_path = workdir.join(LOG);
        let start_offset = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);

        Ok(Session {
            before: snapshot(workdir),
            log_path,
            start_offset,
            workdir: workdir.to_path_buf(),
        })
    }

    /// Collect what happened, from the hook log plus a directory diff.
    pub fn finish(self) -> (Vec<Event>, bool) {
        let mut events = read_log(&self.log_path, self.start_offset);

        // The diff catches writes the hooks cannot see: a compiled binary, a
        // shell redirect, anything not Python or Node.
        let after = snapshot(&self.workdir);
        events.extend(diff(&self.before, &after));

        events.sort_by_key(|e| e.ts_ms);
        let truncated = events.len() > MAX_EVENTS;
        events.truncate(MAX_EVENTS);
        (events, truncated)
    }
}

fn read_log(path: &Path, from: u64) -> Vec<Event> {
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    use std::io::Seek;
    if f.seek(std::io::SeekFrom::Start(from)).is_err() {
        return Vec::new();
    }
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() {
        return Vec::new();
    }
    parse_ndjson(&buf)
}

/// Parse the shim's output. Malformed lines are skipped rather than failing the
/// run — a half-written final line is normal when a process is killed at its
/// timeout, and losing the whole timeline over it would be worse than losing
/// one event.
pub fn parse_ndjson(s: &str) -> Vec<Event> {
    let mut out = Vec::new();
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let kind = v["kind"].as_str().unwrap_or("").to_string();
        let target = v["target"].as_str().unwrap_or("").to_string();
        if kind.is_empty() || is_noise(&kind, &target) {
            continue;
        }
        out.push(Event {
            ts_ms: v["ts"].as_i64().unwrap_or(0),
            pid: v["pid"].as_i64().unwrap_or(0),
            source: v["src"].as_str().unwrap_or("?").to_string(),
            kind,
            target,
            detail: v["detail"].as_str().unwrap_or("").to_string(),
        });
    }
    out
}

/// (relative path, size, mtime ms) for every file under the sandbox, skipping
/// the tracer's own directory.
fn snapshot(root: &Path) -> Vec<(String, u64, i64)> {
    let mut out = Vec::new();
    walk(root, root, &mut out, 0);
    out.sort();
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, u64, i64)>, depth: usize) {
    // A mounted folder can be arbitrarily deep, and a symlink loop inside one
    // would otherwise hang the run at its own bookkeeping.
    if depth > 12 || out.len() > 20_000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let Ok(md) = e.metadata() else { continue };
        if md.is_symlink() {
            continue;
        }
        if md.is_dir() {
            if p.file_name().map(|n| n == DIR).unwrap_or(false) {
                continue;
            }
            walk(root, &p, out, depth + 1);
        } else {
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .to_string();
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            out.push((rel, md.len(), mtime));
        }
    }
}

/// Files created or changed between two snapshots.
pub fn diff(before: &[(String, u64, i64)], after: &[(String, u64, i64)]) -> Vec<Event> {
    let now = chrono::Utc::now().timestamp_millis();
    let mut out = Vec::new();
    for (path, size, mtime) in after {
        // Same rule as the hook path: the app's own bookkeeping is not
        // something the program did.
        if is_noise("file.write", path) {
            continue;
        }
        match before.iter().find(|(p, _, _)| p == path) {
            None => out.push(Event {
                ts_ms: *mtime.max(&0).min(&now),
                pid: 0,
                source: "diff".into(),
                kind: "file.write".into(),
                target: path.clone(),
                detail: format!("created, {size} bytes"),
            }),
            Some((_, s0, m0)) if s0 != size || m0 != mtime => out.push(Event {
                ts_ms: *mtime.max(&0).min(&now),
                pid: 0,
                source: "diff".into(),
                kind: "file.write".into(),
                target: path.clone(),
                detail: format!("modified, {s0} → {size} bytes"),
            }),
            _ => {}
        }
    }
    out
}

/// Environment that turns the hooks on inside the sandbox.
///
/// `home` is the sandbox root **as the workload sees it** — the host path for
/// `direct`, `/work` for docker — because these values are read by the process
/// inside, not by this one.
pub fn env(home: &str) -> Vec<(String, String)> {
    vec![
        ("SENCLAW_TRACE_FILE".into(), format!("{home}/{LOG}")),
        // Prepended, so the sandbox's own modules still win over the shim dir.
        ("PYTHONPATH".into(), format!("{home}/{DIR}")),
        (
            "NODE_OPTIONS".into(),
            format!("--require {home}/{DIR}/node-hook.cjs"),
        ),
    ]
}

// ── the shims ───────────────────────────────────────────────────────────────

/// Installed as `sitecustomize.py`, which CPython imports automatically at
/// interpreter start — so it covers `python script.py`, `python -c`, and any
/// python a shell command spawns, without the caller opting in each time.
///
/// The log is written with `os.write` on a descriptor opened once. That detail
/// is load-bearing: `open()` raises an audit event, so logging through it would
/// make the hook trigger itself, forever.
pub const PYTHON_SHIM: &str = r#"# SenClaw sandbox trace hook (PEP 578). Safe to delete; it only observes.
import sys, os, time

_fd = None
_busy = False
try:
    _p = os.environ.get("SENCLAW_TRACE_FILE")
    if _p:
        _fd = os.open(_p, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
except Exception:
    _fd = None


def _esc(s):
    try:
        s = str(s)
    except Exception:
        return "?"
    return s.replace("\\", "\\\\").replace('"', '\\"').replace("\n", " ").replace("\r", " ")[:400]


def _emit(kind, target, detail=""):
    global _busy
    if _fd is None or _busy:
        return
    _busy = True
    try:
        line = '{"ts":%d,"pid":%d,"src":"python","kind":"%s","target":"%s","detail":"%s"}\n' % (
            int(time.time() * 1000), os.getpid(), kind, _esc(target), _esc(detail))
        os.write(_fd, line.encode("utf-8", "replace"))
    except Exception:
        pass
    finally:
        _busy = False


def _hook(name, args):
    try:
        if name == "open":
            path = args[0]
            if not isinstance(path, (str, bytes)):
                return  # a file descriptor, not a path
            mode = str(args[1] or "r")
            kind = "file.write" if any(c in mode for c in "wxa+") else "file.read"
            _emit(kind, path, mode)
        elif name == "subprocess.Popen":
            _emit("proc.spawn", args[0], args[1])
        elif name in ("os.system", "os.exec", "os.posix_spawn", "os.spawn"):
            _emit("proc.spawn", args[0] if args else "?", name)
        elif name == "socket.connect":
            _emit("net.connect", args[1] if len(args) > 1 else "?", "")
        elif name == "socket.getaddrinfo":
            _emit("net.dns", args[0], args[1] if len(args) > 1 else "")
    except Exception:
        pass


try:
    sys.addaudithook(_hook)
except Exception:
    pass
"#;

/// Loaded with `--require` via `NODE_OPTIONS`, which node passes to child
/// processes too.
pub const NODE_SHIM: &str = r#"// SenClaw sandbox trace hook. Safe to delete; it only observes.
'use strict';
const fs = require('fs');
const path = process.env.SENCLAW_TRACE_FILE;
let fd = null;
try { if (path) fd = fs.openSync(path, 'a', 0o600); } catch (e) { fd = null; }

const esc = (s) => String(s === undefined ? '' : s)
  .replace(/\\/g, '\\\\').replace(/"/g, '\\"').replace(/[\n\r]/g, ' ').slice(0, 400);

function emit(kind, target, detail) {
  if (fd === null) return;
  try {
    // writeSync on the raw fd, so patched fs methods below cannot re-enter.
    fs.writeSync(fd, `{"ts":${Date.now()},"pid":${process.pid},"src":"node",` +
      `"kind":"${kind}","target":"${esc(target)}","detail":"${esc(detail)}"}\n`);
  } catch (e) { /* tracing must never break the program */ }
}

const READ = ['readFile', 'readFileSync', 'createReadStream'];
const WRITE = ['writeFile', 'writeFileSync', 'appendFile', 'appendFileSync', 'createWriteStream'];
for (const [names, kind] of [[READ, 'file.read'], [WRITE, 'file.write']]) {
  for (const n of names) {
    const orig = fs[n];
    if (typeof orig !== 'function') continue;
    fs[n] = function (p, ...rest) { emit(kind, p, n); return orig.call(this, p, ...rest); };
  }
}
const openOrig = fs.openSync;
fs.openSync = function (p, flags, ...rest) {
  const f = String(flags || 'r');
  emit(/[wax+]/.test(f) ? 'file.write' : 'file.read', p, f);
  return openOrig.call(this, p, flags, ...rest);
};

const cp = require('child_process');
for (const n of ['spawn', 'spawnSync', 'exec', 'execSync', 'execFile', 'fork']) {
  const orig = cp[n];
  if (typeof orig !== 'function') continue;
  cp[n] = function (cmd, ...rest) {
    emit('proc.spawn', cmd, n);
    return orig.call(this, cmd, ...rest);
  };
}

const net = require('net');
const connectOrig = net.Socket.prototype.connect;
net.Socket.prototype.connect = function (...a) {
  try {
    const o = a[0];
    if (o && typeof o === 'object') emit('net.connect', `${o.host || o.path || '?'}:${o.port || ''}`, '');
    else emit('net.connect', `${a[1] || '?'}:${a[0] || ''}`, '');
  } catch (e) { /* ignore */ }
  return connectOrig.apply(this, a);
};

const dns = require('dns');
for (const n of ['lookup', 'resolve', 'resolve4', 'resolve6']) {
  const orig = dns[n];
  if (typeof orig !== 'function') continue;
  dns[n] = function (host, ...rest) { emit('net.dns', host, n); return orig.call(this, host, ...rest); };
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpreter_self_reads_are_filtered_but_writes_never_are() {
        assert!(is_noise("file.read", "/usr/lib/python3/os.py"));
        assert!(is_noise("file.read", "/opt/homebrew/Cellar/python@3.14/x.py"));
        assert!(is_noise("file.read", "/System/Library/Frameworks/x"));
        assert!(is_noise("file.read", "/app/node_modules/lodash/index.js"));
        // A write anywhere is always interesting, including system paths — that
        // is precisely what someone turns tracing on to catch.
        assert!(!is_noise("file.write", "/usr/lib/evil.so"));
        assert!(!is_noise("file.read", "/Users/u/du-an/data.csv"));
    }

    #[test]
    fn file_descriptor_targets_are_not_reported_as_paths() {
        // `open(4)` from a subprocess pipe — a number, not a file.
        assert!(is_noise("file.read", "4"));
        assert!(is_noise("file.read", "13"));
    }

    #[test]
    fn relative_paths_are_kept() {
        // Sandboxed code opens its own files relatively — `open('ket-qua.txt')`.
        // An earlier filter dropped everything not starting with `/`, which
        // threw away precisely the reads worth reporting.
        assert!(!is_noise("file.read", "ket-qua.txt"));
        assert!(!is_noise("file.read", "du-lieu/vao.csv"));
        assert!(!is_noise("file.read", "9-bang-ket-qua.txt"));
    }

    #[test]
    fn the_tracer_never_reports_itself() {
        assert!(is_noise("file.write", "/w/sbx/.trace/events.ndjson"));
        assert!(is_noise("file.read", ".trace/sitecustomize.py"));
    }

    #[test]
    fn ndjson_parses_the_shapes_the_shim_emits() {
        let s = r#"
{"ts":1,"pid":9,"src":"python","kind":"file.write","target":"/w/a.txt","detail":"w"}
{"ts":2,"pid":9,"src":"python","kind":"proc.spawn","target":"/bin/echo","detail":"['/bin/echo']"}
{"ts":3,"pid":9,"src":"python","kind":"net.connect","target":"('1.1.1.1', 53)","detail":""}
{"ts":4,"pid":9,"src":"python","kind":"net.dns","target":"example.com","detail":"80"}
"#;
        let ev = parse_ndjson(s);
        assert_eq!(ev.len(), 4);
        assert_eq!(ev[0].kind, "file.write");
        assert_eq!(ev[2].target, "('1.1.1.1', 53)");
        assert_eq!(ev[3].target, "example.com");
    }

    #[test]
    fn a_truncated_last_line_does_not_lose_the_earlier_events() {
        // Exactly what a killed-at-timeout process leaves behind.
        let s = "{\"ts\":1,\"pid\":9,\"src\":\"python\",\"kind\":\"file.read\",\"target\":\"/w/a\",\"detail\":\"r\"}\n{\"ts\":2,\"pid\":9,\"kin";
        assert_eq!(parse_ndjson(s).len(), 1);
    }

    #[test]
    fn garbage_lines_are_skipped_not_fatal() {
        assert!(parse_ndjson("not json\n\n{}\n").is_empty());
    }

    #[test]
    fn diff_reports_new_and_changed_files_only() {
        let before = vec![("a.txt".into(), 1, 100), ("keep.txt".into(), 5, 100)];
        let after = vec![
            ("a.txt".to_string(), 9u64, 200i64),
            ("keep.txt".to_string(), 5, 100),
            ("new.txt".to_string(), 3, 200),
        ];
        let ev = diff(&before, &after);
        let targets: Vec<_> = ev.iter().map(|e| e.target.as_str()).collect();
        assert!(targets.contains(&"a.txt"));
        assert!(targets.contains(&"new.txt"));
        assert!(!targets.contains(&"keep.txt"), "an untouched file is not an event");
        assert!(ev.iter().all(|e| e.kind == "file.write" && e.source == "diff"));
    }

    #[test]
    fn diff_ignores_the_apps_own_bookkeeping() {
        // The snippet file and the generated Seatbelt profile are written by
        // this app, not by the traced program.
        let after = vec![
            (".runs/abc.py".to_string(), 10u64, 5i64),
            (".sandbox-profile.sb".to_string(), 1103, 5),
        ];
        assert!(diff(&[], &after).is_empty());
    }

    #[test]
    fn the_generated_profile_and_snippet_are_never_reported() {
        assert!(is_noise("file.write", ".sandbox-profile.sb"));
        assert!(is_noise("file.write", "/w/sbx/.sandbox-profile.sb"));
        assert!(is_noise("file.read", "/w/sbx/.runs/abc-123.py"));
        assert!(is_noise("file.read", ".runs/abc-123.py"));
        // …but a file the user genuinely named similarly is still reported.
        assert!(!is_noise("file.write", "runs/ket-qua.txt"));
    }

    #[test]
    fn env_points_at_paths_inside_the_sandbox() {
        let e = env("/work");
        let get = |k: &str| e.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(get("SENCLAW_TRACE_FILE"), "/work/.trace/events.ndjson");
        assert_eq!(get("PYTHONPATH"), "/work/.trace");
        assert!(get("NODE_OPTIONS").contains("/work/.trace/node-hook.cjs"));
    }

    #[test]
    fn begin_writes_both_shims_and_starts_at_the_end_of_an_existing_log() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(DIR)).unwrap();
        std::fs::write(d.path().join(LOG), "old line\n").unwrap();

        let s = Session::begin(d.path()).unwrap();
        assert!(d.path().join(".trace/sitecustomize.py").exists());
        assert!(d.path().join(".trace/node-hook.cjs").exists());
        assert_eq!(s.start_offset, 9, "must resume after the existing content");
    }

    #[test]
    fn only_events_appended_after_begin_are_collected() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(DIR)).unwrap();
        let log = d.path().join(LOG);
        std::fs::write(
            &log,
            "{\"ts\":1,\"pid\":1,\"src\":\"python\",\"kind\":\"file.read\",\"target\":\"/old\",\"detail\":\"\"}\n",
        )
        .unwrap();

        let s = Session::begin(d.path()).unwrap();
        // A later run appends.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(b"{\"ts\":2,\"pid\":1,\"src\":\"python\",\"kind\":\"file.read\",\"target\":\"/new\",\"detail\":\"\"}\n")
            .unwrap();

        let (ev, _) = s.finish();
        let targets: Vec<_> = ev.iter().map(|e| e.target.as_str()).collect();
        assert!(targets.contains(&"/new"));
        assert!(
            !targets.contains(&"/old"),
            "an earlier run's events must not be re-reported"
        );
    }

    #[test]
    fn the_python_shim_logs_through_a_raw_descriptor() {
        // Logging via open() would make the audit hook observe itself and
        // recurse. The guard is that the shim writes with os.write.
        assert!(PYTHON_SHIM.contains("os.write(_fd"));
        assert!(PYTHON_SHIM.contains("sys.addaudithook"));
        assert!(
            PYTHON_SHIM.contains("_busy"),
            "a re-entrancy guard is required"
        );
    }

    #[test]
    fn the_node_shim_patches_all_four_families() {
        for needle in ["child_process", "net.Socket.prototype.connect", "dns", "readFileSync"] {
            assert!(NODE_SHIM.contains(needle), "node shim is missing {needle}");
        }
    }
}
