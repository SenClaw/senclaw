//! Background tasks: the processes an agent leaves running, kept where a person
//! can see them, read their output and stop them.
//!
//! Before this, a `nohup npx vite … &` in `Bash` was invisible: the run returned,
//! the dev server lived on in a process group nobody tracked, and the person had
//! no list to find it in and no button to end it. The registry is
//! [`BgJobManager`](crate::tools::bg_jobs::BgJobManager); this module fills it.
//!
//! Two ways in:
//! - **`Bash {run_in_background: true}`** ([`start_background`]): the daemon
//!   starts the shell, captures its output into the job and returns an id.
//! - **Adoption** ([`adopt`]): after a foreground `Bash`, whatever is still alive
//!   in the shell's process group is registered, and the pipes it still holds
//!   keep feeding the job. Small models write `cmd &` whatever the tool says,
//!   and a list that only knew the polite case would be wrong exactly there.
//!
//! A task is **running while its process group has a live member**, not while the
//! shell it started lives: `npm run dev &` ends its shell at once. Stopping
//! signals the group (never the daemon's own); `proc.rs` says why a recorded group
//! number can be trusted, and what a restored entry must prove first.
//!
//! The running tasks are mirrored to `bg-tasks.json` ([`init`]). A graceful
//! shutdown stops them all ([`shutdown_all`]) — the desktop gives the daemon well
//! under a second before SIGKILL — and a boot after a crash lists the ones it can
//! still verify ([`Origin::Restored`](crate::tools::bg_jobs::Origin)).

pub mod capture;
pub mod proc;
pub mod shell;
mod store;

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;

pub use capture::Held;
use capture::Capture;

use crate::tools::bg_jobs::{now_ms, BgJob, BgJobManager, JobKind, JobMeta, JobStatus, Origin};

tokio::task_local! {
    static CHAT: String;
}

/// Run `fut` as the tool call of chat `chat_jid`, so a tool that registers a
/// background task can say whose it is. `ToolContext` carries no chat; adding one
/// would touch every tool and every test that builds a context.
pub async fn with_chat<F: Future>(chat_jid: &str, fut: F) -> F::Output {
    CHAT.scope(chat_jid.to_string(), fut).await
}

/// The chat of the tool call being run; empty outside one.
pub fn current_chat() -> String {
    CHAT.try_with(|c| c.clone()).unwrap_or_default()
}

/// Finished tasks stay in the list for a while (so "it died" can be seen), but not forever.
const KEEP_FINISHED: usize = 50;
const KEEP_FINISHED_FOR: Duration = Duration::from_secs(6 * 3600);
/// A foreground command's leftovers must outlive this to count (`sleep 1 &` is not a task).
const SETTLE: Duration = Duration::from_millis(300);
/// Longest command text kept per task.
const MAX_COMMAND: usize = 2000;

static STORE: Mutex<Option<PathBuf>> = Mutex::new(None);
static WATCHER: OnceLock<()> = OnceLock::new();

// ── starting ────────────────────────────────────────────────────────────────

pub struct StartSpec {
    pub command: String,
    pub cwd: String,
    pub description: String,
    pub chat_jid: String,
}

fn clip(s: &str, max: usize) -> String {
    crate::util::text::truncate_on_char_boundary(s, max).to_string()
}

fn register(job: &Arc<BgJob>) {
    let mgr = BgJobManager::global();
    mgr.register(job.clone());
    mgr.prune_finished(KEEP_FINISHED_FOR, KEEP_FINISHED);
    ensure_watcher();
    persist();
}

/// Start `spec.command` as a background task and return it once registered. The
/// caller decides how long to look at the first output.
pub async fn start_background(spec: StartSpec) -> Result<Arc<BgJob>, String> {
    let (mut child, isolation) = shell::spawn(&spec.command, &spec.cwd).await?;
    let pid = child.id();
    let mgr = BgJobManager::global();
    let job = BgJob::with_meta(
        mgr.next_id(JobKind::Bash),
        JobKind::Bash,
        clip(&spec.command, MAX_COMMAND),
        JobMeta {
            chat_jid: spec.chat_jid,
            description: clip(&spec.description, 200),
            cwd: spec.cwd,
            origin: Origin::Spawned,
            isolation,
            started_ms: now_ms(),
        },
    );
    job.update_state(|s| {
        s.pid = pid;
        if cfg!(unix) {
            s.pgid = pid;
            s.pids = pid.into_iter().collect();
        }
    });

    Capture::feed(child.stdout.take(), &job);
    Capture::feed(child.stderr.take(), &job);
    register(&job);

    let waiter = job.clone();
    tokio::spawn(async move {
        let code = match child.wait().await {
            Ok(status) => exit_code_of(&status),
            Err(_) => None,
        };
        // `cmd &` ends its shell at once while the group runs on: then the
        // watcher ends the task when the last member is gone. Read who is left
        // first, so the list and the persisted pids name the survivors.
        let group_gone = waiter.pgid().map_or(true, |g| !proc::group_alive(g));
        if !group_gone {
            refresh(std::slice::from_ref(&waiter)).await;
        }
        waiter.update_state(|s| {
            s.child_exited = true;
            s.exit_code = code;
        });
        if group_gone {
            waiter.finish(code);
            persist();
        }
    });
    Ok(job)
}

#[cfg(unix)]
fn exit_code_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.code().or_else(|| status.signal().map(|s| 128 + s))
}

#[cfg(not(unix))]
fn exit_code_of(status: &std::process::ExitStatus) -> Option<i32> {
    status.code()
}

// ── adoption ────────────────────────────────────────────────────────────────

pub struct AdoptSpec<'a> {
    pub command: &'a str,
    pub cwd: &'a str,
    pub description: &'a str,
    pub chat_jid: &'a str,
    pub isolation: &'a str,
}

/// After a foreground command: register what it left running in `group`, if
/// anything, and hand it the pipes it still holds. `None` when the group is gone.
pub async fn adopt(group: u32, held: Option<Held>, spec: AdoptSpec<'_>) -> Option<Arc<BgJob>> {
    if !proc::group_alive(group) {
        return None;
    }
    tokio::time::sleep(SETTLE).await;
    // `None`: the process table is unreadable, but the group answered a signal
    // check, so it is still adopted — with no pids until one can be read.
    let members = match proc::members(&[group]).await {
        Some(m) if m.is_empty() => return None,
        Some(m) => m,
        None => Vec::new(),
    };
    let mgr = BgJobManager::global();
    let job = BgJob::with_meta(
        mgr.next_id(JobKind::Bash),
        JobKind::Bash,
        clip(spec.command, MAX_COMMAND),
        JobMeta {
            chat_jid: spec.chat_jid.to_string(),
            description: clip(spec.description, 200),
            cwd: spec.cwd.to_string(),
            origin: Origin::Adopted,
            isolation: spec.isolation.to_string(),
            started_ms: now_ms(),
        },
    );
    job.update_state(|s| {
        s.pgid = Some(group);
        s.pid = members.first().map(|m| m.pid).or(Some(group));
        s.pids = members.iter().map(|m| m.pid).collect();
        s.child_exited = true;
    });
    if let Some(h) = held {
        h.out.attach(job.clone());
        h.err.attach(job.clone());
    }
    register(&job);
    Some(job)
}

/// The line `Bash` appends to its result when it adopted something, so the model
/// learns the id and does not start the same server twice.
pub fn adopted_note(job: &BgJob) -> String {
    let st = job.state();
    let ports = if st.ports.is_empty() {
        String::new()
    } else {
        format!(", listening on {}", st.ports.iter().map(|p| format!(":{p}")).collect::<Vec<_>>().join(" "))
    };
    format!(
        "\n[Background process left running by this command: {} (pid {}{ports}). It is in the person's Background tasks \
         list. Read its output with PeekBgJob {{job_id: \"{}\", wait: false}}, stop it with StopBgJob {{job_id: \"{}\"}}.]",
        job.id,
        st.pid.unwrap_or(0),
        job.id,
        job.id
    )
}

// ── stopping ────────────────────────────────────────────────────────────────

/// Whether a stop has a process group (or, off Unix, a pid) to act on.
pub fn has_group(job: &BgJob) -> bool {
    let st = job.state();
    st.pgid.is_some() || (!cfg!(unix) && st.pid.is_some())
}

/// Stop a job: SIGTERM to its group, SIGKILL for what ignores it after 2 s. True
/// when nothing of it is left.
pub async fn stop(job: &BgJob) -> bool {
    job.request_stop();
    let st = job.state();
    #[cfg(unix)]
    {
        let Some(pgid) = st.pgid else { return false };
        let gone = proc::stop_group(pgid, Duration::from_secs(2)).await;
        if gone {
            job.finish(None);
        }
        persist();
        gone
    }
    #[cfg(not(unix))]
    {
        let Some(pid) = st.pid else { return false };
        let gone = proc::kill_tree(pid).await;
        job.finish(None);
        persist();
        gone
    }
}

/// Stop every running task of a chat (the chat is being deleted).
pub async fn stop_chat(chat_jid: &str) -> usize {
    let mut n = 0;
    for job in BgJobManager::global().list() {
        if job.meta.chat_jid == chat_jid && job.status() == JobStatus::Running && stop(&job).await {
            n += 1;
        }
    }
    n
}

// ── persistence, boot, shutdown ─────────────────────────────────────────────

fn records() -> Vec<store::Record> {
    BgJobManager::global()
        .list()
        .into_iter()
        .filter(|j| j.status() == JobStatus::Running)
        .filter_map(|j| {
            let st = j.state();
            Some(store::Record {
                id: j.id.clone(),
                chat_jid: j.meta.chat_jid.clone(),
                description: j.meta.description.clone(),
                command: j.command.clone(),
                cwd: j.meta.cwd.clone(),
                isolation: j.meta.isolation.clone(),
                started_ms: j.meta.started_ms,
                pgid: st.pgid?,
                pids: st.pids,
            })
        })
        .collect()
}

fn persist() {
    let Some(path) = STORE.lock().unwrap().clone() else { return };
    if let Err(e) = store::save(&path, &records()) {
        tracing::warn!("[bg-tasks] cannot write {}: {e}", path.display());
    }
}

/// Daemon boot: remember where the list is kept, list what a crashed predecessor
/// left running (and can still be told apart from a reused pid), start the watcher.
pub fn init(path: PathBuf) {
    let restored = store::load(&path);
    *STORE.lock().unwrap() = Some(path);
    let mgr = BgJobManager::global();
    for rec in restored {
        let mut recorded = rec.pids.clone();
        recorded.push(rec.pgid);
        if !proc::still_ours(rec.pgid, &recorded) {
            continue;
        }
        let job = BgJob::with_meta(
            rec.id.clone(),
            JobKind::Bash,
            rec.command,
            JobMeta {
                chat_jid: rec.chat_jid,
                description: rec.description,
                cwd: rec.cwd,
                origin: Origin::Restored,
                isolation: rec.isolation,
                started_ms: rec.started_ms,
            },
        );
        job.update_state(|s| {
            s.pgid = Some(rec.pgid);
            s.pid = rec.pids.first().copied().or(Some(rec.pgid));
            s.pids = rec.pids;
            s.child_exited = true;
        });
        job.append_output("(started before this daemon restarted — its output was not kept)\n");
        mgr.bump_counter_past(&rec.id);
        mgr.register(job);
    }
    persist();
    ensure_watcher();
}

/// Daemon shutdown: end every running task, all at once. Nothing an agent
/// started outlives the daemon; the list is emptied so the next boot has no ghosts.
pub async fn shutdown_all() {
    stop_all_where(|_| true).await;
}

/// [`shutdown_all`] for the running tasks `pick` selects.
pub(crate) async fn stop_all_where(pick: impl Fn(&BgJob) -> bool) {
    let jobs: Vec<Arc<BgJob>> = BgJobManager::global()
        .list()
        .into_iter()
        .filter(|j| j.status() == JobStatus::Running && pick(j))
        .collect();
    let mut groups = Vec::new();
    for j in &jobs {
        j.request_stop();
        groups.extend(j.pgid());
    }
    proc::stop_groups(&groups, Duration::from_millis(300)).await;
    for j in &jobs {
        j.finish(None);
    }
    persist();
}

// ── watching ────────────────────────────────────────────────────────────────

fn ensure_watcher() {
    if WATCHER.set(()).is_ok() {
        tokio::spawn(watch_loop());
    }
}

/// A shell the daemon started is finished by its own waiter; every other task is
/// finished here, when its group has no member left.
fn may_finish(job: &BgJob) -> bool {
    job.meta.origin != Origin::Spawned || job.state().child_exited
}

async fn watch_loop() {
    let mut tick = 0u64;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        tick += 1;
        let mgr = BgJobManager::global();
        if tick % 60 == 0 {
            mgr.prune_finished(KEEP_FINISHED_FOR, KEEP_FINISHED);
        }
        let running: Vec<Arc<BgJob>> = mgr
            .list()
            .into_iter()
            .filter(|j| j.status() == JobStatus::Running && j.pgid().is_some())
            .collect();
        if running.is_empty() {
            continue;
        }
        let mut changed = false;
        let mut alive = Vec::new();
        for j in running {
            if j.pgid().is_some_and(|g| !proc::group_alive(g)) && may_finish(&j) {
                j.finish(None);
                changed = true;
            } else {
                alive.push(j);
            }
        }
        if tick % 3 == 0 && !alive.is_empty() {
            changed |= refresh(&alive).await;
        }
        if changed {
            persist();
        }
    }
}

/// Re-read one job's members and ports now, rather than at the watcher's next pass.
pub async fn refresh_now(job: &Arc<BgJob>) {
    if job.pgid().is_some() && job.status() == JobStatus::Running {
        refresh(std::slice::from_ref(job)).await;
    }
}

/// Re-read who is in each group and what they listen on. True when a task ended
/// or its members changed (the file lists the pids).
async fn refresh(jobs: &[Arc<BgJob>]) -> bool {
    let groups: Vec<u32> = jobs.iter().filter_map(|j| j.pgid()).collect();
    let Some(members) = proc::members(&groups).await else {
        return false;
    };
    let all_pids: Vec<u32> = members.iter().map(|m| m.pid).collect();
    let ports = proc::listening_ports(&all_pids).await;
    let mut changed = false;
    for j in jobs {
        let Some(pg) = j.pgid() else { continue };
        let mine: Vec<u32> = members.iter().filter(|m| m.pgid == pg).map(|m| m.pid).collect();
        if mine.is_empty() {
            // Only zombies (or nothing, if `ps` failed — then the group check above decides).
            if proc::group_alive(pg) && !members.is_empty() && may_finish(j) {
                j.finish(None);
                changed = true;
            }
            continue;
        }
        let mut p: Vec<u16> = mine.iter().filter_map(|pid| ports.get(pid)).flatten().copied().collect();
        p.sort_unstable();
        p.dedup();
        let before = j.state();
        if before.pids != mine || before.ports != p {
            changed |= before.pids != mine;
            j.update_state(|s| {
                s.pids = mine;
                s.ports = p;
            });
        }
    }
    changed
}

// ── what a client sees ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskView {
    pub id: String,
    pub kind: &'static str,
    pub chat_jid: String,
    pub description: String,
    pub command: String,
    pub cwd: String,
    pub status: &'static str,
    pub origin: &'static str,
    pub isolation: String,
    pub pid: Option<u32>,
    pub pids: Vec<u32>,
    pub ports: Vec<u16>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub uptime_secs: u64,
    pub exit_code: Option<i32>,
    /// The end of the output, only when asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

pub fn view(job: &BgJob, output_tail: Option<usize>) -> TaskView {
    let st = job.state();
    let status = job.status();
    let end = st.ended_ms.unwrap_or_else(now_ms);
    TaskView {
        id: job.id.clone(),
        kind: job.kind.as_str(),
        chat_jid: job.meta.chat_jid.clone(),
        description: job.meta.description.clone(),
        command: job.command.clone(),
        cwd: job.meta.cwd.clone(),
        status: status.as_str(),
        origin: job.meta.origin.as_str(),
        isolation: job.meta.isolation.clone(),
        pid: st.pid,
        pids: st.pids,
        ports: if status == JobStatus::Running { st.ports } else { Vec::new() },
        started_at: job.meta.started_ms,
        ended_at: st.ended_ms,
        uptime_secs: ((end - job.meta.started_ms).max(0) / 1000) as u64,
        exit_code: st.exit_code,
        output: output_tail.map(|n| tail(&job.output_snapshot(), n)),
    }
}

/// The last `max` bytes of `s`, from a line start when one is near.
pub fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    let cut = &s[start..];
    match cut.find('\n') {
        Some(i) if i < 200 => cut[i + 1..].to_string(),
        _ => cut.to_string(),
    }
}

/// Tasks, newest first; of one chat when `chat_jid` is given.
pub fn views(chat_jid: Option<&str>, include_finished: bool) -> Vec<TaskView> {
    let mut v: Vec<TaskView> = BgJobManager::global()
        .list()
        .iter()
        .filter(|j| chat_jid.map_or(true, |c| j.meta.chat_jid == c))
        .filter(|j| include_finished || j.status() == JobStatus::Running)
        .map(|j| view(j, None))
        .collect();
    v.sort_by_key(|t| std::cmp::Reverse(t.started_at));
    v
}

#[cfg(all(test, unix))]
mod tests;
