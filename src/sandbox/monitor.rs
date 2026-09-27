//! Resource monitor: what is running inside a sandbox, how much CPU and RAM it
//! is using, and how to stop it.
//!
//! ## Why there is a registry
//!
//! The `direct` backend runs sandboxed work as ordinary child processes of this
//! app. There is no container to ask "what is running in you?", so the app has
//! to remember: every spawn registers its process **group** here, every exit
//! deregisters it. The group — not the pid — is the unit, because a run is
//! `sh` plus whatever it spawned, and `setsid` in `direct::exec` puts all of
//! them in one group.
//!
//! ## Why kill is checked against the registry
//!
//! `POST /sandboxes/:id/kill?pid=N` is, in the wrong implementation, an
//! unauthenticated "kill any process on this machine" endpoint — pid 1 included.
//! So a kill is only ever performed on a pid whose process group is one this app
//! started for that specific sandbox. Everything else is refused by name.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::Serialize;

use crate::sandbox::config;
use crate::sandbox::db::Sandbox;

const PS_TIMEOUT: Duration = Duration::from_secs(5);

/// Process groups this app started, per sandbox id.
static LIVE: Mutex<Option<HashMap<String, Vec<u32>>>> = Mutex::new(None);

fn with_live<T>(f: impl FnOnce(&mut HashMap<String, Vec<u32>>) -> T) -> T {
    let mut g = LIVE.lock().unwrap();
    f(g.get_or_insert_with(HashMap::new))
}

/// Record a process group as belonging to a sandbox.
pub fn register(sandbox_id: &str, pgid: u32) {
    with_live(|m| m.entry(sandbox_id.to_string()).or_default().push(pgid));
}

pub fn unregister(sandbox_id: &str, pgid: u32) {
    with_live(|m| {
        if let Some(v) = m.get_mut(sandbox_id) {
            v.retain(|p| *p != pgid);
            if v.is_empty() {
                m.remove(sandbox_id);
            }
        }
    });
}

/// Process groups currently registered for a sandbox.
pub fn groups(sandbox_id: &str) -> Vec<u32> {
    with_live(|m| m.get(sandbox_id).cloned().unwrap_or_default())
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// Process-group leader. Parsed all along and previously dropped; a fleet
    /// view needs it to tell one app's tree from another's in a single listing.
    pub pgid: u32,
    /// CPU percent, as the OS reports it (can exceed 100 on multiple cores).
    pub cpu: f64,
    /// Percent of physical memory.
    pub mem_percent: f64,
    /// Resident set size in MB.
    pub rss_mb: f64,
    /// Wall-clock age, e.g. `01:23` or `1-04:05:06`.
    pub elapsed: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    /// Backend that produced these numbers — the two measure different things
    /// and the UI should not present them as the same.
    pub source: String,
    pub processes: Vec<Proc>,
    pub cpu: f64,
    pub rss_mb: f64,
    /// Configured ceiling, for context. `direct` has no enforced RAM cap, so
    /// this is `None` there rather than a number that is not enforced.
    pub memory_limit_mb: Option<i64>,
    pub running: bool,
    /// Set when the numbers could not be taken; the UI shows it instead of
    /// rendering an empty table that looks like "nothing is running".
    pub note: Option<String>,
}

// ── sampling ────────────────────────────────────────────────────────────────

/// Parse `ps` output. Split out from the process call so the parsing — which is
/// where the bugs are — is testable without spawning anything.
///
/// Columns, in order: pid ppid pgid pcpu pmem rss etime comm
pub fn parse_ps(out: &str, wanted_groups: &[u32]) -> Vec<Proc> {
    parse_ps_any(out)
        .into_iter()
        .filter(|p| wanted_groups.contains(&p.pgid))
        .collect()
}

/// The same parse without the group filter, for callers that do not know the
/// group yet — a Space App this daemon adopted rather than launched is found by
/// pid, and its group has to be read off the listing.
pub fn parse_ps_any(out: &str) -> Vec<Proc> {
    let mut v = Vec::new();
    for line in out.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 {
            continue;
        }
        let (Ok(pid), Ok(ppid), Ok(pgid)) = (
            f[0].parse::<u32>(),
            f[1].parse::<u32>(),
            f[2].parse::<u32>(),
        ) else {
            continue; // the header row, or a line ps mangled
        };
        v.push(Proc {
            pid,
            ppid,
            pgid,
            cpu: f[3].parse().unwrap_or(0.0),
            mem_percent: f[4].parse().unwrap_or(0.0),
            // ps reports RSS in KB on both macOS and Linux.
            rss_mb: f[5].parse::<f64>().unwrap_or(0.0) / 1024.0,
            elapsed: f[6].to_string(),
            // The command can contain spaces, so it is everything left.
            command: f[7..].join(" "),
        });
    }
    v
}

/// Sample the host process table.
///
/// `ps -axo …` lists everything and the filtering happens here, rather than
/// asking ps to select by group: the selection flags differ between macOS and
/// Linux (`-g` means different things), and one full listing parsed in Rust
/// behaves identically on both.
async fn sample_host(groups: &[u32]) -> Result<Vec<Proc>> {
    if groups.is_empty() {
        return Ok(Vec::new());
    }
    let out = run(
        "ps",
        &["-axo", "pid=,ppid=,pgid=,pcpu=,pmem=,rss=,etime=,comm="],
        PS_TIMEOUT,
    )
    .await?;
    Ok(parse_ps(&out, groups))
}

/// `docker top` output: a header row then one row per process. The columns
/// requested match `parse_ps`, so the same parser handles both — except docker
/// has no pgid column, so it is asked for one via `-eo`.
async fn sample_container(sb: &Sandbox) -> Result<Vec<Proc>> {
    let name = format!("senclaw-sbx-{}", sb.id);
    let out = run(
        &config::docker_bin(),
        &[
            "top",
            &name,
            "-eo",
            "pid,ppid,pgid,pcpu,pmem,rss,etime,comm",
        ],
        PS_TIMEOUT,
    )
    .await?;
    // Every process inside the container counts, so instead of filtering by a
    // registry the groups are taken from the output itself.
    let groups: Vec<u32> = out
        .lines()
        .filter_map(|l| l.split_whitespace().nth(2)?.parse().ok())
        .collect();
    Ok(parse_ps(&out, &groups))
}

pub async fn stats(sb: &Sandbox) -> Stats {
    if sb.backend == "docker" {
        return match sample_container(sb).await {
            Ok(procs) => finish(procs, "container", Some(sb.memory_mb), None),
            Err(e) => Stats {
                source: "container".into(),
                processes: Vec::new(),
                cpu: 0.0,
                rss_mb: 0.0,
                memory_limit_mb: Some(sb.memory_mb),
                running: false,
                note: Some(format!("cannot read the container process list: {e}")),
            },
        };
    }

    let g = groups(&sb.id);
    match sample_host(&g).await {
        // `direct` has no enforced RAM ceiling — reporting the configured
        // number here would imply one exists.
        Ok(procs) => finish(procs, "host", None, None),
        Err(e) => Stats {
            source: "host".into(),
            processes: Vec::new(),
            cpu: 0.0,
            rss_mb: 0.0,
            memory_limit_mb: None,
            running: false,
            note: Some(format!("cannot read the process table: {e}")),
        },
    }
}

/// Sample several groups at once and keep them apart, for a fleet view.
///
/// One `ps` for the whole list rather than one per group: the Space Apps screen
/// refreshes every few seconds with a dozen apps on it, and a dozen process
/// listings per tick is a measurable cost for a panel that is only watching.
pub async fn stats_by_group(groups: &[u32]) -> std::collections::HashMap<u32, Stats> {
    let mut out = std::collections::HashMap::new();
    if groups.is_empty() {
        return out;
    }
    let procs = sample_host(groups).await.unwrap_or_default();
    for g in groups {
        // `ps` reports each process's own group, so attribution is exact — no
        // guessing from parent chains.
        let mine: Vec<Proc> = procs.iter().filter(|p| p.pgid == *g).cloned().collect();
        out.insert(*g, finish(mine, "host", None, None));
    }
    out
}

/// Sample by pid, reporting the whole process group each pid belongs to.
///
/// For processes this daemon did not start, where the group is not known up
/// front: one listing, the pid's own row gives the group, the group gives the
/// tree.
pub async fn stats_by_pid(pids: &[u32]) -> std::collections::HashMap<u32, Stats> {
    let mut out = std::collections::HashMap::new();
    if pids.is_empty() {
        return out;
    }
    let listing = match run(
        "ps",
        &["-axo", "pid=,ppid=,pgid=,pcpu=,pmem=,rss=,etime=,comm="],
        PS_TIMEOUT,
    )
    .await
    {
        Ok(o) => parse_ps_any(&o),
        Err(_) => return out,
    };
    for pid in pids {
        let Some(group) = listing.iter().find(|p| p.pid == *pid).map(|p| p.pgid) else {
            continue; // it exited between the socket listing and this sample
        };
        let mine: Vec<Proc> = listing.iter().filter(|p| p.pgid == group).cloned().collect();
        out.insert(*pid, finish(mine, "host", None, None));
    }
    out
}

/// Sample any process group, for callers outside the sandbox registry — a Space
/// App's server tree is launched by the daemon with its own group, so this is
/// the same measurement without inventing a `Sandbox` for it.
///
/// `npm → node` and `sh → app` are the reason this is by *group* and not by pid:
/// the process the daemon holds is usually a shell, and the memory that matters
/// belongs to its children.
pub async fn stats_for_groups(groups: &[u32]) -> Stats {
    if groups.is_empty() {
        return finish(Vec::new(), "host", None, None);
    }
    match sample_host(groups).await {
        Ok(procs) => finish(procs, "host", None, None),
        Err(e) => Stats {
            source: "host".into(),
            processes: Vec::new(),
            cpu: 0.0,
            rss_mb: 0.0,
            memory_limit_mb: None,
            running: false,
            note: Some(format!("cannot read the process table: {e}")),
        },
    }
}

fn finish(procs: Vec<Proc>, source: &str, limit: Option<i64>, note: Option<String>) -> Stats {
    Stats {
        // `+ 0.0` is not redundant: Rust's `Sum for f64` folds from `-0.0`, so
        // an empty process list serialises as `-0.0` and the UI renders an idle
        // sandbox as "-0.0 %".
        cpu: procs.iter().map(|p| p.cpu).sum::<f64>() + 0.0,
        rss_mb: procs.iter().map(|p| p.rss_mb).sum::<f64>() + 0.0,
        running: !procs.is_empty(),
        source: source.into(),
        memory_limit_mb: limit,
        processes: procs,
        note,
    }
}

// ── killing ─────────────────────────────────────────────────────────────────

/// Stop everything this sandbox is running. Returns how many groups were
/// signalled.
pub async fn kill_all(sb: &Sandbox) -> Result<usize> {
    if sb.backend == "docker" {
        // Restarting is the container equivalent: it takes every process with
        // it, and keeps the workdir (a host bind mount) and installed packages
        // (the writable layer).
        crate::sandbox::backend::docker::restart(sb).await?;
        return Ok(1);
    }
    let g = groups(&sb.id);
    for pgid in &g {
        signal_group(*pgid);
    }
    with_live(|m| m.remove(&sb.id));
    Ok(g.len())
}

/// Stop one process.
///
/// The pid must belong to a process group this app started for *this* sandbox.
/// Without that check this is a "kill anything on the machine" endpoint.
pub async fn kill_pid(sb: &Sandbox, pid: u32) -> Result<()> {
    if sb.backend == "docker" {
        // Inside the container the pid namespace is the container's, so there
        // is nothing on the host it could reach.
        let name = format!("senclaw-sbx-{}", sb.id);
        run(
            &config::docker_bin(),
            &["exec", &name, "kill", "-9", &pid.to_string()],
            PS_TIMEOUT,
        )
        .await?;
        return Ok(());
    }

    let g = groups(&sb.id);
    let procs = sample_host(&g).await?;
    if !procs.iter().any(|p| p.pid == pid) {
        return Err(anyhow!(
            "process {pid} does not belong to sandbox `{}` — refusing to kill it",
            sb.name
        ));
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    Ok(())
}

fn signal_group(pgid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pgid as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pgid;
}

async fn run(bin: &str, args: &[&str], timeout: Duration) -> Result<String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| anyhow!("{e}"))?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => Err(anyhow!("`{bin}` did not answer within {}s", timeout.as_secs())),
        Ok(Err(e)) => Err(anyhow!("{e}")),
        Ok(Ok(o)) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).to_string()),
        Ok(Ok(o)) => {
            let e = String::from_utf8_lossy(&o.stderr).trim().to_string();
            Err(anyhow!(if e.is_empty() { format!("`{bin}` failed") } else { e }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
  501   500   501   3.5  0.2  40960       01:23 python3 heavy.py
  502   501   501  12.0  1.1 123456       01:20 /usr/bin/python3 -c import x
  900   800   900   0.1  0.0   2048    1-02:03:04 someone-elses-shell
";

    #[test]
    fn only_the_registered_groups_are_reported() {
        let procs = parse_ps(SAMPLE, &[501]);
        assert_eq!(procs.len(), 2);
        assert!(procs.iter().all(|p| p.pid == 501 || p.pid == 502));
        assert!(
            !procs.iter().any(|p| p.pid == 900),
            "a process from another group leaked into the sandbox's list"
        );
    }

    #[test]
    fn an_empty_group_list_matches_nothing() {
        assert!(parse_ps(SAMPLE, &[]).is_empty());
    }

    #[test]
    fn rss_is_converted_from_kb_to_mb() {
        let p = &parse_ps(SAMPLE, &[501])[0];
        assert!((p.rss_mb - 40.0).abs() < 0.01, "got {}", p.rss_mb);
    }

    #[test]
    fn a_command_with_spaces_survives_intact() {
        let p = &parse_ps(SAMPLE, &[501])[1];
        assert_eq!(p.command, "/usr/bin/python3 -c import x");
    }

    #[test]
    fn a_long_elapsed_time_is_kept_verbatim() {
        let p = &parse_ps(SAMPLE, &[900])[0];
        assert_eq!(p.elapsed, "1-02:03:04");
    }

    #[test]
    fn a_header_row_is_skipped_rather_than_parsed_as_a_process() {
        let with_header = format!("  PID  PPID  PGID %CPU %MEM   RSS ELAPSED COMMAND\n{SAMPLE}");
        assert_eq!(parse_ps(&with_header, &[501]).len(), 2);
    }

    #[test]
    fn a_fleet_listing_keeps_each_app_apart() {
        // One `ps` listing carrying two apps' trees: partitioning by group is
        // what stops app A's memory being reported as app B's.
        let out = "\
100 1 100 5.0 0.5 51200 01:00 sh
101 100 100 7.0 1.0 102400 00:59 node
200 1 200 1.0 0.2 20480 10:00 ba
300 1 300 9.9 9.9 999999 10:00 something-else";
        let procs = parse_ps(out, &[100, 200]);
        assert_eq!(procs.len(), 3, "the unwanted group must not be parsed in");
        let a: Vec<_> = procs.iter().filter(|p| p.pgid == 100).collect();
        let b: Vec<_> = procs.iter().filter(|p| p.pgid == 200).collect();
        assert_eq!(a.len(), 2, "sh + node belong to the same app");
        assert_eq!(b.len(), 1);
        assert!((a.iter().map(|p| p.cpu).sum::<f64>() - 12.0).abs() < 0.01);
        assert!((b[0].rss_mb - 20.0).abs() < 0.01);
    }

    #[test]
    fn totals_add_up_across_the_group() {
        let s = finish(parse_ps(SAMPLE, &[501]), "host", None, None);
        assert!((s.cpu - 15.5).abs() < 0.01);
        assert!(s.running);
        assert!(s.memory_limit_mb.is_none(), "direct has no enforced RAM cap");
    }

    #[test]
    fn nothing_running_reads_as_not_running() {
        let s = finish(Vec::new(), "host", None, None);
        assert!(!s.running);
        assert_eq!(s.cpu, 0.0);
    }

    #[test]
    fn register_and_unregister_are_scoped_per_sandbox() {
        register("sb-a", 111);
        register("sb-a", 222);
        register("sb-b", 333);
        assert_eq!(groups("sb-a"), vec![111, 222]);
        assert_eq!(groups("sb-b"), vec![333]);

        unregister("sb-a", 111);
        assert_eq!(groups("sb-a"), vec![222]);
        unregister("sb-a", 222);
        assert!(groups("sb-a").is_empty(), "the entry should be dropped when empty");
        assert_eq!(groups("sb-b"), vec![333], "another sandbox was affected");
        unregister("sb-b", 333);
    }

    #[tokio::test]
    async fn killing_a_pid_this_sandbox_never_started_is_refused() {
        let sb = Sandbox {
            id: "kill-guard-test".into(),
            name: "n".into(),
            backend: "direct".into(),
            image: None,
            workdir: "/w".into(),
            network: false,
            cpus: 1.0,
            memory_mb: 512,
            pids_limit: 256,
            timeout_ms: 1000,
            env: serde_json::json!({}),
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
        };
        // Nothing registered for this sandbox, so pid 1 must be refused rather
        // than signalled.
        let e = kill_pid(&sb, 1).await.unwrap_err().to_string();
        assert!(e.contains("does not belong to sandbox"), "got: {e}");
    }
}
