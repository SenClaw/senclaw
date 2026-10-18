use rusqlite::Connection;

use crate::config::Config;
use crate::types::{
    AgentMode, ContextMode, GroupBinding, RunStatus, ScheduleType, ScheduledTask, StoredMessage,
    TaskRunLogInsert, TaskStatus,
};

use super::Db;

fn cfg() -> Config {
    Config::from_env()
}

fn sample_group() -> GroupBinding {
    GroupBinding {
        jid: "tg:group:1".into(),
        folder: "team-a".into(),
        name: "Team A".into(),
        channel: "telegram".into(),
        group_type: "chat".into(),
        requires_trigger: false,
        allowed_tools: Some(vec!["Read".into(), "Grep".into()]),
        allowed_paths: None,
        allowed_work_dirs: Some(vec!["/tmp/work".into()]),
        bot_token: Some("tok".into()),
        max_messages: Some(50),
        llm_config_id: Some("llm-openai-1".into()),
        last_active: None,
        added_at: "2026-04-28T00:00:00Z".into(),
        project_id: None,
    }
}

#[test]
fn open_in_memory_smoke() {
    Db::open_in_memory(&cfg()).unwrap();
}

#[test]
fn group_upsert_get_list_delete() {
    let db = Db::open_in_memory(&cfg()).unwrap();
    let g = sample_group();
    db.upsert_group(&g).unwrap();
    let got = db.get_group(&g.jid).unwrap().unwrap();
    assert_eq!(got.folder, g.folder);
    assert_eq!(
        got.allowed_tools.as_deref(),
        Some(&["Read".into(), "Grep".into()][..])
    );
    assert_eq!(got.allowed_paths, None);
    assert_eq!(
        got.allowed_work_dirs.as_deref(),
        Some(&["/tmp/work".into()][..])
    );
    assert_eq!(got.llm_config_id.as_deref(), Some("llm-openai-1"));

    // Clearing the per-group model override round-trips to NULL.
    let mut g_no_model = g.clone();
    g_no_model.llm_config_id = None;
    db.upsert_group(&g_no_model).unwrap();
    assert_eq!(db.get_group(&g.jid).unwrap().unwrap().llm_config_id, None);
    db.upsert_group(&g).unwrap();

    let mut g2 = g.clone();
    g2.name = "Renamed".into();
    db.upsert_group(&g2).unwrap();
    assert_eq!(db.get_group(&g.jid).unwrap().unwrap().name, "Renamed");

    let all = db.list_groups().unwrap();
    assert_eq!(all.len(), 1);

    db.delete_group(&g.jid).unwrap();
    assert!(db.get_group(&g.jid).unwrap().is_none());
}

#[test]
fn rename_group_jid_atomic() {
    let db = Db::open_in_memory(&cfg()).unwrap();
    db.upsert_group(&sample_group()).unwrap();
    let renamed = db
        .rename_group_jid("tg:group:1", "tg:group:99")
        .unwrap()
        .unwrap();
    assert_eq!(renamed.jid, "tg:group:99");
    assert!(db.get_group("tg:group:1").unwrap().is_none());
    assert!(db.get_group("tg:group:99").unwrap().is_some());
}

#[test]
fn approved_tools_separate_from_allowed_whitelist() {
    let db = Db::open_in_memory(&cfg()).unwrap();
    let g = sample_group();
    db.upsert_group(&g).unwrap();

    // "Always allow" approvals land in approved_tools only — the configured
    // allowed_tools whitelist must not grow (that was the tool-roster bug).
    db.append_group_approved_tool(&g.jid, "Skill").unwrap();
    db.append_group_approved_tool(&g.jid, "Skill").unwrap(); // dedup
    db.append_group_approved_tool(&g.jid, "mcp__ssh-manager-mcp__ssh_execute_command")
        .unwrap();
    assert_eq!(
        db.get_group_approved_tools(&g.jid).unwrap(),
        vec![
            "Skill".to_string(),
            "mcp__ssh-manager-mcp__ssh_execute_command".to_string()
        ]
    );
    assert_eq!(
        db.get_group(&g.jid)
            .unwrap()
            .unwrap()
            .allowed_tools
            .as_deref(),
        Some(&["Read".into(), "Grep".into()][..])
    );

    // Upserting the binding again (config reload) must not wipe approvals.
    db.upsert_group(&g).unwrap();
    assert_eq!(db.get_group_approved_tools(&g.jid).unwrap().len(), 2);

    // Renaming the jid carries the approvals across.
    db.rename_group_jid(&g.jid, "tg:group:2").unwrap().unwrap();
    assert_eq!(db.get_group_approved_tools("tg:group:2").unwrap().len(), 2);
    assert!(db.get_group_approved_tools(&g.jid).unwrap().is_empty());

    // Unknown group reads as empty.
    assert!(db.get_group_approved_tools("nope").unwrap().is_empty());
}

#[test]
fn message_fifo_trims() {
    let db = Db::open_in_memory(&cfg()).unwrap();
    for i in 0..5 {
        let msg = StoredMessage {
            message_id: format!("m{i}"),
            chat_jid: "tg:group:1".into(),
            sender_jid: "u".into(),
            sender_name: "u".into(),
            content: format!("hi {i}"),
            timestamp: format!("2026-04-28T00:00:0{i}Z"),
            is_from_me: false,
            is_bot_reply: false,
            reply_to_id: None,
            media_type: None,
            attachments: None,
        };
        db.insert_message(&msg, 3).unwrap();
    }
    let kept = db.get_messages("tg:group:1", None).unwrap();
    assert_eq!(kept.len(), 3);
    let ids: Vec<&str> = kept.iter().map(|m| m.message_id.as_str()).collect();
    assert_eq!(ids, ["m2", "m3", "m4"]);
}

#[test]
fn message_since_filter() {
    let db = Db::open_in_memory(&cfg()).unwrap();
    for i in 0..3 {
        let msg = StoredMessage {
            message_id: format!("m{i}"),
            chat_jid: "tg:group:1".into(),
            sender_jid: "u".into(),
            sender_name: "u".into(),
            content: "x".into(),
            timestamp: format!("2026-04-28T00:00:0{i}Z"),
            is_from_me: false,
            is_bot_reply: false,
            reply_to_id: None,
            media_type: None,
            attachments: None,
        };
        db.insert_message(&msg, 100).unwrap();
    }
    let after = db
        .get_messages("tg:group:1", Some("2026-04-28T00:00:00Z"))
        .unwrap();
    assert_eq!(after.len(), 2);
}

#[test]
fn message_pagination() {
    let db = Db::open_in_memory(&cfg()).unwrap();
    for i in 0..10 {
        let msg = StoredMessage {
            message_id: format!("m{i}"),
            chat_jid: "tg:group:1".into(),
            sender_jid: "u".into(),
            sender_name: "u".into(),
            content: format!("msg {i}"),
            timestamp: format!("2026-04-28T00:00:{:02}Z", i),
            is_from_me: false,
            is_bot_reply: false,
            reply_to_id: None,
            media_type: None,
            attachments: None,
        };
        db.insert_message(&msg, 100).unwrap();
    }

    let p1 = db.get_messages_paginated("tg:group:1", 3, 0).unwrap();
    assert_eq!(p1.len(), 3);
    assert_eq!(p1[0].message_id, "m9");
    assert_eq!(p1[1].message_id, "m8");
    assert_eq!(p1[2].message_id, "m7");

    let p2 = db.get_messages_paginated("tg:group:1", 3, 3).unwrap();
    assert_eq!(p2.len(), 3);
    assert_eq!(p2[0].message_id, "m6");
    assert_eq!(p2[1].message_id, "m5");
    assert_eq!(p2[2].message_id, "m4");
}

#[test]
fn watch_mode_and_its_config_survive_the_round_trip() {
    // The watch column and the `watch` context mode are both new, and both are
    // read back by hand-written mapping code — a silent drop here would make a
    // watch fire forever with no config to tell it when to stop.
    let db = Db::open_in_memory(&cfg()).unwrap();
    let task = ScheduledTask {
        id: "w1".into(),
        group_folder: "team-a".into(),
        chat_jid: "tg:group:1".into(),
        prompt: "resume".into(),
        schedule_type: ScheduleType::Interval,
        schedule_value: "60000".into(),
        context_mode: ContextMode::Watch,
        agent_mode: AgentMode::Agent,
        script_command: None,
        watch_json: Some(
            r#"{"deadline_at":"2999-01-01T00:00:00Z","resume_prompt":"go","max_checks":7}"#.into(),
        ),
        next_run: Some("2026-04-28T00:05:00Z".into()),
        last_run: None,
        last_result: None,
        status: TaskStatus::Active,
        created_at: "2026-04-28T00:00:00Z".into(),
    };
    db.insert_task(&task).unwrap();

    let back = db.get_due_tasks("2026-04-28T00:10:00Z").unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].context_mode, ContextMode::Watch);
    let cfg = crate::scheduler::watch::WatchConfig::parse(back[0].watch_json.as_ref().unwrap())
        .expect("config must survive the column");
    assert_eq!(cfg.max_checks, 7);
    assert!(cfg.is_agent_fallback());

    // The counter is written back between ticks, not through update_task_run.
    let bumped = crate::scheduler::watch::WatchConfig { checks: 3, ..cfg };
    db.update_task_watch_json("w1", &bumped.to_json().unwrap())
        .unwrap();
    let reread = db.get_tasks_by_group("team-a").unwrap();
    let stored =
        crate::scheduler::watch::WatchConfig::parse(reread[0].watch_json.as_ref().unwrap())
            .unwrap();
    assert_eq!(stored.checks, 3);
}

#[test]
fn active_watches_are_scoped_to_their_own_chat() {
    // A watch belongs to the conversation that armed it: that is where its
    // card renders and where its Stop button means anything. Listing by chat
    // is what stops one chat cancelling another's wait.
    let db = Db::open_in_memory(&cfg()).unwrap();
    let mk = |id: &str, jid: &str, mode: ContextMode, status: TaskStatus| ScheduledTask {
        id: id.into(),
        group_folder: "main".into(),
        chat_jid: jid.into(),
        prompt: "resume".into(),
        schedule_type: ScheduleType::Interval,
        schedule_value: "60000".into(),
        context_mode: mode,
        agent_mode: AgentMode::Agent,
        script_command: None,
        watch_json: Some(
            r#"{"deadline_at":"2999-01-01T00:00:00Z","resume_prompt":"go","label":"job"}"#.into(),
        ),
        next_run: Some("2026-04-28T00:05:00Z".into()),
        last_run: None,
        last_result: None,
        status,
        created_at: "2026-04-28T00:00:00Z".into(),
    };
    db.insert_task(&mk(
        "w-mine",
        "web:a",
        ContextMode::Watch,
        TaskStatus::Active,
    ))
    .unwrap();
    db.insert_task(&mk(
        "w-other",
        "web:b",
        ContextMode::Watch,
        TaskStatus::Active,
    ))
    .unwrap();
    db.insert_task(&mk(
        "w-done",
        "web:a",
        ContextMode::Watch,
        TaskStatus::Completed,
    ))
    .unwrap();
    // A plain schedule on the same chat must not be offered as a watch.
    db.insert_task(&mk(
        "s-plain",
        "web:a",
        ContextMode::Group,
        TaskStatus::Active,
    ))
    .unwrap();

    let mine = db.get_active_watches("web:a").unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].id, "w-mine");

    // Stopping is a status change, so the row survives as the record that the
    // wait happened — and it drops out of the active list immediately.
    assert!(db.stop_watch("w-mine").unwrap());
    assert!(db.get_active_watches("web:a").unwrap().is_empty());
    assert_eq!(db.get_tasks_by_group("main").unwrap().len(), 4);

    // Reporting success for something that did not stop would tell the user a
    // watch was cancelled while it in fact kept running.
    assert!(!db.stop_watch("w-mine").unwrap(), "already stopped");
    assert!(!db.stop_watch("khong-co").unwrap(), "unknown id");
    assert!(
        !db.stop_watch("s-plain").unwrap(),
        "an ordinary schedule must not be stoppable through the watch path"
    );
    assert_eq!(
        db.get_tasks_by_group("main")
            .unwrap()
            .iter()
            .filter(|t| t.id == "s-plain" && t.status == TaskStatus::Active)
            .count(),
        1
    );
}

#[test]
fn task_lifecycle_and_logs() {
    let db = Db::open_in_memory(&cfg()).unwrap();
    let task = ScheduledTask {
        id: "t1".into(),
        group_folder: "team-a".into(),
        chat_jid: "tg:group:1".into(),
        prompt: "do thing".into(),
        schedule_type: ScheduleType::Cron,
        schedule_value: "*/5 * * * *".into(),
        context_mode: ContextMode::Isolated,
        agent_mode: AgentMode::Agent,
        script_command: None,
        watch_json: None,
        next_run: Some("2026-04-28T00:05:00Z".into()),
        last_run: None,
        last_result: None,
        status: TaskStatus::Active,
        created_at: "2026-04-28T00:00:00Z".into(),
    };
    db.insert_task(&task).unwrap();
    assert_eq!(db.get_tasks_by_group("team-a").unwrap().len(), 1);

    let due = db.get_due_tasks("2026-04-28T00:10:00Z").unwrap();
    assert_eq!(due.len(), 1);

    let big = "x".repeat(800);
    db.update_task_run(
        "t1",
        Some("2026-04-28T00:10:00Z"),
        "2026-04-28T00:05:00Z",
        Some(&big),
        TaskStatus::Active,
    )
    .unwrap();
    let after = &db.get_tasks_by_group("team-a").unwrap()[0];
    assert_eq!(after.last_result.as_deref().unwrap().chars().count(), 500);

    db.insert_task_run_log(&TaskRunLogInsert {
        task_id: "t1".into(),
        run_at: "2026-04-28T00:05:00Z".into(),
        duration_ms: Some(120),
        status: RunStatus::Success,
        result: Some("ok".into()),
        error: None,
    })
    .unwrap();
    let logs = db.get_task_run_logs("t1", 10).unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].status, RunStatus::Success);
    assert_eq!(logs[0].duration_ms, Some(120));

    assert!(db.delete_task("t1").unwrap());
    assert!(!db.delete_task("t1").unwrap());
}

#[test]
fn router_state_get_set() {
    let db = Db::open_in_memory(&cfg()).unwrap();
    assert!(db.get_router_state("k").unwrap().is_none());
    db.set_router_state("k", "v").unwrap();
    assert_eq!(db.get_router_state("k").unwrap().as_deref(), Some("v"));
    db.set_router_state("k", "v2").unwrap();
    assert_eq!(db.get_router_state("k").unwrap().as_deref(), Some("v2"));

    db.set_last_agent_timestamp("tg:group:1", "2026-04-28T00:00:00Z")
        .unwrap();
    assert_eq!(
        db.get_last_agent_timestamp("tg:group:1")
            .unwrap()
            .as_deref(),
        Some("2026-04-28T00:00:00Z")
    );
}

#[test]
fn delete_messages_and_timestamp() {
    let db = Db::open_in_memory(&cfg()).unwrap();
    for i in 0..5 {
        let msg = StoredMessage {
            message_id: format!("m{i}"),
            chat_jid: "tg:group:1".into(),
            sender_jid: "u".into(),
            sender_name: "u".into(),
            content: format!("hi {i}"),
            timestamp: format!("2026-04-28T00:00:0{i}Z"),
            is_from_me: false,
            is_bot_reply: false,
            reply_to_id: None,
            media_type: None,
            attachments: None,
        };
        db.insert_message(&msg, 100).unwrap();
    }
    assert_eq!(db.count_messages("tg:group:1").unwrap(), 5);

    db.set_last_agent_timestamp("tg:group:1", "2026-04-28T00:00:04Z")
        .unwrap();
    assert!(db.get_last_agent_timestamp("tg:group:1").unwrap().is_some());

    let deleted = db.delete_messages_for_jid("tg:group:1").unwrap();
    assert_eq!(deleted, 5);
    assert_eq!(db.count_messages("tg:group:1").unwrap(), 0);

    db.delete_agent_timestamp("tg:group:1").unwrap();
    assert!(db.get_last_agent_timestamp("tg:group:1").unwrap().is_none());
}

#[test]
fn count_messages_by_jid() {
    let db = Db::open_in_memory(&cfg()).unwrap();
    assert_eq!(db.count_messages("tg:group:1").unwrap(), 0);
    let msg = StoredMessage {
        message_id: "m1".into(),
        chat_jid: "tg:group:1".into(),
        sender_jid: "u".into(),
        sender_name: "u".into(),
        content: "hi".into(),
        timestamp: "2026-04-28T00:00:00Z".into(),
        is_from_me: false,
        is_bot_reply: false,
        reply_to_id: None,
        media_type: None,
        attachments: None,
    };
    db.insert_message(&msg, 100).unwrap();
    assert_eq!(db.count_messages("tg:group:1").unwrap(), 1);
    assert_eq!(db.count_messages("tg:group:2").unwrap(), 0);
}

#[test]
fn migration_adds_missing_columns_on_existing_db() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    {
        let conn = Connection::open(tmp.path()).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE groups (
              jid TEXT PRIMARY KEY, folder TEXT UNIQUE NOT NULL, name TEXT NOT NULL DEFAULT '',
              channel TEXT NOT NULL DEFAULT 'telegram', is_admin INTEGER NOT NULL DEFAULT 0,
              requires_trigger INTEGER NOT NULL DEFAULT 1, allowed_tools TEXT, allowed_paths TEXT,
              bot_token TEXT, max_messages INTEGER, last_active TEXT, added_at TEXT NOT NULL
            );
            CREATE TABLE scheduled_tasks (
              id TEXT PRIMARY KEY, group_folder TEXT NOT NULL, chat_jid TEXT NOT NULL,
              prompt TEXT NOT NULL, schedule_type TEXT NOT NULL, schedule_value TEXT NOT NULL,
              context_mode TEXT NOT NULL DEFAULT 'isolated', next_run TEXT, last_run TEXT,
              last_result TEXT, status TEXT NOT NULL DEFAULT 'active', created_at TEXT NOT NULL
            );
            "#,
        )
        .unwrap();
    }
    // Test uses a single SQLite file for both main + cognitive — the
    // public API supports any two paths but reusing the same file
    // keeps the fixture simple. Db::open_at opens two separate
    // Connections regardless, so locking still works.
    let db = Db::open_at(tmp.path(), tmp.path(), &cfg()).unwrap();
    db.upsert_group(&sample_group()).unwrap();
    let got = db.get_group("tg:group:1").unwrap().unwrap();
    assert_eq!(
        got.allowed_work_dirs.as_deref(),
        Some(&["/tmp/work".into()][..])
    );
}

#[test]
fn migration_moves_schedule_allowed_tools_pollution_to_approved() {
    // Legacy DB: no approved_tools column, and a schedule group whose
    // allowed_tools was polluted by the old "always allow" handler. The
    // migration must move that value to approved_tools (schedule groups are
    // never created with a whitelist) while leaving chat groups untouched.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    {
        let conn = Connection::open(tmp.path()).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE groups (
              jid TEXT PRIMARY KEY, folder TEXT NOT NULL, name TEXT NOT NULL DEFAULT '',
              channel TEXT NOT NULL DEFAULT 'telegram', group_type TEXT NOT NULL DEFAULT 'chat',
              is_admin INTEGER NOT NULL DEFAULT 0, requires_trigger INTEGER NOT NULL DEFAULT 1,
              allowed_tools TEXT, allowed_paths TEXT, allowed_work_dirs TEXT,
              bot_token TEXT, max_messages INTEGER, llm_config_id TEXT,
              last_active TEXT, added_at TEXT NOT NULL
            );
            INSERT INTO groups (jid, folder, allowed_tools, added_at)
              VALUES ('schedule:abc', 'sched', '["Skill"]', '2026-01-01');
            INSERT INTO groups (jid, folder, allowed_tools, added_at)
              VALUES ('tg:group:7', 'chatty', '["Read"]', '2026-01-01');
            "#,
        )
        .unwrap();
    }
    let db = Db::open_at(tmp.path(), tmp.path(), &cfg()).unwrap();

    let sched = db.get_group("schedule:abc").unwrap().unwrap();
    assert_eq!(sched.allowed_tools, None, "whitelist pollution cleared");
    assert_eq!(
        db.get_group_approved_tools("schedule:abc").unwrap(),
        vec!["Skill".to_string()]
    );

    let chat = db.get_group("tg:group:7").unwrap().unwrap();
    assert_eq!(
        chat.allowed_tools.as_deref(),
        Some(&["Read".into()][..]),
        "chat-group whitelist untouched"
    );
}

// ===== Projects: the schema upgrade and the session's project =====

const PRE_PROJECT_GROUPS: &str = r#"
    CREATE TABLE groups (
      jid TEXT PRIMARY KEY, folder TEXT NOT NULL, name TEXT NOT NULL DEFAULT '',
      channel TEXT NOT NULL DEFAULT 'telegram', group_type TEXT NOT NULL DEFAULT 'chat',
      is_admin INTEGER NOT NULL DEFAULT 0, requires_trigger INTEGER NOT NULL DEFAULT 1,
      allowed_tools TEXT, approved_tools TEXT, allowed_paths TEXT, allowed_work_dirs TEXT,
      bot_token TEXT, max_messages INTEGER, llm_config_id TEXT,
      last_active TEXT, added_at TEXT NOT NULL
    );
    INSERT INTO groups (jid, folder, name, channel, group_type, allowed_work_dirs, added_at)
      VALUES ('web:main:old1', 'main', 'Old chat', '', 'chat', NULL, '2026-01-01T00:00:00Z');
    INSERT INTO groups (jid, folder, name, channel, group_type, allowed_work_dirs, approved_tools, added_at)
      VALUES ('web:main:old2', 'main', 'Old code', '', 'code', '["/work/old"]', '["Bash"]', '2026-01-02T00:00:00Z');
"#;

fn project_input(name: &str, root: &str) -> crate::projects::NewProject {
    crate::projects::NewProject {
        name: name.into(),
        root: root.into(),
        description: String::new(),
        facts_write: crate::projects::FactsWrite::Project,
        inherit_common: true,
    }
}

#[test]
fn a_pre_project_database_is_upgraded_in_place_and_keeps_every_session() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    {
        let conn = Connection::open(tmp.path()).unwrap();
        conn.execute_batch(PRE_PROJECT_GROUPS).unwrap();
    }
    let db = Db::open_at(tmp.path(), tmp.path(), &cfg()).unwrap();

    // The sessions that were there are there, untouched, and in no project.
    let old1 = db.get_group("web:main:old1").unwrap().unwrap();
    let old2 = db.get_group("web:main:old2").unwrap().unwrap();
    assert_eq!(
        (old1.name.as_str(), old1.project_id.as_deref()),
        ("Old chat", None)
    );
    assert_eq!(
        old2.allowed_work_dirs.as_deref(),
        Some(&["/work/old".to_string()][..])
    );
    assert_eq!(old2.project_id, None);
    assert_eq!(
        db.get_group_approved_tools("web:main:old2").unwrap(),
        ["Bash"]
    );

    // The new table works, and an old session can be created into one afterwards.
    let p = db
        .create_project(project_input("Sen", "/work/sen"), 1)
        .unwrap();
    db.upsert_group(&GroupBinding {
        jid: "web:main:new".into(),
        project_id: Some(p.id.clone()),
        ..old1.clone()
    })
    .unwrap();
    assert_eq!(
        db.get_group("web:main:new").unwrap().unwrap().project_id,
        Some(p.id.clone())
    );
    assert_eq!(db.list_groups_in_project(&p.id).unwrap().len(), 1);

    // Opening it again changes nothing (the migration is idempotent).
    drop(db);
    let db = Db::open_at(tmp.path(), tmp.path(), &cfg()).unwrap();
    assert_eq!(db.get_project(&p.id).unwrap().unwrap().root, "/work/sen");
    assert_eq!(
        db.get_group("web:main:new").unwrap().unwrap().project_id,
        Some(p.id)
    );
    assert_eq!(db.list_groups().unwrap().len(), 3);
}

#[test]
fn the_legacy_unique_folder_rebuild_does_not_lose_the_new_column() {
    // The oldest shape (`folder UNIQUE`) is rebuilt into a table with a fixed
    // column list; the project column must be added after that, not before.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    {
        let conn = Connection::open(tmp.path()).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE groups (
              jid TEXT PRIMARY KEY, folder TEXT UNIQUE NOT NULL, name TEXT NOT NULL DEFAULT '',
              channel TEXT NOT NULL DEFAULT 'telegram', is_admin INTEGER NOT NULL DEFAULT 0,
              requires_trigger INTEGER NOT NULL DEFAULT 1, allowed_tools TEXT, allowed_paths TEXT,
              bot_token TEXT, max_messages INTEGER, last_active TEXT, added_at TEXT NOT NULL
            );
            INSERT INTO groups (jid, folder, name, added_at)
              VALUES ('tg:group:9', 'legacy', 'Legacy', '2025-01-01T00:00:00Z');
            "#,
        )
        .unwrap();
    }
    let db = Db::open_at(tmp.path(), tmp.path(), &cfg()).unwrap();
    let legacy = db.get_group("tg:group:9").unwrap().unwrap();
    assert_eq!(
        (legacy.folder.as_str(), legacy.project_id.as_deref()),
        ("legacy", None)
    );

    let p = db
        .create_project(project_input("Sen", "/work/sen"), 1)
        .unwrap();
    db.upsert_group(&GroupBinding {
        jid: "web:legacy:1".into(),
        folder: "legacy".into(), // the UNIQUE constraint is gone: a second session on the profile
        channel: String::new(),
        project_id: Some(p.id.clone()),
        ..legacy
    })
    .unwrap();
    assert_eq!(
        db.get_group("web:legacy:1").unwrap().unwrap().project_id,
        Some(p.id)
    );
}

#[test]
fn a_fresh_database_has_the_project_schema() {
    let db = Db::open_in_memory(&cfg()).unwrap();
    db.with_conn(|c| {
        let tables: Vec<String> = c
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='projects'")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        assert_eq!(tables, ["projects"]);
        let index: i64 = c.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_groups_project'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(index, 1);
        // The column is constrained: a spelling the code does not know cannot be stored.
        let bad = c.execute(
            "INSERT INTO projects (id, name, root, facts_write, created_at, updated_at)
             VALUES ('prj_00000000', 'x', '/x', 'sometimes', 1, 1)",
            [],
        );
        assert!(bad.is_err());
        Ok(())
    })
    .unwrap();
}

#[test]
fn saving_a_session_without_a_project_never_detaches_it() {
    // Several code paths rebuild a binding from scratch and upsert it
    // (scheduler self-heal, boot reconciliation, channel auto-registration).
    // None of them may silently take a session out of its project.
    let db = Db::open_in_memory(&cfg()).unwrap();
    let p = db
        .create_project(project_input("Sen", "/work/sen"), 1)
        .unwrap();
    let mut s = sample_group();
    s.jid = "web:main:1".into();
    s.channel = String::new();
    s.project_id = Some(p.id.clone());
    db.upsert_group(&s).unwrap();

    s.project_id = None;
    s.name = "renamed".into();
    db.upsert_group(&s).unwrap();
    let got = db.get_group("web:main:1").unwrap().unwrap();
    assert_eq!(
        (got.name.as_str(), got.project_id.as_deref()),
        ("renamed", Some(p.id.as_str()))
    );

    // Only deleting the project detaches (see `delete_project`).
    db.delete_project(&p.id).unwrap();
    assert_eq!(
        db.get_group("web:main:1").unwrap().unwrap().project_id,
        None
    );
}

#[test]
fn renaming_a_jid_keeps_the_project() {
    let db = Db::open_in_memory(&cfg()).unwrap();
    let p = db
        .create_project(project_input("Sen", "/work/sen"), 1)
        .unwrap();
    let mut s = sample_group();
    s.project_id = Some(p.id.clone());
    db.upsert_group(&s).unwrap();
    let moved = db.rename_group_jid(&s.jid, "tg:group:2").unwrap().unwrap();
    assert_eq!(moved.project_id, Some(p.id.clone()));
    assert_eq!(
        db.get_group("tg:group:2").unwrap().unwrap().project_id,
        Some(p.id)
    );
}
