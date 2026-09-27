use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::bridge::DispatchBridge;
use super::dag::{build_augmented_prompt, is_ready};
use super::locks::lock_path_for;
use super::resume::build_dispatch_resume_hint;
use super::traits::{DispatchBridgeApi, NoopDispatchBridge};
use super::types::{DispatchParent, DispatchTask, DispatchTaskStatus};

#[test]
fn noop_bridge_returns_no_parents() {
    let b = NoopDispatchBridge;
    assert!(b.get_parents().is_empty());
    assert!(build_dispatch_resume_hint(Some(&b), "main").is_none());
}

#[test]
fn resume_hint_handles_no_bridge() {
    assert!(build_dispatch_resume_hint(None, "main").is_none());
}

struct FakeBridge {
    parents: Vec<DispatchParent>,
}
impl DispatchBridgeApi for FakeBridge {
    fn get_parents(&self) -> Vec<DispatchParent> {
        self.parents.clone()
    }
}

#[test]
fn resume_hint_renders_active_parents_only() {
    let now = "2025-01-01T00:00:00Z".to_string();
    let parents = vec![
        DispatchParent {
            id: "p1".into(),
            goal: "goal-1".into(),
            admin_folder: "main".into(),
            chat_jid: None,
            shared_workspace: None,
            status: "active".into(),
            created_at: now.clone(),
            completed_at: None,
            tasks: vec![DispatchTask {
                id: "t1".into(),
                label: "writer".into(),
                agent_id: "writer-agent".into(),
                agent_jid: String::new(),
                depends_on: vec![],
                prompt: "do thing".into(),
                status: DispatchTaskStatus::Processing,
                result: None,
                created_at: now.clone(),
                started_at: None,
                timeout_seconds: 0,
                timeout_at: None,
                completed_at: None,
                is_virtual: false,
                persona_name: None,
                checklist: vec![],
                checklist_auto: false,
                retry_count: 0,
                file_changes: vec![],
                verification_result: None,
                io: Default::default(),
                writes: Vec::new(),
                isolation: Default::default(),
                worktree: None,
            }],
        },
        DispatchParent {
            id: "p2".into(),
            goal: "goal-2".into(),
            admin_folder: "main".into(),
            chat_jid: None,
            shared_workspace: None,
            status: "completed".into(),
            created_at: now.clone(),
            completed_at: None,
            tasks: vec![],
        },
        DispatchParent {
            id: "p3".into(),
            goal: "goal-3".into(),
            admin_folder: "other".into(),
            chat_jid: None,
            shared_workspace: None,
            status: "active".into(),
            created_at: now,
            completed_at: None,
            tasks: vec![],
        },
    ];
    let hint = build_dispatch_resume_hint(Some(&FakeBridge { parents }), "main").unwrap();
    assert!(hint.contains("Task group p1"));
    assert!(hint.contains("processing"));
    assert!(!hint.contains("p2"));
    assert!(!hint.contains("p3"));
}

fn tmp_state_path(suffix: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "senclaw-dispatch-{}-{}.json",
        suffix,
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(lock_path_for(&p));
    p
}

fn make_task(id: &str, label: &str, jid: &str) -> DispatchTask {
    DispatchTask {
        id: id.into(),
        label: label.into(),
        agent_id: "writer".into(),
        agent_jid: jid.into(),
        depends_on: vec![],
        prompt: "do".into(),
        status: DispatchTaskStatus::Processing,
        result: None,
        created_at: "2025-01-01T00:00:00Z".into(),
        started_at: Some("2025-01-01T00:00:01Z".into()),
        timeout_seconds: 60,
        timeout_at: None,
        completed_at: None,
        is_virtual: false,
        persona_name: None,
        checklist: vec![],
        checklist_auto: false,
        retry_count: 0,
        file_changes: vec![],
        verification_result: None,
        io: Default::default(),
        writes: Vec::new(),
        isolation: Default::default(),
        worktree: None,
    }
}

#[test]
fn modify_state_round_trips_through_disk() {
    let path = tmp_state_path("roundtrip");
    let bridge = DispatchBridge::new(&path);
    bridge
        .modify_state(|s| {
            s.parents.push(DispatchParent {
                id: "p1".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "active".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![make_task("d1", "writer", "jid-a")],
            });
        })
        .unwrap();

    // Re-open and confirm the state survives a fresh bridge instance.
    let bridge2 = DispatchBridge::new(&path);
    let parents = bridge2.get_parents();
    assert_eq!(parents.len(), 1);
    assert_eq!(parents[0].tasks[0].agent_jid, "jid-a");
    assert_eq!(parents[0].tasks[0].status, DispatchTaskStatus::Processing);
    let _ = std::fs::remove_file(path);
}

#[test]
fn notify_task_done_marks_terminal_and_completes_parent() {
    let path = tmp_state_path("done");
    let bridge = DispatchBridge::new(&path);
    bridge
        .modify_state(|s| {
            s.parents.push(DispatchParent {
                id: "p1".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "active".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![make_task("d1", "only", "jid-a")],
            });
        })
        .unwrap();

    bridge.notify_task_done("d1", "result-text");

    let parents = bridge.get_parents();
    assert_eq!(parents[0].tasks[0].status, DispatchTaskStatus::Done);
    assert_eq!(parents[0].tasks[0].result.as_deref(), Some("result-text"));
    assert_eq!(parents[0].status, "done");
    assert!(parents[0].completed_at.is_some());
    let _ = std::fs::remove_file(path);
}

#[test]
fn auto_checklist_failure_keeps_task_done() {
    // Auto-generated checklists are advisory: unticked items must NOT flip a
    // successful task to error (the worker never even sees the checklist).
    let path = tmp_state_path("auto_checklist");
    let bridge = DispatchBridge::new(&path);
    let mut task = make_task("d1", "scout", "jid-a");
    task.checklist = vec![crate::agent::dispatch_bridge::types::ChecklistItem {
        id: "item-0".into(),
        description: "Today is 2026-07-03".into(),
        status: "pending".into(),
        depends_on: vec![],
        verification_note: None,
    }];
    task.checklist_auto = true;
    bridge
        .modify_state(|s| {
            s.parents.push(DispatchParent {
                id: "p1".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "active".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![task],
            });
        })
        .unwrap();

    bridge.notify_task_done("d1", "full research output");

    let parents = bridge.get_parents();
    let t = &parents[0].tasks[0];
    assert_eq!(t.status, DispatchTaskStatus::Done);
    assert_eq!(t.result.as_deref(), Some("full research output"));
    // Verification result is still recorded for the UI badge.
    assert!(t.verification_result.as_ref().is_some_and(|v| !v.verified));
    let _ = std::fs::remove_file(path);
}

#[test]
fn explicit_checklist_failure_marks_error() {
    let path = tmp_state_path("explicit_checklist");
    let bridge = DispatchBridge::new(&path);
    let mut task = make_task("d1", "scout", "jid-a");
    task.checklist = vec![crate::agent::dispatch_bridge::types::ChecklistItem {
        id: "item-0".into(),
        description: "must produce a table".into(),
        status: "pending".into(),
        depends_on: vec![],
        verification_note: None,
    }];
    task.checklist_auto = false; // orchestrator-supplied
    bridge
        .modify_state(|s| {
            s.parents.push(DispatchParent {
                id: "p1".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "active".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![task],
            });
        })
        .unwrap();

    bridge.notify_task_done("d1", "no table here");

    let parents = bridge.get_parents();
    let t = &parents[0].tasks[0];
    assert_eq!(t.status, DispatchTaskStatus::Error);
    // The worker's output is preserved inside the enhanced result.
    assert!(t.result.as_deref().unwrap().contains("no table here"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn infra_error_retries_in_place_once() {
    let path = tmp_state_path("infra_retry");
    let bridge = DispatchBridge::new(&path);
    bridge
        .modify_state(|s| {
            s.parents.push(DispatchParent {
                id: "p1".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "active".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![make_task("d1", "scout", "jid-a")],
            });
        })
        .unwrap();

    // First infra failure → reset to registered with retry_count = 1.
    bridge.mark_task_error("d1", "Virtual agent timed out after 600s");
    let parents = bridge.get_parents();
    let t = &parents[0].tasks[0];
    assert_eq!(t.status, DispatchTaskStatus::Registered);
    assert_eq!(t.retry_count, 1);
    assert_eq!(parents[0].status, "active");

    // Simulate the retry starting, then failing again → terminal error.
    bridge
        .modify_state(|s| {
            s.parents[0].tasks[0].status = DispatchTaskStatus::Processing;
        })
        .unwrap();
    bridge.mark_task_error("d1", "Virtual agent timed out after 600s");
    let parents = bridge.get_parents();
    assert_eq!(parents[0].tasks[0].status, DispatchTaskStatus::Error);
    assert_eq!(parents[0].status, "done");
    let _ = std::fs::remove_file(path);
}

#[test]
fn content_error_is_terminal_immediately() {
    let path = tmp_state_path("content_err");
    let bridge = DispatchBridge::new(&path);
    bridge
        .modify_state(|s| {
            s.parents.push(DispatchParent {
                id: "p1".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "active".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![make_task("d1", "scout", "jid-a")],
            });
        })
        .unwrap();

    bridge.mark_task_error(
        "d1",
        "Virtual agent setup error: persona \"x\" not available",
    );
    let parents = bridge.get_parents();
    assert_eq!(parents[0].tasks[0].status, DispatchTaskStatus::Error);
    assert_eq!(parents[0].tasks[0].retry_count, 0);
    let _ = std::fs::remove_file(path);
}

#[test]
fn notify_reply_resolves_earliest_processing_task() {
    let path = tmp_state_path("reply");
    let bridge = DispatchBridge::new(&path);
    let mut t_old = make_task("d_old", "old", "jid-a");
    t_old.started_at = Some("2025-01-01T00:00:01Z".into());
    let mut t_new = make_task("d_new", "new", "jid-a");
    t_new.started_at = Some("2025-01-01T00:00:09Z".into());
    bridge
        .modify_state(|s| {
            s.parents.push(DispatchParent {
                id: "p1".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "active".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![t_old, t_new],
            });
        })
        .unwrap();
    // Both are tracked as in-flight against the same jid.
    bridge.add_active_task("d_old", "jid-a");
    bridge.add_active_task("d_new", "jid-a");

    bridge.notify_reply("jid-a", "old-result");

    let parents = bridge.get_parents();
    let by_id: HashMap<_, _> = parents[0]
        .tasks
        .iter()
        .map(|t| (t.id.as_str(), t))
        .collect();
    assert_eq!(by_id["d_old"].status, DispatchTaskStatus::Done);
    assert_eq!(by_id["d_new"].status, DispatchTaskStatus::Processing);
    let _ = std::fs::remove_file(path);
}

#[test]
fn is_ready_with_terminal_deps_returns_true() {
    let mut a = make_task("a", "a", "j");
    a.status = DispatchTaskStatus::Done;
    let mut b = make_task("b", "b", "j");
    b.status = DispatchTaskStatus::Error; // continue-on-error
    let mut c = make_task("c", "c", "j");
    c.depends_on = vec!["a".into(), "b".into()];
    c.status = DispatchTaskStatus::Registered;
    let all = vec![a, b, c.clone()];
    assert!(is_ready(&c, &all));

    // Flip one dep back to processing → not ready.
    let mut all2 = all.clone();
    all2[0].status = DispatchTaskStatus::Processing;
    assert!(!is_ready(&c, &all2));
}

#[test]
fn build_augmented_prompt_includes_parent_goal_and_prereq_results() {
    let mut dep = make_task("d_dep", "writer", "j");
    dep.status = DispatchTaskStatus::Done;
    dep.result = Some("dep-result".into());
    dep.prompt = "draft a thing".into();

    let mut other = make_task("d_other", "reviewer", "j");
    other.status = DispatchTaskStatus::Processing;
    other.prompt = "review later".into();

    let mut me = make_task("d_me", "publisher", "j");
    me.depends_on = vec!["writer".into()];
    me.prompt = "publish it".into();

    let parent = DispatchParent {
        id: "p1".into(),
        goal: "ship the thing".into(),
        admin_folder: "main".into(),
        chat_jid: None,
        shared_workspace: None,
        status: "active".into(),
        created_at: "2025-01-01T00:00:00Z".into(),
        completed_at: None,
        tasks: vec![dep, other, me.clone()],
    };
    let augmented = build_augmented_prompt(&parent, &me);
    assert!(augmented.contains("<parent_goal>ship the thing</parent_goal>"));
    assert!(augmented.contains("<prerequisites>"));
    assert!(augmented.contains("<result>dep-result</result>"));
    assert!(augmented.contains("<other_tasks>"));
    assert!(augmented.contains("review later"));
    assert!(augmented.ends_with("\n\npublish it"));
}

#[test]
fn process_pending_launches_ready_task_via_callback() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let path = tmp_state_path("scheduler");
    let bridge = DispatchBridge::new(&path);
    let fired = Arc::new(AtomicBool::new(false));
    {
        let f = Arc::clone(&fired);
        bridge.set_send_to_agent(Arc::new(
            move |jid: &str, task_id: &str, prompt: &str, _ws: &str| {
                assert_eq!(jid, "jid-x");
                assert_eq!(task_id, "d1");
                assert!(prompt.contains("<parent_goal>g</parent_goal>"));
                f.store(true, Ordering::SeqCst);
            },
        ));
    }
    let mut t = make_task("d1", "only", "jid-x");
    t.status = DispatchTaskStatus::Registered;
    bridge
        .modify_state(|s| {
            s.parents.push(DispatchParent {
                id: "p1".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "active".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![t],
            });
        })
        .unwrap();
    bridge.process_pending();
    assert!(fired.load(Ordering::SeqCst));
    let parents = bridge.get_parents();
    assert_eq!(parents[0].tasks[0].status, DispatchTaskStatus::Processing);
    assert!(parents[0].tasks[0].started_at.is_some());
    assert!(parents[0].tasks[0].timeout_at.is_some());
    let _ = std::fs::remove_file(path);
}

#[test]
fn activate_next_queued_promotes_oldest_and_picks_up_admin_workspace() {
    // state file lives under a tmp dir so the workspace-state file we
    // write next to it is found via state_path.parent().
    let dir = std::env::temp_dir().join(format!("senclaw-q-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let state_path = dir.join("dispatch-state.json");
    let _ = std::fs::remove_file(&state_path);
    let _ = std::fs::remove_file(lock_path_for(&state_path));
    std::fs::write(
        dir.join("workspace-state-main.json"),
        r#"{"currentDir":"/tmp/admin-workspace"}"#,
    )
    .unwrap();

    let bridge = DispatchBridge::new(&state_path);
    let now = chrono::Utc::now();
    let older = (now - chrono::Duration::seconds(10)).to_rfc3339();
    let newer = now.to_rfc3339();
    bridge
        .modify_state(|s| {
            s.parents.push(DispatchParent {
                id: "p_old".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "queued".into(),
                created_at: older,
                completed_at: None,
                tasks: vec![],
            });
            s.parents.push(DispatchParent {
                id: "p_new".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "queued".into(),
                created_at: newer,
                completed_at: None,
                tasks: vec![],
            });
        })
        .unwrap();

    bridge.activate_next_queued("main");
    let parents = bridge.get_parents();
    let by_id: HashMap<_, _> = parents.iter().map(|p| (p.id.as_str(), p)).collect();
    assert_eq!(by_id["p_old"].status, "active");
    assert_eq!(
        by_id["p_old"].shared_workspace.as_deref(),
        Some("/tmp/admin-workspace")
    );
    assert_eq!(by_id["p_new"].status, "queued");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cleanup_drops_old_done_parents() {
    let path = tmp_state_path("cleanup");
    let bridge = DispatchBridge::new(&path);
    let stale = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
    let fresh = chrono::Utc::now().to_rfc3339();
    bridge
        .modify_state(|s| {
            s.parents.push(DispatchParent {
                id: "old".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "done".into(),
                created_at: stale.clone(),
                completed_at: Some(stale),
                tasks: vec![],
            });
            s.parents.push(DispatchParent {
                id: "new".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "done".into(),
                created_at: fresh.clone(),
                completed_at: Some(fresh),
                tasks: vec![],
            });
        })
        .unwrap();
    bridge.cleanup();
    let ids: Vec<_> = bridge.get_parents().iter().map(|p| p.id.clone()).collect();
    assert_eq!(ids, vec!["new".to_string()]);
    let _ = std::fs::remove_file(path);
}

#[test]
fn cancel_admin_parents_marks_active_jids_and_clears_tasks() {
    let path = tmp_state_path("cancel");
    let bridge = DispatchBridge::new(&path);
    bridge
        .modify_state(|s| {
            s.parents.push(DispatchParent {
                id: "p1".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "active".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![make_task("d1", "x", "jid-a")],
            });
        })
        .unwrap();
    bridge.add_active_task("d1", "jid-a");

    let affected = bridge.cancel_admin_parents("main");
    assert_eq!(affected, vec!["jid-a".to_string()]);
    let parents = bridge.get_parents();
    assert_eq!(parents[0].status, "done");
    assert_eq!(parents[0].tasks[0].status, DispatchTaskStatus::Error);
    assert!(!bridge.has_active_jid_tasks("jid-a"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn cancel_parents_for_shared_workspace_only_matching_root() {
    let path = tmp_state_path("cancel_ws_root");
    let bridge = DispatchBridge::new(&path);
    bridge
        .modify_state(|s| {
            s.parents.push(DispatchParent {
                id: "p-a".into(),
                goal: "g1".into(),
                admin_folder: "lead-a".into(),
                chat_jid: None,
                shared_workspace: Some("/tmp/cowork-ws-a".into()),
                status: "active".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![make_task("d-a", "t-a", "jid-a")],
            });
            s.parents.push(DispatchParent {
                id: "p-b".into(),
                goal: "g2".into(),
                admin_folder: "lead-b".into(),
                chat_jid: None,
                shared_workspace: Some("/tmp/cowork-ws-b".into()),
                status: "active".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![make_task("d-b", "t-b", "jid-b")],
            });
        })
        .unwrap();
    bridge.add_active_task("d-a", "jid-a");
    bridge.add_active_task("d-b", "jid-b");

    let affected = bridge.cancel_parents_for_shared_workspace("/tmp/cowork-ws-a/");
    assert_eq!(affected, vec!["jid-a".to_string()]);

    let parents = bridge.get_parents();
    assert_eq!(parents[0].status, "done");
    assert_eq!(parents[0].tasks[0].status, DispatchTaskStatus::Error);
    assert_eq!(
        parents[0].tasks[0].result.as_deref(),
        Some("Cancelled: cowork workspace deleted")
    );
    assert_eq!(parents[1].status, "active");
    assert_eq!(parents[1].tasks[0].status, DispatchTaskStatus::Processing);
    assert!(!bridge.has_active_jid_tasks("jid-a"));
    assert!(bridge.has_active_jid_tasks("jid-b"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn parents_snapshot_matches_the_dispatch_update_wire_shape() {
    // `GET /api/dispatch` exists because `dispatch:update` reaches WebSocket
    // admin clients only. The snapshot has to be the *same* shape the event
    // carries, or a client written against one breaks on the other.
    let path = tmp_state_path("snapshot");
    let bridge = DispatchBridge::new(&path);
    bridge
        .modify_state(|s| {
            s.parents.push(DispatchParent {
                id: "p1".into(),
                goal: "ship it".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "active".into(),
                created_at: "2026-08-21T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![make_task("d1", "writer", "jid-a")],
            });
        })
        .unwrap();

    let snapshot = bridge.parents_snapshot();
    let arr = snapshot.as_array().expect("parents is an array");
    assert_eq!(arr.len(), 1);
    // camelCase, matching `DispatchParent`'s serde rename.
    assert_eq!(arr[0]["id"], "p1");
    assert_eq!(arr[0]["adminFolder"], "main");
    assert_eq!(arr[0]["tasks"][0]["agentJid"], "jid-a");
    assert_eq!(arr[0]["tasks"][0]["status"], "processing");
    let _ = std::fs::remove_file(path);
}

#[test]
fn parents_snapshot_is_empty_when_no_dispatch_has_run() {
    // No state file is the normal case on a fresh daemon, not a fault: the
    // endpoint must answer with an empty array rather than a 500.
    let path = tmp_state_path("snapshot-missing");
    let _ = std::fs::remove_file(&path);
    let bridge = DispatchBridge::new(&path);
    assert_eq!(bridge.parents_snapshot(), serde_json::json!([]));
}

// ===== User-initiated retry =====

/// Build a one-parent state whose single task already failed.
fn failed_parent(path: &std::path::Path, status: DispatchTaskStatus) -> DispatchBridge {
    let bridge = DispatchBridge::new(path);
    // Without a dispatcher the relaunch `retry_task` triggers fails instantly
    // with "callback not wired" and re-errors the task — which is correct
    // behaviour, but it would hide whether the retry itself worked.
    bridge.set_send_to_agent(Arc::new(|_, _, _, _| {}));
    bridge
        .modify_state(|s| {
            let mut t = make_task("d1", "scout", "jid-a");
            t.status = status;
            t.result = Some("boom".into());
            t.completed_at = Some("2025-01-01T00:01:00Z".into());
            s.parents.push(DispatchParent {
                id: "p1".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "done".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: Some("2025-01-01T00:01:00Z".into()),
                tasks: vec![t],
            });
        })
        .unwrap();
    bridge
}

#[test]
fn retry_requeues_a_failed_task_and_revives_its_parent() {
    // Reviving the parent is the load-bearing half: a DAG whose last task
    // failed is already "done", and the scheduler skips non-active parents —
    // so without this the task would be re-queued and never picked up.
    let path = tmp_state_path("retry_error");
    let bridge = failed_parent(&path, DispatchTaskStatus::Error);

    assert_eq!(bridge.retry_task("d1").unwrap(), "scout");

    let parents = bridge.get_parents();
    assert_eq!(parents[0].status, "active");
    assert!(parents[0].completed_at.is_none());
    let t = &parents[0].tasks[0];
    // Re-queued *and* picked straight back up — `retry_task` kicks the
    // scheduler rather than waiting for the next 300ms tick.
    assert_eq!(t.status, DispatchTaskStatus::Processing);
    assert!(t.result.is_none(), "stale failure text must not survive");
    assert!(t.completed_at.is_none());
    let _ = std::fs::remove_file(path);
}

#[test]
fn retry_works_for_a_timed_out_task_too() {
    let path = tmp_state_path("retry_timeout");
    let bridge = failed_parent(&path, DispatchTaskStatus::Timeout);
    assert!(bridge.retry_task("d1").is_ok());
    assert_eq!(
        bridge.get_parents()[0].tasks[0].status,
        DispatchTaskStatus::Processing
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn retry_clears_the_previous_attempts_verdict() {
    // A verification result or file list left over from the failed attempt
    // would be read as belonging to the new run.
    let path = tmp_state_path("retry_verdict");
    let bridge = DispatchBridge::new(&path);
    bridge.set_send_to_agent(Arc::new(|_, _, _, _| {}));
    bridge
        .modify_state(|s| {
            let mut t = make_task("d1", "scout", "jid-a");
            t.status = DispatchTaskStatus::Error;
            t.verification_result =
                Some(crate::agent::dispatch_bridge::types::VerificationResult {
                    verified: false,
                    missing_items: vec!["x".into()],
                    failed_items: vec![],
                    warnings: vec![],
                    note: None,
                });
            t.file_changes = vec![crate::agent::dispatch_bridge::types::FileChange {
                path: "a.rs".into(),
                change_type: "modified".into(),
                lines_added: Some(1),
                lines_removed: None,
                summary: None,
            }];
            t.checklist = vec![crate::agent::dispatch_bridge::types::ChecklistItem {
                id: "i0".into(),
                description: "d".into(),
                status: "failed".into(),
                depends_on: vec![],
                verification_note: Some("nope".into()),
            }];
            s.parents.push(DispatchParent {
                id: "p1".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "done".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![t],
            });
        })
        .unwrap();

    bridge.retry_task("d1").unwrap();
    let t = &bridge.get_parents()[0].tasks[0];
    assert!(t.verification_result.is_none());
    assert!(t.file_changes.is_empty());
    assert_eq!(t.checklist[0].status, "pending");
    assert!(t.checklist[0].verification_note.is_none());
    let _ = std::fs::remove_file(path);
}

#[test]
fn retry_refuses_a_task_that_succeeded_or_is_still_running() {
    let path = tmp_state_path("retry_refuse");
    let bridge = DispatchBridge::new(&path);
    bridge
        .modify_state(|s| {
            let mut done = make_task("d1", "ok", "jid-a");
            done.status = DispatchTaskStatus::Done;
            // make_task's default status is Processing.
            let running = make_task("d2", "running", "jid-b");
            s.parents.push(DispatchParent {
                id: "p1".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "active".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![done, running],
            });
        })
        .unwrap();

    // The reason reaches a person through the UI, so it must name the task.
    let e = bridge.retry_task("d1").unwrap_err();
    assert!(e.contains("ok"), "got: {e}");
    let e = bridge.retry_task("d2").unwrap_err();
    assert!(e.contains("running"), "got: {e}");
    assert!(bridge.retry_task("nope").unwrap_err().contains("not found"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn retry_parent_requeues_every_failed_task_and_leaves_the_rest_alone() {
    let path = tmp_state_path("retry_parent");
    let bridge = DispatchBridge::new(&path);
    bridge.set_send_to_agent(Arc::new(|_, _, _, _| {}));
    bridge
        .modify_state(|s| {
            let mut ok = make_task("d1", "ok", "jid-a");
            ok.status = DispatchTaskStatus::Done;
            ok.result = Some("kept".into());
            let mut bad = make_task("d2", "bad", "jid-b");
            bad.status = DispatchTaskStatus::Error;
            let mut slow = make_task("d3", "slow", "jid-c");
            slow.status = DispatchTaskStatus::Timeout;
            s.parents.push(DispatchParent {
                id: "p1".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: None,
                status: "done".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![ok, bad, slow],
            });
        })
        .unwrap();

    let labels = bridge.retry_parent_failed("p1").unwrap();
    assert_eq!(labels.len(), 2);

    let parents = bridge.get_parents();
    assert_eq!(parents[0].tasks[0].status, DispatchTaskStatus::Done);
    assert_eq!(
        parents[0].tasks[0].result.as_deref(),
        Some("kept"),
        "a successful task's result must not be thrown away by a retry-all"
    );
    assert_eq!(parents[0].tasks[1].status, DispatchTaskStatus::Processing);
    assert_eq!(parents[0].tasks[2].status, DispatchTaskStatus::Processing);

    assert!(bridge
        .retry_parent_failed("p1")
        .unwrap_err()
        .contains("No failed"));
    assert!(bridge
        .retry_parent_failed("nope")
        .unwrap_err()
        .contains("not found"));
    let _ = std::fs::remove_file(path);
}

// ===== Stall sweep =====

/// A virtual task whose persona cannot be resolved. `can_start_task` returns
/// false for it forever, and the timeout sweep never looks at it because it
/// never reaches `processing`.
fn unstartable_virtual(id: &str, label: &str) -> DispatchTask {
    let mut t = make_task(id, label, "");
    t.status = DispatchTaskStatus::Registered;
    t.is_virtual = true;
    t.persona_name = Some("ghost".into());
    t
}

fn parent_with(created_at: &str, tasks: Vec<DispatchTask>) -> DispatchParent {
    DispatchParent {
        id: "p1".into(),
        goal: "g".into(),
        admin_folder: "main".into(),
        chat_jid: None,
        shared_workspace: None,
        status: "active".into(),
        created_at: created_at.into(),
        completed_at: None,
        tasks,
    }
}

#[test]
fn a_parent_whose_tasks_can_never_start_is_failed_rather_than_left_active() {
    // The hole this closes: nothing is `processing`, so the timeout sweep has
    // nothing to expire; nothing is startable, so no future event can change
    // the state. Without the sweep the parent stays `active` until the daemon
    // restarts, which from the chat looks exactly like a DAG still working.
    let path = tmp_state_path("stall_unstartable");
    let bridge = DispatchBridge::new(&path);
    bridge
        .modify_state(|s| {
            s.parents.push(parent_with(
                "2020-01-01T00:00:00Z",
                vec![unstartable_virtual("d1", "ghost-task")],
            ));
        })
        .unwrap();

    bridge.process_pending();

    let parents = bridge.get_parents();
    assert_eq!(parents[0].status, "done");
    assert_eq!(parents[0].tasks[0].status, DispatchTaskStatus::Error);
    let msg = parents[0].tasks[0].result.clone().unwrap_or_default();
    assert!(
        msg.contains("Never started"),
        "the reason must say the task never launched, not invent a failure: {msg}"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_freshly_created_parent_is_never_swept() {
    // At creation and at boot the persona registry and worker pool are briefly
    // unwired, so every task is legitimately un-startable for a moment.
    let path = tmp_state_path("stall_grace");
    let bridge = DispatchBridge::new(&path);
    let now = chrono::Utc::now().to_rfc3339();
    bridge
        .modify_state(|s| {
            s.parents.push(parent_with(
                &now,
                vec![unstartable_virtual("d1", "ghost-task")],
            ));
        })
        .unwrap();

    bridge.process_pending();

    assert_eq!(bridge.get_parents()[0].status, "active");
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_parent_with_work_in_flight_is_never_swept() {
    // `processing` means an outcome is still coming, however old the parent is.
    let path = tmp_state_path("stall_inflight");
    let bridge = DispatchBridge::new(&path);
    bridge
        .modify_state(|s| {
            // make_task defaults to Processing.
            let running = make_task("d1", "running", "jid-a");
            s.parents.push(parent_with(
                "2020-01-01T00:00:00Z",
                vec![running, unstartable_virtual("d2", "ghost-task")],
            ));
        })
        .unwrap();

    bridge.process_pending();

    let parents = bridge.get_parents();
    assert_eq!(parents[0].status, "active");
    assert_eq!(parents[0].tasks[1].status, DispatchTaskStatus::Registered);
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_parent_whose_tasks_all_finished_is_closed_by_the_sweep() {
    // Belt-and-braces: if a completion path ever misses the "all terminal"
    // check, the parent is still closed instead of hanging active forever.
    let path = tmp_state_path("stall_allterminal");
    let bridge = DispatchBridge::new(&path);
    bridge
        .modify_state(|s| {
            let mut done = make_task("d1", "ok", "jid-a");
            done.status = DispatchTaskStatus::Done;
            s.parents
                .push(parent_with("2020-01-01T00:00:00Z", vec![done]));
        })
        .unwrap();

    bridge.process_pending();

    let parents = bridge.get_parents();
    assert_eq!(parents[0].status, "done");
    assert!(parents[0].completed_at.is_some());
    assert_eq!(
        parents[0].tasks[0].status,
        DispatchTaskStatus::Done,
        "a task that succeeded must not be rewritten to error by the sweep"
    );
    let _ = std::fs::remove_file(path);
}

/// Wire a real persona registry (one persona, `max_concurrent` as given) and a
/// worker pool into `bridge`. `max_concurrent: 0` is how a test says "the
/// persona resolves but has no free slot" without having to actually run a
/// worker to occupy one.
fn wire_persona(bridge: &DispatchBridge, persona: &str, max_concurrent: u32) -> tempfile::TempDir {
    use crate::agent::persona_registry::PersonaRegistry;
    use crate::agent::virtual_worker_pool::{VirtualWorkerPool, ZenVirtualCoreApi};

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(format!("{persona}.md")),
        format!(
            "---\nname: {persona}\ndescription: test persona\nmax_concurrent: {max_concurrent}\n---\n\nBody.\n"
        ),
    )
    .unwrap();

    let registry = Arc::new(std::sync::Mutex::new(PersonaRegistry::new(
        dir.path().to_path_buf(),
    )));
    let pool = Arc::new(VirtualWorkerPool::new(Arc::new(ZenVirtualCoreApi::new(
        None,
    ))));
    bridge.set_virtual_workers(registry, pool);
    dir
}

fn resolvable_virtual(id: &str, label: &str, persona: &str) -> DispatchTask {
    let mut t = make_task(id, label, "");
    t.status = DispatchTaskStatus::Registered;
    t.is_virtual = true;
    t.persona_name = Some(persona.into());
    t
}

#[test]
fn a_task_merely_waiting_for_a_busy_persona_is_not_swept_as_stalled() {
    // The bug this closes: capacity is contended globally, so two parents under
    // different admins sharing one persona at `max_concurrent` leave the second
    // with nothing `processing` and nothing startable — the exact shape the
    // sweep kills, and it blamed persona resolution in the message. Waiting for
    // a peer to finish is not a stall.
    let path = tmp_state_path("stall_busy_persona");
    let bridge = DispatchBridge::new(&path);
    let _dir = wire_persona(&bridge, "busy", 0);
    bridge
        .modify_state(|s| {
            s.parents.push(parent_with(
                "2020-01-01T00:00:00Z",
                vec![resolvable_virtual("d1", "waiting", "busy")],
            ));
        })
        .unwrap();

    bridge.process_pending();

    let parents = bridge.get_parents();
    assert_eq!(
        parents[0].status, "active",
        "a parent whose only task is queued behind a busy persona must stay active"
    );
    assert_eq!(parents[0].tasks[0].status, DispatchTaskStatus::Registered);
    let _ = std::fs::remove_file(path);
}

#[test]
fn an_unresolvable_persona_is_still_swept_when_the_registry_is_wired() {
    // The counterpart: with the registry present and the persona genuinely
    // absent from it, nothing will ever deliver this task — that *is* a stall,
    // and loosening the sweep must not have made it unreachable.
    let path = tmp_state_path("stall_absent_persona");
    let bridge = DispatchBridge::new(&path);
    let _dir = wire_persona(&bridge, "present", 4);
    bridge
        .modify_state(|s| {
            s.parents.push(parent_with(
                "2020-01-01T00:00:00Z",
                vec![resolvable_virtual("d1", "ghost", "absent")],
            ));
        })
        .unwrap();

    bridge.process_pending();

    let parents = bridge.get_parents();
    assert_eq!(parents[0].status, "done");
    assert_eq!(parents[0].tasks[0].status, DispatchTaskStatus::Error);
    let _ = std::fs::remove_file(path);
}

#[test]
fn the_launch_gate_and_the_stall_gate_disagree_on_a_busy_persona() {
    // The invariant the split exists for, pinned directly: at capacity the
    // launch gate says "not now" while the stall gate says "yes, eventually".
    // Collapsing them back into one predicate makes the sweep kill queued work.
    let path = tmp_state_path("stall_predicate_split");
    let bridge = DispatchBridge::new(&path);
    let _dir = wire_persona(&bridge, "busy", 0);
    let task = resolvable_virtual("d1", "waiting", "busy");
    let all = vec![task.clone()];

    assert!(
        bridge.is_startable_in_principle(&task, &all),
        "the persona resolves, so this task can start once a slot frees"
    );
    assert!(
        !bridge.can_start_task(&task, &all, ""),
        "but there is no free slot right now"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn repeated_writes_to_one_file_record_a_single_change() {
    // An agent editing the same file in a loop must not grow the state file
    // (and re-broadcast the whole parents tree) once per edit.
    let path = tmp_state_path("file_change_dedup");
    let bridge = DispatchBridge::new(&path);
    bridge
        .modify_state(|s| {
            s.parents.push(parent_with(
                "2020-01-01T00:00:00Z",
                vec![make_task("d1", "writer", "jid-a")],
            ));
        })
        .unwrap();

    bridge.add_file_change("d1", "/w/a.rs", "modified");
    bridge.add_file_change("d1", "/w/a.rs", "modified");
    bridge.add_file_change("d1", "/w/b.rs", "modified");

    let changes = &bridge.get_parents()[0].tasks[0].file_changes;
    assert_eq!(changes.len(), 2, "same path twice is one fact, not two");
    let mut paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, vec!["/w/a.rs", "/w/b.rs"]);
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_file_change_for_an_unknown_task_is_dropped_without_panicking() {
    let path = tmp_state_path("file_change_unknown");
    let bridge = DispatchBridge::new(&path);
    bridge.add_file_change("nope", "/w/a.rs", "modified");
    assert!(bridge.get_parents().is_empty());
    let _ = std::fs::remove_file(path);
}

// ===== Write-set gate =====

use super::types::TaskIo;

/// A persistent task on its own jid, so the per-agent concurrency gate never
/// interferes with what these tests are actually measuring.
fn writer(id: &str, writes: &[&str]) -> DispatchTask {
    let mut t = make_task(id, id, &format!("jid-{id}"));
    t.status = DispatchTaskStatus::Registered;
    t.io = TaskIo::Exclusive;
    t.writes = writes.iter().map(|s| s.to_string()).collect();
    t
}

/// Run one scheduler tick over `tasks` in `workspace` and report which task ids
/// were actually handed to an agent.
fn launched_in_one_tick(
    name: &str,
    workspace: Option<&str>,
    tasks: Vec<DispatchTask>,
) -> (Vec<String>, PathBuf) {
    let path = tmp_state_path(name);
    let bridge = DispatchBridge::new(&path);
    let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let seen = Arc::clone(&seen);
        bridge.set_send_to_agent(Arc::new(
            move |_jid: &str, task_id: &str, _prompt: &str, _ws: &str| {
                seen.lock().unwrap().push(task_id.to_string());
            },
        ));
    }
    let ws = workspace.map(|s| s.to_string());
    bridge
        .modify_state(|s| {
            s.parents.push(DispatchParent {
                id: "p1".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: ws,
                status: "active".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks,
            });
        })
        .unwrap();
    bridge.process_pending();
    let out = seen.lock().unwrap().clone();
    (out, path)
}

#[test]
fn tasks_writing_different_places_both_start_in_the_same_tick() {
    let (launched, path) = launched_in_one_tick(
        "ws_disjoint",
        Some("/w"),
        vec![
            writer("d1", &["src/agent/**"]),
            writer("d2", &["src/mcp/**"]),
        ],
    );
    assert_eq!(launched.len(), 2, "disjoint write-sets must not serialize");
    let _ = std::fs::remove_file(path);
}

#[test]
fn two_tasks_claiming_the_same_file_cannot_both_start_in_one_tick() {
    // The case a gate reading the state file would miss entirely: within a
    // single `process_pending` both siblings still look `Registered` on disk,
    // so only an in-memory claim taken at launch can catch this pair.
    let (launched, path) = launched_in_one_tick(
        "ws_same_tick",
        Some("/w"),
        vec![
            writer("d1", &["src/agent/pool.rs"]),
            writer("d2", &["src/agent/**"]),
        ],
    );
    assert_eq!(
        launched.len(),
        1,
        "overlapping write-sets launched together: {launched:?}"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn read_only_tasks_never_block_each_other_or_a_writer() {
    let mut a = writer("d1", &["src/**"]);
    a.io = TaskIo::ReadOnly;
    let mut b = writer("d2", &["src/**"]);
    b.io = TaskIo::ReadOnly;
    let c = writer("d3", &["src/**"]);
    let (launched, path) = launched_in_one_tick("ws_readonly", Some("/w"), vec![a, b, c]);
    assert_eq!(
        launched.len(),
        3,
        "read-only tasks cannot collide, so nothing may be held back: {launched:?}"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn an_isolated_task_takes_the_whole_workspace() {
    let mut a = writer("d1", &[]);
    a.io = TaskIo::Isolated;
    let (launched, path) = launched_in_one_tick(
        "ws_isolated",
        Some("/w"),
        vec![a, writer("d2", &["docs/x.md"])],
    );
    assert_eq!(
        launched.len(),
        1,
        "isolated must exclude peers: {launched:?}"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn undeclared_tasks_keep_the_parallelism_they_have_today() {
    // Turning the gate on must not serialize DAGs that predate it. An empty
    // write-set means "not declared", and stays unconstrained.
    let (launched, path) = launched_in_one_tick(
        "ws_undeclared",
        Some("/w"),
        vec![writer("d1", &[]), writer("d2", &[])],
    );
    assert_eq!(launched.len(), 2, "undeclared tasks must not be gated");
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_claim_is_released_on_every_way_a_task_can_end() {
    let path = tmp_state_path("ws_release");
    let bridge = DispatchBridge::new(&path);
    let held = |b: &DispatchBridge| b.inner.lock().unwrap().active_writes.len();

    for (id, finish) in [
        ("d1", 0usize), // done
        ("d2", 1),      // error
        ("d3", 2),      // timeout
    ] {
        let task = writer(id, &["src/a.rs"]);
        bridge.claim_writes(&task, "/w");
        assert_eq!(held(&bridge), 1, "claim did not register for {id}");
        match finish {
            0 => bridge.mark_task_done(id, "ok"),
            1 => bridge.mark_task_error(id, "boom"),
            _ => bridge.release_writes(id),
        }
        assert_eq!(held(&bridge), 0, "claim leaked after {id} finished");
    }
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_write_set_in_another_workspace_does_not_block() {
    let path = tmp_state_path("ws_other_dir");
    let bridge = DispatchBridge::new(&path);
    let holder = writer("d1", &["src/a.rs"]);
    bridge.claim_writes(&holder, "/other");
    let candidate = writer("d2", &["src/a.rs"]);
    assert!(
        bridge.writes_are_free(&candidate, "/w"),
        "a claim on a different workspace is unrelated"
    );
    assert!(
        !bridge.writes_are_free(&candidate, "/other"),
        "but the same workspace still collides"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_state_file_written_before_this_field_existed_still_loads() {
    let path = tmp_state_path("ws_legacy_state");
    std::fs::write(
        &path,
        r#"{"_seq":2,"agents":[],"parents":[{"id":"p1","goal":"g","adminFolder":"main",
        "sharedWorkspace":null,"status":"active","createdAt":"2025-01-01T00:00:00Z",
        "completedAt":null,"tasks":[{"id":"d1","label":"l","agentId":"a","agentJid":"j",
        "dependsOn":[],"prompt":"p","status":"registered","result":null,
        "createdAt":"2025-01-01T00:00:00Z","startedAt":null,"timeoutAt":null,
        "completedAt":null}]}]}"#,
    )
    .unwrap();
    let bridge = DispatchBridge::new(&path);
    let parents = bridge.get_parents();
    assert_eq!(
        parents.len(),
        1,
        "a pre-write-set state file must still load"
    );
    assert_eq!(parents[0].tasks[0].io, TaskIo::Exclusive);
    assert!(parents[0].tasks[0].writes.is_empty());
    let _ = std::fs::remove_file(path);
}

#[test]
fn an_unknown_io_value_falls_back_to_the_constrained_option() {
    // A future variant read by an older binary must not lose the dispatch tree,
    // and must not be read as the permissive option either.
    let t: DispatchTask = serde_json::from_str(
        r#"{"id":"d1","label":"l","agentId":"a","agentJid":"j","dependsOn":[],"prompt":"p",
        "status":"registered","result":null,"createdAt":"x","startedAt":null,
        "timeoutAt":null,"completedAt":null,"io":"someFutureMode"}"#,
    )
    .unwrap();
    assert_eq!(t.io, TaskIo::Exclusive);
}

#[test]
fn worktree_isolation_runs_the_task_in_its_own_checkout() {
    use std::sync::atomic::{AtomicBool, Ordering};
    // A real repository as the shared workspace.
    let repo_dir = tempfile::tempdir().unwrap();
    let repo = repo_dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let st = std::process::Command::new("git").args(args).current_dir(&repo).status().unwrap();
        assert!(st.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    std::env::set_var("SENCLAW_WORKTREES_DIR", repo_dir.path().join("wt"));

    let path = tmp_state_path("worktree-isolation");
    let bridge = DispatchBridge::new(&path);
    let fired = Arc::new(AtomicBool::new(false));
    let seen_ws = Arc::new(std::sync::Mutex::new(String::new()));
    {
        let f = Arc::clone(&fired);
        let seen = Arc::clone(&seen_ws);
        bridge.set_send_to_agent(Arc::new(move |_jid: &str, _task_id: &str, _prompt: &str, ws: &str| {
            *seen.lock().unwrap() = ws.to_string();
            f.store(true, Ordering::SeqCst);
        }));
    }
    let mut t = make_task("d1", "fix-thing", "jid-x");
    t.status = DispatchTaskStatus::Registered;
    t.isolation = crate::agent::dispatch_bridge::TaskIsolation::Worktree;
    bridge
        .modify_state(|s| {
            s.parents.push(DispatchParent {
                id: "p9".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: Some(repo.to_string_lossy().to_string()),
                status: "active".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![t],
            });
        })
        .unwrap();
    bridge.process_pending();
    assert!(fired.load(Ordering::SeqCst));

    // The agent was pointed at the worktree, not the shared checkout.
    let ws = seen_ws.lock().unwrap().clone();
    assert_ne!(ws, repo.to_string_lossy());
    assert!(ws.contains("p9-fix-thing"), "{ws}");
    assert!(std::path::Path::new(&ws).join("a.txt").exists());
    let state = bridge.read_state().unwrap();
    let task = &state.parents[0].tasks[0];
    let wt = task.worktree.as_ref().expect("worktree recorded on the task");
    assert_eq!(wt.branch, "senclaw/p9-fix-thing");
    assert_eq!(wt.base, "main");

    // An edit in the worktree does not touch the shared checkout, and the
    // result tells the orchestrator where the branch is.
    std::fs::write(std::path::Path::new(&ws).join("a.txt"), "two\n").unwrap();
    bridge.mark_task_done("d1", "done editing");
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "one\n");
    let state = bridge.read_state().unwrap();
    let result = state.parents[0].tasks[0].result.clone().unwrap();
    assert!(result.contains("<worktree>"), "{result}");
    assert!(result.contains("senclaw/p9-fix-thing"));
    assert!(result.contains("1 file(s) changed"));
}

#[test]
fn worktree_isolation_falls_back_loudly_outside_a_repository() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let plain = tempfile::tempdir().unwrap();
    let path = tmp_state_path("worktree-fallback");
    let bridge = DispatchBridge::new(&path);
    let fired = Arc::new(AtomicBool::new(false));
    let seen_ws = Arc::new(std::sync::Mutex::new(String::new()));
    {
        let f = Arc::clone(&fired);
        let seen = Arc::clone(&seen_ws);
        bridge.set_send_to_agent(Arc::new(move |_j: &str, _t: &str, _p: &str, ws: &str| {
            *seen.lock().unwrap() = ws.to_string();
            f.store(true, Ordering::SeqCst);
        }));
    }
    let mut t = make_task("d1", "x", "jid-x");
    t.status = DispatchTaskStatus::Registered;
    t.isolation = crate::agent::dispatch_bridge::TaskIsolation::Worktree;
    bridge
        .modify_state(|s| {
            s.parents.push(DispatchParent {
                id: "p8".into(),
                goal: "g".into(),
                admin_folder: "main".into(),
                chat_jid: None,
                shared_workspace: Some(plain.path().to_string_lossy().to_string()),
                status: "active".into(),
                created_at: "2025-01-01T00:00:00Z".into(),
                completed_at: None,
                tasks: vec![t],
            });
        })
        .unwrap();
    bridge.process_pending();
    assert!(fired.load(Ordering::SeqCst));
    assert_eq!(*seen_ws.lock().unwrap(), plain.path().to_string_lossy());
    bridge.mark_task_done("d1", "did it");
    let state = bridge.read_state().unwrap();
    let task = &state.parents[0].tasks[0];
    assert!(task.worktree.is_none());
    let result = task.result.clone().unwrap();
    assert!(result.contains("isolation=worktree ignored"), "{result}");
}
