//! Process-table helpers for background tasks: who is in a group, is it alive,
//! what does it listen on, and ending it.
//!
//! The unit is the process **group**, not the pid. A dev server is a shell, npm
//! and node, and the shell that started it is a session leader (`setsid`), so its
//! pid names the whole family.
//!
//! Why signalling a recorded group number is safe: the registry creates the group
//! (or finds it with its members listed), and the watcher checks every second that
//! it still has a member, ending the task the moment it has none. A pgid can only
//! be handed to something else after the group is empty and the pid counter has
//! wrapped, which takes far longer than that second. After a daemon restart there
//! is no such continuity, so a restored entry must pass [`still_ours`] (one of its
//! recorded pids is still in the group) before it is listed. The daemon's own
//! group is never signalled, and never a kill by command pattern or by port
//! (`lsof -t … | kill` once took the daemon down with the thing it meant to stop).

use std::time::Duration;

/// One row of the process table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub command: String,
}

/// Parse `ps -axww -o pid=,ppid=,pgid=,stat=,command=`, keeping the live
/// members (not zombies) of `groups`.
pub fn parse_ps(out: &str, groups: &[u32]) -> Vec<Member> {
    let mut v = Vec::new();
    for line in out.lines() {
        let mut rest = line.trim_start();
        let mut head = [""; 4];
        for slot in head.iter_mut() {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            *slot = &rest[..end];
            rest = rest[end..].trim_start();
        }
        let (Ok(pid), Ok(ppid), Ok(pgid)) =
            (head[0].parse::<u32>(), head[1].parse::<u32>(), head[2].parse::<u32>())
        else {
            continue;
        };
        if !groups.contains(&pgid) || head[3].starts_with('Z') {
            continue;
        }
        v.push(Member { pid, ppid, pgid, command: rest.trim_end().to_string() });
    }
    v
}

/// The live members of these process groups. `None` when the process table
/// could not be read (no `ps`, or the platform refuses to run it from here — an
/// empty answer would read as "nothing is running").
#[cfg(unix)]
pub async fn members(groups: &[u32]) -> Option<Vec<Member>> {
    if groups.is_empty() {
        return Some(Vec::new());
    }
    let run = tokio::process::Command::new("ps")
        .args(["-axww", "-o", "pid=,ppid=,pgid=,stat=,command="])
        .stdin(std::process::Stdio::null())
        .output();
    match tokio::time::timeout(Duration::from_secs(4), run).await {
        Ok(Ok(out)) if out.status.success() => {
            Some(parse_ps(&String::from_utf8_lossy(&out.stdout), groups))
        }
        _ => None,
    }
}

#[cfg(not(unix))]
pub async fn members(_groups: &[u32]) -> Option<Vec<Member>> {
    None
}

/// Whether anything is left in the group. One syscall; a zombie still counts,
/// which [`members`] filters out.
#[cfg(unix)]
pub fn group_alive(pgid: u32) -> bool {
    if pgid == 0 || pgid > i32::MAX as u32 {
        return false;
    }
    unsafe {
        libc::kill(-(pgid as i32), 0) == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

#[cfg(not(unix))]
pub fn group_alive(_pgid: u32) -> bool {
    false
}

/// The group a pid is in now, `None` when it is gone.
#[cfg(unix)]
pub fn group_of(pid: u32) -> Option<u32> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    let g = unsafe { libc::getpgid(pid as i32) };
    (g > 0).then_some(g as u32)
}

#[cfg(not(unix))]
pub fn group_of(_pid: u32) -> Option<u32> {
    None
}

/// True when one of `recorded` is still in `pgid`: this group is the task's, not
/// a number the OS reused.
pub fn still_ours(pgid: u32, recorded: &[u32]) -> bool {
    recorded.iter().any(|p| group_of(*p) == Some(pgid))
}

#[cfg(unix)]
fn signal_group(pgid: u32, sig: i32) -> bool {
    // Never the daemon's own group (a bug upstream must not be able to take the
    // daemon, and every child sharing its group, down with a task), never init's.
    if pgid <= 1 || pgid > i32::MAX as u32 || pgid == unsafe { libc::getpgrp() } as u32 {
        return false;
    }
    unsafe { libc::kill(-(pgid as i32), sig) == 0 }
}

/// End a group: SIGTERM, then SIGKILL for what is still there after `grace`.
/// True when the group is gone.
#[cfg(unix)]
pub async fn stop_group(pgid: u32, grace: Duration) -> bool {
    if !group_alive(pgid) {
        return true;
    }
    signal_group(pgid, libc::SIGTERM);
    if wait_gone(pgid, grace).await {
        return true;
    }
    signal_group(pgid, libc::SIGKILL);
    wait_gone(pgid, Duration::from_secs(1)).await
}

#[cfg(unix)]
async fn wait_gone(pgid: u32, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if !group_alive(pgid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

/// SIGKILL a group right now — a run that timed out, where nothing may survive.
pub fn kill_group_now(pgid: Option<u32>) {
    #[cfg(unix)]
    if let Some(g) = pgid {
        signal_group(g, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pgid;
}

/// Signal every group at once (daemon shutdown has well under a second before the
/// desktop's SIGKILL), then SIGKILL what ignored it.
#[cfg(unix)]
pub async fn stop_groups(groups: &[u32], grace: Duration) {
    for g in groups {
        signal_group(*g, libc::SIGTERM);
    }
    let deadline = tokio::time::Instant::now() + grace;
    while tokio::time::Instant::now() < deadline && groups.iter().any(|g| group_alive(*g)) {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    for g in groups {
        if group_alive(*g) {
            signal_group(*g, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
pub async fn stop_groups(_groups: &[u32], _grace: Duration) {}

/// Windows has no groups here: end the process and its children by pid.
#[cfg(not(unix))]
pub async fn kill_tree(pid: u32) -> bool {
    tokio::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Parse `lsof -nP -a -p … -iTCP -sTCP:LISTEN -Fn` (`n*:5173`, `n127.0.0.1:5173`,
/// `n[::1]:5173`) into sorted, distinct ports.
pub fn parse_listen_ports(out: &str) -> Vec<u16> {
    let mut ports: Vec<u16> = out
        .lines()
        .filter_map(|l| l.strip_prefix('n'))
        .filter_map(|name| name.rsplit(':').next()?.parse::<u16>().ok())
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// The TCP ports these pids listen on, per pid. Read-only; empty when `lsof` is
/// missing or slow.
#[cfg(unix)]
pub async fn listening_ports(pids: &[u32]) -> std::collections::HashMap<u32, Vec<u16>> {
    let mut map = std::collections::HashMap::new();
    if pids.is_empty() {
        return map;
    }
    let list = pids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let run = tokio::process::Command::new("lsof")
        .args(["-nP", "-a", "-p", &list, "-iTCP", "-sTCP:LISTEN", "-Fpn"])
        .stdin(std::process::Stdio::null())
        .output();
    let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(4), run).await else {
        return map;
    };
    // `-F` output: `p<pid>` opens a process, each `n<name>` after it is a socket.
    let text = String::from_utf8_lossy(&out.stdout);
    let mut current: Option<u32> = None;
    let mut block = String::new();
    let mut flush = |pid: Option<u32>, block: &mut String| {
        if let Some(p) = pid {
            let ports = parse_listen_ports(block);
            if !ports.is_empty() {
                map.insert(p, ports);
            }
        }
        block.clear();
    };
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('p') {
            flush(current, &mut block);
            current = p.parse().ok();
        } else {
            block.push_str(line);
            block.push('\n');
        }
    }
    flush(current, &mut block);
    map
}

#[cfg(not(unix))]
pub async fn listening_ports(_pids: &[u32]) -> std::collections::HashMap<u32, Vec<u16>> {
    std::collections::HashMap::new()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const PS: &str = "\
  100     1   100 Ss   /bin/sh -s
  101   100   100 S    node /app/vite.js --port 5173
  102   100   100 Z    (defunct)
  200     1   200 Ss   python3 -m http.server 8000 --bind 127.0.0.1
  300     1   300 S+   vim
";

    #[test]
    fn keeps_the_live_members_of_the_wanted_groups_with_their_full_command() {
        let m = parse_ps(PS, &[100, 200]);
        let pids: Vec<u32> = m.iter().map(|x| x.pid).collect();
        assert_eq!(pids, vec![100, 101, 200], "zombie 102 and foreign group 300 must be out");
        assert_eq!(m[1].command, "node /app/vite.js --port 5173");
        assert_eq!(m[2].command, "python3 -m http.server 8000 --bind 127.0.0.1");
        assert_eq!(m[1].ppid, 100);
    }

    #[test]
    fn a_garbled_row_is_skipped_not_fatal() {
        assert!(parse_ps("not a row\n\n  x y z\n", &[1]).is_empty());
    }

    #[test]
    fn parses_listen_ports_of_every_address_form() {
        let out = "n*:5173\nn127.0.0.1:5173\nn[::1]:8080\nnlocalhost:3000\nn*:notaport\n";
        assert_eq!(parse_listen_ports(out), vec![3000, 5173, 8080]);
    }

    #[test]
    fn a_dead_or_invalid_group_is_not_alive() {
        assert!(!group_alive(0));
        assert!(!group_alive(u32::MAX));
        // Our own group exists.
        let own = group_of(std::process::id()).unwrap();
        assert!(group_alive(own));
    }

    #[test]
    fn the_daemons_own_group_is_never_signalled() {
        let own = group_of(std::process::id()).unwrap();
        assert!(!signal_group(own, 0), "signal_group must refuse the caller's own group");
        assert!(!signal_group(1, 0));
        assert!(!signal_group(0, 0));
    }

    #[test]
    fn the_group_check_refuses_a_group_none_of_the_recorded_pids_is_in() {
        let own = group_of(std::process::id()).unwrap();
        assert!(still_ours(own, &[std::process::id()]));
        assert!(!still_ours(own, &[u32::MAX - 1]));
        assert!(!still_ours(own, &[]));
    }
}
