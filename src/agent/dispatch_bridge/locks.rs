//! File-lock helpers for atomic read-modify-write of the dispatch state file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::types::DispatchState;

// ===== File-lock helpers =====

pub(crate) fn lock_path_for(state_path: &Path) -> PathBuf {
    let mut p = state_path.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

/// Read the dispatch state file at `state_path`, returning the default empty
/// state when the file is missing.
pub(crate) fn read_state_file(state_path: &Path) -> std::io::Result<DispatchState> {
    match std::fs::read_to_string(state_path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DispatchState::default()),
        Err(e) => Err(e),
    }
}

/// Acquire the lock, read-modify-write the state file. Returns the new state
/// on success. Does not fire WS notify — callers wanting that behavior must
/// go through `DispatchBridge::modify_state` instead. Used by the MCP
/// dispatch server (which runs in a separate stdio process and can't reach
/// the bridge in-memory).
pub(crate) fn modify_state_file<F: FnOnce(&mut DispatchState)>(
    state_path: &Path,
    f: F,
) -> std::io::Result<DispatchState> {
    let lock_path = lock_path_for(state_path);
    if !acquire_lock(&lock_path) {
        tracing::warn!("[dispatch] Failed to acquire state lock, skipping modification");
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "lock contention",
        ));
    }
    let result = (|| -> std::io::Result<DispatchState> {
        let mut state = read_state_file(state_path)?;
        f(&mut state);
        if let Some(parent) = state_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&state)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(state_path, json)?;
        Ok(state)
    })();
    let _ = std::fs::remove_file(&lock_path);
    result
}

/// Acquire a PID-stamped advisory lock by `O_CREAT|O_EXCL`-creating the lock
/// file. Retries up to 50× with ~10 ms backoff; on persistent failure checks
/// whether the holder's PID is still alive and clears the lock if not.
pub(crate) fn acquire_lock(lock_path: &Path) -> bool {
    use std::fs::OpenOptions;
    use std::io::Write;
    let pid = std::process::id();
    for _ in 0..50 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(mut f) => {
                let _ = write!(f, "{pid}");
                return true;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    // Stale-lock recovery: if the recorded PID is gone, clear and retry once.
    if let Ok(raw) = std::fs::read_to_string(lock_path) {
        if let Ok(holder) = raw.trim().parse::<i32>() {
            let alive = pid_alive(holder);
            if !alive {
                let _ = std::fs::remove_file(lock_path);
                if let Ok(mut f) = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(lock_path)
                {
                    let _ = write!(f, "{pid}");
                    return true;
                }
            }
        }
    }
    false
}

/// Is a process with this PID alive? Used for stale-lock recovery.
#[cfg(unix)]
fn pid_alive(pid: i32) -> bool {
    // Signal 0 = existence probe, no signal delivered.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Windows: probe via `tasklist` (no extra deps). On any probe failure assume
/// the holder is alive — stealing a live lock is worse than waiting.
#[cfg(not(unix))]
fn pid_alive(pid: i32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(true)
}
