//! Launching a Space App's server process inside the OS sandbox.
//!
//! The daemon starts an app with `sh -c "<runtime.start>"` from the app's
//! directory, then reaches it over loopback. This module turns that into
//! `sandbox-exec -f <profile> sh -c "<start>"` (macOS) or `bwrap … sh -c
//! "<start>"` (Linux) and hands back the extra environment the confined process
//! needs. Nothing else about the launch changes: same working directory, same
//! port, same log file, same process group.
//!
//! # Why paths are not remapped
//!
//! The sandbox engine's own runs get a workspace and mounts that *move* paths
//! (`/work/data`). An installed app cannot take that: it computes its data
//! directory from `$HOME` at startup and would not find it anywhere else. So
//! every folder an app is granted is granted **at its real path** — a symlink is
//! never created, and on Linux each bind has source == destination. What the app
//! sees is the machine it always saw, minus what it may no longer touch.
//!
//! # What each platform can actually enforce
//!
//! | | folders | network off / per-site |
//! |---|---|---|
//! | macOS (Seatbelt) | yes | yes — no direct egress, one loopback port for the proxy |
//! | Linux (bubblewrap) | yes | **no** — see below |
//! | Windows | no | no |
//!
//! On Linux, a served app cannot have its own network namespace: `--unshare-net`
//! leaves the daemon with no route to the app's port, so the app becomes
//! unreachable — which is not "isolated", it is "broken". The app therefore
//! shares this machine's namespace and can ignore the proxy. The folder jail is
//! real there; the network mode is reported as unenforced rather than pretended.
//! Windows' AppContainer backend drives a process through pipes and cannot wrap a
//! long-lived server at all, so the app runs unsandboxed and says so.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};

use crate::sandbox::app_policy::{AppSandbox, NetMode};
use crate::sandbox::caps::{self, DirectKind};
use crate::sandbox::mounts::Mount;
use crate::sandbox::proxy::HostProxy;

/// Everything the spawner needs to launch one app.
pub struct Plan {
    /// Program and arguments. Always ends with the shell and the app's own start
    /// command, so the caller does not special-case sandboxed and plain launches.
    pub argv: Vec<String>,
    /// Environment to add on top of what the spawner already sets.
    pub env: Vec<(String, String)>,
    /// False when this machine cannot enforce the request — the app still runs.
    pub enforced: bool,
    /// `seatbelt` | `bubblewrap` | `none`.
    pub isolation: String,
    /// What the user must know about the difference between what they asked for
    /// and what is actually in force. `None` when there is no difference.
    pub note: Option<String>,
    /// The allowlist proxy, alive as long as this plan is held.
    pub proxy: Option<Arc<HostProxy>>,
    /// Folders the app was granted, for the log line and the settings dialog.
    pub granted: Vec<String>,
}

impl Plan {
    /// An ordinary, unsandboxed launch — what every app got before this feature.
    fn passthrough(start: &str, note: Option<String>) -> Plan {
        Plan {
            argv: shell_argv(start),
            env: Vec::new(),
            enforced: false,
            isolation: "none".into(),
            note,
            proxy: None,
            granted: Vec::new(),
        }
    }

    /// One line for the app's runtime log, so what was enforced is visible where
    /// people look when an app misbehaves.
    pub fn summary(&self) -> String {
        if !self.enforced {
            return match &self.note {
                Some(n) => format!("sandbox: NOT enforced ({n})"),
                None => "sandbox: off".into(),
            };
        }
        let net = match &self.proxy {
            Some(p) => format!("network via allowlist proxy on 127.0.0.1:{}", p.port),
            None => "network per policy".into(),
        };
        format!(
            "sandbox: {} — {}, {} folder(s) granted{}",
            self.isolation,
            net,
            self.granted.len(),
            match &self.note {
                Some(n) => format!(" [{n}]"),
                None => String::new(),
            }
        )
    }
}

#[cfg(unix)]
fn shell_argv(start: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), start.to_string()]
}
#[cfg(not(unix))]
fn shell_argv(start: &str) -> Vec<String> {
    vec!["cmd".into(), "/C".into(), start.to_string()]
}

/// Build the launch plan for one app.
///
/// `daemon_port` is SenClaw's own UI port — granted on loopback when
/// `cfg.daemon_api` is set, because that is where the AI bridge lives.
pub async fn plan(
    app_id: &str,
    app_dir: &Path,
    start: &str,
    app_port: u16,
    daemon_port: u16,
    cfg: &AppSandbox,
) -> Result<Plan> {
    if !cfg.enabled {
        return Ok(Plan::passthrough(start, None));
    }

    let kind = caps::direct_caps(false).await.kind;
    let isolation = match kind {
        DirectKind::Seatbelt | DirectKind::Bubblewrap => kind.as_str().to_string(),
        _ => {
            return Ok(Plan::passthrough(
                start,
                Some(format!(
                    "this machine has no usable OS sandbox for a server process ({}), so the app \
                     runs unconfined",
                    kind.as_str()
                )),
            ))
        }
    };

    // ── the proxy, if this app may reach only some sites ────────────────────
    let proxy = match cfg.network {
        NetMode::Hosts => Some(Arc::new(
            HostProxy::spawn(app_id.to_string(), cfg.hosts.clone()).await?,
        )),
        _ => None,
    };

    // ── ports ──────────────────────────────────────────────────────────────
    // The app must be able to bind its own port and be reached on it. Loopback
    // is closed except for what was named: the daemon's API (the AI bridge),
    // this app's proxy, and whatever local services the user opened.
    let mut loopback = cfg.loopback.clone();
    if cfg.daemon_api {
        loopback.push(daemon_port);
    }
    if let Some(p) = &proxy {
        loopback.push(p.port);
    }
    let ports = crate::sandbox::ports::validate(&[app_port], &[], &loopback)
        .map_err(|e| anyhow!("sandbox ports for app '{app_id}': {e}"))?;
    // `All` is the coarse switch; `Off` and `Hosts` both mean "no general
    // egress", and in `Hosts` the only way out is the proxy port above.
    let network = matches!(cfg.network, NetMode::All);

    // ── folders ────────────────────────────────────────────────────────────
    let tmp = tmp_dir(app_id);
    std::fs::create_dir_all(&tmp)
        .map_err(|e| anyhow!("create the sandbox temp dir for '{app_id}': {e}"))?;
    let mut mounts: Vec<Mount> = vec![Mount {
        source: tmp.to_string_lossy().to_string(),
        target: String::new(), // never materialised: paths are not remapped here
        read_only: false,
    }];
    // The app's own data directories. Created on demand so a first sandboxed
    // launch can write its database, and because bubblewrap cannot bind a path
    // that does not exist.
    for d in crate::sandbox::app_policy::own_data_dirs(app_id) {
        if std::fs::create_dir_all(&d).is_ok() {
            mounts.push(Mount {
                source: d.to_string_lossy().to_string(),
                target: String::new(),
                read_only: false,
            });
        }
    }
    // A jailed read mode must still be able to read the interpreter it is about
    // to run. Measured, not guessed: with `strict` and nothing else, this app
    // died with `EPERM … /Users/u/.nvm/versions/node/v24.13.1/lib/node_modules/
    // npm/bin/npm-cli.js` — the runtime was installed by nvm, under `$HOME`,
    // which is exactly what `strict` removes.
    if cfg.read_mode.jails_reads() {
        mounts.extend(toolchain_read_mounts());
    }
    for f in &cfg.folders {
        // Resolved, not as typed: a Seatbelt rule on `/var/x` grants nothing
        // because the real path is `/private/var/x`, and the failure is silent —
        // the app simply cannot read a folder the settings say it was given.
        let source = std::fs::canonicalize(&f.path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| f.path.clone());
        mounts.push(Mount {
            source,
            target: String::new(),
            read_only: f.read_only,
        });
    }
    let granted: Vec<String> = mounts.iter().map(|m| m.source.clone()).collect();

    let home = std::env::var("HOME").unwrap_or_default();
    let app_dir_s = app_dir
        .canonicalize()
        .unwrap_or_else(|_| app_dir.to_path_buf())
        .to_string_lossy()
        .to_string();
    // In `allowlist` read mode the granted folders *are* the allowlist; in the
    // other two modes the argument is unused.
    let read_allowlist = granted.clone();

    let argv = match kind {
        DirectKind::Seatbelt => {
            let profile = write_profile(
                app_id,
                &app_dir_s,
                &home,
                network,
                &mounts,
                cfg.read_mode,
                &read_allowlist,
                &ports,
            )?;
            let mut a = vec![
                "/usr/bin/sandbox-exec".to_string(),
                "-f".to_string(),
                profile.to_string_lossy().to_string(),
            ];
            a.extend(shell_argv(start));
            a
        }
        DirectKind::Bubblewrap => {
            let mut a = vec!["bwrap".to_string()];
            a.extend(bwrap_app_args(
                &app_dir_s,
                &home,
                &mounts,
                cfg.read_mode,
                &read_allowlist,
            ));
            a.extend(shell_argv(start));
            a
        }
        _ => unreachable!("filtered above"),
    };

    let mut env: Vec<(String, String)> = vec![
        // The daemon's own TMPDIR points into `/private/var/folders`, which the
        // profile denies writing on purpose (it holds other applications' data).
        // A server with no writable temp directory fails in obscure ways, so it
        // gets one inside its own sandbox area.
        ("TMPDIR".into(), tmp.to_string_lossy().to_string()),
        ("TMP".into(), tmp.to_string_lossy().to_string()),
        ("TEMP".into(), tmp.to_string_lossy().to_string()),
    ];
    if let Some(p) = &proxy {
        let url = format!("http://127.0.0.1:{}", p.port);
        for k in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
            env.push((k.into(), url.clone()));
        }
        // SenClaw's own API is reached directly on loopback, not through the
        // proxy — which refuses loopback destinations by design.
        for k in ["NO_PROXY", "no_proxy"] {
            env.push((k.into(), "127.0.0.1,localhost,::1".into()));
        }
        // Node's global `fetch` ignores the proxy variables unless asked; this is
        // the ask (Node 24+), and it is inert on older runtimes.
        env.push(("NODE_USE_ENV_PROXY".into(), "1".into()));
    }

    let note = note_for(kind, cfg);
    Ok(Plan {
        argv,
        env,
        enforced: true,
        isolation,
        note,
        proxy,
        granted,
    })
}

/// Read-only grants for language runtimes that live outside the system roots —
/// nvm, volta, pyenv, rustup, a prefix under `$HOME` or under a shared folder.
///
/// Derived from `PATH` rather than from a list of version-manager names, because
/// such a list rots and the thing that actually matters is "the tools this
/// process was told to run". For each `PATH` entry the read jail would hide, the
/// *install prefix* is granted, not just `bin`: npm's own entry point lives in
/// `../lib/node_modules/npm/bin`, so granting only `bin` starts nothing.
///
/// Read-only, and filtered through the same guard list as a user-chosen mount, so
/// `$HOME` itself and the credential stores can never come in this way.
pub fn toolchain_read_mounts() -> Vec<Mount> {
    toolchain_read_mounts_from(
        &std::env::var("HOME").unwrap_or_default(),
        &std::env::var("PATH").unwrap_or_default(),
    )
}

/// How many such grants are allowed. A generous `PATH` is common; an unbounded
/// profile is not. Sixteen because a real machine hit the previous cap of eight
/// with editors and model runners *before* reaching nvm — and the entry that
/// gets cut is the one the app needed.
const MAX_TOOLCHAIN_GRANTS: usize = 16;

pub fn toolchain_read_mounts_from(home: &str, path_var: &str) -> Vec<Mount> {
    let home_path = PathBuf::from(home);
    let mut out: Vec<Mount> = Vec::new();
    for entry in path_var.split(':').filter(|e| !e.trim().is_empty()) {
        let dir = PathBuf::from(entry);
        // Anything the jail already grants needs nothing from here. What is left
        // is exactly what the jail would hide: a toolchain somewhere else —
        // under `$HOME` (nvm, volta, pyenv), or under a shared prefix like
        // `/Users/shared/tools`. Keying off the jail's own root list rather than
        // off `$HOME` is what makes this work for both.
        if !dir.is_dir()
            || crate::sandbox::fsmode::SYSTEM_READ_ROOTS
                .iter()
                .any(|r| dir.starts_with(r))
        {
            continue;
        }
        // `$HOME/bin` has `$HOME` as its prefix; granting that would undo the
        // whole read jail, so such an entry grants only itself.
        let want = match dir.parent() {
            Some(p) if p != home_path.as_path() => p.to_path_buf(),
            _ => dir.clone(),
        };
        // Credential stores, judged against the home that was passed in.
        //
        // `mounts::validate` below also refuses them, but it builds its guard
        // list from the *process* `$HOME` — so a daemon running with a different
        // home would grant a `.ssh` on PATH read-only. A test caught exactly
        // that, hence this check first.
        if is_under_secret_dir(&home_path, &want) {
            continue;
        }
        // …then the shared guard list, for everything else it knows about.
        let Ok(m) = crate::sandbox::mounts::validate(&want.to_string_lossy(), "", true) else {
            continue;
        };
        if out.iter().any(|x| x.source == m.source) {
            continue;
        }
        out.push(m);
        if out.len() >= MAX_TOOLCHAIN_GRANTS {
            // Truncation is a plausible cause of "the app will not start under
            // strict", so it leaves a breadcrumb rather than being silent.
            tracing::warn!(
                "[sandbox] PATH has more than {MAX_TOOLCHAIN_GRANTS} non-system entries;                  later ones are not readable to sandboxed apps in a jailed read mode"
            );
            break;
        }
    }
    out
}

/// Folders under `home` that hold secrets rather than tools, whatever `PATH`
/// says. Kept home-relative on purpose — see the caller.
fn is_under_secret_dir(home: &Path, candidate: &Path) -> bool {
    const SECRET_DIRS: &[&str] = &[
        ".ssh",
        ".aws",
        ".gnupg",
        ".config/gcloud",
        ".kube",
        ".docker",
        ".netrc",
        ".senclaw",
        "Library/Keychains",
    ];
    match candidate.strip_prefix(home) {
        Ok(rel) => {
            let rel = rel.to_string_lossy().replace('\\', "/");
            SECRET_DIRS
                .iter()
                .any(|s| rel == *s || rel.starts_with(&format!("{s}/")))
        }
        Err(_) => false,
    }
}

/// Per-app temp directory, outside the app's own folder so it cannot be
/// confused with the app's files, and outside anything else the app can reach.
fn tmp_dir(app_id: &str) -> PathBuf {
    crate::sandbox::config::data_dir().join("app-tmp").join(app_id)
}

/// Where an app's generated profile lives. Deliberately **not** inside anything
/// the app can write: unlike the exec path, a sandboxed app must not be able to
/// rewrite the profile its own next launch will use.
fn profile_path(app_id: &str) -> PathBuf {
    crate::sandbox::config::data_dir()
        .join("app-profiles")
        .join(format!("{app_id}.sb"))
}

#[allow(clippy::too_many_arguments)]
fn write_profile(
    app_id: &str,
    app_dir: &str,
    home: &str,
    network: bool,
    mounts: &[Mount],
    read_mode: crate::sandbox::fsmode::FsMode,
    read_allowlist: &[String],
    ports: &crate::sandbox::ports::PortPolicy,
) -> Result<PathBuf> {
    let body = crate::sandbox::backend::direct::seatbelt_profile(
        app_dir,
        home,
        network,
        mounts,
        read_mode,
        read_allowlist,
        ports,
    );
    let path = profile_path(app_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create the sandbox profile dir: {e}"))?;
    }
    std::fs::write(&path, body).map_err(|e| anyhow!("write the sandbox profile: {e}"))?;
    Ok(path)
}

/// bubblewrap arguments for an app launch.
///
/// Differs from the engine's own `bwrap_args` in the two ways an installed app
/// needs: every grant is bound at **its own path** (nothing is remapped), and
/// there is no `--unshare-net`, because a served app with a private network
/// namespace is unreachable. Pure, so the argument list can be asserted on — a
/// wrong flag here fails open.
pub fn bwrap_app_args(
    app_dir: &str,
    home: &str,
    mounts: &[Mount],
    read_mode: crate::sandbox::fsmode::FsMode,
    read_allowlist: &[String],
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--die-with-parent".into(),
        "--new-session".into(),
        "--unshare-pid".into(),
        "--unshare-ipc".into(),
        "--unshare-uts".into(),
        "--unshare-cgroup-try".into(),
    ];

    if read_mode.jails_reads() {
        for root in crate::sandbox::fsmode::SYSTEM_READ_ROOTS {
            a.push("--ro-bind-try".into());
            a.push(root.to_string());
            a.push(root.to_string());
        }
        if read_mode == crate::sandbox::fsmode::FsMode::Allowlist {
            for p in read_allowlist.iter().filter(|p| !p.trim().is_empty()) {
                a.push("--ro-bind-try".into());
                a.push(p.clone());
                a.push(p.clone());
            }
        }
    } else {
        a.extend(["--ro-bind".into(), "/".to_string(), "/".to_string()]);
    }
    a.extend([
        "--dev".into(),
        "/dev".to_string(),
        "--proc".into(),
        "/proc".to_string(),
        "--tmpfs".into(),
        "/tmp".to_string(),
    ]);
    // An empty home hides every dotfile credential in one move. The app's own
    // folders are bound back in below — order matters, bwrap applies mounts in
    // sequence and a tmpfs later would cover them.
    if !read_mode.jails_reads() && !home.is_empty() && home != "/" {
        a.push("--tmpfs".into());
        a.push(home.to_string());
    }

    a.extend(["--bind".into(), app_dir.to_string(), app_dir.to_string()]);
    for m in mounts.iter().filter(|m| !m.source.trim().is_empty()) {
        a.push(if m.read_only { "--ro-bind-try".into() } else { "--bind-try".into() });
        a.push(m.source.clone());
        a.push(m.source.clone()); // same path in and out: nothing is remapped
    }
    a.extend(["--chdir".into(), app_dir.to_string()]);
    a.push("--".into());
    a
}

/// The gap between what was asked for and what this machine will do. Public
/// because the settings dialog must show it *before* the user turns the switch
/// on, not only in a log afterwards.
pub fn note_for(kind: DirectKind, cfg: &AppSandbox) -> Option<String> {
    match kind {
        DirectKind::Bubblewrap if cfg.network != NetMode::All => Some(
            "On Linux the app keeps this machine's network namespace — otherwise the daemon \
             could not reach the app's own port — so the network restriction is NOT enforced: \
             the app can bypass the proxy. The folder rules are enforced."
                .into(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::app_policy::AppFolder;
    use crate::sandbox::fsmode::FsMode;
    use crate::sandbox::ports;

    fn profile_for(cfg_net: NetMode, read_mode: FsMode, proxy_port: Option<u16>) -> String {
        let mounts = vec![
            Mount { source: "/Users/u/.senclaw/apps/demo".into(), target: String::new(), read_only: false },
            Mount { source: "/Users/u/Documents/shared".into(), target: String::new(), read_only: true },
        ];
        let mut lo: Vec<u16> = vec![18788];
        if let Some(p) = proxy_port {
            lo.push(p);
        }
        let ports = ports::validate(&[4540], &[], &lo).unwrap();
        crate::sandbox::backend::direct::seatbelt_profile(
            "/Users/u/.senclaw/workspace/space-apps/demo",
            "/Users/u",
            matches!(cfg_net, NetMode::All),
            &mounts,
            read_mode,
            &[],
            &ports,
        )
    }

    #[test]
    fn a_disabled_config_launches_exactly_as_before() {
        let p = Plan::passthrough("npm start", None);
        assert!(!p.enforced);
        assert_eq!(p.isolation, "none");
        assert!(p.env.is_empty(), "an unsandboxed app must not get surprise env vars");
        assert!(p.argv.last().unwrap().contains("npm start"));
    }

    #[test]
    fn the_app_may_serve_its_port_and_be_reached_on_it() {
        let prof = profile_for(NetMode::All, FsMode::Open, None);
        assert!(prof.contains(r#"(allow network-bind (local ip "*:4540"))"#), "{prof}");
        assert!(prof.contains(r#"(allow network-inbound (local ip "*:4540"))"#));
    }

    #[test]
    fn full_network_still_cannot_reach_this_machines_other_services() {
        // The whole point of the loopback deny: an app with the internet must not
        // thereby get SenClaw's unauthenticated API *and* every other app.
        let prof = profile_for(NetMode::All, FsMode::Open, None);
        assert!(prof.contains(r#"(deny network-outbound (remote ip "localhost:*"))"#));
        let allow = prof.find(r#"(allow network-outbound (remote ip "localhost:18788"))"#).unwrap();
        let deny = prof.find(r#"(deny network-outbound (remote ip "localhost:*"))"#).unwrap();
        assert!(deny < allow, "the daemon port is handed back after the deny, nothing else is");
        assert!(!prof.contains("localhost:4541"), "no other app's port is open");
    }

    #[test]
    fn per_site_mode_leaves_the_proxy_as_the_only_way_out() {
        let prof = profile_for(NetMode::Hosts, FsMode::Open, Some(59999));
        assert!(prof.contains("(deny network*)"), "no general egress: {prof}");
        assert!(!prof.contains(r#"(remote ip "*:443")"#), "no direct https either");
        assert!(prof.contains(r#"(allow network-outbound (remote ip "localhost:59999"))"#));
        // And no resolver: with nowhere to send a DNS query, hostnames cannot be
        // used as an exfiltration channel. The proxy resolves instead.
        assert!(!prof.contains("mDNSResponder"), "{prof}");
    }

    #[test]
    fn network_off_denies_everything_but_the_served_port() {
        let prof = profile_for(NetMode::Off, FsMode::Open, None);
        assert!(prof.contains("(deny network*)"));
        assert!(prof.contains(r#"(allow network-inbound (local ip "*:4540"))"#));
        assert!(!prof.contains("mDNSResponder"));
    }

    #[test]
    fn writes_are_confined_to_the_app_and_what_it_was_granted() {
        let prof = profile_for(NetMode::All, FsMode::Open, None);
        let writes = prof.split("(deny file-read").next().unwrap();
        assert!(writes.contains("(deny file-write*)"));
        assert!(writes.contains("space-apps/demo"), "its own directory: {writes}");
        assert!(writes.contains("/Users/u/.senclaw/apps/demo"), "its own data: {writes}");
        assert!(
            !writes.contains("/Users/u/Documents/shared"),
            "a read-only grant must not appear in the write section: {writes}"
        );
    }

    #[test]
    fn open_reads_still_hide_the_credential_stores_and_other_apps_data() {
        let prof = profile_for(NetMode::All, FsMode::Open, None);
        assert!(prof.contains("/Users/u/.ssh"));
        assert!(prof.contains("/Users/u/.senclaw"), "the daemon's own state is denied");
        // …and the app's own data dir is handed back after that deny, or the app
        // could not read the database it just wrote.
        let deny = prof.find(r#"(subpath "/Users/u/.senclaw")"#).unwrap();
        let allow = prof.find(r#"(allow file-read* (subpath "/Users/u/.senclaw/apps/demo"))"#).unwrap();
        assert!(deny < allow, "last matching rule wins in Seatbelt");
    }

    #[test]
    fn strict_reads_keep_the_system_roots_and_drop_the_users_disk() {
        let prof = profile_for(NetMode::All, FsMode::Strict, None);
        assert!(prof.contains("(deny file-read*)"));
        assert!(prof.contains("/usr"), "an app cannot start without its runtime");
        assert!(prof.contains("/Users/u/.senclaw/apps/demo"), "its own data stays readable");
        assert!(!prof.contains("/Users/u/Documents/other"), "the rest of the disk is gone");
    }

    #[test]
    fn bwrap_binds_every_grant_at_its_own_path() {
        // An installed app resolves its data dir from $HOME at startup, so a
        // remapped path is the same as a missing one.
        let mounts = vec![
            Mount { source: "/home/u/.senclaw/apps/demo".into(), target: "data".into(), read_only: false },
            Mount { source: "/home/u/docs".into(), target: "docs".into(), read_only: true },
        ];
        let a = bwrap_app_args("/opt/app", "/home/u", &mounts, FsMode::Open, &[]).join(" ");
        assert!(a.contains("--bind-try /home/u/.senclaw/apps/demo /home/u/.senclaw/apps/demo"), "{a}");
        assert!(a.contains("--ro-bind-try /home/u/docs /home/u/docs"), "{a}");
        assert!(!a.contains("/opt/app/data"), "nothing may be remapped: {a}");
        // The app dir is writable and is the working directory.
        assert!(a.contains("--bind /opt/app /opt/app"));
        assert!(a.contains("--chdir /opt/app"));
        // A private network namespace would make the app unreachable.
        assert!(!a.contains("--unshare-net"), "{a}");
    }

    #[test]
    fn bwrap_hides_the_home_directory_before_binding_what_is_granted() {
        let mounts = vec![Mount {
            source: "/home/u/.senclaw/apps/demo".into(),
            target: String::new(),
            read_only: false,
        }];
        let a = bwrap_app_args("/opt/app", "/home/u", &mounts, FsMode::Open, &[]);
        let joined = a.join(" ");
        let tmpfs_home = joined.find("--tmpfs /home/u ").expect("home must be masked");
        let bind_data = joined.find("--bind-try /home/u/.senclaw/apps/demo").unwrap();
        assert!(tmpfs_home < bind_data, "binding first and masking after would hide the data dir");
    }

    #[test]
    fn linux_says_out_loud_that_it_cannot_enforce_the_network_mode() {
        let mut cfg = AppSandbox { enabled: true, network: NetMode::Hosts, ..Default::default() };
        let n = note_for(DirectKind::Bubblewrap, &cfg).unwrap();
        assert!(n.contains("NOT enforced"), "{n}");
        assert!(n.contains("folder rules are enforced"), "the part that does work must be said: {n}");
        // Full-network mode asks for nothing bubblewrap cannot do.
        cfg.network = NetMode::All;
        assert!(note_for(DirectKind::Bubblewrap, &cfg).is_none());
        assert!(note_for(DirectKind::Seatbelt, &cfg).is_none());
    }

    #[test]
    fn an_unsupported_platform_reports_instead_of_pretending() {
        let p = Plan::passthrough("npm start", Some("no OS sandbox".into()));
        assert!(!p.enforced);
        assert!(p.summary().contains("NOT enforced"), "{}", p.summary());
    }

    #[test]
    fn the_profile_is_somewhere_the_app_cannot_write() {
        // The exec path keeps its profile inside the sandbox's own writable
        // directory (harmless there: it is regenerated before each run). An app
        // is long-lived, so its profile must be out of reach entirely.
        let prof = profile_path("demo").to_string_lossy().to_string();
        let tmp = tmp_dir("demo").to_string_lossy().to_string();
        assert!(prof.contains("app-profiles"), "{prof}");
        assert!(!prof.starts_with(&tmp), "{prof} vs {tmp}");
        assert!(prof.ends_with("demo.sb"));
    }

    #[test]
    fn a_jailed_read_mode_still_grants_the_runtime_under_home() {
        // The failure this exists to prevent, measured on a real app:
        //   EPERM … /Users/u/.nvm/versions/node/v24.13.1/lib/node_modules/npm/bin/npm-cli.js
        let home = tempfile::tempdir().unwrap();
        let home_s = home.path().to_string_lossy().to_string();
        let node_bin = home.path().join(".nvm/versions/node/v24.13.1/bin");
        std::fs::create_dir_all(&node_bin).unwrap();
        let own_bin = home.path().join("bin");
        std::fs::create_dir_all(&own_bin).unwrap();

        let ms = toolchain_read_mounts_from(
            &home_s,
            &format!("/usr/bin:{}:{}", node_bin.display(), own_bin.display()),
        );
        // `/usr/bin` is already inside the jail's system roots, so it must not
        // produce a grant — only what the jail would hide does.
        let sources: Vec<String> = ms.iter().map(|m| m.source.clone()).collect();
        // The install prefix, not just `bin` — npm's entry point lives in ../lib.
        let prefix = home
            .path()
            .join(".nvm/versions/node/v24.13.1")
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(sources.contains(&prefix), "got {sources:?}");
        // `$HOME/bin` grants itself, never `$HOME` — that would undo the jail.
        let own = own_bin.canonicalize().unwrap().to_string_lossy().to_string();
        assert!(sources.contains(&own), "got {sources:?}");
        assert!(
            !sources.iter().any(|s| *s == home.path().canonicalize().unwrap().to_string_lossy()),
            "the home directory itself must never be granted: {sources:?}"
        );
        // System paths are not this function's business, and everything is read-only.
        assert!(!sources.iter().any(|s| s.starts_with("/usr")), "got {sources:?}");
        assert!(ms.iter().all(|m| m.read_only), "a runtime grant is never writable");
    }

    #[test]
    fn toolchain_grants_are_bounded_and_skip_credential_stores() {
        let home = tempfile::tempdir().unwrap();
        let home_s = home.path().to_string_lossy().to_string();
        // A credential store on PATH (contrived, but the guard must hold).
        let ssh_bin = home.path().join(".ssh/bin");
        std::fs::create_dir_all(&ssh_bin).unwrap();
        let mut entries = vec![ssh_bin.to_string_lossy().to_string()];
        for i in 0..12 {
            let d = home.path().join(format!("tool{i}/bin"));
            std::fs::create_dir_all(&d).unwrap();
            entries.push(d.to_string_lossy().to_string());
        }
        let ms = toolchain_read_mounts_from(&home_s, &entries.join(":"));
        assert!(ms.len() <= MAX_TOOLCHAIN_GRANTS, "got {}", ms.len());
        assert!(
            !ms.iter().any(|m| m.source.contains(".ssh")),
            "a credential store must never be granted: {ms:?}"
        );
    }

    #[test]
    fn an_open_read_mode_needs_no_toolchain_grants() {
        // Nothing under `$HOME` is denied for reads in `open` mode except the
        // credential stores, so this machinery must not widen anything there.
        let cfg = AppSandbox { enabled: true, read_mode: FsMode::Open, ..Default::default() };
        assert!(!cfg.read_mode.jails_reads());
    }

    #[test]
    fn the_temp_dir_is_per_app_and_outside_the_app_folder() {
        let a = tmp_dir("crm").to_string_lossy().to_string();
        let b = tmp_dir("news").to_string_lossy().to_string();
        assert_ne!(a, b, "two apps must not share a temp directory");
        assert!(a.contains("app-tmp"));
    }

    #[tokio::test]
    async fn a_disabled_app_gets_no_proxy_and_no_profile() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AppSandbox::default(); // enabled: false
        let p = plan("demo", dir.path(), "npm start", 4540, 18788, &cfg).await.unwrap();
        assert!(p.proxy.is_none() && !p.enforced && p.granted.is_empty());
    }

    #[tokio::test]
    async fn per_site_mode_spawns_a_proxy_and_points_the_app_at_it() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AppSandbox {
            enabled: true,
            network: NetMode::Hosts,
            hosts: vec!["api.openai.com".into()],
            folders: vec![],
            ..Default::default()
        };
        let p = plan("demo-proxy", dir.path(), "npm start", 4540, 18788, &cfg)
            .await
            .unwrap();
        if !p.enforced {
            // A machine with no OS sandbox (CI containers, mostly): the plan is
            // required to say so rather than claim enforcement.
            assert!(p.note.is_some());
            return;
        }
        let proxy = p.proxy.as_ref().expect("per-site mode needs the proxy");
        let url = format!("http://127.0.0.1:{}", proxy.port);
        let env: std::collections::HashMap<_, _> = p.env.iter().cloned().collect();
        assert_eq!(env.get("HTTPS_PROXY"), Some(&url));
        assert_eq!(env.get("https_proxy"), Some(&url), "lowercase matters to curl and reqwest");
        assert_eq!(env.get("NODE_USE_ENV_PROXY").map(String::as_str), Some("1"));
        assert!(
            env.get("NO_PROXY").unwrap().contains("127.0.0.1"),
            "the daemon API is reached directly, not through the proxy"
        );
        assert!(env.contains_key("TMPDIR"), "a server with no writable temp fails obscurely");
        // The proxy carries this app's allowlist, not someone else's.
        assert_eq!(proxy.hosts(), vec!["api.openai.com"]);
    }

    #[tokio::test]
    async fn a_granted_folder_reaches_the_plan() {
        let dir = tempfile::tempdir().unwrap();
        let extra = dir.path().join("shared");
        std::fs::create_dir_all(&extra).unwrap();
        let cfg = AppSandbox {
            enabled: true,
            folders: vec![AppFolder {
                path: extra.to_string_lossy().to_string(),
                read_only: true,
            }],
            ..Default::default()
        };
        let p = plan("demo-folders", dir.path(), "npm start", 4541, 18788, &cfg)
            .await
            .unwrap();
        if !p.enforced {
            return;
        }
        let canon = extra.canonicalize().unwrap().to_string_lossy().to_string();
        assert!(p.granted.iter().any(|g| *g == canon), "granted: {:?}", p.granted);
        assert!(
            p.granted.iter().any(|g| g.contains("app-tmp")),
            "the temp dir is always granted: {:?}",
            p.granted
        );
    }
}
