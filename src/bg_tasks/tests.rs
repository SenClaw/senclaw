//! Registry behaviour against real processes. Each test owns its own group and
//! cleans it up; none of them reads `bg-tasks.json` (no store path is set here).

use std::time::Duration;

use super::*;
use crate::tools::bg_jobs::JobStatus;

fn spec(command: &str, chat: &str) -> StartSpec {
    StartSpec {
        command: command.into(),
        cwd: "/tmp".into(),
        description: "test task".into(),
        chat_jid: chat.into(),
    }
}

async fn until(what: &str, mut ok: impl FnMut() -> bool) {
    for _ in 0..100 {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn a_background_command_is_listed_streams_output_and_stops_with_its_children() {
    let job = start_background(spec("echo ready; sleep 60 & sleep 60", "web:t1")).await.unwrap();
    assert_eq!(job.status(), JobStatus::Running);
    until("output", || job.output_snapshot().contains("ready")).await;

    let listed = views(Some("web:t1"), false);
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, job.id);
    assert_eq!(listed[0].description, "test task");
    assert_eq!(listed[0].origin, "spawned");
    assert!(views(Some("web:other"), false).is_empty(), "another chat's list must not show it");

    let pgid = job.pgid().unwrap();
    assert!(proc::group_alive(pgid));
    assert!(stop(&job).await, "stop must report the group gone");
    assert_eq!(job.status(), JobStatus::Stopped, "a requested stop is 'stopped', not 'failed'");
    assert!(!proc::group_alive(pgid), "the backgrounded sleep must die with the shell");
}

#[tokio::test]
async fn a_command_that_fails_at_once_is_failed_with_its_exit_code() {
    let job = start_background(spec("echo nope >&2; exit 7", "web:t2")).await.unwrap();
    until("finish", || job.status() != JobStatus::Running).await;
    assert_eq!(job.status(), JobStatus::Failed);
    assert_eq!(job.state().exit_code, Some(7));
    assert!(job.output_snapshot().contains("nope"));
}

#[tokio::test]
async fn a_shell_that_ends_but_leaves_a_member_stays_running_until_the_group_is_empty() {
    let job = start_background(spec("sleep 60 &", "web:t3")).await.unwrap();
    until("shell exit", || job.state().child_exited).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(job.status(), JobStatus::Running, "the sleep is still in the group");
    assert!(stop(&job).await);
    assert_eq!(job.status(), JobStatus::Stopped);
}

#[tokio::test]
async fn adoption_registers_what_a_foreground_command_left_and_ignores_a_clean_exit() {
    let clean = shell::run_raw("echo done", "/tmp", 10_000).await.unwrap();
    let none = adopt(
        clean.group.unwrap(),
        clean.held,
        AdoptSpec { command: "echo done", cwd: "/tmp", description: "", chat_jid: "web:t4", isolation: "" },
    )
    .await;
    assert!(none.is_none(), "nothing alive, nothing to adopt");

    let run = shell::run_raw("nohup sleep 60 > /dev/null 2>&1 &\necho started", "/tmp", 10_000)
        .await
        .unwrap();
    assert!(run.held.is_none(), "output was redirected, so no pipe is held");
    let job = adopt(
        run.group.unwrap(),
        run.held,
        AdoptSpec {
            command: "nohup sleep 60 &",
            cwd: "/tmp",
            description: "keep a sleeper",
            chat_jid: "web:t4",
            isolation: "",
        },
    )
    .await
    .expect("the sleeper outlived the shell");
    assert_eq!(job.meta.origin, Origin::Adopted);
    assert_eq!(job.status(), JobStatus::Running);
    assert!(adopted_note(&job).contains(&job.id));
    assert!(stop(&job).await);
    assert!(!proc::group_alive(run.group.unwrap()));
}

#[tokio::test]
async fn adoption_keeps_streaming_a_leftover_that_holds_the_pipes() {
    let run = shell::run_raw("(while true; do echo tick; sleep 0.2; done) & echo hello", "/tmp", 10_000)
        .await
        .unwrap();
    assert!(run.held.is_some());
    let job = adopt(
        run.group.unwrap(),
        run.held,
        AdoptSpec { command: "loop &", cwd: "/tmp", description: "", chat_jid: "web:t5", isolation: "" },
    )
    .await
    .unwrap();
    until("streamed output", || job.output_snapshot().matches("tick").count() >= 2).await;
    assert!(stop(&job).await);
}

#[tokio::test]
async fn a_job_whose_group_is_already_gone_is_ended_without_signalling_anything() {
    let job = BgJob::with_meta(
        "bash-99991".into(),
        JobKind::Bash,
        "x".into(),
        JobMeta { chat_jid: "web:t6".into(), ..Default::default() },
    );
    job.update_state(|s| s.pgid = Some(u32::MAX - 7));
    BgJobManager::global().register(job.clone());
    assert!(stop(&job).await);
    assert_eq!(job.status(), JobStatus::Stopped);
}

#[tokio::test]
async fn the_test_runners_own_group_survives_a_stop_that_names_it() {
    // A task record that (wrongly) carries the daemon's own group must not take
    // the process down with it.
    let own = proc::group_of(std::process::id()).unwrap();
    let job = BgJob::with_meta(
        "bash-99992".into(),
        JobKind::Bash,
        "x".into(),
        JobMeta { chat_jid: "web:t6b".into(), ..Default::default() },
    );
    job.update_state(|s| s.pgid = Some(own));
    BgJobManager::global().register(job.clone());
    assert!(!stop(&job).await, "the group is still alive, so the stop did not succeed");
    assert!(proc::group_alive(own));
    assert_eq!(job.status(), JobStatus::Running);
}

#[tokio::test]
async fn the_chat_of_a_tool_call_is_what_with_chat_set() {
    assert_eq!(current_chat(), "");
    let seen = with_chat("web:abc", async { current_chat() }).await;
    assert_eq!(seen, "web:abc");
}

#[test]
fn tail_cuts_to_a_line_start_and_never_inside_a_character() {
    let s = "một\nhai\nba\nbốn\n";
    assert_eq!(tail(s, 1000), s);
    let t = tail(s, 9);
    assert!(s.ends_with(&t));
    assert!(!t.contains('\u{FFFD}'));
}

#[tokio::test]
async fn shutdown_stops_everything_that_is_running() {
    let a = start_background(spec("sleep 60", "web:t7")).await.unwrap();
    let b = start_background(spec("sleep 60 & sleep 60", "web:t7")).await.unwrap();
    let (ga, gb) = (a.pgid().unwrap(), b.pgid().unwrap());
    stop_all_where(|j| j.meta.chat_jid == "web:t7").await;
    assert!(!proc::group_alive(ga) && !proc::group_alive(gb));
    assert_ne!(a.status(), JobStatus::Running);
    assert_ne!(b.status(), JobStatus::Running);
}
