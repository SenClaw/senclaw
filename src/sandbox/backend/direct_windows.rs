//! `direct` backend on Windows — AppContainer for isolation, Job Object for
//! limits. No Docker, no Hyper-V, no administrator rights.
//!
//! # What has been checked
//!
//! The string and path logic (command-line quoting, the environment block, mount
//! guards) lives in [`super::winproc`] and is unit-tested on every OS. This
//! file is type-checked against the `windows` crate for `x86_64-pc-windows-*`.
//! Its behaviour — that an AppContainer really refuses a write, a read and a
//! socket — is exercised by the `tests` module below on a Windows machine
//! (`.github/workflows/sandbox-windows.yml`). It was written on macOS, so read a
//! green type-check as "the API is used correctly", not "the jail holds".
//! `docs/sandbox-windows-research.md` has the findings and what is still open.
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
//!   the sandbox's network switch than the rule-based approach on macOS. The
//!   same isolation also blocks *loopback*, so a sandboxed process cannot reach
//!   a service on this machine either way; that cannot be undone without
//!   administrator rights (`CheckNetIsolation`).
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
//!
//! # With the sandbox switched off
//!
//! `confine == false` runs the same launch sequence without the AppContainer:
//! an ordinary child process, still inside a Job Object (so the memory ceiling
//! and tree-kill hold) and still with a built environment. It is reported as
//! isolation `none`.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, SetHandleInformation, ERROR_ALREADY_EXISTS, FILETIME, HANDLE,
    HANDLE_FLAGS, HANDLE_FLAG_INHERIT, HLOCAL, WAIT_OBJECT_0,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::SystemInformation::{GetSystemTimeAsFileTime, GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, ACCESS_MODE,
    EXPLICIT_ACCESS_W, GRANT_ACCESS, REVOKE_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN,
    TRUSTEE_W,
};
use windows::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
    GetAppContainerFolderPath,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::Security::{
    AclSizeInformation, DeriveCapabilitySidsFromName, EqualSid, FreeSid, GetAce, GetAclInformation,
    ACE_FLAGS, ACE_HEADER, ACL, ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicProcessIdList, OpenJobObjectW,
    QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    JobObjectExtendedLimitInformation, JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
};

// Job access rights, spelled out from the SDK header: the windows crate does not
// export them under JobObjects.
const JOB_OBJECT_QUERY: u32 = 0x0004;
const JOB_OBJECT_TERMINATE: u32 = 0x0008;
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, GetProcessTimes,
    InitializeProcThreadAttributeList, OpenProcess, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    STARTF_USESTDHANDLES, STARTUPINFOEXW,
};

use super::{build_env, clamp, winproc, ExecSpec, Outcome, MAX_OUTPUT};
use crate::sandbox::db::Sandbox;
use crate::sandbox::fsmode::FsMode;

/// Full control, for directories the sandbox may write.
const FILE_ALL: u32 = 0x001F_01FF;

/// `FILE_GENERIC_READ | FILE_GENERIC_EXECUTE` spelled as specific rights.
/// Generic rights in an ACE are not mapped for a file by every code path that
/// reads the ACL later, so the ACE carries the file-specific bits instead.
const FILE_READ_EXEC: u32 = 0x0012_0089 | 0x0012_00A0;

/// `CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE`: the whole tree is covered, not
/// only the directory entry.
const INHERIT_TREE: u32 = 0x3;

/// Turn a sandbox id into an AppContainer moniker.
///
/// The name is the identity of the profile on this machine, so it must be
/// stable per sandbox and short — the API rejects monikers over 64 characters.
fn moniker(sandbox_id: &str) -> String {
    let short: String = sandbox_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(24).collect();
    format!("SenClawSbx{short}")
}

/// Grants already applied by this process, so a sandbox that runs a hundred
/// commands rewrites each directory's ACL once. Rewriting an ACL propagates to
/// every file under it; on a project with a `node_modules` that is seconds.
fn granted() -> &'static Mutex<HashSet<String>> {
    static G: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(HashSet::new()))
}

/// A per-sandbox AppContainer profile.
pub struct Container {
    sid: PSID,
    moniker: String,
}

impl Drop for Container {
    fn drop(&mut self) {
        // SIDs from CreateAppContainerProfile / Derive… are freed with FreeSid.
        unsafe {
            let _ = FreeSid(self.sid);
        }
    }
}

impl Container {
    /// Create the profile, or find it when it already exists.
    ///
    /// The profile persists on the machine across restarts, so "already
    /// exists" is the normal case for every run after the first. Only that one
    /// error falls through to the lookup: deriving a SID from a name always
    /// succeeds, even for a profile that was never made, so using it to paper
    /// over a real failure (a group policy that forbids profiles) would produce
    /// a container the system does not know and a launch error that names
    /// neither.
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
                Err(e) if e.code() == ERROR_ALREADY_EXISTS.to_hresult() => {
                    DeriveAppContainerSidFromAppContainerName(PCWSTR(h.as_ptr()))
                        .map_err(|e| format!("cannot obtain the AppContainer SID for `{name}`: {e}"))?
                }
                Err(e) => return Err(format!("cannot create the AppContainer profile `{name}`: {e}")),
            };
            Ok(Container { sid, moniker: name })
        }
    }

    /// The SID of a profile, without creating it. For cleaning up after one.
    fn derive(sandbox_id: &str) -> Result<Container, String> {
        let name = moniker(sandbox_id);
        let h = HSTRING::from(name.as_str());
        unsafe {
            let sid = DeriveAppContainerSidFromAppContainerName(PCWSTR(h.as_ptr()))
                .map_err(|e| format!("cannot obtain the AppContainer SID for `{name}`: {e}"))?;
            Ok(Container { sid, moniker: name })
        }
    }

    /// The profile's own storage folder (`…\AppData\Local\Packages\<moniker>\AC`):
    /// writable by the container, short, and removed with the profile.
    fn folder(&self) -> Option<String> {
        unsafe {
            let mut sid_text = PWSTR::null();
            ConvertSidToStringSidW(self.sid, &mut sid_text).ok()?;
            let folder = GetAppContainerFolderPath(PCWSTR(sid_text.0));
            let _ = LocalFree(HLOCAL(sid_text.0 as *mut core::ffi::c_void));
            let folder = folder.ok()?;
            let path = folder.to_string().ok();
            CoTaskMemFree(Some(folder.0 as *const core::ffi::c_void));
            path
        }
    }

    /// Grant this container access to a path (and everything under it).
    ///
    /// This is the Windows counterpart of `SYSTEM_READ_ROOTS` on Unix, and the
    /// direction is inverted: on macOS the interpreter is readable unless
    /// denied, here it is unreadable unless granted. A Python installed under
    /// `%LOCALAPPDATA%` carries no `ALL APPLICATION PACKAGES` ACE, so without
    /// this call the container cannot even load `python.exe`.
    ///
    /// The ACE is **added to the existing DACL**, never substituted for it: the
    /// directory may be the user's own project, and replacing its ACL with one
    /// entry would take the access of everyone else away.
    pub fn grant(&self, path: &Path, write: bool) -> Result<(), String> {
        let path_s = winproc::strip_verbatim(&path.to_string_lossy());
        if !Path::new(&path_s).exists() {
            return Ok(()); // nothing to grant; not an error
        }
        // Windows system dirs already grant ALL APPLICATION PACKAGES read/exec
        // by default; modifying their DACL requires elevation and fails with
        // 0x80070005.  Silently skip — the AppContainer can already reach them.
        if winproc::is_windows_dir(&path_s, &|v| std::env::var(v).ok()) {
            return Ok(());
        }
        let key = format!("{}|{}|{}", self.moniker, write, winproc::path_key(&path_s));
        if granted().lock().map(|g| g.contains(&key)).unwrap_or(false) {
            return Ok(());
        }
        let mask = if write { FILE_ALL } else { FILE_READ_EXEC };
        // Already there (a daemon restarted since, or another run of this sandbox
        // in another process): writing it again would re-propagate the DACL to
        // every file under the folder for nothing.
        let present = explicit_access(self.sid, &path_s).is_some_and(|m| m & mask == mask);
        if !present {
            edit_dacl(self.sid, &path_s, mask, GRANT_ACCESS)
                .map_err(|e| format!("granting the sandbox access to `{path_s}` failed: {e}"))?;
        }
        if let Ok(mut g) = granted().lock() {
            g.insert(key);
        }
        // A junction or symlink is checked as it is followed, so every link of a
        // chain needs the ACE as well as the final target (nvm's `.nodejs` is a
        // junction to `.link\nodejs`, a junction to the active version).
        if let Ok(next) = std::fs::read_link(&path_s) {
            let next = match Path::new(&path_s).parent() {
                Some(parent) if next.is_relative() => parent.join(next),
                _ => next,
            };
            self.grant(&next, write)?;
        }
        if let Ok(real) = std::fs::canonicalize(&path_s) {
            let real_s = winproc::strip_verbatim(&real.to_string_lossy());
            if winproc::path_key(&real_s) != winproc::path_key(&path_s) {
                self.grant(Path::new(&real_s), write)?;
            }
        }
        Ok(())
    }

    /// Take this container's access to a path away again.
    fn revoke(&self, path: &Path) -> Result<(), String> {
        let path_s = winproc::strip_verbatim(&path.to_string_lossy());
        if !Path::new(&path_s).exists() {
            return Ok(());
        }
        edit_dacl(self.sid, &path_s, 0, REVOKE_ACCESS)
    }
}

/// `path` followed through every junction or symlink to its target, the same
/// hops [`Container::grant`] gives an ACE.
fn link_chain(path: &str) -> Vec<String> {
    let mut chain = vec![path.to_string()];
    let mut cur = PathBuf::from(path);
    for _ in 0..8 {
        let Ok(next) = std::fs::read_link(&cur) else { break };
        let next = match cur.parent() {
            Some(parent) if next.is_relative() => parent.join(next),
            _ => next,
        };
        let next_s = winproc::strip_verbatim(&next.to_string_lossy());
        if chain.iter().any(|c| winproc::path_key(c) == winproc::path_key(&next_s)) {
            break;
        }
        chain.push(next_s);
        cur = next;
    }
    if let Ok(real) = std::fs::canonicalize(path) {
        let real_s = winproc::strip_verbatim(&real.to_string_lossy());
        if !chain.iter().any(|c| winproc::path_key(c) == winproc::path_key(&real_s)) {
            chain.push(real_s);
        }
    }
    chain
}

/// Does the DACL of `path` carry an ACE of its own (not inherited) for `sid`?
/// Read before revoking, because writing a DACL back re-propagates it to every
/// file under the folder — seconds on a `node_modules`.
fn has_explicit_ace(sid: PSID, path: &str) -> bool {
    explicit_access(sid, path).is_some()
}

/// The rights `sid`'s own (not inherited) allow-ACEs on `path` add up to, or
/// `None` when it has no ACE of its own there at all.
fn explicit_access(sid: PSID, path: &str) -> Option<u32> {
    let wide = HSTRING::from(path);
    unsafe {
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let read = GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut dacl),
            None,
            &mut descriptor,
        );
        let mut found: Option<u32> = None;
        let mut info = ACL_SIZE_INFORMATION::default();
        if read.is_ok()
            && !dacl.is_null()
            && GetAclInformation(
                dacl,
                &mut info as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
            .is_ok()
        {
            for i in 0..info.AceCount {
                let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
                if GetAce(dacl, i, &mut ace).is_err() {
                    continue;
                }
                let header = &*(ace as *const ACE_HEADER);
                // Allowed (0) and denied (1) ACEs keep the SID after the 4-byte
                // header and the 4-byte mask. 0x10 is INHERITED_ACE.
                if header.AceType <= 1
                    && header.AceFlags & 0x10 == 0
                    && EqualSid(sid, PSID((ace as *mut u8).add(8) as *mut core::ffi::c_void)).is_ok()
                {
                    let mask = *((ace as *const u8).add(4) as *const u32);
                    let allowed = if header.AceType == 0 { mask } else { 0 };
                    found = Some(found.unwrap_or(0) | allowed);
                }
            }
        }
        if !descriptor.0.is_null() {
            let _ = LocalFree(HLOCAL(descriptor.0));
        }
        found
    }
}

/// Read the DACL of `path`, apply one entry for `sid` to it, write it back.
fn edit_dacl(sid: PSID, path: &str, mask: u32, mode: ACCESS_MODE) -> Result<(), String> {
    let wide = HSTRING::from(path);
    unsafe {
        let mut old: *mut ACL = std::ptr::null_mut();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        GetNamedSecurityInfoW(
            PCWSTR(wide.as_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(&mut old),
            None,
            &mut descriptor,
        )
        .ok()
        .map_err(|e| format!("reading the current permissions failed: {e}"))?;

        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: mask,
            grfAccessMode: mode,
            grfInheritance: if mask == 0 { ACE_FLAGS(0) } else { ACE_FLAGS(INHERIT_TREE) },
            Trustee: TRUSTEE_W {
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: PWSTR(sid.0 as *mut u16),
                ..Default::default()
            },
        };
        let mut merged: *mut ACL = std::ptr::null_mut();
        let built = SetEntriesInAclW(Some(&[ea]), Some(old as *const ACL), &mut merged).ok();
        let result = match built {
            Ok(()) => SetNamedSecurityInfoW(
                PCWSTR(wide.as_ptr()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(merged as *const ACL),
                None,
            )
            .ok()
            .map_err(|e| e.to_string()),
            Err(e) => Err(format!("building the new permissions failed: {e}")),
        };
        // `old` points into `descriptor`; `merged` is its own allocation.
        if !merged.is_null() {
            let _ = LocalFree(HLOCAL(merged as *mut core::ffi::c_void));
        }
        if !descriptor.0.is_null() {
            let _ = LocalFree(HLOCAL(descriptor.0));
        }
        result
    }
}

/// The `internetClient` capability SID, derived once for the process.
///
/// The system allocates the SID; it is kept for the life of the process rather
/// than freed and re-derived on every run.
fn internet_client() -> Option<PSID> {
    static SID: OnceLock<Option<usize>> = OnceLock::new();
    let raw = *SID.get_or_init(|| unsafe {
        let name = HSTRING::from("internetClient");
        let mut group_sids: *mut PSID = std::ptr::null_mut();
        let mut group_count = 0u32;
        let mut cap_sids: *mut PSID = std::ptr::null_mut();
        let mut cap_count = 0u32;
        let derived = DeriveCapabilitySidsFromName(
            PCWSTR(name.as_ptr()),
            &mut group_sids,
            &mut group_count,
            &mut cap_sids,
            &mut cap_count,
        );
        if derived.is_err() || cap_count == 0 || cap_sids.is_null() {
            None
        } else {
            Some((*cap_sids).0 as usize)
        }
    });
    raw.map(|p| PSID(p as *mut core::ffi::c_void))
}

/// Capability SIDs for the token. Empty means no network, which is the default.
fn capabilities(network: bool) -> Vec<SID_AND_ATTRIBUTES> {
    if !network {
        return Vec::new();
    }
    match internet_client() {
        // SE_GROUP_ENABLED (0x4) — the windows crate does not re-export this
        // one under Win32::Security, so it is spelled out from the SDK header.
        Some(sid) => vec![SID_AND_ATTRIBUTES { Sid: sid, Attributes: 0x0000_0004 }],
        None => Vec::new(),
    }
}

/// A Job Object carrying the sandbox's resource limits.
///
/// `KILL_ON_JOB_CLOSE` is the important flag: when the handle drops — including
/// because this app crashed — the kernel terminates everything in the job. The
/// Unix side has no equivalent guarantee.
///
/// The job is named after the sandbox and the first free slot, so a process
/// other than this one (the MCP server stopping a run the daemon started) can
/// find it. With every slot taken the job is unnamed: the run still works, it
/// is only out of reach of another process.
fn make_job(sandbox_id: &str, memory_mb: i64, pids: i64) -> Result<OwnedHandle, String> {
    let (bytes, active) = winproc::job_limits(memory_mb, pids);
    unsafe {
        let mut named = None;
        for slot in 0..winproc::MAX_NAMED_RUNS {
            let name = HSTRING::from(winproc::job_name(sandbox_id, slot));
            let Ok(h) = CreateJobObjectW(None, PCWSTR(name.as_ptr())) else { continue };
            if GetLastError() == ERROR_ALREADY_EXISTS {
                // Another run holds this slot; this handle is to *its* job.
                let _ = CloseHandle(h);
                continue;
            }
            named = Some(h);
            break;
        }
        let job = match named {
            Some(h) => h,
            None => CreateJobObjectW(None, PCWSTR::null())
                .map_err(|e| format!("creating the Job Object failed: {e}"))?,
        };
        // Owned from here, so an early return closes it.
        let owned = OwnedHandle::from_raw_handle(job.0 as _);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            // No crash dialog waiting for someone to click it.
            | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        info.ProcessMemoryLimit = bytes;
        info.BasicLimitInformation.ActiveProcessLimit = active;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .map_err(|e| format!("setting Job Object limits failed: {e}"))?;
        Ok(owned)
    }
}

/// The jobs of a sandbox's runs that are live right now, in this process or any
/// other of this user's (opened by name).
fn open_jobs(sandbox_id: &str) -> Vec<OwnedHandle> {
    (0..winproc::MAX_NAMED_RUNS)
        .filter_map(|slot| unsafe {
            let name = HSTRING::from(winproc::job_name(sandbox_id, slot));
            OpenJobObjectW(JOB_OBJECT_TERMINATE | JOB_OBJECT_QUERY, false, PCWSTR(name.as_ptr()))
                .ok()
                .map(|h| OwnedHandle::from_raw_handle(h.0 as _))
        })
        .collect()
}

/// The processes in a job.
fn job_pids(job: &OwnedHandle) -> Vec<u32> {
    const CAP: usize = 1024;
    // Header (two u32) then the ids, each a ULONG_PTR; u64 words keep it aligned.
    let mut buf = vec![0u64; 1 + CAP];
    unsafe {
        if QueryInformationJobObject(
            HANDLE(job.as_raw_handle() as _),
            JobObjectBasicProcessIdList,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            (buf.len() * 8) as u32,
            None,
        )
        .is_err()
        {
            return Vec::new();
        }
        let list = &*(buf.as_ptr() as *const JOBOBJECT_BASIC_PROCESS_ID_LIST);
        let n = (list.NumberOfProcessIdsInList as usize).min(CAP);
        std::slice::from_raw_parts(list.ProcessIdList.as_ptr(), n).iter().map(|p| *p as u32).collect()
    }
}

/// Stop every run of a sandbox, whichever process started it. Returns how many
/// runs were stopped.
///
/// A run's job is named a moment before its process joins it. Terminating the
/// job in that moment ends nothing, and the process then joins and runs on — so
/// an empty job is given up to two seconds to fill first.
pub fn kill_all(sandbox_id: &str) -> usize {
    open_jobs(sandbox_id)
        .iter()
        .filter(|job| {
            let t = Instant::now();
            while job_pids(job).is_empty() && t.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_millis(20));
            }
            unsafe { TerminateJobObject(HANDLE(job.as_raw_handle() as _), 1).is_ok() }
        })
        .count()
}

/// Stop one process — only one that is inside a run of this sandbox. `Ok(false)`
/// means it is not, and nothing was touched.
pub fn kill_pid(sandbox_id: &str, pid: u32) -> Result<bool, String> {
    if !open_jobs(sandbox_id).iter().any(|job| job_pids(job).contains(&pid)) {
        return Ok(false);
    }
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, false, pid).map_err(|e| format!("cannot open process {pid}: {e}"))?;
        let done = TerminateProcess(h, 1).map_err(|e| format!("cannot stop process {pid}: {e}"));
        let _ = CloseHandle(h);
        done.map(|_| true)
    }
}

fn filetime_100ns(t: FILETIME) -> u64 {
    ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
}

/// What a sandbox's runs are running: every process of every live job, with
/// CPU measured over a short window. Blocks for that window.
pub fn processes(sandbox_id: &str) -> Vec<crate::sandbox::monitor::Proc> {
    let pids: Vec<u32> = open_jobs(sandbox_id).iter().flat_map(job_pids).collect();
    if pids.is_empty() {
        return Vec::new();
    }
    // Parent and image name, from one snapshot of the process table.
    let mut names: std::collections::HashMap<u32, (u32, String)> = std::collections::HashMap::new();
    unsafe {
        if let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
            let mut ok = Process32FirstW(snap, &mut e).is_ok();
            while ok {
                if pids.contains(&e.th32ProcessID) {
                    let len = e.szExeFile.iter().position(|c| *c == 0).unwrap_or(e.szExeFile.len());
                    names.insert(e.th32ProcessID, (e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..len])));
                }
                ok = Process32NextW(snap, &mut e).is_ok();
            }
            let _ = CloseHandle(snap);
        }
    }
    let total_mem = unsafe {
        let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        if GlobalMemoryStatusEx(&mut m).is_ok() { m.ullTotalPhys } else { 0 }
    };
    let times = |h: HANDLE| unsafe {
        let (mut c, mut x, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        GetProcessTimes(h, &mut c, &mut x, &mut k, &mut u)
            .ok()
            .map(|_| (filetime_100ns(c), filetime_100ns(k) + filetime_100ns(u)))
    };
    let handles: Vec<(u32, HANDLE)> = pids
        .iter()
        .filter_map(|pid| unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, *pid).ok().map(|h| (*pid, h)) })
        .collect();
    let before: Vec<Option<(u64, u64)>> = handles.iter().map(|(_, h)| times(*h)).collect();
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_millis(250));
    let window = t0.elapsed().as_nanos() as u64 / 100;
    let now = unsafe { filetime_100ns(GetSystemTimeAsFileTime()) };
    let mut out = Vec::new();
    for ((pid, h), first) in handles.iter().zip(before) {
        let second = times(*h);
        let rss = unsafe {
            let mut pmc = PROCESS_MEMORY_COUNTERS { cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
            if K32GetProcessMemoryInfo(*h, &mut pmc, pmc.cb).as_bool() { pmc.WorkingSetSize as u64 } else { 0 }
        };
        unsafe {
            let _ = CloseHandle(*h);
        }
        let (ppid, command) = names.get(pid).cloned().unwrap_or((0, String::new()));
        let (cpu, elapsed) = match (first, second) {
            (Some((created, a)), Some((_, b))) => (
                winproc::cpu_percent(b.saturating_sub(a), window),
                winproc::ps_elapsed(now.saturating_sub(created) / 10_000_000),
            ),
            _ => (0.0, String::new()),
        };
        out.push(crate::sandbox::monitor::Proc {
            pid: *pid,
            ppid,
            pgid: 0,
            cpu,
            mem_percent: if total_mem > 0 { (rss as f64 / total_mem as f64 * 1000.0).round() / 10.0 } else { 0.0 },
            rss_mb: (rss as f64 / 1_048_576.0 * 10.0).round() / 10.0,
            elapsed,
            command,
        });
    }
    out
}

/// One anonymous pipe: the end this process keeps and the end the child gets.
struct Pipe {
    parent: OwnedHandle,
    child: OwnedHandle,
}

/// A pipe whose child end is inheritable and whose parent end is not.
///
/// `CreatePipe` with an inheritable attribute marks **both** ends inheritable.
/// A child that inherits the parent's end of its own stdout pipe holds the pipe
/// open from the wrong side, and the reader never sees end-of-file.
fn pipe(parent_reads: bool) -> Result<Pipe, String> {
    unsafe {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: true.into(),
        };
        let mut r = HANDLE::default();
        let mut w = HANDLE::default();
        CreatePipe(&mut r, &mut w, Some(&sa), 0).map_err(|e| format!("creating a pipe failed: {e}"))?;
        let (r, w) = (OwnedHandle::from_raw_handle(r.0 as _), OwnedHandle::from_raw_handle(w.0 as _));
        let (parent, child) = if parent_reads { (r, w) } else { (w, r) };
        SetHandleInformation(HANDLE(parent.as_raw_handle() as _), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))
            .map_err(|e| format!("making a pipe private failed: {e}"))?;
        Ok(Pipe { parent, child })
    }
}

/// Everything the container is allowed to touch, granted before launch.
///
/// Only the sandbox's own directory and a mount that was asked for are fatal
/// when they cannot be granted. A read grant that fails (the interpreter lives
/// in `C:\Program Files`, which an ordinary user may not edit — and does not
/// need to, since it is already readable by every AppContainer) is noted and
/// the run goes on; the note is shown if the run then fails.
fn grant_access(
    c: &Container,
    sb: &Sandbox,
    interpreter: &Path,
    allowlist: &[String],
    notes: &mut Vec<String>,
) -> Result<(), String> {
    // Its own directory: read and write.
    c.grant(Path::new(&sb.workdir), true)?;

    // The interpreter's install tree. Granted whatever the isolation mode,
    // because without it nothing runs at all.
    let mut read_roots: Vec<PathBuf> = Vec::new();
    if let Some(dir) = interpreter.parent() {
        read_roots.push(dir.to_path_buf());
        // A virtualenv's `python.exe` forwards to the interpreter named in
        // `pyvenv.cfg`, which sits beside `Scripts` (or in the same folder).
        for cfg in [dir.join("pyvenv.cfg"), dir.join("..").join("pyvenv.cfg")] {
            if let Some(home) = std::fs::read_to_string(&cfg).ok().and_then(|t| winproc::venv_home(&t)) {
                read_roots.push(PathBuf::from(home));
                break;
            }
        }
    }
    // A cmd.exe script may call any program on PATH (npm, node, git, …), so the
    // PATH folders are granted too — except those a mount would be refused for:
    // system folders are already readable, and credential trees (`.senclaw`,
    // `AppData\Local\Microsoft`) must never be opened to a sandbox.
    if interpreter
        .file_name()
        .map(|n| n.to_string_lossy().eq_ignore_ascii_case("cmd.exe"))
        .unwrap_or(false)
    {
        let host = |v: &str| std::env::var(v).ok();
        let mut seen: HashSet<String> = HashSet::new();
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path_var) {
                let dir_s = winproc::strip_verbatim(&dir.to_string_lossy());
                if dir_s.trim().is_empty()
                    || winproc::forbidden_mount_reason(&dir_s, &host).is_some()
                    || !seen.insert(winproc::path_key(&dir_s))
                {
                    continue;
                }
                read_roots.push(PathBuf::from(dir_s));
            }
        }
    }

    if sb.fs_mode == FsMode::Allowlist {
        read_roots.extend(allowlist.iter().filter(|p| !p.trim().is_empty()).map(PathBuf::from));
    }
    for root in &read_roots {
        if let Err(e) = c.grant(root, false) {
            notes.push(e);
        }
    }

    for m in &sb.mounts {
        c.grant(Path::new(&m.source), !m.read_only)?;
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
///
/// Every Win32 value used below — `HANDLE`, `PSID`, `PROCESS_INFORMATION` — is a
/// raw pointer, and therefore `!Send`. Holding one across an `.await` makes the
/// *caller's* future `!Send` too, and axum handlers, `rmcp` tools and the
/// scheduler all require `Send` futures: the first version of this file
/// awaited inside the launch sequence and broke the build of 24 unrelated call
/// sites, none of which mention Windows.
///
/// So the whole sequence runs inside one blocking task and never awaits within
/// it. Everything crossing the boundary is owned, plain data.
///
/// `confine == false` skips the AppContainer (the sandbox switch is off).
pub async fn exec(sb: &Sandbox, spec: &ExecSpec, allowlist: &[String], confine: bool) -> Outcome {
    let start = Instant::now();
    let (sb, spec, allowlist) = (sb.clone(), spec.clone(), allowlist.to_vec());
    match tokio::task::spawn_blocking(move || exec_blocking(&sb, &spec, &allowlist, confine)).await {
        Ok(outcome) => outcome,
        Err(e) => failed(format!("the sandbox task did not finish: {e}"), start, confine),
    }
}

fn exec_blocking(sb: &Sandbox, spec: &ExecSpec, allowlist: &[String], confine: bool) -> Outcome {
    let start = Instant::now();
    let argv = match spec.argv.as_ref().filter(|a| !a.is_empty()) {
        Some(a) => a.clone(),
        None => {
            return failed(
                "the direct backend on Windows needs a resolved argv (there is no /bin/sh)".into(),
                start,
                confine,
            )
        }
    };

    // `TEMP` points here; a tool that creates a temp file in a folder that does
    // not exist fails in ways that never mention the folder.
    let workdir = winproc::strip_verbatim(&sb.workdir);
    if let Err(e) = std::fs::create_dir_all(Path::new(&workdir).join(".tmp")) {
        return failed(format!("cannot create sandbox directory: {e}"), start, confine);
    }

    let mut notes: Vec<String> = Vec::new();
    let container = if confine {
        let c = match Container::open(&sb.id) {
            Ok(c) => c,
            Err(e) => return failed(with_way_out(e), start, confine),
        };
        if let Err(e) = grant_access(&c, sb, Path::new(&argv[0]), allowlist, &mut notes) {
            return failed(with_way_out(e), start, confine);
        }
        Some(c)
    } else {
        None
    };

    let job = match make_job(&sb.id, sb.memory_mb, sb.pids_limit) {
        Ok(j) => j,
        Err(e) => return failed(e, start, confine),
    };

    match spawn(container.as_ref(), sb, &workdir, &argv, spec, &job, &notes) {
        Ok(o) => o,
        Err(e) => {
            let e = with_notes(e, &notes);
            failed(if confine { with_way_out(e) } else { e }, start, confine)
        }
    }
}

/// A launch failure inside the container always names the way out: on a machine
/// where the AppContainer will not start, the sandbox switch is the only thing
/// that lets code run at all.
fn with_way_out(msg: String) -> String {
    format!("{msg} (Plugins → Sandbox: switch the sandbox off to run without it.)")
}

fn with_notes(mut msg: String, notes: &[String]) -> String {
    for n in notes {
        msg.push_str("\nnote: ");
        msg.push_str(n);
    }
    msg
}

/// Reads a pipe to its end, keeping at most `cap` bytes. The rest is read and
/// dropped so a chatty child never blocks on a full pipe.
fn read_capped(mut f: File, cap: usize) -> (Vec<u8>, bool) {
    use std::io::Read;
    let mut kept: Vec<u8> = Vec::new();
    let mut over = false;
    let mut chunk = [0u8; 8192];
    loop {
        match f.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
                if n > room {
                    over = true;
                }
            }
        }
    }
    (kept, over)
}

fn spawn(
    container: Option<&Container>,
    sb: &Sandbox,
    workdir: &str,
    argv: &[String],
    spec: &ExecSpec,
    job: &OwnedHandle,
    notes: &[String],
) -> Result<Outcome, String> {
    let start = Instant::now();
    let confine = container.is_some();

    let out = pipe(true)?;
    let err = pipe(true)?;
    // The child reads its stdin from a pipe whose write end is closed straight
    // away: any command that asks for input sees end-of-file at once, instead of
    // waiting on a handle nobody will write to.
    let input = pipe(false)?;
    drop(input.parent);

    let mut caps = capabilities(sb.network);
    let mut sec = SECURITY_CAPABILITIES {
        AppContainerSid: container.map(|c| c.sid).unwrap_or_default(),
        Capabilities: if caps.is_empty() { std::ptr::null_mut() } else { caps.as_mut_ptr() },
        CapabilityCount: caps.len() as u32,
        Reserved: 0,
    };

    let mut cmdline_w: Vec<u16> =
        winproc::command_line(argv).encode_utf16().chain(std::iter::once(0)).collect();
    let mut vars = winproc::windows_env(
        build_env(sb, &spec.extra_env, workdir),
        workdir,
        &|name: &str| std::env::var(name).ok(),
    );
    // Windows appends `\Packages\<moniker>\AC` to LOCALAPPDATA when it starts a
    // process in the container, so the folder three levels above the container's
    // own is what makes the child's LOCALAPPDATA that folder exactly.
    let base = container
        .and_then(|c| c.folder())
        .and_then(|f| Path::new(&f).ancestors().nth(3).map(|p| p.to_string_lossy().into_owned()));
    if let Some(base) = base {
        winproc::shorten_local_appdata(&mut vars, &base);
    }
    if confine {
        winproc::node_without_realpath(&mut vars);
    }
    let env = winproc::env_block(&vars);
    let cwd = HSTRING::from(workdir);

    // Exactly these three handles cross into the child. Without the list it
    // inherits every inheritable handle in this process, including the pipe ends
    // of any other sandbox starting on another thread at the same moment.
    let child_handles = [
        HANDLE(input.child.as_raw_handle() as _),
        HANDLE(out.child.as_raw_handle() as _),
        HANDLE(err.child.as_raw_handle() as _),
    ];

    let pi = unsafe {
        let attribute_count = if confine { 2u32 } else { 1u32 };
        let mut size = 0usize;
        // The first call is meant to fail: it reports the buffer size needed.
        let _ = InitializeProcThreadAttributeList(
            LPPROC_THREAD_ATTRIBUTE_LIST::default(),
            attribute_count,
            0,
            &mut size,
        );
        // `u64` words, so the list starts on an 8-byte boundary.
        let mut buf = vec![0u64; size.div_ceil(8).max(1)];
        let attrs = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr() as *mut _);
        InitializeProcThreadAttributeList(attrs, attribute_count, 0, &mut size)
            .map_err(|e| format!("InitializeProcThreadAttributeList: {e}"))?;
        let built = (|| -> Result<(), String> {
            UpdateProcThreadAttribute(
                attrs,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(child_handles.as_ptr() as *const core::ffi::c_void),
                std::mem::size_of_val(&child_handles),
                None,
                None,
            )
            .map_err(|e| format!("UpdateProcThreadAttribute(handle list): {e}"))?;
            if confine {
                UpdateProcThreadAttribute(
                    attrs,
                    0,
                    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES as usize,
                    Some(&mut sec as *mut _ as *const core::ffi::c_void),
                    std::mem::size_of::<SECURITY_CAPABILITIES>(),
                    None,
                    None,
                )
                .map_err(|e| format!("UpdateProcThreadAttribute(AppContainer): {e}"))?;
            }
            Ok(())
        })();
        if let Err(e) = built {
            DeleteProcThreadAttributeList(attrs);
            return Err(e);
        }

        let mut si = STARTUPINFOEXW::default();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = child_handles[0];
        si.StartupInfo.hStdOutput = child_handles[1];
        si.StartupInfo.hStdError = child_handles[2];
        si.lpAttributeList = attrs;

        let mut pi = PROCESS_INFORMATION::default();
        // SUSPENDED so the process can be put in the job *before* it runs. A
        // process that starts first could spawn a child that escapes the job,
        // and the limits would silently apply to nothing. NO_WINDOW because the
        // daemon has no console of its own: a console program would otherwise
        // open a visible one for every run.
        let r = CreateProcessW(
            None,
            PWSTR(cmdline_w.as_mut_ptr()),
            None,
            None,
            true,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            Some(env.as_ptr() as *const core::ffi::c_void),
            PCWSTR(cwd.as_ptr()),
            &si.StartupInfo,
            &mut pi,
        );
        DeleteProcThreadAttributeList(attrs);
        r.map_err(|e| {
            if confine {
                format!("cannot launch the process inside the AppContainer: {e}")
            } else {
                format!("cannot launch the process: {e}")
            }
        })?;

        if let Err(e) = AssignProcessToJobObject(HANDLE(job.as_raw_handle() as _), pi.hProcess) {
            // Never leave a suspended process behind with nothing to reap it.
            let _ = TerminateProcess(pi.hProcess, 1);
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(pi.hProcess);
            return Err(format!("AssignProcessToJobObject: {e}"));
        }
        ResumeThread(pi.hThread);
        let _ = CloseHandle(pi.hThread);
        pi
    };

    // The parent's copies of the child ends must go, or the reads never see EOF.
    drop(input.child);
    drop(out.child);
    drop(err.child);

    crate::sandbox::monitor::register(&sb.id, pi.dwProcessId);

    // Plain threads, not `spawn_blocking`: this is already a blocking task, and
    // joining a tokio handle would need the `.await` this function must not have.
    let cap = MAX_OUTPUT + 8;
    let (out_tx, out_rx) = mpsc::channel();
    let (err_tx, err_rx) = mpsc::channel();
    let out_file = File::from(out.parent);
    let err_file = File::from(err.parent);
    std::thread::spawn(move || {
        let _ = out_tx.send(read_capped(out_file, cap));
    });
    std::thread::spawn(move || {
        let _ = err_tx.send(read_capped(err_file, cap));
    });

    // One deadline covers the process *and* its pipes. A script that starts
    // something and exits while it still holds the pipes is waited for until the
    // deadline, then ended — the same rule the Unix side applies.
    let deadline = start + Duration::from_millis(spec.timeout_ms);
    let job_handle = HANDLE(job.as_raw_handle() as _);
    let waited = unsafe { WaitForSingleObject(pi.hProcess, winproc::wait_millis(spec.timeout_ms)) };
    let mut timed_out = waited != WAIT_OBJECT_0;
    let mut stdout = (Vec::new(), false);
    let mut stderr = (Vec::new(), false);
    if !timed_out {
        match out_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(v) => stdout = v,
            Err(_) => timed_out = true,
        }
    }
    if !timed_out {
        match err_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(v) => stderr = v,
            Err(_) => timed_out = true,
        }
    }
    if timed_out {
        // One call takes the whole tree, which is what the Unix side needs a
        // process group and two signals to approximate.
        unsafe {
            let _ = TerminateJobObject(job_handle, 1);
        }
        // Everything holding a pipe is dead now; the readers finish at once. The
        // bound only guards against a handle that escaped the job.
        let grace = Duration::from_secs(5);
        if let Ok(v) = out_rx.recv_timeout(grace) {
            stdout = v;
        }
        if let Ok(v) = err_rx.recv_timeout(grace) {
            stderr = v;
        }
    }

    let mut code = 0u32;
    unsafe {
        let _ = GetExitCodeProcess(pi.hProcess, &mut code);
        let _ = CloseHandle(pi.hProcess);
    }
    crate::sandbox::monitor::unregister(&sb.id, pi.dwProcessId);

    let (over_out, over_err) = (stdout.1, stderr.1);
    let (stdout, t1) = clamp(String::from_utf8_lossy(&stdout.0).to_string());
    let (mut stderr_text, t2) = clamp(String::from_utf8_lossy(&stderr.0).to_string());
    if timed_out {
        stderr_text = format!("Timed out after {} ms — the whole Job Object was terminated.", spec.timeout_ms);
    }
    if !notes.is_empty() && (timed_out || code != 0) {
        stderr_text = with_notes(stderr_text, notes);
    }
    Ok(Outcome {
        exit_code: if timed_out { None } else { Some(code as i32) },
        stdout,
        stderr: stderr_text,
        truncated: t1 || t2 || over_out || over_err,
        timed_out,
        duration_ms: start.elapsed().as_millis() as i64,
        isolation: isolation_label(confine),
        ..Default::default()
    })
}

fn isolation_label(confine: bool) -> String {
    if confine { "appcontainer" } else { "none" }.to_string()
}

fn failed(msg: String, start: Instant, confine: bool) -> Outcome {
    Outcome {
        exit_code: None,
        stdout: String::new(),
        stderr: msg,
        truncated: false,
        timed_out: false,
        duration_ms: start.elapsed().as_millis() as i64,
        isolation: isolation_label(confine),
        ..Default::default()
    }
}

/// Drop what a sandbox left on this machine: its AppContainer profile (a folder
/// under `%LOCALAPPDATA%\Packages` and a registry entry each) and the access it
/// was granted on folders that outlive it — the interpreter's folder, the PATH
/// folders a `cmd.exe` run got, a mount's source, or the project an agent
/// sandbox was pointed at. Best effort throughout; a workspace that is about to
/// be deleted is not worth an ACL rewrite.
pub fn forget(sb: &Sandbox) {
    // A daemon restarted since the grants has no record of them. PATH is where a
    // `cmd.exe` run got its read grants, and where interpreters usually live; an
    // interpreter elsewhere is only known to the process that granted it.
    let path_dirs: Vec<String> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| winproc::strip_verbatim(&d.to_string_lossy())).collect())
        .unwrap_or_default();
    forget_with(sb, path_dirs);
}

fn forget_with(sb: &Sandbox, mut candidates: Vec<String>) {
    let prefix = format!("{}|", moniker(&sb.id));
    if let Ok(mut g) = granted().lock() {
        candidates.extend(
            g.iter()
                .filter(|k| k.starts_with(&prefix))
                .filter_map(|k| k.splitn(3, '|').nth(2).map(str::to_string)),
        );
        g.retain(|k| !k.starts_with(&prefix));
    }
    let workspaces = crate::sandbox::config::workspaces_dir();
    if !Path::new(&sb.workdir).starts_with(&workspaces) {
        candidates.push(sb.workdir.clone());
    }
    candidates.extend(sb.mounts.iter().map(|m| m.source.clone()));
    if let Ok(c) = Container::derive(&sb.id) {
        let mut seen: HashSet<String> = HashSet::new();
        for p in candidates.iter().filter(|p| !p.trim().is_empty()) {
            for link in link_chain(p) {
                if seen.insert(winproc::path_key(&link)) && has_explicit_ace(c.sid, &link) {
                    let _ = c.revoke(Path::new(&link));
                }
            }
        }
    }
    let name = HSTRING::from(moniker(&sb.id));
    unsafe {
        let _ = DeleteAppContainerProfile(PCWSTR(name.as_ptr()));
    }
}

/// Can this machine make an AppContainer profile at all? Run by the capability
/// probe so the UI says so up front instead of every run failing.
pub fn probe() -> Result<(), String> {
    Container::open("probe").map(|_| ())
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

    // ── on a real Windows machine ───────────────────────────────────────────
    //
    // Everything below launches real processes in a real AppContainer. They are
    // the checks the research document asked for, expressed with `cmd.exe` (it
    // is on every Windows and readable by every AppContainer, so no interpreter
    // has to be installed for them to mean something).

    fn comspec() -> String {
        std::env::var("COMSPEC").unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".into())
    }

    fn sandbox(dir: &Path, network: bool) -> Sandbox {
        Sandbox {
            id: crate::sandbox::db::new_id(),
            name: "windows-test".into(),
            backend: "direct".into(),
            image: None,
            workdir: dir.to_string_lossy().to_string(),
            network,
            cpus: 1.0,
            memory_mb: 512,
            pids_limit: 64,
            timeout_ms: 30_000,
            env: serde_json::json!({}),
            mounts: Vec::new(),
            fs_mode: FsMode::Strict,
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

    fn run(sb: &Sandbox, line: &str, timeout_ms: u64, confine: bool) -> Outcome {
        let spec = ExecSpec {
            script: String::new(),
            timeout_ms,
            extra_env: BTreeMap::new(),
            argv: Some(vec![comspec(), "/d".into(), "/c".into(), line.into()]),
            keep_leftovers: false,
        };
        exec_blocking(sb, &spec, &[], confine)
    }

    #[test]
    fn a_command_runs_in_the_container_and_its_output_comes_back() {
        let d = tempfile::tempdir().unwrap();
        let sb = sandbox(d.path(), false);
        let o = run(&sb, "echo hello from the container", 30_000, true);
        forget(&sb);
        assert_eq!(o.exit_code, Some(0), "stderr: {}", o.stderr);
        assert!(o.stdout.contains("hello from the container"), "stdout: {:?}", o.stdout);
        assert_eq!(o.isolation, "appcontainer");
    }

    #[test]
    fn a_cmd_script_runs_in_the_container_the_way_the_runner_writes_it() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".runs")).unwrap();
        std::fs::write(d.path().join(".runs").join("t.cmd"), winproc::cmd_script("echo from the script")).unwrap();
        let sb = sandbox(d.path(), false);
        let o = run(&sb, r".runs\t.cmd", 15_000, true);
        forget(&sb);
        assert!(!o.timed_out, "the script never finished: {o:?}");
        assert!(o.stdout.contains("from the script"), "{o:?}");
    }

    #[test]
    fn a_deep_sandbox_folder_does_not_hang_the_shell() {
        // A `LOCALAPPDATA` of 209+ characters left every console program `cmd`
        // started stuck at creation; a folder this deep produced one.
        let base = tempfile::tempdir().unwrap();
        let wd = base.path().join("w".repeat(220 - base.path().to_string_lossy().len() - 1));
        std::fs::create_dir_all(wd.join(".runs")).unwrap();
        std::fs::write(wd.join(".runs").join("t.cmd"), winproc::cmd_script("echo deep-ok\nset LOCALAPPDATA")).unwrap();
        let sb = sandbox(&wd, false);
        let o = run(&sb, r".runs\t.cmd", 10_000, true);
        forget(&sb);
        assert!(!o.timed_out, "the shell hung in a deep folder: {o:?}");
        assert!(o.stdout.contains("deep-ok"), "{o:?}");
        let local = o.stdout.lines().find(|l| l.starts_with("LOCALAPPDATA=")).unwrap_or_default();
        assert!(local.trim_end().ends_with(r"\AC"), "{local}");
        assert_eq!(local.matches(r"\Packages\").count(), 1, "the container's own folder, once: {local}");
    }

    #[test]
    fn node_runs_a_script_and_npm_starts_in_the_container() {
        // Node `lstat`s the drive root on every module load, which a container
        // may not read; and nvm's node folder is a junction, whose target is
        // what must be granted.
        let Some(node) = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).map(|d| d.join("node.exe")).find(|n| n.exists()))
            .flatten()
        else {
            eprintln!("SKIP: no node.exe on PATH");
            return;
        };
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("lib.js"), "module.exports = 'node-module-ok';").unwrap();
        std::fs::write(d.path().join("main.js"), "console.log(require('./lib.js'))").unwrap();
        let sb = sandbox(d.path(), false);
        let node_s = node.to_string_lossy().to_string();
        let run_node = |args: Vec<String>| {
            let mut argv = vec![node_s.clone()];
            argv.extend(args);
            let spec = ExecSpec { script: String::new(), timeout_ms: 30_000, extra_env: BTreeMap::new(), argv: Some(argv), keep_leftovers: false };
            exec_blocking(&sb, &spec, &[], true)
        };
        let script = run_node(vec!["main.js".into()]);
        let npm_cli = node.parent().unwrap().join(r"node_modules\npm\bin\npm-cli.js");
        let npm = npm_cli.exists().then(|| run_node(vec![npm_cli.to_string_lossy().into(), "--version".into()]));
        forget(&sb);
        assert_eq!(script.exit_code, Some(0), "{script:?}");
        assert!(script.stdout.contains("node-module-ok"), "{script:?}");
        if let Some(npm) = npm {
            assert_eq!(npm.exit_code, Some(0), "{npm:?}");
            assert!(!npm.stdout.trim().is_empty(), "{npm:?}");
        }
    }

    /// first → second → real, the shape nvm-windows gives its node folder.
    fn junction_chain(outside: &Path) -> [PathBuf; 3] {
        let real = outside.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("tool.txt"), "through-the-chain").unwrap();
        let (second, first) = (outside.join("second"), outside.join("first"));
        for (link, target) in [(&second, &real), (&first, &second)] {
            let ok = std::process::Command::new(comspec())
                .args(["/d", "/c", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            assert!(ok, "mklink /J failed for {}", link.display());
        }
        [first, second, real]
    }

    fn aces_left(sb: &Sandbox, paths: &[PathBuf]) -> Vec<String> {
        let c = Container::derive(&sb.id).unwrap();
        paths
            .iter()
            .filter(|p| has_explicit_ace(c.sid, &p.to_string_lossy()))
            .map(|p| p.display().to_string())
            .collect()
    }

    #[test]
    fn forget_takes_back_every_grant_along_a_junction_chain() {
        let outside = tempfile::tempdir().unwrap();
        let chain = junction_chain(outside.path());
        let d = tempfile::tempdir().unwrap();
        let sb = sandbox(d.path(), false);
        Container::open(&sb.id).unwrap().grant(&chain[0], false).unwrap();
        assert_eq!(aces_left(&sb, &chain).len(), 3, "every hop is granted");
        forget_with(&sb, Vec::new());
        assert!(aces_left(&sb, &chain).is_empty(), "left behind: {:?}", aces_left(&sb, &chain));
    }

    #[test]
    fn forget_finds_grants_a_restarted_daemon_has_no_record_of() {
        let outside = tempfile::tempdir().unwrap();
        let chain = junction_chain(outside.path());
        let d = tempfile::tempdir().unwrap();
        let sb = sandbox(d.path(), false);
        Container::open(&sb.id).unwrap().grant(&chain[0], false).unwrap();
        let prefix = format!("{}|", moniker(&sb.id));
        granted().lock().unwrap().retain(|k| !k.starts_with(&prefix));
        forget_with(&sb, vec![chain[0].to_string_lossy().into_owned()]);
        assert!(aces_left(&sb, &chain).is_empty(), "left behind: {:?}", aces_left(&sb, &chain));
    }

    #[test]
    fn a_grant_reaches_through_a_chain_of_junctions() {
        let outside = tempfile::tempdir().unwrap();
        let [first, _, _] = junction_chain(outside.path());
        let d = tempfile::tempdir().unwrap();
        let line = format!("type \"{}\"", first.join("tool.txt").display());
        std::fs::write(d.path().join("read.cmd"), winproc::cmd_script(&line)).unwrap();
        let sb = sandbox(d.path(), false);
        Container::open(&sb.id).unwrap().grant(&first, false).unwrap();
        let o = run(&sb, "read.cmd", 15_000, true);
        forget(&sb);
        assert!(o.stdout.contains("through-the-chain"), "{o:?}");
    }

    const BUSY: &str = "for /l %i in (1,1,2000000000) do @rem";

    /// Wait until the sandbox has a live run another process could open — its
    /// job may still be empty, which is the moment a stop must not miss.
    fn wait_for_run(sb: &Sandbox) {
        let t = Instant::now();
        while open_jobs(&sb.id).is_empty() {
            assert!(t.elapsed() < Duration::from_secs(60), "the run never showed up");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait until a run of the sandbox has a process in its job.
    fn wait_for_process(sb: &Sandbox) {
        let t = Instant::now();
        while !open_jobs(&sb.id).iter().any(|j| !job_pids(j).is_empty()) {
            assert!(t.elapsed() < Duration::from_secs(60), "no process ever joined the run");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn stopping_a_sandbox_ends_its_run_even_from_another_process() {
        // The job is found by name, the way the MCP server finds a run the
        // daemon started; signalling a process group was a no-op on Windows.
        let d = tempfile::tempdir().unwrap();
        let sb = sandbox(d.path(), false);
        let o = std::thread::scope(|s| {
            let run = s.spawn(|| run(&sb, BUSY, 30_000, true));
            wait_for_run(&sb);
            assert_eq!(kill_all(&sb.id), 1);
            run.join().unwrap()
        });
        forget(&sb);
        assert!(!o.timed_out, "the run went on until its deadline: {o:?}");
        assert!(open_jobs(&sb.id).is_empty(), "nothing is left to find");
    }

    #[test]
    fn stats_list_a_run_and_a_pid_outside_the_sandbox_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let sb = sandbox(d.path(), false);
        let (procs, refused, stopped, o) = std::thread::scope(|s| {
            let run = s.spawn(|| run(&sb, BUSY, 30_000, true));
            wait_for_process(&sb);
            let procs = processes(&sb.id);
            let refused = kill_pid(&sb.id, std::process::id());
            let shell = procs.iter().find(|p| p.command.eq_ignore_ascii_case("cmd.exe")).map(|p| p.pid);
            let stopped = shell.map(|pid| kill_pid(&sb.id, pid));
            (procs, refused, stopped, run.join().unwrap())
        });
        forget(&sb);
        let shell = procs.iter().find(|p| p.command.eq_ignore_ascii_case("cmd.exe"));
        assert!(shell.is_some(), "{procs:?}");
        assert!(shell.unwrap().rss_mb > 0.0 && !shell.unwrap().elapsed.is_empty(), "{procs:?}");
        assert_eq!(refused, Ok(false), "a process outside the sandbox must not be stoppable");
        assert_eq!(stopped, Some(Ok(true)));
        assert!(!o.timed_out, "stopping the shell ends the run: {o:?}");
    }

    #[test]
    fn its_own_directory_is_writable() {
        let d = tempfile::tempdir().unwrap();
        let sb = sandbox(d.path(), false);
        let o = run(&sb, "echo inside> inside.txt", 30_000, true);
        forget(&sb);
        assert_eq!(o.exit_code, Some(0), "stderr: {}", o.stderr);
        assert!(d.path().join("inside.txt").exists(), "the write inside the sandbox must land");
    }

    #[test]
    fn a_write_outside_the_directory_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("escape.txt");
        let sb = sandbox(d.path(), false);
        let o = run(&sb, &format!("echo escaped> {}", target.display()), 30_000, true);
        forget(&sb);
        assert!(!target.exists(), "the container wrote outside its directory: {o:?}");
        assert_ne!(o.exit_code, Some(0));
    }

    #[test]
    fn a_file_outside_the_directory_cannot_be_read() {
        let d = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "TOP-SECRET-CONTENT").unwrap();
        let sb = sandbox(d.path(), false);
        let o = run(&sb, &format!("type {}", secret.display()), 30_000, true);
        forget(&sb);
        assert!(!o.stdout.contains("TOP-SECRET-CONTENT"), "user data was readable: {o:?}");
        assert_ne!(o.exit_code, Some(0));
    }

    #[test]
    fn without_the_network_capability_a_connection_fails() {
        // `curl.exe` ships with Windows 10 1803 and later.
        let curl = Path::new(r"C:\Windows\System32\curl.exe");
        if !curl.exists() {
            eprintln!("SKIP: no curl.exe on this machine");
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let sb = sandbox(d.path(), false);
        let o = run(&sb, "curl.exe -s -m 8 -o NUL https://example.com", 30_000, true);
        forget(&sb);
        assert_ne!(o.exit_code, Some(0), "the container reached the network without the capability: {o:?}");
    }

    #[test]
    fn an_unconfined_run_is_labelled_none_and_is_not_in_a_container() {
        let d = tempfile::tempdir().unwrap();
        let sb = sandbox(d.path(), false);
        let o = run(&sb, "echo plain", 30_000, false);
        assert_eq!(o.exit_code, Some(0), "stderr: {}", o.stderr);
        assert!(o.stdout.contains("plain"));
        assert_eq!(o.isolation, "none");
    }

    #[test]
    fn a_timeout_ends_the_run_and_the_tree() {
        let d = tempfile::tempdir().unwrap();
        let sb = sandbox(d.path(), false);
        let t = Instant::now();
        // A busy loop that never ends, started twice: once in the shell and once
        // in a child that holds the same pipes.
        let o = run(
            &sb,
            "start /b cmd /c \"for /l %i in (1,1,2000000000) do @rem\" & for /l %i in (1,1,2000000000) do @rem",
            3_000,
            true,
        );
        forget(&sb);
        assert!(o.timed_out, "{o:?}");
        assert!(t.elapsed() < Duration::from_secs(30), "the run did not end at its deadline");
    }

    #[test]
    fn a_leftover_holding_the_pipes_is_ended_at_the_deadline_not_waited_for_forever() {
        let d = tempfile::tempdir().unwrap();
        let sb = sandbox(d.path(), false);
        let t = Instant::now();
        let o = run(&sb, "start /b cmd /c \"for /l %i in (1,1,2000000000) do @rem\"", 3_000, true);
        forget(&sb);
        assert!(t.elapsed() < Duration::from_secs(30), "the reader waited on a leftover: {o:?}");
    }

    #[test]
    fn granting_a_second_container_does_not_erase_the_first() {
        // The grant used to build its ACL from nothing and install it, which
        // dropped every explicit entry the folder had — here, the first
        // container's.
        let shared = tempfile::tempdir().unwrap();
        let (da, db_) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = (sandbox(da.path(), false), sandbox(db_.path(), false));
        let ca = Container::open(&a.id).unwrap();
        let cb = Container::open(&b.id).unwrap();
        ca.grant(shared.path(), true).unwrap();
        cb.grant(shared.path(), true).unwrap();

        let o = run(&a, &format!("echo still> {}\\a.txt", shared.path().display()), 30_000, true);
        forget(&a);
        forget(&b);
        assert_eq!(o.exit_code, Some(0), "the first container lost its access: {o:?}");
        assert!(shared.path().join("a.txt").exists());
    }
}
