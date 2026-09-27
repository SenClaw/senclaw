//! `direct` backend — run on this machine, confined by the OS.
//!
//! macOS uses Seatbelt (`sandbox-exec`), Linux uses bubblewrap (`bwrap`). Both
//! are configured for the same shape of boundary:
//!
//! * the whole filesystem is readable, but **writable only inside the
//!   sandbox's own directory** (plus its private temp);
//! * the user's credential stores are **not readable** — on Linux the home
//!   directory is replaced wholesale by an empty tmpfs, on macOS the known
//!   secret paths are denied;
//! * the network is off unless the sandbox was created with it on.
//!
//! Read access to the rest of the disk is deliberate rather than an oversight:
//! interpreters, their standard libraries and system frameworks live out there,
//! and a read-jail tight enough to exclude the user's documents also excludes
//! Python. The property this backend sells is therefore "cannot change your
//! machine, cannot read your keys" — not "cannot read anything". Callers who
//! need a full read boundary get told to use the docker backend, and the API
//! says so in the same words.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Instant;

use tokio::io::AsyncWriteExt;

use super::{build_env, clamp, ExecSpec, Outcome};
use crate::sandbox::caps::DirectKind;
use crate::sandbox::db::Sandbox;

/// Paths on macOS that hold credentials. Denied for reading, because "cannot
/// modify the machine" is worth much less if the code can still walk off with
/// the SSH keys.
const MAC_SECRET_SUBPATHS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".config/gcloud",
    ".kube",
    ".docker",
    ".netrc",
    "Library/Keychains",
    "Library/Application Support/Google/Chrome",
    "Library/Cookies",
    // The daemon's own state: DB, tokens, every other Space App's data.
    ".senclaw",
];

pub async fn exec(
    sb: &Sandbox,
    spec: &ExecSpec,
    kind: DirectKind,
    allowlist: &[String],
) -> Outcome {
    let start = Instant::now();
    let workdir = PathBuf::from(&sb.workdir);
    if let Err(e) = std::fs::create_dir_all(workdir.join(".tmp")) {
        return failed(format!("cannot create sandbox directory: {e}"), kind, start);
    }

    // Canonicalize before it reaches a sandbox profile. On macOS `/tmp` and the
    // home directory are symlinks; Seatbelt matches on the *resolved* path, so
    // an un-canonicalized `subpath` silently matches nothing and every write is
    // denied — the failure looks like "the sandbox is broken", not "the path
    // was wrong".
    let workdir = workdir.canonicalize().unwrap_or(workdir);
    let workdir_s = workdir.to_string_lossy().to_string();

    let mut cmd = match kind {
        DirectKind::Seatbelt => match seatbelt_command(sb, &workdir_s, allowlist) {
            Ok(c) => c,
            Err(e) => return failed(e, kind, start),
        },
        DirectKind::Bubblewrap => bwrap_command(sb, &workdir_s, allowlist),
        DirectKind::Degraded => {
            let mut c = tokio::process::Command::new("/bin/sh");
            c.arg("-s");
            c
        }
        // AppContainer is handled by `direct_windows`; reaching it here means
        // the caller dispatched to the wrong backend for this OS.
        DirectKind::AppContainer | DirectKind::Unsupported => {
            return failed(
                "direct execution is not supported on this OS — use the docker backend"
                    .into(),
                kind,
                start,
            )
        }
    };

    cmd.current_dir(&workdir)
        .env_clear()
        .envs(build_env(sb, &spec.extra_env, &workdir_s))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Put the child in its own process group. Without this, killing the timed
    // out `sh` leaves its children running: `python -c 'while True: pass'`
    // survives the timeout and burns a core forever. The whole group gets
    // signalled instead.
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return failed(format!("cannot start process: {e}"), kind, start),
    };
    let pid = child.id();

    // `setsid` above made the child its own group leader, so its pid *is* the
    // process group id. Registering it is what lets the monitor see this run,
    // and what makes a later kill provably ours rather than an arbitrary pid.
    // Deregistered on every exit path below via `Registered`.
    let _reg = pid.map(|p| Registered::new(&sb.id, p));

    // The script goes in on stdin — see the module docs on why it is never
    // interpolated into a command line.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(spec.script.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }

    let timeout = std::time::Duration::from_millis(spec.timeout_ms);
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => {
            let (stdout, t1) = clamp(String::from_utf8_lossy(&out.stdout).to_string());
            let (stderr, t2) = clamp(String::from_utf8_lossy(&out.stderr).to_string());
            Outcome {
                exit_code: out.status.code(),
                stdout,
                stderr,
                truncated: t1 || t2,
                timed_out: false,
                duration_ms: start.elapsed().as_millis() as i64,
                isolation: kind.as_str().to_string(),
            }
        }
        Ok(Err(e)) => failed(format!("error while waiting for the process: {e}"), kind, start),
        Err(_) => {
            kill_group(pid);
            Outcome {
                exit_code: None,
                stdout: String::new(),
                stderr: format!("Timed out after {} ms — the process was killed.", spec.timeout_ms),
                truncated: false,
                timed_out: true,
                duration_ms: start.elapsed().as_millis() as i64,
                isolation: kind.as_str().to_string(),
            }
        }
    }
}

/// RAII registration in the monitor.
///
/// A guard rather than a pair of calls because `exec` has five exit paths
/// (success, wait error, timeout, and two spawn failures); one of them would
/// eventually forget to deregister, and a stale group id makes the monitor
/// report a run that finished long ago — and, worse, makes `kill_pid` accept a
/// pid the OS has since reused for something else.
struct Registered {
    sandbox_id: String,
    pgid: u32,
}

impl Registered {
    fn new(sandbox_id: &str, pgid: u32) -> Self {
        crate::sandbox::monitor::register(sandbox_id, pgid);
        Registered {
            sandbox_id: sandbox_id.to_string(),
            pgid,
        }
    }
}

impl Drop for Registered {
    fn drop(&mut self) {
        crate::sandbox::monitor::unregister(&self.sandbox_id, self.pgid);
    }
}

/// Signal the child's whole process group, then the child itself as a fallback
/// for the case where `setsid` did not take effect.
fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

// ── macOS Seatbelt ──────────────────────────────────────────────────────────

/// Generate this sandbox's Seatbelt profile on disk and return its path.
///
/// Regenerated on every run. The profile lives inside the sandbox's own
/// directory, which the profile makes writable — so sandboxed code *can* edit
/// it, and that is harmless precisely because the next run overwrites it before
/// `sandbox-exec` reads it.
pub fn write_seatbelt_profile(sb: &Sandbox, allowlist: &[String]) -> Result<PathBuf, String> {
    let workdir = PathBuf::from(&sb.workdir);
    let workdir = workdir.canonicalize().unwrap_or(workdir);
    let workdir_s = workdir.to_string_lossy().to_string();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/nobody".into());

    // The symlinks are what make a mount visible at `<workdir>/<target>`; the
    // profile rules are what make it usable. Both are refreshed per run so a
    // mount added while the sandbox existed takes effect without recreating it.
    crate::sandbox::mounts::materialise_symlinks(&workdir, &sb.mounts)
        .map_err(|e| format!("cannot mount folder: {e}"))?;

    let path = workdir.join(".sandbox-profile.sb");
    std::fs::write(
        &path,
        seatbelt_profile(
            &workdir_s,
            &home,
            sb.network,
            &sb.mounts,
            sb.fs_mode,
            allowlist,
            &sb.ports,
        ),
    )
    .map_err(|e| format!("cannot write the sandbox profile: {e}"))?;
    Ok(path)
}

fn seatbelt_command(
    sb: &Sandbox,
    _workdir: &str,
    allowlist: &[String],
) -> Result<tokio::process::Command, String> {
    let path = write_seatbelt_profile(sb, allowlist)?;
    let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
    cmd.arg("-f").arg(&path).arg("/bin/sh").arg("-s");
    Ok(cmd)
}

/// Build the Seatbelt profile. Pure so it can be asserted on directly — a
/// mistake in here is silent (things merely stop being denied).
///
/// `mounts` are host folders the user explicitly bound in. macOS cannot remap a
/// path for a process, so a mount here is a symlink plus these rules: the real
/// source path becomes readable, and writable unless the mount is read-only.
/// Ancestor directories of `path` that lie inside one of `denied_roots`,
/// nearest first. `path` itself is never included — it is already granted.
///
/// These are exactly the directories that a granted path cannot be reached
/// through unless they are handed back.
fn denied_ancestors(path: &str, denied_roots: &[String]) -> Vec<String> {
    let inside = |p: &std::path::Path| {
        denied_roots
            .iter()
            .any(|root| p.starts_with(std::path::Path::new(root)))
    };
    let mut out = Vec::new();
    let mut cur = std::path::Path::new(path);
    while let Some(parent) = cur.parent() {
        if parent.as_os_str().is_empty() || parent == std::path::Path::new("/") {
            break;
        }
        if inside(parent) {
            out.push(parent.to_string_lossy().to_string());
        }
        cur = parent;
    }
    out
}

pub fn seatbelt_profile(
    workdir: &str,
    home: &str,
    network: bool,
    mounts: &[crate::sandbox::mounts::Mount],
    fs_mode: crate::sandbox::fsmode::FsMode,
    allowlist: &[String],
    ports: &crate::sandbox::ports::PortPolicy,
) -> String {
    let mut p = String::from(
        "(version 1)\n\
         ;; Start permissive, then carve away. A deny-by-default profile also \n\
         ;; denies the dozens of mach services an interpreter needs to start.\n\
         (allow default)\n\n",
    );

    p.push_str(";; ── writes: nothing outside the sandbox's own directory ──\n");
    p.push_str("(deny file-write*)\n");
    p.push_str("(allow file-write*\n");
    // `/private/var/folders` is deliberately NOT here. It is macOS's per-user
    // temp/cache root and holds other applications' containers and saved state,
    // so allowing it hands the sandbox a write path into real user data — an
    // end-to-end test caught exactly that escape. Sandboxed code does not need
    // it either: TMPDIR points inside the sandbox (see `build_env`).
    //
    // `/dev` stays, for /dev/null and /dev/urandom.
    for sub in [workdir, "/dev"] {
        p.push_str(&format!("  (subpath {})\n", sb_str(sub)));
    }
    // Writable mounts. Read-only ones are deliberately absent here — that is
    // the whole difference between the two.
    for m in mounts.iter().filter(|m| !m.read_only) {
        p.push_str(&format!("  (subpath {})\n", sb_str(&m.source)));
    }
    p.push_str(")\n\n");

    if fs_mode.jails_reads() {
        // Deny *everything*, then hand back the system trees an interpreter is
        // made of, the sandbox itself, its mounts, and (in allowlist mode) what
        // the user configured. Last matching rule wins, so every allow below
        // has to come after this deny.
        p.push_str(";; ── reads: jailed — only what was explicitly granted ──\n");
        p.push_str("(deny file-read*)\n");
        // The root directory itself must be readable or nothing can be resolved
        // through it; `literal` grants the directory, not its subtree.
        p.push_str("(allow file-read-metadata)\n");
        p.push_str("(allow file-read* (literal \"/\")\n");
        let sources: Vec<String> = mounts.iter().map(|m| m.source.clone()).collect();
        for root in crate::sandbox::fsmode::read_roots(fs_mode, workdir, &sources, allowlist) {
            p.push_str(&format!("  (subpath {})\n", sb_str(&root)));
        }
        p.push_str(")\n\n");
    } else {
        p.push_str(";; ── reads: open, minus the credential stores ──\n");
        p.push_str("(deny file-read*\n");
        for sub in MAC_SECRET_SUBPATHS {
            p.push_str(&format!(
                "  (subpath {})\n",
                sb_str(&format!("{}/{}", home.trim_end_matches('/'), sub))
            ));
        }
        p.push_str(")\n");
        // The sandbox's own directory is under ~/.senclaw, which the block above
        // just denied. Re-allow it — last matching rule wins in Seatbelt, so this
        // must come after the deny.
        p.push_str(&format!("(allow file-read* (subpath {}))\n", sb_str(workdir)));
        // Mounted folders, likewise after the deny: a mount under a denied path
        // (say `~/.config/something`) is an explicit user decision and must win.
        for m in mounts {
            p.push_str(&format!("(allow file-read* (subpath {}))\n", sb_str(&m.source)));
        }
        // Re-allowing a directory is not enough: opening a file inside it also
        // resolves every parent, and a parent that cannot be read at all breaks
        // that. Measured — a Space App whose SQLite database lives in
        // `~/.senclaw/space-app-data/<id>` died with
        // `SQLITE_CANTOPEN: unable to open database file` even though its own
        // directory was granted, because `~/.senclaw` above it was denied.
        //
        // So each granted path's ancestors *inside a denied tree* get metadata
        // back, and nothing else: names, sizes and timestamps of two directories
        // the app already knows the path of. Their contents — the daemon's
        // database, its token, every other app's data — stay dark.
        //
        // The jailed branch above needs no equivalent: it allows metadata outright.
        let denied_roots: Vec<String> = MAC_SECRET_SUBPATHS
            .iter()
            .map(|s| format!("{}/{}", home.trim_end_matches('/'), s))
            .collect();
        let granted = std::iter::once(workdir.to_string()).chain(mounts.iter().map(|m| m.source.clone()));
        let mut rescued: Vec<String> = Vec::new();
        for path in granted {
            for anc in denied_ancestors(&path, &denied_roots) {
                if !rescued.contains(&anc) {
                    rescued.push(anc);
                }
            }
        }
        for a in &rescued {
            p.push_str(&format!("(allow file-read-metadata (literal {}))\n", sb_str(a)));
        }
        p.push('\n');
    }

    // Network section: the coarse switch, narrowed by any opened ports.
    p.push_str(&crate::sandbox::ports::seatbelt_rules(ports, network));
    p
}

/// Quote a path as a Seatbelt string literal.
fn sb_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

// ── Linux bubblewrap ────────────────────────────────────────────────────────

fn bwrap_command(
    sb: &Sandbox,
    workdir: &str,
    allowlist: &[String],
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("bwrap");
    for a in bwrap_args(
        workdir,
        &std::env::var("HOME").unwrap_or_default(),
        sb.network,
        &sb.mounts,
        sb.fs_mode,
        allowlist,
        &sb.ports,
    ) {
        cmd.arg(a);
    }
    cmd
}

/// Build the bubblewrap argument list. Pure, for the same reason as the
/// Seatbelt profile: the failure mode of a wrong flag is "less isolation", not
/// an error.
pub fn bwrap_args(
    workdir: &str,
    home: &str,
    network: bool,
    mounts: &[crate::sandbox::mounts::Mount],
    fs_mode: crate::sandbox::fsmode::FsMode,
    allowlist: &[String],
    ports: &crate::sandbox::ports::PortPolicy,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--die-with-parent".into(),
        "--new-session".into(), // no terminal to inject keystrokes into
        "--unshare-pid".into(),
        "--unshare-ipc".into(),
        "--unshare-uts".into(),
        "--unshare-cgroup-try".into(),
    ];
    // A private network namespace has no route back to this machine, so a
    // sandbox that is meant to be reachable cannot have one. Reported by
    // `ports::note_for` rather than silently traded away.
    if !network && !ports.wants_network() {
        a.push("--unshare-net".into());
    }

    if fs_mode.jails_reads() {
        // Nothing is bound except what was granted. On Linux this is a真 read
        // jail: a path that is not bound simply does not exist in the mount
        // namespace, so there is no rule to get wrong — unlike Seatbelt, where
        // the whole disk is present and only rules keep it out of reach.
        //
        // `--ro-bind-try` throughout: the system-root list is deliberately
        // generous across distros (`/lib64` on glibc, `/nix/store` on NixOS),
        // and a missing entry must be skipped rather than abort the sandbox.
        for root in crate::sandbox::fsmode::SYSTEM_READ_ROOTS {
            a.push("--ro-bind-try".into());
            a.push(root.to_string());
            a.push(root.to_string());
        }
        if fs_mode == crate::sandbox::fsmode::FsMode::Allowlist {
            for p in allowlist.iter().filter(|p| !p.trim().is_empty()) {
                a.push("--ro-bind-try".into());
                a.push(p.clone());
                a.push(p.clone());
            }
        }
        a.extend([
            "--dev".into(),
            "/dev".to_string(),
            "--proc".into(),
            "/proc".to_string(),
            "--tmpfs".into(),
            "/tmp".to_string(),
        ]);
    } else {
        // Whole system read-only, then the pseudo-filesystems a process needs.
        a.extend([
            "--ro-bind".into(),
            "/".to_string(),
            "/".to_string(),
            "--dev".into(),
            "/dev".to_string(),
            "--proc".into(),
            "/proc".to_string(),
            "--tmpfs".into(),
            "/tmp".to_string(),
        ]);

        // An empty tmpfs over the home directory hides every dotfile credential
        // in one move — no list of secret paths to keep up to date, unlike
        // macOS. Not needed when jailing: an unbound home is already absent.
        if !home.is_empty() && home != "/" {
            a.push("--tmpfs".into());
            a.push(home.to_string());
        }
    }

    // …then the sandbox's own directory is bound back in, writable. This comes
    // after the tmpfs on purpose: bwrap applies mounts in order, so binding
    // first and covering with tmpfs second would hide the workdir.
    a.extend([
        "--bind".into(),
        workdir.to_string(),
        workdir.to_string(),
    ]);

    // Mounts go in after the workdir bind, so they land inside it. A read-only
    // mount uses `--ro-bind`, which the kernel enforces — not a flag the
    // sandboxed process can remount away, since it has no CAP_SYS_ADMIN.
    for m in mounts {
        a.push(if m.read_only { "--ro-bind".into() } else { "--bind".into() });
        a.push(m.source.clone());
        a.push(format!("{}/{}", workdir.trim_end_matches('/'), m.target));
    }

    a.extend([
        "--chdir".into(),
        workdir.to_string(),
        "--".into(),
        "/bin/sh".to_string(),
        "-s".to_string(),
    ]);
    a
}

fn failed(msg: String, kind: DirectKind, start: Instant) -> Outcome {
    Outcome {
        exit_code: None,
        stdout: String::new(),
        stderr: msg,
        truncated: false,
        timed_out: false,
        duration_ms: start.elapsed().as_millis() as i64,
        isolation: kind.as_str().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seatbelt_denies_writes_before_allowing_the_workdir() {
        let p = seatbelt_profile("/w/sbx", "/Users/u", false, &[], crate::sandbox::fsmode::FsMode::Open, &[], &Default::default());
        let deny = p.find("(deny file-write*)").expect("must deny writes");
        let allow = p.find("(allow file-write*").expect("must allow the workdir");
        assert!(deny < allow, "the allow must come after the deny to win");
        assert!(p.contains("\"/w/sbx\""));
    }

    #[test]
    fn a_granted_path_can_be_reached_through_its_denied_parents() {
        // The regression: a Space App whose SQLite database lives in
        // `~/.senclaw/space-app-data/<id>` panicked with
        // `SQLITE_CANTOPEN: unable to open database file` although that very
        // directory was granted — because `~/.senclaw` above it was denied, and
        // opening a file resolves every parent.
        let mounts = vec![crate::sandbox::mounts::Mount {
            source: "/Users/u/.senclaw/space-app-data/ba".into(),
            target: String::new(),
            read_only: false,
        }];
        let p = seatbelt_profile(
            "/Users/u/senclaw/workspace/space-apps/ba",
            "/Users/u",
            false,
            &mounts,
            crate::sandbox::fsmode::FsMode::Open,
            &[],
            &Default::default(),
        );
        assert!(
            p.contains(r#"(allow file-read-metadata (literal "/Users/u/.senclaw/space-app-data"))"#),
            "the parent of the granted folder must be stat-able:\n{p}"
        );
        assert!(
            p.contains(r#"(allow file-read-metadata (literal "/Users/u/.senclaw"))"#),
            "…and so must the denied root above it:\n{p}"
        );
        // Metadata only: the contents of the denied tree stay denied, so the
        // daemon's database and every other app's data are still unreadable.
        assert!(p.contains(r#"(subpath "/Users/u/.senclaw")"#), "the deny must remain");
        assert!(
            !p.contains(r#"(allow file-read* (subpath "/Users/u/.senclaw"))"#),
            "nothing may re-open the whole tree:\n{p}"
        );
        // The rescue is scoped to what was granted — a credential store nobody
        // mounted gains nothing.
        assert!(
            !p.contains(r#"file-read-metadata (literal "/Users/u/.ssh""#),
            "unrelated denied trees must not be touched:\n{p}"
        );
    }

    #[test]
    fn denied_ancestors_stops_at_the_denied_root() {
        let denied = vec!["/Users/u/.senclaw".to_string()];
        let got = denied_ancestors("/Users/u/.senclaw/space-app-data/ba", &denied);
        assert_eq!(got, vec!["/Users/u/.senclaw/space-app-data", "/Users/u/.senclaw"]);
        // A path outside every denied tree needs no rescue at all.
        assert!(denied_ancestors("/Users/u/Projects/app", &denied).is_empty());
        // Never `/` — and never the granted path itself.
        assert!(!got.iter().any(|p| p == "/" || p.ends_with("/ba")));
    }

    #[test]
    fn seatbelt_denies_the_credential_paths() {
        let p = seatbelt_profile("/w/sbx", "/Users/u", false, &[], crate::sandbox::fsmode::FsMode::Open, &[], &Default::default());
        assert!(p.contains("\"/Users/u/.ssh\""));
        assert!(p.contains("\"/Users/u/Library/Keychains\""));
        assert!(p.contains("\"/Users/u/.senclaw\""));
    }

    #[test]
    fn a_workdir_under_dot_senclaw_stays_readable() {
        // The real workdir lives under ~/.senclaw, which the secret-deny list
        // covers. Without the re-allow the sandbox cannot read its own files.
        let wd = "/Users/u/.senclaw/space-app-data/sandbox/workspaces/abc";
        let p = seatbelt_profile(wd, "/Users/u", false, &[], crate::sandbox::fsmode::FsMode::Open, &[], &Default::default());
        let deny = p.find("(deny file-read*").unwrap();
        let reallow = p.find(&format!("(allow file-read* (subpath \"{wd}\"))")).unwrap();
        assert!(deny < reallow, "re-allow must come after the secret deny");
    }

    #[test]
    fn network_rule_follows_the_sandbox_setting() {
        assert!(seatbelt_profile("/w", "/h", false, &[], crate::sandbox::fsmode::FsMode::Open, &[], &Default::default()).contains("(deny network*)"));
        assert!(!seatbelt_profile("/w", "/h", true, &[], crate::sandbox::fsmode::FsMode::Open, &[], &Default::default()).contains("(deny network*)"));
    }

    #[test]
    fn seatbelt_paths_are_quoted_and_escaped() {
        let p = seatbelt_profile("/w/a\"b", "/h", false, &[], crate::sandbox::fsmode::FsMode::Open, &[], &Default::default());
        assert!(p.contains("\"/w/a\\\"b\""), "a quote in a path must be escaped");
    }

    #[test]
    fn bwrap_covers_home_before_binding_the_workdir_back() {
        let args = bwrap_args("/home/u/.senclaw/ws/a", "/home/u", false, &[], crate::sandbox::fsmode::FsMode::Open, &[], &Default::default());
        let joined = args.join(" ");
        let tmpfs_home = joined.find("--tmpfs /home/u ").expect("home must be covered");
        let bind = joined.find("--bind /home/u/.senclaw/ws/a").expect("workdir bound");
        assert!(tmpfs_home < bind, "binding before the tmpfs would hide the workdir");
    }

    #[test]
    fn bwrap_unshares_the_network_only_when_disabled() {
        assert!(bwrap_args("/w", "/h", false, &[], crate::sandbox::fsmode::FsMode::Open, &[], &Default::default()).iter().any(|a| a == "--unshare-net"));
        assert!(!bwrap_args("/w", "/h", true, &[], crate::sandbox::fsmode::FsMode::Open, &[], &Default::default()).iter().any(|a| a == "--unshare-net"));
    }

    #[test]
    fn bwrap_reads_the_script_from_stdin_not_from_argv() {
        let args = bwrap_args("/w", "/h", false, &[], crate::sandbox::fsmode::FsMode::Open, &[], &Default::default());
        assert_eq!(args.last().map(String::as_str), Some("-s"));
        assert!(!args.iter().any(|a| a == "-c"), "never build a `sh -c` command line");
    }

    #[test]
    fn bwrap_never_tmpfses_a_root_home() {
        // HOME=/ (or unset) must not turn into `--tmpfs /`, which would hide
        // the entire filesystem including the interpreter.
        let args = bwrap_args("/w", "/", false, &[], crate::sandbox::fsmode::FsMode::Open, &[], &Default::default()).join(" ");
        assert!(!args.contains("--tmpfs / "));
        let args = bwrap_args("/w", "", false, &[], crate::sandbox::fsmode::FsMode::Open, &[], &Default::default()).join(" ");
        assert!(!args.contains("--tmpfs  "));
    }
}
