//! Tie every process the daemon launches to the daemon's own lifetime.
//!
//! Windows has no SIGTERM: the desktop app stops the daemon with
//! `TerminateProcess`, so the graceful shutdown in `run_daemon` (which stops
//! runtimes and Space Apps) never runs there. Every child — engine runtimes,
//! llama.cpp, Space Apps, MCP servers — used to survive the app's Quit and keep
//! its port. A Job Object with `KILL_ON_JOB_CLOSE` closes that gap: the daemon
//! holds the only handle, so when it exits for any reason the kernel ends
//! everything still in the job. Children join automatically at spawn.
//!
//! Unix keeps its signal-driven shutdown; this is a no-op there.

/// `CREATE_BREAKAWAY_FROM_JOB`: a spawn that must outlive the daemon (the
/// user's web browser opened for a link) passes this so it leaves the job.
#[cfg(windows)]
pub const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

/// Put the current process in a kill-on-close Job Object. Call once at daemon
/// boot, before anything is spawned. Failure is logged, never fatal: the
/// daemon still runs, only without the orphan guarantee.
#[cfg(windows)]
pub fn bind_children_to_daemon() {
    use windows::core::PCWSTR;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    let result: Result<(), String> = unsafe {
        (|| {
            let job = CreateJobObjectW(None, PCWSTR::null()).map_err(|e| format!("CreateJobObjectW: {e}"))?;
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .map_err(|e| format!("SetInformationJobObject: {e}"))?;
            // Nested jobs (Windows 8+) let this succeed even when whoever
            // launched us already put us in a job of its own.
            AssignProcessToJobObject(job, GetCurrentProcess()).map_err(|e| format!("AssignProcessToJobObject: {e}"))?;
            // The handle is deliberately never closed: closing it is what
            // kills the job, and the OS closes it when this process ends.
            Ok(())
        })()
    };
    match result {
        Ok(()) => tracing::info!("[SenClaw] child processes are bound to the daemon's lifetime (Job Object)"),
        Err(e) => tracing::warn!("[SenClaw] cannot bind child processes to the daemon ({e}); they may outlive it"),
    }
}

#[cfg(not(windows))]
pub fn bind_children_to_daemon() {}
