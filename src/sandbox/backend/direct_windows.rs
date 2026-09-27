//! `direct` backend on Windows — AppContainer for isolation, Job Object for
//! limits. No Docker, no Hyper-V, no administrator rights.
//!
//! # UNVERIFIED AT RUNTIME
//!
//! This module type-checks against the `windows` crate but has **never been
//! executed on Windows** — it was written on macOS. Treat every behavioural
//! claim below as a design intent that still needs
//! `examples/win_sandbox_probe.rs` run on a real machine to confirm. The most
//! likely thing to be wrong is step 5 (granting the interpreter its own
//! directory); see `docs/sandbox-windows-research.md`.
//!
//! # The mechanism
//!
//! An AppContainer token grants **the intersection** of what the user may do
//! and what the container's SID is granted — so it is deny-by-default, the same
//! shape as a Seatbelt profile or a bubblewrap namespace. Two consequences make
//! it a good fit:
//!
//! * **Network is a capability.** Without `internetClient` in the token, the
//!   process cannot open an outbound connection. That is a cleaner mapping to
//!   the sandbox's network switch than the rule-based approach on macOS.
//! * **AppContainers run at Low Integrity**, so writes to ordinary user objects
//!   are refused before the DACL is even consulted.
//!
//! A Job Object supplies what macOS could not: an **enforced** memory ceiling,
//! a process-count cap, and `KILL_ON_JOB_CLOSE`, which makes the kernel reap
//! the whole tree if this app dies — stronger than the `setsid` + `killpg`
//! arrangement used on Unix.
//!
//! # Why no shell
//!
//! Windows has no `/bin/sh`, so the Unix "feed the script to `sh -s` on stdin"
//! trick does not carry over. Instead the interpreter is resolved **here, in
//! Rust**, and executed directly with the script file as an argument. That
//! keeps the no-interpolation property (the program is a file, never a command
//! line) and has a second benefit that matters more: knowing the interpreter's
//! absolute path is exactly what lets us grant its install directory to the
//! container, which is the step a user-installed Python needs in order to load
//! at all.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::time::Instant;

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GENERIC_EXECUTE, GENERIC_READ, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W, GRANT_ACCESS, SE_FILE_OBJECT,
    TRUSTEE_IS_GROUP, TRUSTEE_IS_SID, TRUSTEE_W,
};
use windows::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows::Win32::Security::{
    DeriveCapabilitySidsFromName, ACE_FLAGS, ACL, DACL_SECURITY_INFORMATION, PSID,
    SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject, TerminateJobObject,
    JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
    InitializeProcThreadAttributeList, ResumeThread, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

use super::{build_env, clamp, ExecSpec, Outcome};
use crate::sandbox::db::Sandbox;
use crate::sandbox::fsmode::FsMode;

/// Full control, for directories the sandbox may write.
const FILE_ALL: u32 = 0x001F01FF;

/// Turn a sandbox id into an AppContainer moniker.
///
/// The name is the identity of the profile on this machine, so it must be
/// stable per sandbox and short — the API rejects long monikers.
fn moniker(sandbox_id: &str) -> String {
    let short: String = sandbox_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(24).collect();
    format!("SenClawSbx{short}")
}

/// A per-sandbox AppContainer profile.
pub struct Container {
    pub sid: PSID,
    pub moniker: String,
}

impl Container {
    /// Create the profile, or derive the SID when it already exists.
    ///
    /// The profile persists on the machine across restarts, so "already
    /// exists" is the normal case for every run after the first.
    pub fn open(sandbox_id: &str) -> Result<Container, String> {
        let name = moniker(sandbox_id);
        let h = HSTRING::from(name.as_str());
        let display = HSTRING::from("SenClaw sandbox");
        unsafe {
            let sid = match CreateAppContainerProfile(
                PCWSTR(h.as_ptr()),
                PCWSTR(display.as_ptr()),
                PCWSTR(display.as_ptr()),
                None,
            ) {
                Ok(sid) => sid,
                Err(_) => DeriveAppContainerSidFromAppContainerName(PCWSTR(h.as_ptr()))
                    .map_err(|e| format!("cannot obtain the AppContainer SID: {e}"))?,
            };
            Ok(Container { sid, moniker: name })
        }
    }

    /// Grant this container access to a path.
    ///
    /// This is the Windows counterpart of `SYSTEM_READ_ROOTS` on Unix, and the
    /// direction is inverted: on macOS the interpreter is readable unless
    /// denied, here it is unreadable unless granted. A Python installed under
    /// `%LOCALAPPDATA%` carries no `ALL_APPLICATION_PACKAGES` ACE, so without
    /// this call the container cannot even load `python.exe`.
    pub fn grant(&self, path: &Path, write: bool) -> Result<(), String> {
        if !path.exists() {
            return Ok(()); // nothing to grant; not an error
        }
        let wide = HSTRING::from(path.as_os_str());
        unsafe {
            let mut ea = EXPLICIT_ACCESS_W {
                grfAccessPermissions: if write {
                    FILE_ALL
                } else {
                    GENERIC_READ.0 | GENERIC_EXECUTE.0
                },
                grfAccessMode: GRANT_ACCESS,
                // CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE, so the whole tree
                // is covered rather than only the directory entry itself.
                grfInheritance: ACE_FLAGS(3),
                Trustee: TRUSTEE_W {
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_GROUP,
                    ptstrName: PWSTR(self.sid.0 as *mut u16),
                    ..Default::default()
                },
            };
            let mut acl: *mut ACL = std::ptr::null_mut();
            SetEntriesInAclW(Some(std::slice::from_mut(&mut ea)), None, &mut acl)
                .ok()
                .map_err(|e| format!("building the ACL for `{}` failed: {e}", path.display()))?;
            SetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(acl),
                None,
            )
            .ok()
            .map_err(|e| format!("granting the sandbox access to `{}` failed: {e}", path.display()))?;
        }
        Ok(())
    }
}

/// Capability SIDs for the token. Empty means no network, which is the default.
fn capabilities(network: bool) -> Vec<SID_AND_ATTRIBUTES> {
    if !network {
        return Vec::new();
    }
    let name = HSTRING::from("internetClient");
    unsafe {
        let mut group_sids: *mut PSID = std::ptr::null_mut();
        let mut group_count = 0u32;
        let mut cap_sids: *mut PSID = std::ptr::null_mut();
        let mut cap_count = 0u32;
        if DeriveCapabilitySidsFromName(
            PCWSTR(name.as_ptr()),
            &mut group_sids,
            &mut group_count,
            &mut cap_sids,
            &mut cap_count,
        )
        .is_err()
            || cap_count == 0
        {
            return Vec::new();
        }
        // SE_GROUP_ENABLED (0x4) — the windows crate does not re-export this
        // one under Win32::Security, so it is spelled out from the SDK header.
        vec![SID_AND_ATTRIBUTES {
            Sid: *cap_sids,
            Attributes: 0x0000_0004,
        }]
    }
}

/// A Job Object carrying the sandbox's resource limits.
///
/// `KILL_ON_JOB_CLOSE` is the important flag: when the handle drops — including
/// because this app crashed — the kernel terminates everything in the job. The
/// Unix side has no equivalent guarantee.
fn make_job(memory_mb: i64, pids: i64) -> Result<OwnedHandle, String> {
    unsafe {
        let job = CreateJobObjectW(None, PCWSTR::null())
            .map_err(|e| format!("creating the Job Object failed: {e}"))?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        info.ProcessMemoryLimit = (memory_mb.max(64) as usize) * 1024 * 1024;
        info.BasicLimitInformation.ActiveProcessLimit = pids.clamp(16, 8192) as u32;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .map_err(|e| format!("setting Job Object limits failed: {e}"))?;
        Ok(OwnedHandle::from_raw_handle(job.0 as _))
    }
}

/// An inheritable pipe: (read end kept here, write end handed to the child).
fn pipe() -> Result<(OwnedHandle, OwnedHandle), String> {
    unsafe {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let mut r = HANDLE::default();
        let mut w = HANDLE::default();
        CreatePipe(&mut r, &mut w, Some(&sa), 0).map_err(|e| format!("creating a pipe failed: {e}"))?;
        Ok((
            OwnedHandle::from_raw_handle(r.0 as _),
            OwnedHandle::from_raw_handle(w.0 as _),
        ))
    }
}

/// Everything the container is allowed to touch, granted before launch.
fn grant_access(
    c: &Container,
    sb: &Sandbox,
    interpreter: Option<&Path>,
    allowlist: &[String],
) -> Result<(), String> {
    // Its own directory: read and write.
    c.grant(Path::new(&sb.workdir), true)?;

    // The interpreter's install tree. Granted whatever the isolation mode,
    // because without it nothing runs at all.
    if let Some(exe) = interpreter {
        if let Some(dir) = exe.parent() {
            c.grant(dir, false)?;
        }
    }

    for m in &sb.mounts {
        c.grant(Path::new(&m.source), !m.read_only)?;
    }

    if sb.fs_mode == FsMode::Allowlist {
        for p in allowlist.iter().filter(|p| !p.trim().is_empty()) {
            c.grant(Path::new(p), false)?;
        }
    }
    // `FsMode::Open` is deliberately NOT a blanket grant: handing a container
    // the whole disk would mean rewriting the DACL of every user directory,
    // which is a destructive, machine-wide change to make on someone's behalf.
    // On Windows `open` therefore behaves like `strict` plus the allowlist, and
    // `caps` reports that difference rather than pretending otherwise.
    Ok(())
}

/// Run one command in the sandbox.
///
/// `spec.argv` is the program to run, already resolved. `spec.script` is
/// written to a file and passed as its final argument by the caller, so nothing
/// here interpolates user text into a command line.
/// Every Win32 value used below — `HANDLE`, `PSID`, `PROCESS_INFORMATION` — is a
/// raw pointer, and therefore `!Send`. Holding one across an `.await` makes the
/// *caller's* future `!Send` too, and axum handlers, `rmcp` tools and the
/// scheduler all require `Send` futures: the first version of this file
/// awaited inside the launch sequence and broke the build of 24 unrelated call
/// sites, none of which mention Windows.
///
/// So the whole sequence runs inside one blocking task and never awaits within
/// it. Everything crossing the boundary is owned, plain data.
pub async fn exec(sb: &Sandbox, spec: &ExecSpec, allowlist: &[String]) -> Outcome {
    let start = Instant::now();
    let (sb, spec, allowlist) = (sb.clone(), spec.clone(), allowlist.to_vec());
    match tokio::task::spawn_blocking(move || exec_blocking(&sb, &spec, &allowlist)).await {
        Ok(outcome) => outcome,
        Err(e) => failed(format!("the sandbox task did not finish: {e}"), start),
    }
}

fn exec_blocking(sb: &Sandbox, spec: &ExecSpec, allowlist: &[String]) -> Outcome {
    let start = Instant::now();
    let argv = match spec.argv.as_ref().filter(|a| !a.is_empty()) {
        Some(a) => a.clone(),
        None => {
            return failed(
                "the direct backend on Windows needs a resolved argv (there is no /bin/sh)".into(),
                start,
            )
        }
    };

    let container = match Container::open(&sb.id) {
        Ok(c) => c,
        Err(e) => return failed(e, start),
    };
    let interpreter = PathBuf::from(&argv[0]);
    if let Err(e) = grant_access(&container, sb, Some(&interpreter), allowlist) {
        return failed(e, start);
    }

    let job = match make_job(sb.memory_mb, sb.pids_limit) {
        Ok(j) => j,
        Err(e) => return failed(e, start),
    };

    match spawn(&container, sb, &argv, spec, &job) {
        Ok(o) => o,
        Err(e) => failed(e, start),
    }
}

fn spawn(
    container: &Container,
    sb: &Sandbox,
    argv: &[String],
    spec: &ExecSpec,
    job: &OwnedHandle,
) -> Result<Outcome, String> {
    let start = Instant::now();
    let (out_r, out_w) = pipe()?;
    let (err_r, err_w) = pipe()?;
    let (in_r, in_w) = pipe()?;

    let mut caps = capabilities(sb.network);
    let mut sec = SECURITY_CAPABILITIES {
        AppContainerSid: container.sid,
        Capabilities: if caps.is_empty() {
            std::ptr::null_mut()
        } else {
            caps.as_mut_ptr()
        },
        CapabilityCount: caps.len() as u32,
        Reserved: 0,
    };

    // Command line: argv joined with quoting. The program path is app-chosen
    // and the script is a file path we generated, so there is no user text here.
    let cmdline: String = argv
        .iter()
        .map(|a| format!("\"{}\"", a.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(" ");
    let mut cmdline_w: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();

    let env_block = env_block(sb, spec);
    let cwd = HSTRING::from(sb.workdir.as_str());

    let pi = unsafe {
        let mut size = 0usize;
        let _ = InitializeProcThreadAttributeList(
            LPPROC_THREAD_ATTRIBUTE_LIST::default(),
            1,
            0,
            &mut size,
        );
        let mut buf = vec![0u8; size];
        let attrs = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr() as *mut _);
        InitializeProcThreadAttributeList(attrs, 1, 0, &mut size)
            .map_err(|e| format!("InitializeProcThreadAttributeList: {e}"))?;
        UpdateProcThreadAttribute(
            attrs,
            0,
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
            Some(&mut sec as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
            None,
            None,
        )
        .map_err(|e| format!("UpdateProcThreadAttribute: {e}"))?;

        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = HANDLE(in_r.as_raw_handle() as _);
        si.StartupInfo.hStdOutput = HANDLE(out_w.as_raw_handle() as _);
        si.StartupInfo.hStdError = HANDLE(err_w.as_raw_handle() as _);
        si.lpAttributeList = attrs;

        let mut pi = PROCESS_INFORMATION::default();
        // SUSPENDED so the process can be put in the job *before* it runs. A
        // process that starts first could spawn a child that escapes the job,
        // and the limits would silently apply to nothing.
        let r = CreateProcessW(
            None,
            PWSTR(cmdline_w.as_mut_ptr()),
            None,
            None,
            true,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
            Some(env_block.as_ptr() as *const core::ffi::c_void),
            PCWSTR(cwd.as_ptr()),
            &si.StartupInfo,
            &mut pi,
        );
        DeleteProcThreadAttributeList(attrs);
        r.map_err(|e| format!("cannot launch the process inside the AppContainer: {e}"))?;

        AssignProcessToJobObject(HANDLE(job.as_raw_handle() as _), pi.hProcess)
            .map_err(|e| format!("AssignProcessToJobObject: {e}"))?;
        ResumeThread(pi.hThread);
        let _ = CloseHandle(pi.hThread);
        pi
    };

    // The parent's copies of the child ends must go, or the reads never see EOF.
    drop(in_r);
    drop(out_w);
    drop(err_w);

    crate::sandbox::monitor::register(&sb.id, pi.dwProcessId);

    // Feed the program on stdin (kept for parity with Unix; harmless if unused).
    let script = spec.script.clone();
    let mut stdin_file = unsafe { std::fs::File::from_raw_handle(in_w.as_raw_handle()) };
    std::mem::forget(in_w);
    std::thread::spawn(move || {
        use std::io::Write;
        let _ = stdin_file.write_all(script.as_bytes());
    });

    let out_h = out_r.as_raw_handle() as isize;
    let err_h = err_r.as_raw_handle() as isize;
    std::mem::forget(out_r);
    std::mem::forget(err_r);
    // Plain threads, not `spawn_blocking`: this is already a blocking task, and
    // joining a tokio handle would need the `.await` this function must not have.
    let out_task = std::thread::spawn(move || read_all(out_h));
    let err_task = std::thread::spawn(move || read_all(err_h));

    let proc = pi.hProcess;
    let timeout_ms = spec.timeout_ms as u32;
    let waited = unsafe { WaitForSingleObject(proc, timeout_ms) };

    let timed_out = waited != WAIT_OBJECT_0;
    if timed_out {
        // One call takes the whole tree, which is what the Unix side needs a
        // process group and two signals to approximate.
        unsafe {
            let _ = TerminateJobObject(HANDLE(job.as_raw_handle() as _), 1);
        }
    }

    let stdout = out_task.join().unwrap_or_default();
    let stderr = err_task.join().unwrap_or_default();
    let mut code = 0u32;
    unsafe {
        let _ = GetExitCodeProcess(proc, &mut code);
        let _ = CloseHandle(proc);
    }
    crate::sandbox::monitor::unregister(&sb.id, pi.dwProcessId);

    let (stdout, t1) = clamp(stdout);
    let (stderr, t2) = clamp(stderr);
    Ok(Outcome {
        exit_code: if timed_out { None } else { Some(code as i32) },
        stdout,
        stderr: if timed_out {
            format!("Timed out after {} ms — the whole Job Object was terminated.", spec.timeout_ms)
        } else {
            stderr
        },
        truncated: t1 || t2,
        timed_out,
        duration_ms: start.elapsed().as_millis() as i64,
        isolation: "appcontainer".into(),
    })
}

fn read_all(handle: isize) -> String {
    use std::io::Read;
    let mut f = unsafe { std::fs::File::from_raw_handle(handle as _) };
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).to_string()
}

/// A UTF-16 environment block: `K=V\0K=V\0\0`, built rather than inherited so
/// the daemon's API keys never reach the sandbox — the same rule as Unix.
fn env_block(sb: &Sandbox, spec: &ExecSpec) -> Vec<u16> {
    let mut out = Vec::new();
    for (k, v) in build_env(sb, &spec.extra_env, &sb.workdir) {
        out.extend(format!("{k}={v}").encode_utf16());
        out.push(0);
    }
    out.push(0);
    out
}

fn failed(msg: String, start: Instant) -> Outcome {
    Outcome {
        exit_code: None,
        stdout: String::new(),
        stderr: msg,
        truncated: false,
        timed_out: false,
        duration_ms: start.elapsed().as_millis() as i64,
        isolation: "appcontainer".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_moniker_is_stable_short_and_alphanumeric() {
        let a = moniker("196f074f-6e75-4972-8387-1c1a707d603e");
        assert_eq!(a, moniker("196f074f-6e75-4972-8387-1c1a707d603e"));
        assert!(a.len() <= 34, "AppContainer monikers have a length limit");
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, moniker("00000000-0000-0000-0000-000000000000"));
    }

    #[test]
    fn no_network_means_no_capability_in_the_token() {
        assert!(capabilities(false).is_empty(), "an empty capability set IS the network block");
    }
}
