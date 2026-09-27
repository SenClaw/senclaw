use anyhow::Result;
use rusqlite::Connection;

use crate::config::Config;
use crate::memory::schema::{apply_memory_schema, build_model_key};

pub(crate) fn apply_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS groups (
          jid                  TEXT PRIMARY KEY,
          -- folder is NOT unique: multiple chat sessions can share the same
          -- agent profile (folder). The old UNIQUE constraint silently broke
          -- every new chat after the first per profile (insert violated
          -- constraint → error swallowed → FE saw a phantom group → user
          -- never got a reply). Migrated away in apply_schema below.
          folder               TEXT NOT NULL,
          name                 TEXT NOT NULL DEFAULT '',
          channel              TEXT NOT NULL DEFAULT 'telegram',
          is_admin             INTEGER NOT NULL DEFAULT 0,
          requires_trigger     INTEGER NOT NULL DEFAULT 1,
          allowed_tools        TEXT,
          approved_tools       TEXT,
          allowed_paths        TEXT,
          allowed_work_dirs    TEXT,
          bot_token            TEXT,
          max_messages         INTEGER,
          llm_config_id        TEXT,
          last_active          TEXT,
          added_at             TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS channel_messages (
          message_id   TEXT NOT NULL,
          chat_jid     TEXT NOT NULL,
          sender_jid   TEXT NOT NULL DEFAULT '',
          sender_name  TEXT NOT NULL DEFAULT '',
          content      TEXT NOT NULL DEFAULT '',
          timestamp    TEXT NOT NULL,
          is_from_me   INTEGER NOT NULL DEFAULT 0,
          is_bot_reply INTEGER NOT NULL DEFAULT 0,
          reply_to_id  TEXT,
          media_type   TEXT,
          attachments  TEXT,
          PRIMARY KEY (message_id, chat_jid)
        );
        CREATE INDEX IF NOT EXISTS idx_msg_timestamp
          ON channel_messages(chat_jid, timestamp);

        CREATE TABLE IF NOT EXISTS channels (
          id                INTEGER PRIMARY KEY AUTOINCREMENT,
          platform_type     TEXT NOT NULL,
          name              TEXT NOT NULL,
          credentials_json  TEXT NOT NULL DEFAULT '{}',
          connection_state  TEXT NOT NULL DEFAULT 'disconnected',
          created_at        TEXT NOT NULL,
          updated_at        TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS agents (
          id                INTEGER PRIMARY KEY AUTOINCREMENT,
          folder            TEXT UNIQUE NOT NULL,
          name              TEXT NOT NULL DEFAULT '',
          requires_trigger  INTEGER NOT NULL DEFAULT 1,
          allowed_tools     TEXT,
          allowed_paths     TEXT,
          allowed_work_dirs TEXT,
          core_prompt       TEXT NOT NULL DEFAULT '',
          model_id          TEXT,
          created_at        TEXT NOT NULL,
          updated_at        TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS bindings (
          id                  INTEGER PRIMARY KEY AUTOINCREMENT,
          jid                 TEXT UNIQUE,
          agent_id            INTEGER NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
          channel_id          INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
          is_admin            INTEGER NOT NULL DEFAULT 0,
          bot_token_override  TEXT,
          max_messages        INTEGER,
          last_active         TEXT,
          created_at          TEXT NOT NULL
        );

        -- Pairing requests: an unknown chat asking to be let in.
        --
        -- A message from a chat with no binding no longer claims a pending
        -- binding on sight (that was first-come-first-served). It lands here
        -- instead, the bot answers with `code`, and a human approves it.
        -- `sender_jid`/`sender_name` are kept so the approver sees *who* they
        -- are letting in, not just a chat id.
        CREATE TABLE IF NOT EXISTS channel_pairings (
          id           INTEGER PRIMARY KEY AUTOINCREMENT,
          channel_id   INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
          chat_jid     TEXT NOT NULL,
          chat_type    TEXT NOT NULL DEFAULT 'user',
          sender_jid   TEXT NOT NULL DEFAULT '',
          sender_name  TEXT NOT NULL DEFAULT '',
          chat_title   TEXT,
          code         TEXT NOT NULL UNIQUE,
          status       TEXT NOT NULL DEFAULT 'pending',
          bot_token    TEXT,
          created_at   TEXT NOT NULL,
          expires_at   TEXT NOT NULL,
          resolved_at  TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_channel_pairings_lookup
          ON channel_pairings(channel_id, chat_jid, status);

        CREATE TABLE IF NOT EXISTS group_messages (
          message_id   TEXT NOT NULL,
          chat_jid     TEXT NOT NULL,
          sender_jid   TEXT NOT NULL DEFAULT '',
          sender_name  TEXT NOT NULL DEFAULT '',
          content      TEXT NOT NULL DEFAULT '',
          timestamp    TEXT NOT NULL,
          is_from_me   INTEGER NOT NULL DEFAULT 0,
          is_bot_reply INTEGER NOT NULL DEFAULT 0,
          reply_to_id  TEXT,
          media_type   TEXT,
          attachments  TEXT,
          PRIMARY KEY (message_id, chat_jid)
        );
        CREATE INDEX IF NOT EXISTS idx_group_msg_ts
          ON group_messages(chat_jid, timestamp);

        -- Tool execution events. Kept in a SEPARATE table from group_messages
        -- so the discriminator (`role: 'tool'`) stays explicit at the storage
        -- layer too. Replayed on subscribe so the chat UI can re-render the
        -- claude-code-style tool cards after a page reload.
        -- Persisted log of every space-event reminder/renotify the
        -- EventNotifier has fired. Today reminders only broadcast as
        -- live WS frames — if the user wasn't connected, they're gone.
        -- The DB row lets reload still surface the notification, and
        -- tracks `delayed_ms` so the UI can flag late reminders (daemon
        -- was down past the trigger time).
        CREATE TABLE IF NOT EXISTS event_notifications (
          id          TEXT PRIMARY KEY,
          event_id    TEXT NOT NULL,
          title       TEXT NOT NULL,
          start_at    INTEGER NOT NULL,
          kind        TEXT NOT NULL,
          fired_at    INTEGER NOT NULL,
          delayed_ms  INTEGER NOT NULL DEFAULT 0,
          read_at     INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_event_notif_fired
          ON event_notifications(fired_at DESC);
        CREATE INDEX IF NOT EXISTS idx_event_notif_event
          ON event_notifications(event_id);

        -- Ephemeral chat events (agent state transitions, permission/question
        -- requests + their resolutions). Replayed on subscribe so the
        -- chat UI can rebuild interactive state after a page reload.
        CREATE TABLE IF NOT EXISTS chat_events (
          id          INTEGER PRIMARY KEY AUTOINCREMENT,
          chat_jid    TEXT NOT NULL,
          event_type  TEXT NOT NULL,
          request_id  TEXT,
          payload     TEXT NOT NULL DEFAULT '{}',
          timestamp   TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_chat_events_chat_ts
          ON chat_events(chat_jid, timestamp);
        CREATE INDEX IF NOT EXISTS idx_chat_events_req
          ON chat_events(request_id);

        -- Per-agent TODO snapshot for the Agent Console. Replayed on
        -- admin reconnect so the list survives daemon restart, not just
        -- in-memory cache rebuilds.
        CREATE TABLE IF NOT EXISTS agent_todos (
          agent_jid   TEXT PRIMARY KEY,
          agent_name  TEXT NOT NULL DEFAULT '',
          todos_json  TEXT NOT NULL DEFAULT '[]',
          updated_at  TEXT NOT NULL
        );

        -- Plans produced by ExitPlanMode. Persisted at request time so
        -- the markdown is queryable as history even if the user never
        -- clicks approve. The approval column tracks the eventual response.
        CREATE TABLE IF NOT EXISTS plans (
          id           TEXT PRIMARY KEY,
          chat_jid     TEXT NOT NULL,
          agent_id     TEXT NOT NULL DEFAULT 'main',
          title        TEXT NOT NULL DEFAULT '',
          file_path    TEXT NOT NULL,
          content_md   TEXT NOT NULL,
          approval     TEXT NOT NULL DEFAULT 'pending',
          created_at   TEXT NOT NULL,
          approved_at  TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_plans_chat_ts
          ON plans(chat_jid, created_at DESC);

        -- Tool auto-accept/deny rules. Stored as a JSON blob so new
        -- matcher/action variants in the Rust enum don't need a schema
        -- migration. The server is the source of truth; the browser
        -- localStorage cache mirrors the canonical list.
        CREATE TABLE IF NOT EXISTS tool_rules (
          id          TEXT PRIMARY KEY,
          rule_json   TEXT NOT NULL,
          updated_at  TEXT NOT NULL
        );

        -- MCP tool aliases (Plugins → Alias). `alias` is the name agents call:
        -- a brand-new name re-identifies (renames) the target tool, while an
        -- alias equal to an existing tool name overrides that tool with the
        -- target. `source` is 'user' for rows created in the web UI (enabled by
        -- default) or 'app:<app_id>' for rows imported from a Space App's
        -- senclaw-manifest.json `mcp.toolAliases` (imported disabled — the user
        -- must opt in from Plugins → Alias before they take effect).
        CREATE TABLE IF NOT EXISTS mcp_tool_aliases (
          alias        TEXT PRIMARY KEY,
          target_tool  TEXT NOT NULL,
          description  TEXT,
          enabled      INTEGER NOT NULL DEFAULT 1,
          source       TEXT NOT NULL DEFAULT 'user',
          created_at   INTEGER NOT NULL,
          updated_at   INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_mcp_tool_aliases_source
          ON mcp_tool_aliases(source);

        CREATE TABLE IF NOT EXISTS tool_executions (
          id            INTEGER PRIMARY KEY AUTOINCREMENT,
          chat_jid      TEXT NOT NULL,
          agent_id      TEXT NOT NULL DEFAULT 'main',
          tool_name     TEXT NOT NULL,
          title         TEXT NOT NULL DEFAULT '',
          summary       TEXT NOT NULL DEFAULT '',
          -- The model's own one-line account of what the step was for
          -- (`Bash`/`Task` take a required `description`). Empty for tools
          -- that take none; the clients then show the tool's verb instead.
          description   TEXT NOT NULL DEFAULT '',
          content_json  TEXT NOT NULL DEFAULT '{}',
          ok            INTEGER NOT NULL DEFAULT 1,
          timestamp     TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_tool_exec_chat_ts
          ON tool_executions(chat_jid, timestamp);

        -- Failure ledger: one row per run of consecutive failures of the SAME
        -- tool in the same chat, plus what happened next. Deliberately NOT the
        -- `tool_executions` table above: that one is the chat's replay buffer
        -- and is FIFO-trimmed with the chat's message cap, so the oldest
        -- failures — the ones that prove a failure repeats — are the first to
        -- go. This table is durable, episode-level, and stores the SHAPE of the
        -- failing call (`args_shape`: key name -> JSON type) but never the
        -- argument values, which are user data.
        --
        -- Written by `crate::failures` off the engine event bus. It only
        -- records; nothing reads it back into a prompt.
        CREATE TABLE IF NOT EXISTS failure_episodes (
          id                INTEGER PRIMARY KEY AUTOINCREMENT,
          chat_jid          TEXT NOT NULL,
          agent_id          TEXT NOT NULL DEFAULT 'main',
          tool_name         TEXT NOT NULL,
          -- JSON object: argument name -> JSON type. Never values.
          args_shape        TEXT NOT NULL DEFAULT '{}',
          first_error       TEXT NOT NULL DEFAULT '',
          last_error        TEXT NOT NULL DEFAULT '',
          -- Normalised form of `first_error` (digits, paths and quoted spans
          -- folded away) so "the same failure twice" is a GROUP BY, not a
          -- judgement call.
          error_class       TEXT NOT NULL DEFAULT '',
          -- Consecutive failures of this tool inside the episode. The engine
          -- nudges at `stall_tool_turns()` and hard-stops at twice that, so
          -- the streak is also how far the loop guard had got.
          streak            INTEGER NOT NULL DEFAULT 1,
          -- open | resolved | gave_up
          status            TEXT NOT NULL DEFAULT 'open',
          -- model | user — who supplied the fix, set when status becomes
          -- 'resolved'. NULL while open or given up.
          fix_source        TEXT,
          -- The engine's error-loop guard fired on this tool (hard stop).
          loop_stopped      INTEGER NOT NULL DEFAULT 0,
          -- A user message arrived after the first failure.
          user_input_after  INTEGER NOT NULL DEFAULT 0,
          -- Agent turns that ended while this episode was still failing.
          turns_ended       INTEGER NOT NULL DEFAULT 0,
          first_error_at    TEXT NOT NULL,
          last_error_at     TEXT NOT NULL,
          closed_at         TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_failure_open
          ON failure_episodes(chat_jid, tool_name, status);
        CREATE INDEX IF NOT EXISTS idx_failure_class
          ON failure_episodes(tool_name, error_class);
        CREATE INDEX IF NOT EXISTS idx_failure_seen
          ON failure_episodes(first_error_at);

        -- Tool-call gate (src/decision/gate): one row per command the gate
        -- judged, what it concluded, whether that skipped the prompt, and —
        -- when a prompt was shown — what the person answered. The answer is
        -- what shadow mode measures the gate against before anyone trusts it.
        -- Durable and row-capped, like the failure ledger.
        CREATE TABLE IF NOT EXISTS tool_gate_log (
          id          INTEGER PRIMARY KEY AUTOINCREMENT,
          at          INTEGER NOT NULL,              -- unix millis
          chat_jid    TEXT NOT NULL DEFAULT '',
          tool        TEXT NOT NULL,
          command     TEXT NOT NULL,
          mode        TEXT NOT NULL,                 -- shadow | on
          outcome     TEXT NOT NULL,                 -- allow | ask
          applied     INTEGER NOT NULL DEFAULT 0,    -- 1 = the prompt was skipped
          stage       TEXT NOT NULL,                 -- risky | engine | error
          reason      TEXT NOT NULL DEFAULT '',
          p           REAL,                          -- P(reversible), when answered
          choice      TEXT,                          -- the kind the model chose
          questions   TEXT NOT NULL DEFAULT '',      -- laya | cookbook
          model       TEXT,
          engine      TEXT,
          latency_ms  REAL,
          human       TEXT,                          -- agree | allow | refuse | other
          human_at    INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_tool_gate_log_at ON tool_gate_log(at);

        -- Pre-turn skill router (src/decision/skill_route): per turn, what the
        -- legacy keyword matcher did and what the router decided, and the
        -- decision engine's pick. Shadow mode is judged from these rows. The
        -- prompt is cut to 300 characters: enough to judge the pick by, and
        -- this table outlives the chat.
        CREATE TABLE IF NOT EXISTS skill_route_log (
          id            INTEGER PRIMARY KEY AUTOINCREMENT,
          at            INTEGER NOT NULL,             -- unix millis
          chat_jid      TEXT NOT NULL DEFAULT '',
          prompt        TEXT NOT NULL,
          mode          TEXT NOT NULL,                -- shadow | on
          pre_trigger   INTEGER NOT NULL DEFAULT 0,
          legacy_name   TEXT,
          legacy_force  INTEGER NOT NULL DEFAULT 0,
          route_name    TEXT,
          route_force   INTEGER NOT NULL DEFAULT 0,
          reason        TEXT NOT NULL DEFAULT '',
          engine_pick   TEXT,
          engine_p      REAL,
          model         TEXT,
          latency_ms    REAL,
          fallback      TEXT,
          candidates    TEXT NOT NULL DEFAULT '[]'    -- JSON: [{name, score, phraseHit}]
        );
        CREATE INDEX IF NOT EXISTS idx_skill_route_log_at ON skill_route_log(at);

        -- Shadow-git checkpoints: one row per commit taken after a tool wrote
        -- into the chat's working directory (src/checkpoints). The commit
        -- itself lives in ~/.senclaw/checkpoints/<chat>/, never in the
        -- project's own .git.
        CREATE TABLE IF NOT EXISTS chat_checkpoints (
          id            INTEGER PRIMARY KEY AUTOINCREMENT,
          chat_jid      TEXT NOT NULL,
          sha           TEXT NOT NULL,
          parent_sha    TEXT,
          tool_name     TEXT NOT NULL DEFAULT '',
          summary       TEXT NOT NULL DEFAULT '',
          workspace     TEXT NOT NULL,
          files_changed INTEGER NOT NULL DEFAULT 0,
          created_at    TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_chat_checkpoints_jid
          ON chat_checkpoints(chat_jid, id);

        -- One-way chat widgets (chart/image/clock/weather/video/audio + Space
        -- App widgets, kind "app") pushed by `emit_widget`. Persisted so a
        -- page reload replays them in `history:load`. `widget_json` is the
        -- full WidgetSpec {kind,title,data}. See WIDGET_CONTRACT.md.
        -- Mirrors tool_executions (FIFO-trimmed per chat on insert).
        CREATE TABLE IF NOT EXISTS chat_widgets (
          id            TEXT PRIMARY KEY,
          chat_jid      TEXT NOT NULL,
          widget_json   TEXT NOT NULL DEFAULT '{}',
          created_at    TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_chat_widgets_chat_ts
          ON chat_widgets(chat_jid, created_at);

        CREATE TABLE IF NOT EXISTS dispatch_activity (
          id            INTEGER PRIMARY KEY AUTOINCREMENT,
          task_id       TEXT NOT NULL,
          parent_id     TEXT NOT NULL DEFAULT '',
          entry_type    TEXT NOT NULL DEFAULT 'tool',
          tool_name     TEXT,
          title         TEXT,
          summary       TEXT,
          content_json  TEXT,
          ok            INTEGER,
          text          TEXT,
          ts            TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_dispatch_activity_task
          ON dispatch_activity(task_id, ts);
        CREATE INDEX IF NOT EXISTS idx_dispatch_activity_parent
          ON dispatch_activity(parent_id, ts);

        CREATE TABLE IF NOT EXISTS scheduled_tasks (
          id             TEXT PRIMARY KEY,
          group_folder   TEXT NOT NULL,
          chat_jid       TEXT NOT NULL,
          prompt         TEXT NOT NULL,
          schedule_type  TEXT NOT NULL,
          schedule_value TEXT NOT NULL,
          context_mode   TEXT NOT NULL DEFAULT 'isolated',
          agent_mode     TEXT NOT NULL DEFAULT 'agent',
          script_path    TEXT,
          next_run       TEXT,
          last_run       TEXT,
          last_result    TEXT,
          status         TEXT NOT NULL DEFAULT 'active',
          created_at     TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_task_next_run
          ON scheduled_tasks(next_run, status);

        CREATE TABLE IF NOT EXISTS task_run_logs (
          id          INTEGER PRIMARY KEY AUTOINCREMENT,
          task_id     TEXT NOT NULL,
          run_at      TEXT NOT NULL,
          duration_ms INTEGER,
          status      TEXT NOT NULL,
          result      TEXT,
          error       TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_run_task_id
          ON task_run_logs(task_id, run_at);

        -- Background tasks: autonomous work SenClaw runs by itself. Kept apart
        -- from scheduled_tasks above, which is the *user's* schedule and runs
        -- in a chat session. See docs/background-tasks-design.md.
        CREATE TABLE IF NOT EXISTS background_tasks (
          id                   TEXT PRIMARY KEY,
          owner_kind           TEXT NOT NULL,
          owner_id             TEXT NOT NULL,
          owner_key            TEXT NOT NULL,
          title                TEXT NOT NULL,
          description          TEXT,
          job_kind             TEXT NOT NULL DEFAULT 'prompt',
          native_job           TEXT,
          prompt_kind          TEXT NOT NULL DEFAULT 'static',
          prompt               TEXT,
          context_url          TEXT,
          persona              TEXT,
          agent_folder         TEXT,
          workspace_dir        TEXT,
          use_tools            TEXT,
          mcp                  TEXT,
          model_id             TEXT,
          max_turns            INTEGER,
          timeout_secs         INTEGER,
          continuity           TEXT NOT NULL DEFAULT 'fresh',
          memory_folder        TEXT,
          trigger_type         TEXT NOT NULL,
          trigger_value        TEXT,
          next_run             TEXT,
          last_run             TEXT,
          overlap_policy       TEXT NOT NULL DEFAULT 'skip',
          catch_up             INTEGER NOT NULL DEFAULT 0,
          max_failures         INTEGER NOT NULL DEFAULT 5,
          consecutive_failures INTEGER NOT NULL DEFAULT 0,
          visibility           TEXT NOT NULL DEFAULT 'normal',
          notify               INTEGER NOT NULL DEFAULT 0,
          status               TEXT NOT NULL DEFAULT 'active',
          created_at           TEXT NOT NULL,
          updated_at           TEXT NOT NULL,
          UNIQUE(owner_id, owner_key)
        );
        CREATE INDEX IF NOT EXISTS idx_bg_due
          ON background_tasks(next_run, status);
        CREATE INDEX IF NOT EXISTS idx_bg_owner
          ON background_tasks(owner_kind, owner_id);

        CREATE TABLE IF NOT EXISTS background_runs (
          id           TEXT PRIMARY KEY,
          task_id      TEXT NOT NULL,
          session_id   TEXT NOT NULL,
          trigger_kind TEXT NOT NULL,
          status       TEXT NOT NULL,
          started_at   TEXT NOT NULL,
          finished_at  TEXT,
          duration_ms  INTEGER,
          turn_count   INTEGER,
          tokens_in    INTEGER,
          tokens_out   INTEGER,
          prompt       TEXT,
          result       TEXT,
          error        TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_bg_runs_task
          ON background_runs(task_id, started_at DESC);
        CREATE INDEX IF NOT EXISTS idx_bg_runs_status
          ON background_runs(status, started_at DESC);

        CREATE TABLE IF NOT EXISTS background_activity (
          id     INTEGER PRIMARY KEY AUTOINCREMENT,
          run_id TEXT NOT NULL,
          ts     TEXT NOT NULL,
          kind   TEXT NOT NULL,
          detail TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_bg_activity_run
          ON background_activity(run_id, id);

        CREATE TABLE IF NOT EXISTS router_state (
          key   TEXT PRIMARY KEY,
          value TEXT NOT NULL
        );

        -- Cowork teams (multi-agent dispatch): a named team with one
        -- "manager" profile (agent folder) plus a JSON list of "member"
        -- profile folders the manager can delegate to via the dispatch
        -- MCP tools. Maps 1:1 to a chat group (jid = "cowork:<id>")
        -- created on first open.
        CREATE TABLE IF NOT EXISTS cowork_teams (
          id              TEXT PRIMARY KEY,
          name            TEXT NOT NULL,
          manager_folder  TEXT NOT NULL,
          members_json    TEXT NOT NULL DEFAULT '[]',
          workspace_dir   TEXT,
          created_at      TEXT NOT NULL
        );

        -- Cowork tasks (manager-tracked work items): one row per task the
        -- team is working on. Status moves through a small kanban — backlog
        -- / todo / in_progress / review / done / blocked. `assignee` and
        -- `reviewer` reference a member folder. `depends_on` is a JSON
        -- array of task ids (free-form dependency graph).
        CREATE TABLE IF NOT EXISTS cowork_team_tasks (
          id               TEXT PRIMARY KEY,
          team_id          TEXT NOT NULL,
          title            TEXT NOT NULL,
          description      TEXT,
          status           TEXT NOT NULL DEFAULT 'todo',
          assignee         TEXT,
          reviewer         TEXT,
          priority         TEXT NOT NULL DEFAULT 'medium',
          depends_on       TEXT NOT NULL DEFAULT '[]',
          result_output    TEXT,
          created_at       TEXT NOT NULL,
          updated_at       TEXT NOT NULL,
          due_at           TEXT,
          completed_at     TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_cowork_team_tasks_team
          ON cowork_team_tasks(team_id);

        -- Custom cowork templates (user-authored squad blueprints). Built-in
        -- templates live in code (`BUILTIN_TEMPLATES`); these are the editable,
        -- user-managed ones. `members_json` is an array of TeamMember specs and
        -- `settings_json` holds CoworkTeamSettings (manager preamble override,
        -- manager tool whitelist, auto-task toggle).
        CREATE TABLE IF NOT EXISTS cowork_templates (
          id              TEXT PRIMARY KEY,
          name            TEXT NOT NULL,
          description     TEXT NOT NULL DEFAULT '',
          icon            TEXT NOT NULL DEFAULT '🧩',
          manager_folder  TEXT NOT NULL,
          manager_role    TEXT NOT NULL DEFAULT 'lead',
          members_json    TEXT NOT NULL DEFAULT '[]',
          settings_json   TEXT NOT NULL DEFAULT '{}',
          created_at      TEXT NOT NULL,
          updated_at      TEXT NOT NULL
        );

        -- Code executor "artifacts": saved, reusable code snippets (JS/TS/Bash)
        -- published from the Code REPL.
        CREATE TABLE IF NOT EXISTS code_artifacts (
          id           TEXT PRIMARY KEY,
          name         TEXT NOT NULL,
          language     TEXT NOT NULL,
          code         TEXT NOT NULL,
          description  TEXT NOT NULL DEFAULT '',
          tags_json    TEXT NOT NULL DEFAULT '[]',
          created_at   TEXT NOT NULL,
          updated_at   TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_code_artifacts_created
          ON code_artifacts(created_at DESC);

        "#,
    )?;

    // Run schema migrations
    run_migrations(conn)?;
    Ok(())
}

fn run_migrations(conn: &Connection) -> Result<()> {
    // Per-model edit format declared by a Space App LLM provider
    // (`llm.editFormat`); absent = exact.
    // The table itself is created later by `apply_space_tables` (with the
    // column); this only upgrades a database from before the column existed.
    let provider_cols = column_names(conn, "space_app_llm_providers")?;
    if !provider_cols.is_empty() && !provider_cols.iter().any(|c| c == "edit_format") {
        conn.execute("ALTER TABLE space_app_llm_providers ADD COLUMN edit_format TEXT", [])?;
    }

    // Step descriptions on tool rows. Rows written before this existed keep
    // an empty string, which the clients already render as the tool's verb.
    let tool_exec_cols = column_names(conn, "tool_executions")?;
    if !tool_exec_cols.is_empty() && !tool_exec_cols.iter().any(|c| c == "description") {
        conn.execute(
            "ALTER TABLE tool_executions ADD COLUMN description TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }

    let group_cols = column_names(conn, "groups")?;
    if !group_cols.iter().any(|c| c == "allowed_work_dirs") {
        conn.execute("ALTER TABLE groups ADD COLUMN allowed_work_dirs TEXT", [])?;
    }
    if !group_cols.iter().any(|c| c == "group_type") {
        conn.execute(
            "ALTER TABLE groups ADD COLUMN group_type TEXT NOT NULL DEFAULT 'chat'",
            [],
        )?;
    }
    if !group_cols.iter().any(|c| c == "llm_config_id") {
        conn.execute("ALTER TABLE groups ADD COLUMN llm_config_id TEXT", [])?;
    }
    if !group_cols.iter().any(|c| c == "approved_tools") {
        conn.execute("ALTER TABLE groups ADD COLUMN approved_tools TEXT", [])?;
        // One-time repair: before this column existed, the permission prompt's
        // "always allow" handler appended tool names into `allowed_tools`,
        // silently turning that column into a use_tools whitelist that stripped
        // every other tool from the group's next session. Schedule groups are
        // always created with no whitelist, so any value there is that
        // pollution — move it to the new column.
        conn.execute(
            "UPDATE groups SET approved_tools = allowed_tools, allowed_tools = NULL \
             WHERE jid LIKE 'schedule:%' AND allowed_tools IS NOT NULL",
            [],
        )?;
    }

    // Drop the legacy UNIQUE constraint on `groups.folder`.
    // Old behaviour: only one chat per agent profile (folder), so a new chat
    // for the same profile silently failed on insert and the web UI showed
    // a "Group not found" reply.
    // SQLite can't ALTER a column to drop a constraint — we rebuild the
    // table only when the legacy UNIQUE index is still present.
    // Detect the legacy UNIQUE column by inspecting the table's CREATE
    // statement directly. (Auto-generated UNIQUE indexes are hidden from
    // sqlite_master's `sql` column, so searching for the index doesn't work.)
    let folder_unique: bool = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='groups'",
            [],
            |r| r.get::<_, String>(0),
        )
        .map(|sql| sql.to_uppercase().contains("FOLDER") && sql.to_uppercase().contains("UNIQUE"))
        .unwrap_or(false);
    if folder_unique {
        conn.execute_batch(
            r#"
            BEGIN;
            CREATE TABLE groups_new (
              jid                  TEXT PRIMARY KEY,
              folder               TEXT NOT NULL,
              name                 TEXT NOT NULL DEFAULT '',
              channel              TEXT NOT NULL DEFAULT 'telegram',
              group_type           TEXT NOT NULL DEFAULT 'chat',
              is_admin             INTEGER NOT NULL DEFAULT 0,
              requires_trigger     INTEGER NOT NULL DEFAULT 1,
              allowed_tools        TEXT,
              approved_tools       TEXT,
              allowed_paths        TEXT,
              allowed_work_dirs    TEXT,
              bot_token            TEXT,
              max_messages         INTEGER,
              llm_config_id        TEXT,
              last_active          TEXT,
              added_at             TEXT NOT NULL
            );
            INSERT INTO groups_new (jid, folder, name, channel, group_type, is_admin,
              requires_trigger, allowed_tools, approved_tools, allowed_paths,
              allowed_work_dirs, bot_token, max_messages, llm_config_id,
              last_active, added_at)
            SELECT jid, folder, name, channel,
              COALESCE(group_type, 'chat'),
              is_admin, requires_trigger, allowed_tools, approved_tools,
              allowed_paths, allowed_work_dirs, bot_token, max_messages,
              llm_config_id, last_active, added_at
            FROM groups;
            DROP TABLE groups;
            ALTER TABLE groups_new RENAME TO groups;
            COMMIT;
            "#,
        )?;
    }

    let task_cols = column_names(conn, "scheduled_tasks")?;
    if !task_cols.iter().any(|c| c == "script_path") {
        conn.execute(
            "ALTER TABLE scheduled_tasks ADD COLUMN script_path TEXT",
            [],
        )?;
    }
    if !task_cols.iter().any(|c| c == "agent_mode") {
        conn.execute(
            "ALTER TABLE scheduled_tasks ADD COLUMN agent_mode TEXT NOT NULL DEFAULT 'agent'",
            [],
        )?;
    }
    if !task_cols.iter().any(|c| c == "watch_json") {
        conn.execute("ALTER TABLE scheduled_tasks ADD COLUMN watch_json TEXT", [])?;
    }
    let bg_cols = column_names(conn, "background_tasks")?;
    if !bg_cols.is_empty() && !bg_cols.iter().any(|c| c == "notify") {
        conn.execute(
            "ALTER TABLE background_tasks ADD COLUMN notify INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }

    let agent_cols = column_names(conn, "agents")?;
    if !agent_cols.iter().any(|c| c == "core_prompt") {
        conn.execute(
            "ALTER TABLE agents ADD COLUMN core_prompt TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    if !agent_cols.iter().any(|c| c == "model_id") {
        conn.execute("ALTER TABLE agents ADD COLUMN model_id TEXT", [])?;
    }

    // space_events — add notification tracking columns
    let event_cols = column_names(conn, "space_events").unwrap_or_default();
    for (col, def) in &[
        ("status", "TEXT NOT NULL DEFAULT 'upcoming'"),
        ("reminder_sent_at", "INTEGER"),
        ("renotify_min", "INTEGER"),
        ("renotify_sent_at", "INTEGER"),
        // start_sent_at: tracks the "event is starting now" notification so
        // EVERY event pings at its start time, even without a reminder_min.
        ("start_sent_at", "INTEGER"),
        // link: where opening this event should take the user — an INTERNAL
        // Space-App route such as `/space/app/study?session=<id>`. Without it
        // an event can name a lesson but not open it. Deliberately not a free
        // URL field: see `sanitize_event_link`.
        ("link", "TEXT"),
        // app_id: which Space App owns the link, so the UI can label the button
        // and a reinstalled app can find its own events.
        ("app_id", "TEXT"),
    ] {
        if !event_cols.iter().any(|c| c == col) {
            let _ = conn.execute(
                &format!("ALTER TABLE space_events ADD COLUMN {col} {def}"),
                [],
            );
        }
    }

    Ok(())
}

pub(crate) fn column_names(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names)
}

/// Apply Space (personal productivity) tables.
pub(crate) fn apply_space_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS space_notes (
            id          TEXT PRIMARY KEY,
            title       TEXT NOT NULL DEFAULT '',
            body        TEXT NOT NULL DEFAULT '',
            body_html   TEXT,
            tags        TEXT NOT NULL DEFAULT '[]',
            folder_id   TEXT,
            pinned      INTEGER NOT NULL DEFAULT 0,
            created_at  INTEGER NOT NULL,
            updated_at  INTEGER NOT NULL,
            deleted_at  INTEGER
        );

        CREATE TABLE IF NOT EXISTS space_note_folders (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            parent_id   TEXT,
            created_at  INTEGER NOT NULL
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS space_notes_fts USING fts5(
            id UNINDEXED, title, body, tags,
            content=space_notes, content_rowid=rowid
        );

        CREATE TRIGGER IF NOT EXISTS space_notes_ai AFTER INSERT ON space_notes BEGIN
            INSERT INTO space_notes_fts(rowid, id, title, body, tags)
            VALUES (new.rowid, new.id, new.title, new.body, new.tags);
        END;
        CREATE TRIGGER IF NOT EXISTS space_notes_ad AFTER DELETE ON space_notes BEGIN
            INSERT INTO space_notes_fts(space_notes_fts, rowid, id, title, body, tags)
            VALUES ('delete', old.rowid, old.id, old.title, old.body, old.tags);
        END;
        CREATE TRIGGER IF NOT EXISTS space_notes_au AFTER UPDATE ON space_notes BEGIN
            INSERT INTO space_notes_fts(space_notes_fts, rowid, id, title, body, tags)
            VALUES ('delete', old.rowid, old.id, old.title, old.body, old.tags);
            INSERT INTO space_notes_fts(rowid, id, title, body, tags)
            VALUES (new.rowid, new.id, new.title, new.body, new.tags);
        END;

        CREATE TABLE IF NOT EXISTS space_events (
            id              TEXT PRIMARY KEY,
            title           TEXT NOT NULL,
            description     TEXT,
            start_at        INTEGER NOT NULL,
            end_at          INTEGER NOT NULL,
            all_day         INTEGER DEFAULT 0,
            location        TEXT,
            color           TEXT,
            recurrence      TEXT,
            reminder_min    INTEGER,
            task_id         TEXT,
            link            TEXT,
            app_id          TEXT,
            source          TEXT DEFAULT 'manual',
            status          TEXT NOT NULL DEFAULT 'upcoming',
            reminder_sent_at INTEGER,
            renotify_min    INTEGER,
            renotify_sent_at INTEGER,
            start_sent_at   INTEGER,
            created_at      INTEGER NOT NULL,
            updated_at      INTEGER NOT NULL,
            deleted_at      INTEGER
        );

        CREATE INDEX IF NOT EXISTS idx_space_events_start ON space_events(start_at);

        CREATE TABLE IF NOT EXISTS space_apps (
            id          TEXT PRIMARY KEY,
            manifest    TEXT NOT NULL,
            enabled     INTEGER DEFAULT 1,
            installed_at INTEGER NOT NULL,
            last_seen_at INTEGER
        );

        -- One access token per installed app: the identity it presents on
        -- /api/space/apps/<id>/… so the daemon can tell app A's calls from
        -- app B's. Minted on first launch, handed to the process in
        -- SENCLAW_TOKEN_ACCESS_APP, deleted when the app is uninstalled so a
        -- later app reusing the id cannot inherit it.
        -- See src/apps/token.rs and docs/space-app-api-token.md.
        CREATE TABLE IF NOT EXISTS space_app_tokens (
            app_id      TEXT PRIMARY KEY,
            token       TEXT NOT NULL UNIQUE,
            created_at  INTEGER NOT NULL,
            rotated_at  INTEGER
        );

        -- Space Apps that serve models. One row per app, holding the resolved
        -- endpoint and the model list read from the app's /v1/models (or from
        -- the cache it wrote at startup, while it is stopped). Rebuilt into a
        -- process-global registry at boot, which is what `load_llm_configs`
        -- appends to the user's own configs.
        -- See src/apps/llm_provider.rs.
        CREATE TABLE IF NOT EXISTS space_app_llm_providers (
            app_id      TEXT PRIMARY KEY,
            label       TEXT NOT NULL,
            adapt       TEXT NOT NULL,
            base_url    TEXT NOT NULL,
            models      TEXT NOT NULL,
            updated_at  INTEGER NOT NULL,
            edit_format TEXT
        );

        CREATE TABLE IF NOT EXISTS space_app_config (
            app_id      TEXT NOT NULL,
            key         TEXT NOT NULL,
            value       TEXT NOT NULL,
            updated_at  INTEGER NOT NULL,
            PRIMARY KEY (app_id, key)
        );
        "#,
    )?;

    // Ensure FTS table has the tags column (older databases lack it).
    // Check column count: id, title, body = 3 old; id, title, body, tags = 4 new.
    let needs_rebuild: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('space_notes_fts') WHERE name='tags'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        == 0;

    if needs_rebuild {
        conn.execute_batch(
            r#"
            DROP TABLE IF EXISTS space_notes_fts;
            CREATE VIRTUAL TABLE space_notes_fts USING fts5(
                id UNINDEXED, title, body, tags,
                content=space_notes, content_rowid=rowid
            );
            CREATE TRIGGER IF NOT EXISTS space_notes_ai AFTER INSERT ON space_notes BEGIN
                INSERT INTO space_notes_fts(rowid, id, title, body, tags)
                VALUES (new.rowid, new.id, new.title, new.body, new.tags);
            END;
            CREATE TRIGGER IF NOT EXISTS space_notes_ad AFTER DELETE ON space_notes BEGIN
                INSERT INTO space_notes_fts(space_notes_fts, rowid, id, title, body, tags)
                VALUES ('delete', old.rowid, old.id, old.title, old.body, old.tags);
            END;
            CREATE TRIGGER IF NOT EXISTS space_notes_au AFTER UPDATE ON space_notes BEGIN
                INSERT INTO space_notes_fts(space_notes_fts, rowid, id, title, body, tags)
                VALUES ('delete', old.rowid, old.id, old.title, old.body, old.tags);
                INSERT INTO space_notes_fts(rowid, id, title, body, tags)
                VALUES (new.rowid, new.id, new.title, new.body, new.tags);
            END;
            INSERT INTO space_notes_fts(space_notes_fts) VALUES('rebuild');
            "#,
        )?;
    } else {
        // Triggers use IF NOT EXISTS, so just rebuild to fill any gaps.
        conn.execute_batch("INSERT INTO space_notes_fts(space_notes_fts) VALUES('rebuild');")?;
    }

    Ok(())
}

/// Marketplace tables — installed_skills, installed_plugins, plugin_runtime.
pub(crate) fn apply_marketplace_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS installed_skills (
            slug            TEXT PRIMARY KEY,
            display_name    TEXT,
            summary         TEXT,
            version         TEXT NOT NULL DEFAULT '',
            registry        TEXT NOT NULL DEFAULT 'https://lightmake.site',
            source          TEXT NOT NULL DEFAULT 'clawhub',
            enabled         INTEGER NOT NULL DEFAULT 1,
            installed_at    INTEGER NOT NULL DEFAULT 0,
            updated_at      INTEGER NOT NULL DEFAULT 0,
            manifest_json   TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_installed_skills_enabled
            ON installed_skills(enabled, source);

        CREATE TABLE IF NOT EXISTS installed_plugins (
            slug            TEXT PRIMARY KEY,
            display_name    TEXT,
            summary         TEXT,
            version         TEXT NOT NULL DEFAULT '',
            plugin_type     TEXT NOT NULL DEFAULT 'mcp_server',
            registry        TEXT NOT NULL DEFAULT 'https://lightmake.site',
            enabled         INTEGER NOT NULL DEFAULT 1,
            installed_at    INTEGER NOT NULL DEFAULT 0,
            updated_at      INTEGER NOT NULL DEFAULT 0,
            config_json     TEXT NOT NULL DEFAULT '{}',
            manifest_json   TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_installed_plugins_type
            ON installed_plugins(plugin_type, enabled);

        CREATE TABLE IF NOT EXISTS plugin_runtime (
            slug        TEXT PRIMARY KEY REFERENCES installed_plugins(slug) ON DELETE CASCADE,
            status      TEXT NOT NULL DEFAULT 'stopped',
            pid         INTEGER,
            port        INTEGER,
            started_at  INTEGER,
            error_msg   TEXT,
            last_ping   INTEGER
        );

        -- Token accounting: one row per LLM call, batched in by UsageRecorder
        -- (src/usage). Metadata only — never prompt/response content, never
        -- api keys. Raw rows retained 90 days; llm_usage_daily keeps history.
        CREATE TABLE IF NOT EXISTS llm_usage_log (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            ts          INTEGER NOT NULL,
            source      TEXT NOT NULL,
            jid         TEXT NOT NULL DEFAULT '',
            agent_id    TEXT NOT NULL DEFAULT '',
            session_id  TEXT NOT NULL DEFAULT '',
            app_id      TEXT NOT NULL DEFAULT '',
            profile     TEXT NOT NULL DEFAULT '',
            provider    TEXT NOT NULL DEFAULT '',
            model       TEXT NOT NULL DEFAULT '',
            input_tokens          INTEGER NOT NULL DEFAULT 0,
            output_tokens         INTEGER NOT NULL DEFAULT 0,
            cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
            latency_ms  INTEGER NOT NULL DEFAULT 0,
            ok          INTEGER NOT NULL DEFAULT 1,
            estimated   INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_ulog_ts    ON llm_usage_log(ts);
        CREATE INDEX IF NOT EXISTS idx_ulog_jid   ON llm_usage_log(jid, ts);
        CREATE INDEX IF NOT EXISTS idx_ulog_model ON llm_usage_log(model, ts);
        CREATE INDEX IF NOT EXISTS idx_ulog_src   ON llm_usage_log(source, ts);
        CREATE INDEX IF NOT EXISTS idx_ulog_app   ON llm_usage_log(app_id, ts);

        -- Daily rollup, upserted hourly by the usage aggregator. Kept forever.
        -- est_cost_usd is NULL when the model has no pricing row (shown as
        -- "n/a", never a fake $0).
        CREATE TABLE IF NOT EXISTS llm_usage_daily (
            date    TEXT NOT NULL,
            source  TEXT NOT NULL,
            jid     TEXT NOT NULL,
            app_id  TEXT NOT NULL,
            model   TEXT NOT NULL,
            calls   INTEGER NOT NULL DEFAULT 0,
            input_tokens          INTEGER NOT NULL DEFAULT 0,
            output_tokens         INTEGER NOT NULL DEFAULT 0,
            cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
            est_cost_usd REAL,
            PRIMARY KEY (date, source, jid, app_id, model)
        );

        -- USD per 1M tokens. NULL cache prices fall back to input_per_1m.
        CREATE TABLE IF NOT EXISTS model_pricing (
            model              TEXT PRIMARY KEY,
            input_per_1m       REAL NOT NULL,
            output_per_1m      REAL NOT NULL,
            cache_read_per_1m  REAL,
            cache_write_per_1m REAL
        );
        "#,
    )?;

    // Migration: Add attachments column if it doesn't exist
    conn.execute(
        "ALTER TABLE channel_messages ADD COLUMN attachments TEXT",
        [],
    )
    .ok(); // Ignore error if column already exists
    conn.execute("ALTER TABLE group_messages ADD COLUMN attachments TEXT", [])
        .ok(); // Ignore error if column already exists

    // Cowork team behaviour settings (manager preamble override, manager tool
    // whitelist, auto-task toggle). Added after the table shipped, so guard it.
    if let Ok(cols) = column_names(conn, "cowork_teams") {
        if !cols.iter().any(|c| c == "settings_json") {
            conn.execute(
                "ALTER TABLE cowork_teams ADD COLUMN settings_json TEXT NOT NULL DEFAULT '{}'",
                [],
            )
            .ok();
        }
    }

    Ok(())
}

/// Apply memory schema if embedding is enabled.
pub(crate) fn apply_memory_tables(conn: &mut Connection, config: &Config) -> Result<()> {
    let provider = config.memory.embedding_provider;
    let enable_vec = provider != crate::config::EmbeddingProvider::None;
    let dimensions = Config::resolve_dimensions(provider, config.memory.embedding_dimensions);
    let model_name = match provider {
        crate::config::EmbeddingProvider::Openrouter => config.memory.openrouter_model.clone(),
        crate::config::EmbeddingProvider::Ollama => config.memory.ollama_model.clone(),
        crate::config::EmbeddingProvider::Local => {
            let m = config.memory.local_model.clone();
            if m.is_empty() {
                "default".to_owned()
            } else {
                m
            }
        }
        crate::config::EmbeddingProvider::Openai => {
            let m = config.memory.openai_model.clone();
            if m.is_empty() {
                "text-embedding-3-small".to_owned()
            } else {
                m
            }
        }
        crate::config::EmbeddingProvider::None => String::new(),
    };
    let model_key = if enable_vec {
        build_model_key(provider.as_str(), &model_name, dimensions)
    } else {
        String::new()
    };
    if let Err(e) = apply_memory_schema(conn, enable_vec, dimensions, &model_key) {
        tracing::error!(
            error = %e,
            "[DB] applyMemorySchema failed, memory search will be unavailable"
        );
    }
    Ok(())
}
