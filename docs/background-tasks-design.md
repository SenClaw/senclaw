# Background Tasks: autonomous background sessions for SenClaw

**Status:** 📐 Design proposal (2026-07-17) · **Complements:** `docs/mcp-dispatcher-design.md` · **Does not replace:** the user-facing `scheduled_tasks` schedule

---

## 0. Goal

SenClaw already runs agent work on a timer, but only *for a user, in a chat*. There is no way to say:

> "Every morning at 9, without any human in the loop, look at CRM customers whose follow-up is overdue, draft the outreach, and log what you did."

or

> "Every 6 hours, review the knowledge base for contradictions and clean them up."

This subsystem adds that. Its properties, as requested:

1. **Not a chat session.** A background run produces a *background session* — an agent run with a transcript, but no `GroupBinding`, no `channel_messages`, no chat window, no reply to anybody.
2. **Its own scheduler**, independent of `TaskScheduler`. Different owner (SenClaw itself and its Apps, not the user), different failure semantics, different UI.
3. **Prompts SenClaw generates and runs itself.** Static, template-with-live-context, or LLM-generated.
4. **Once, at a fixed time, or repeating** (cron / interval / one-shot / on-install).
5. **Fully visible to the user**: list running tasks, cancel, create, read run history, see errors — plus aggregate statistics to judge whether the automation is worth keeping.
6. **Registrable by Apps**: an App declares background prompts in its manifest; they are installed with the App and removed with it.

Concretely, the first consumers are: periodic knowledge/memory upkeep (core), CRM customer follow-up (`apps/crm`), AI-Office standing duties (`apps/ai-office`), and chatbot re-engagement sweeps.

---

## 1. Why not extend `scheduled_tasks`

`scheduled_tasks` is the **user's** schedule. Its every field assumes a human on the other end: `chat_jid`, `group_folder`, `context_mode: group` (the only mode that really works), and a reply delivered into a chat. `SpaceServer::recurring_create` (`src/mcp/space_server.rs:1381`) mints a `GroupBinding` per schedule precisely so the run has a chat to talk into, and conversation history accumulates across runs.

Background tasks invert all of that: no chat, no binding, no reply, ownership by an App or by core rather than by a user, and failure handling that must auto-quarantine a task nobody is watching. Bolting an `is_background` flag onto `scheduled_tasks` would mean every column is meaningless for half the rows, and the two schedulers' policies (sequential-and-blocking vs concurrent-with-overlap-policy) would fight.

**Decision: a separate table, a separate scheduler, a separate UI section.** They coexist; the boundary is *who owns the task*.

| | `scheduled_tasks` (existing) | `background_tasks` (new) |
|---|---|---|
| Owner | a user | core / an App / the agent itself |
| Runs in | a chat session (`GroupBinding`) | a background session (no binding) |
| Output goes to | the chat, as a reply | a run record + activity log |
| Created via | Space → Định kỳ, `space_recurring_create` | App manifest, core registry, Chạy ngầm UI |
| On failure | stays `active` forever (bug) | backoff → auto-pause → surfaced |

`ContextMode::Isolated` — an unimplemented stub since day one (`src/scheduler/executor.rs:47-53`, logs *"will be dispatched as a fresh session when agent pool is wired"*) — is the seam this design fills, but it fills it in the new subsystem rather than in the old one. See §16 open question 2.

---

## 2. What already exists in core (verified against code)

Nearly every primitive is present. The subsystem is mostly assembly.

| Primitive | Entry point | Location | Notes |
|---|---|---|---|
| One-shot agent run, returns text | `run_one_shot(OneShotOptions) -> Result<OneShotResult>` | `src/agent/isolated_runner.rs:162` | **The execution core.** Per-call MCP injection, tool allowlist, system prompt, turn budget, timeout, `CancellationToken`, `on_activity` stream |
| Live activity stream | `OneShotOptions.on_activity: Fn(&str, &str)` | `isolated_runner.rs:102` | kinds: `think`/`text`/`tool`/`tool_error`/`message` — this *is* the background-session transcript |
| Persona → run | `PersonaRegistry::get(name)` → `system_prompt` + `tools` | `src/agent/persona_registry.rs:113` | pattern in `src/workflow/step_runners.rs:92` |
| Poll-loop scheduler shape | `MCPDispatcher::tick` | `src/agent/mcp_dispatch/mod.rs:76` | concurrency counter, lease heartbeat, live enable/disable re-read per tick |
| Cron / interval math | `compute_next_run(&task)` | `src/scheduler/task_scheduler.rs:126` | 5-field cron normalized by `"0 "` prefix; local TZ |
| App install hook | `try_autoregister_app_mcp(s, app_id, manifest)` | `src/gateway/ui_server/space.rs:1922` | already installs skills + personas from the manifest |
| App uninstall hook | `space_apps_delete` | `src/gateway/ui_server/space.rs:1034` | already removes skills + personas + MCP + process |
| Ownership-tag cleanup pattern | `install_app_skills` / `remove_app_skills` | `src/gateway/ui_server/space_skills.rs:60,82` | tag the artifact with `app_id`, scan-by-tag on removal |
| WS push to UI | `AgentEventSink` (all-default no-op trait) | `src/agent/agent_pool/traits.rs:25` | extension point; no AgentPool changes needed |
| App → core RPC | `POST /api/space/apps/:id/bridge` | `src/gateway/ui_server/space.rs:1183` | `agent.run`, `llm.request`, `knowledge.*` |

**What's missing** (built in §13):

1. No table, model, scheduler, or REST surface for background tasks.
2. `run_one_shot` has **no model selection** — `ZenCoreOptions.model_config_id` exists (`src/zen_core/mod.rs:1023`) but `run_one_shot` never sets it. A ~3-line plumb.
3. `scheduled_tasks` has **no ownership column**, so "which schedules does app X own?" is unanswerable — the new table fixes this by design.
4. **No server-initiated WS push when a task fires.** The existing task WS messages (`list:tasks`, `list:task-logs`) are strictly request/response.
5. No statistics anywhere, for either subsystem.

---

## 3. Architecture

```
   ┌─────────────────────────────────────────────────────────────────────┐
   │ REGISTRATION (who declares background work)                         │
   │                                                                     │
   │  App manifest          Core native registry      User (UI)          │
   │  senclaw-manifest.json  register_native_job()    POST /api/         │
   │    "background": [...]  ("core.cognitive.*")      background/tasks  │
   │         │                      │                      │             │
   └─────────┼──────────────────────┼──────────────────────┼─────────────┘
             │  install/uninstall   │  boot                │
             ▼                      ▼                      ▼
   ┌─────────────────────────────────────────────────────────────────────┐
   │ background_tasks  (owner_kind, owner_id, owner_key) UNIQUE          │
   └──────────────────────────────┬──────────────────────────────────────┘
                                  │ next_run <= now AND status='active'
                                  ▼
   ┌─────────────────────────────────────────────────────────────────────┐
   │ BackgroundScheduler   tick every N s                                │
   │   ├─ advance next_run (before run — no re-pickup)                   │
   │   ├─ overlap policy: skip | queue | cancel_previous                 │
   │   ├─ global Semaphore(max_concurrent) + per-owner cap               │
   │   └─ tokio::spawn per task   ◄── NOT sequential (see §5)            │
   └──────────────────────────────┬──────────────────────────────────────┘
                                  ▼
   ┌─────────────────────────────────────────────────────────────────────┐
   │ BACKGROUND SESSION   id = bg:<run_id>                               │
   │                                                                     │
   │  1. resolve prompt  ── static │ template+contextUrl │ generator     │
   │  2. resolve persona → system_prompt, use_tools                      │
   │  3. continuity=thread? inject last N run summaries                  │
   │  4. run_one_shot(OneShotOptions{ instance_id: "bg:<run_id>",        │
   │                                  on_activity: → persist + WS })     │
   │  5. record run + result/error; backoff on failure                   │
   │                                                                     │
   │  NO GroupBinding · NO channel_messages · NO reply to any channel    │
   └──────────────────────────────┬──────────────────────────────────────┘
                                  ▼
   ┌─────────────────────────────────────────────────────────────────────┐
   │ background_runs + background_activity                               │
   │        │                                                            │
   │        ├──► WS: bg:run:started / bg:run:activity / bg:run:finished  │
   │        ├──► REST: run history, activity timeline, stats             │
   │        └──► UI: Chạy ngầm (list · detail · session viewer · stats)  │
   └─────────────────────────────────────────────────────────────────────┘
```

Native jobs (§7) enter at the same registration layer and produce the same run/activity records — so periodic memory upkeep is visible and pausable in the same place as prompt tasks, even though its body is Rust rather than an agent.

---

## 4. Data model

```sql
CREATE TABLE IF NOT EXISTS background_tasks (
  id                   TEXT PRIMARY KEY,
  -- Ownership. (owner_id, owner_key) is the idempotency key: re-installing an
  -- app upserts its tasks instead of duplicating them.
  owner_kind           TEXT NOT NULL,                    -- 'system' | 'app' | 'user'
  owner_id             TEXT NOT NULL,                    -- 'core.cognitive' | 'crm' | 'ui'
  owner_key            TEXT NOT NULL,                    -- 'daily-followup'
  title                TEXT NOT NULL,
  description          TEXT,

  -- Body: either an agent prompt or a registered native Rust job.
  job_kind             TEXT NOT NULL DEFAULT 'prompt',   -- 'prompt' | 'native'
  native_job           TEXT,                             -- registry key when job_kind='native'
  prompt_kind          TEXT NOT NULL DEFAULT 'static',   -- 'static' | 'template' | 'generator'
  prompt               TEXT,
  context_url          TEXT,                             -- prompt_kind='template': GET → JSON vars

  -- Agent run configuration (maps 1:1 onto OneShotOptions).
  persona              TEXT,
  agent_folder         TEXT,
  workspace_dir        TEXT,
  use_tools            TEXT,                             -- JSON array; empty/null = all
  mcp                  TEXT,                             -- JSON array of McpServerSpec
  model_id             TEXT,                             -- needs the §13 run_one_shot plumb
  max_turns            INTEGER,
  timeout_secs         INTEGER,
  continuity           TEXT NOT NULL DEFAULT 'fresh',    -- 'fresh' | 'thread'
  memory_folder        TEXT,

  -- Trigger.
  trigger_type         TEXT NOT NULL,                    -- 'cron'|'interval'|'once'|'on_install'|'manual'
  trigger_value        TEXT,                             -- cron expr | ms | RFC3339
  next_run             TEXT,
  last_run             TEXT,

  -- Policy.
  overlap_policy       TEXT NOT NULL DEFAULT 'skip',     -- 'skip'|'queue'|'cancel_previous'
  catch_up             INTEGER NOT NULL DEFAULT 0,       -- run once on missed window?
  max_failures         INTEGER NOT NULL DEFAULT 5,       -- 0 = never auto-pause
  consecutive_failures INTEGER NOT NULL DEFAULT 0,
  visibility           TEXT NOT NULL DEFAULT 'normal',   -- 'normal'|'internal'

  status               TEXT NOT NULL DEFAULT 'active',   -- active|paused|completed|failed|cancelled
  created_at           TEXT NOT NULL,
  updated_at           TEXT NOT NULL,
  UNIQUE(owner_id, owner_key)
);
CREATE INDEX IF NOT EXISTS idx_bg_due   ON background_tasks(next_run, status);
CREATE INDEX IF NOT EXISTS idx_bg_owner ON background_tasks(owner_kind, owner_id);

CREATE TABLE IF NOT EXISTS background_runs (
  id           TEXT PRIMARY KEY,
  task_id      TEXT NOT NULL,
  session_id   TEXT NOT NULL,                            -- 'bg:<run_id>'
  trigger_kind TEXT NOT NULL,                            -- 'schedule'|'manual'|'install'|'catch_up'
  status       TEXT NOT NULL,                            -- running|success|error|timeout|cancelled|skipped
  started_at   TEXT NOT NULL,
  finished_at  TEXT,
  duration_ms  INTEGER,
  turn_count   INTEGER,
  tokens_in    INTEGER,
  tokens_out   INTEGER,
  prompt       TEXT,                                     -- the resolved prompt actually sent
  result       TEXT,
  error        TEXT
);
CREATE INDEX IF NOT EXISTS idx_bg_runs_task   ON background_runs(task_id, started_at DESC);
CREATE INDEX IF NOT EXISTS idx_bg_runs_status ON background_runs(status, started_at DESC);

CREATE TABLE IF NOT EXISTS background_activity (
  id     INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id TEXT NOT NULL,
  ts     TEXT NOT NULL,
  kind   TEXT NOT NULL,                                  -- think|text|tool|tool_error|message
  detail TEXT
);
CREATE INDEX IF NOT EXISTS idx_bg_activity_run ON background_activity(run_id, id);
```

Schema goes in `src/db/schema.rs` alongside `scheduled_tasks` (`:225`). Timestamps are RFC3339 TEXT, matching the rest of the codebase.

Two deliberate departures from `scheduled_tasks`:

- **`last_run` is written on every run.** In `scheduled_tasks` it never is — `update_task_run` (`src/db/scheduled_tasks.rs:79`) has exactly one caller, `src/db/tests.rs:198` — so the column is permanently NULL and every consumer of it renders empty. `serialize_schedule` works around this with an N+1 log query per row (`space_server.rs:1794`). We write it inline and skip the workaround.
- **`consecutive_failures` + `max_failures`** exist because nobody is watching a background task. A `scheduled_tasks` row that fails every run stays `active` forever; a background task quarantines itself.

### The background session

A background session **is a run**: `session_id = bg:<run_id>`, passed as `OneShotOptions.instance_id`, with `background_activity` rows as its transcript.

It is explicitly **not** a `GroupBinding`. That is the whole point, and it also sidesteps a landmine: `is_dynamic_system_jid` (`src/gateway/group_manager/config.rs:76`) whitelists `schedule:`/`cowork:`/`web:`/`virtual:` prefixes, and any `groups` row *not* on that list and *not* in `config.json` is deleted at boot by `sync_groups_from_config`. Since we never create a `groups` row, **no change to that function is needed** — worth stating explicitly, because the comment at `config.rs:145-149` records that this exact reconciliation once wiped every `schedule:` session.

`continuity` is how a background task remembers across runs, since it has no chat history to accumulate:

- **`fresh`** (default) — each run starts clean.
- **`thread`** — the last N run summaries are injected into `custom_rules`, plus `memory_folder` gives the run a persistent memory namespace (default `bg-<owner_id>-<owner_key>`, mirroring `agent.run`'s `space-app-<id>` convention at `space.rs:1203`).

Customer follow-up needs `thread` — it must not re-contact the same person twice. Knowledge upkeep is fine with `fresh`.

---

## 5. The scheduler

`src/background/scheduler.rs`, modelled on `MCPDispatcher::tick` (`src/agent/mcp_dispatch/mod.rs:76`) rather than on `TaskScheduler::tick`.

```rust
pub struct BackgroundScheduler { db, personas, config, running: Mutex<HashMap<String, RunHandle>>, sem: Arc<Semaphore> }

async fn tick(self: &Arc<Self>) -> Result<()> {
    if !self.enabled() { return Ok(()); }                  // re-read per tick, live toggle
    for task in self.db.get_due_background_tasks(&now())? {
        match self.overlap_check(&task) { Skip => { record_skipped(); continue }, ... }
        self.db.advance_background_next_run(&task.id, compute_next_run(&task))?;   // before run
        let this = self.clone();
        tokio::spawn(async move { this.execute(task, TriggerKind::Schedule).await });  // ◄── concurrent
    }
    Ok(())
}
```

Differences from `TaskScheduler` that matter:

- **Concurrent dispatch.** `TaskScheduler::tick` (`task_scheduler.rs:71-79`) awaits `dispatch(task)` in a `for` loop, so one slow agent run blocks every other due task behind it. Background tasks are long by nature (a CRM sweep is minutes), so we spawn per task and bound with `Semaphore(background.max_concurrent)`, default 3, plus a per-owner cap so one App can't starve the rest.
- **Overlap policy**, because a 5-minute task on a 1-minute interval is a real configuration. `skip` (default) records a `skipped` run and moves on; `queue` waits for the permit; `cancel_previous` fires the previous run's `CancellationToken`.
- **Catch-up.** `advance_next_run` walks forward past all missed windows; with `catch_up=1` a single run fires immediately for the gap, with `catch_up=0` (default) the gap is dropped. `TaskScheduler` has no notion of this.
- **Backoff.** On failure, `consecutive_failures += 1` and `next_run` is pushed out by `min(2^n × interval, 1h)`. At `max_failures` the task flips to `status='failed'` and emits `bg:task:changed` — the UI shows it in a "Cần chú ý" band. Success resets the counter.
- **Real `run-now`.** `POST .../run-now` executes inline and returns the `run_id`. Contrast `recurring_run_now` (`space_server.rs:1554`), which rewinds `next_run` to `now - 1s` and lets the 30 s poll pick it up — the Space UI's run-now button therefore appears dead for up to 30 seconds.

Poll interval: `background.interval_secs`, default 20, floor 5. Config block in `src/config.rs` next to `DispatchConfig` (`:291`), env prefix `SENCLAW_BACKGROUND_*`.

Execution (`src/background/runner.rs`) is a thin wrapper over `run_one_shot`:

```rust
let opts = OneShotOptions {
    prompt: resolved_prompt,
    working_dir: task.workspace_dir.unwrap_or(scratch_dir),
    instance_id: Some(format!("bg:{run_id}")),
    use_tools: persona_tools_or_task_tools,
    system_prompt: persona.map(|p| p.system_prompt),
    custom_rules: continuity_context,
    mcp_configs: task.mcp,
    timeout: Some(Duration::from_secs(task.timeout_secs.unwrap_or(300))),
    max_agent_turns: task.max_turns,
    cancel: Some(token.clone()),
    on_activity: Some(Arc::new(move |kind, detail| {
        db.insert_background_activity(&run_id, kind, detail);
        ws.emit("bg:run:activity", json!({ "runId": run_id, "kind": kind, "detail": detail }));
    })),
    ..Default::default()
};
```

Reuse `run_one_shot` rather than `VirtualWorkerPool::run`: the pool injects MCP **globally** via `set_extra_mcp_servers` (`src/agent/virtual_worker_pool.rs:215`), which is unsafe for concurrent heterogeneous background tasks. `run_one_shot` injects per call. (Do **not** use `AgentPool::run_isolated` at `pool.rs:2826` — it is a dead stub with zero callers whose "wait for idle" closure returns `Ok(())` immediately.)

---

## 6. Prompt sources

`prompt_kind` decides how the prompt for a run is produced. This is the "tự tạo prompt rồi chạy" requirement.

| kind | Behavior | Use case |
|---|---|---|
| `static` | `prompt` verbatim | "Review the knowledge base for contradictions." |
| `template` | `GET context_url` → JSON; substitute `{{var}}` into `prompt`; skip the run (status `skipped`) if the context is empty | "Follow up with these customers: `{{overdue}}`" — CRM computes the list in SQL; no LLM needed to decide *who* |
| `generator` | one `llm.request` with `prompt` as the instruction; its output becomes the real prompt | open-ended standing duties where the task itself must be decided |

`template` is the workhorse and should be the documented default for Apps. It keeps the App's own data logic in the App (which already has the SQL), keeps the daemon from guessing, and — importantly — **the empty-context skip means a task with nothing to do costs zero tokens**. `context_url` is resolved against the App's own base URL, reusing the launcher's port registry.

`generator` exists for genuinely open-ended cases but doubles the token cost and adds a failure mode; flag it in the UI.

---

## 7. Native jobs (unifying existing upkeep)

The request names "định kì check memory" as an example. That work already exists, but as ad-hoc `tokio::spawn` loops scattered through boot, invisible and un-pausable:

| Job | Cadence | Today |
|---|---|---|
| Cognitive decay ticker | 300 s | `src/memory/cognitive/system.rs:228` |
| Cognitive maintenance (junk cleanup, entity merge) | `cognitive.maintenance_interval_hours` | `src/memory/cognitive/maintenance.rs:79` |
| SOUL.md re-ingest watcher | 30 s | `src/lib.rs:1247` |

`job_kind='native'` brings these into the same registry: a `HashMap<String, NativeJob>` where `NativeJob` is an async closure, registered at boot under keys like `core.cognitive.decay`. They get the same run records, the same statistics, the same pause/run-now, the same UI row — but their body is Rust, not an agent.

**Scope limit.** Only these three (plus possibly the 30 s DispatchBridge agent sync at `src/lib.rs:2477`) are candidates. Deliberately excluded: infrastructure watchdogs that must never be user-pausable (Space-App supervisor `lib.rs:1634`, MCP client watchdog `src/mcp/client.rs:460`, persona/memory file watchers) and 1.5–2 s change-detection pollers (Kanban→WS `lib.rs:2102`), which are two orders of magnitude below any scheduler tick. Those stay as they are.

Note that `consolidate.rs` and `reflection.rs` are **not** timer-driven — they ride the conversation lifecycle (post-compaction at `pool.rs:2651`, per-turn window flush at `pool.rs:1527`). They are not candidates and should not be migrated.

Native jobs default to `visibility='internal'`, shown under a "Hệ thống" filter so core upkeep doesn't bury the user's own tasks.

---

## 8. App registration

### Manifest

The manifest has no Rust struct — it is read as untyped `serde_json::Value` (`space.rs:89`) and unknown keys are silently ignored, so a new top-level array is additive and backward-compatible by construction. (Precedent: `widgets` is read by no Rust code at all.)

```json
"background": [
  {
    "key": "daily-followup",
    "title": "Chăm sóc khách hàng hàng ngày",
    "description": "Rà khách quá hạn follow-up và soạn tin nhắn tiếp cận.",
    "persona": "sale-closer",
    "prompt": {
      "kind": "template",
      "text": "Những khách sau đã quá hạn follow-up:\n{{customers}}\n\nVới mỗi khách, soạn tin nhắn phù hợp và ghi lại tương tác.",
      "contextUrl": "/api/bg/context/daily-followup"
    },
    "trigger": { "type": "cron", "value": "0 9 * * *" },
    "tools": ["mcp__crm-mcp__crm_customer_*", "mcp__crm-mcp__crm_log_interaction"],
    "continuity": "thread",
    "overlapPolicy": "skip",
    "timeoutSecs": 600,
    "maxTurns": 40,
    "runOnInstall": false,
    "enabledByDefault": true
  }
]
```

### Lifecycle

Mirrors skills and personas exactly — the pattern the codebase already committed to:

- **Install** — `install_app_background_tasks(db, app_id, manifest)` in a new `src/gateway/ui_server/space_background.rs`, called from `try_autoregister_app_mcp` (`space.rs:1933`) next to `install_app_skills` / `install_app_personas`. Upserts each entry on `(owner_id=app_id, owner_key=key)`, so reinstall updates rather than duplicates. `runOnInstall: true` fires one run immediately with `trigger_kind='install'`. `enabledByDefault: false` installs the task `paused` — the right default for anything that contacts customers.
- **Uninstall** — `remove_app_background_tasks(db, app_id)` called from `space_apps_delete` (`space.rs:1065`): cancel in-flight runs, `DELETE FROM background_tasks WHERE owner_kind='app' AND owner_id=?`. Runs are kept for audit (matching `task_run_logs`, which deliberately survives `delete_task` — `schedule_server.rs:381`) and pruned by retention.

Ownership is a real column here rather than the filename-prefix convention used for personas (`<app_id>__<name>.md`, `space_personas.rs:7-9`) or the `.senclaw-app.json` marker used for skills (`space_skills.rs:60`) — those work around the filesystem having no metadata. We have a database.

### Runtime registration

Bridge actions on the existing `POST /api/space/apps/:id/bridge` (`space.rs:1183`): `background.register`, `background.unregister`, `background.list`. `owner_id` is forced from the URL path segment, never taken from the payload.

**Security note, stated plainly:** the bridge has **no authentication** — no app token, no bearer, and `CorsLayer::permissive()` (`core.rs:760`). The app id in the path is self-asserted, so any local process can already act as any app. Runtime registration does not create this hole, but it does widen what's behind it, since a background task is a standing scheduled agent run with tools. **Manifest-declarative registration is therefore the recommended path** — it is parsed by the daemon at install time from a file the user chose to install, which sidesteps the issue entirely and matches how skills and personas already work. Runtime registration should exist for dynamic cases but be treated as the exception. Closing the bridge authn gap is out of scope here and deserves its own doc.

---

## 9. HTTP + WS contract

| Method + path | Body → Response | Purpose |
|---|---|---|
| `GET /api/background/tasks` | `?owner=&status=&visibility=` → `{tasks:[…]}` | list |
| `POST /api/background/tasks` | task spec → `{id}` | create (user-owned) |
| `GET /api/background/tasks/:id` | → `{task, recentRuns, stats}` | detail |
| `PATCH /api/background/tasks/:id` | `{status?|prompt?|trigger?…}` → `{success}` | edit, pause, resume |
| `DELETE /api/background/tasks/:id` | → `{success}` | delete (cancels in-flight) |
| `POST /api/background/tasks/:id/run-now` | → `{runId}` | **executes inline** |
| `GET /api/background/tasks/:id/runs` | `?limit=&status=` → `{runs:[…]}` | history |
| `GET /api/background/runs/:id` | → `{run, activity:[…]}` | **the background-session viewer** |
| `POST /api/background/runs/:id/cancel` | → `{success}` | cancel in-flight |
| `GET /api/background/stats` | `?window=24h\|7d\|30d` → see §12 | statistics |

Routes in `src/gateway/ui_server/core.rs` next to the space routes (`:562-626`); handlers in a new `src/gateway/ui_server/background.rs`.

WS events, emitted through a `BackgroundEventSink` following `AgentEventSink`'s all-default-no-op shape (`traits.rs:25`):

| Event | Payload | When |
|---|---|---|
| `bg:run:started` | `{taskId, runId, title, triggerKind}` | run begins |
| `bg:run:activity` | `{runId, kind, detail, ts}` | each `on_activity` callback |
| `bg:run:finished` | `{taskId, runId, status, durationMs, error?}` | run ends |
| `bg:task:changed` | `{taskId, status, nextRun, consecutiveFailures}` | create/edit/pause/auto-pause/delete |

This fills the gap where the existing scheduler pushes nothing when a task fires — the only scheduler-adjacent pushes today are the calendar notifier's `space:event:reminder` (`notify.rs:419`), which is unrelated to tasks.

---

## 10. MCP server — `senclaw-background`

Everything in §9 must also be reachable from a chat session, so the user can say *"mỗi sáng 9h tự rà khách quá hạn giúp tôi"* and get a background task, or *"task nào đang lỗi?"* and get an answer, without opening the UI.

Naming follows the registry in `CLAUDE.md`: server **`senclaw-background`**, tool prefix **`background_`**. Not `bg_` — `cog_` is the one historical exception in the registry and we are not adding a second. (`bg:` remains the *session-id* prefix; that namespace is unrelated to tool naming.)

Wiring, mirroring `senclaw-schedule` exactly:

| Piece | Location | Change |
|---|---|---|
| Config builder | `src/mcp/helper.rs:42` (next to `schedule_mcp_config`) | `background_mcp_config(db_path, group_folder, chat_jid)` → `McpServerConfig::new("senclaw-background", "background-server")` |
| Subcommand | `src/main.rs:194` | `Command::BackgroundServer => senclaw::mcp::background_server::run_stdio_server().await` |
| Injection | `src/agent/agent_pool/pool.rs:1047` | `mcp_servers.push(background_mcp_config(&db_path_s, &binding.folder, &binding.jid))` |
| Server | `src/mcp/background_server.rs` | **new** — `#[rmcp::tool_router(server_handler)] impl McpBackgroundServer` |

### Tools

| Tool | Params | Purpose |
|---|---|---|
| `background_create` | `title`, `prompt`, `trigger_type`, `trigger_value`, `description?`, `persona?`, `tools?`, `prompt_kind?`, `context_url?`, `continuity?`, `timeout_secs?`, `max_turns?`, `overlap_policy?` | Create a task. `owner_*` is **not** a param — see below |
| `background_list` | `owner?`, `status?`, `include_internal?` | All tasks: title, owner, trigger in prose, next run, last status, failure count |
| `background_get` | `task_id` | Task config + last N runs + this task's stats |
| `background_update` | `task_id`, any of `prompt`/`trigger_*`/`tools`/`timeout_secs`/… | Edit. User-owned only |
| `background_pause` | `task_id` | Stop firing; keep config |
| `background_resume` | `task_id` | Recompute `next_run`, clear `consecutive_failures` |
| `background_delete` | `task_id` | Remove. User-owned only |
| `background_run_now` | `task_id` | Execute inline; returns `run_id` + result |
| `background_get_run` | `run_id`, `include_activity?` | **The background-session transcript**: resolved prompt, activity timeline, result/error, duration, turns |
| `background_cancel_run` | `run_id` | Fire the run's `CancellationToken` |
| `background_stats` | `window?` (`24h`/`7d`/`30d`), `owner?` | §12 payload, prose-summarized |

### Ownership is pinned from env, never from params

`background_create` takes **no owner parameter**. The server reads `SENCLAW_GROUP_FOLDER` / `SENCLAW_CHAT_JID` from the env the config builder injected, and forces `owner_kind='user'`, `owner_id=<group_folder>`. The model cannot claim `owner_kind='app'` or `'system'`.

This is a deliberate departure from `senclaw-schedule`, where `McpScheduleServer.group_folder` and `.chat_jid` (`src/mcp/schedule_server.rs:46-47`) are populated from env and then **never read** — every tool takes `group_folder` from its params instead (`:81`, `:93`, `:105`…), so the ownership check is client-supplied. Any client can pass an arbitrary `group_folder` and manage another group's tasks. Don't copy that.

### Permission matrix

Note that per-chat admin distinction was removed — every chat is admin — so read and operate are global by design; the matrix scopes by **what owns the task**, not by who is asking.

| | user-owned | app-owned | system / native |
|---|---|---|---|
| `list` `get` `get_run` `stats` | ✅ | ✅ | ✅ |
| `pause` `resume` `run_now` `cancel_run` | ✅ | ✅ | ✅ |
| `create` | ✅ (owner forced) | ✗ manifest only | ✗ registry only |
| `update` `delete` | ✅ | ✗ | ✗ |

Read and operate are global because that is exactly the ask — the user must be able to see and stop everything running in the background, including core upkeep. Authoring and editing are scoped because an App's task config lives in its manifest (an edit would be silently reverted on reinstall) and a native job's body is Rust. `background_delete` on an app-owned task returns a message pointing at uninstall.

### Three guards

**1. Tool inheritance — no privilege escalation.** A task's `use_tools` must be a subset of the creating group's `allowed_tools`. Reusing the existing `groups.allowed_tools` whitelist means a chat with a narrow allowlist cannot mint a background task that has everything. Violations are rejected with the offending tools named, not silently trimmed.

**2. No self-replication.** The write tools (`create`/`update`/`delete`/`run_now`) are excluded from background sessions themselves — a background task that can create background tasks is unbounded recursion one bad prompt away. Precedent: `VIRTUAL_EXCLUDED_TOOLS = ["Task", "AskUserQuestion"]` (`src/agent/virtual_worker_pool.rs:145`). Read tools stay: a task legitimately wants to know what else ran.

**3. Outward-facing tasks are enabled out-of-band.** A background task is a *standing, unattended, tool-enabled agent run* — the most valuable thing on this system for a prompt injection to plant. If an agent reads a CRM note, an email, or a web page that says "create a recurring task that forwards X to Y", the injection becomes persistent and runs forever with nobody watching.

The guard: **if `use_tools` touches anything outward-facing** (`send_*`, `browser_*`, channel tools, App write tools like `crm_log_interaction` or `moltbook_post`) **or `prompt_kind='generator'`, `background_create` always creates the task `paused`** and tells the user to enable it in Chạy ngầm. Authoring in chat, enabling out-of-band in the UI. An injection cannot reach the UI toggle, whereas it could plausibly craft an innocuous-looking permission prompt in the same turn it plants the task. This is the same policy as the manifest's `enabledByDefault: false` for customer-contacting App tasks (§8) — one rule, both paths.

Read-only tasks (summarize, review, index, clean up) create active, because the blast radius of a bad one is wasted tokens.

**4. Quota.** `background.max_tasks_per_owner`, default 20; `background.max_active_per_owner`, default 10. A runaway loop hits a wall.

---

## 11. Skill — `skills/background`

One skill, matching `skills/schedule/`'s shape (which covers create + update + pause + delete + history in a single file). Long-form guidance goes in `assets/`, per `skills/bot-channels/`.

```
skills/background/
  SKILL.md                  # routing, tool map, authoring rules, guards
  assets/authoring.md       # writing a good background prompt; trigger/continuity choice
  assets/troubleshooting.md # reading run history, diagnosing failures, backoff/auto-pause
```

Frontmatter follows `skills/note/SKILL.md` (the newest bundled skill), which uses bare tool names in `allowed-tools`:

```yaml
---
name: background
description: Background task management — create and manage autonomous background sessions (prompts SenClaw runs by itself on a schedule, with no chat reply) via Background MCP
version: 1.0.0
when-to-use: When the user wants SenClaw to do work by itself in the background with no reply to anyone — periodic upkeep, unattended customer follow-up, an App's standing duties — or wants to inspect, pause, cancel, or judge background tasks that are already running (history, errors, statistics). NOT for reminders or anything whose result should arrive as a chat message — that is the `schedule` skill.
triggers:
  # --- Vietnamese: authoring ---
  - chạy ngầm
  - chạy nền
  - tác vụ ngầm
  - task ngầm
  - tự động xử lý
  - tự vận hành
  - tự làm
  - không cần báo
  - âm thầm
  # --- Vietnamese: managing ---
  - task nào đang chạy
  - đang chạy gì
  - dừng task
  - huỷ task
  - lịch sử chạy ngầm
  - task lỗi
  - thống kê task
  - task bị tạm dừng
  # --- English ---
  - background task
  - background job
  - run in background
  - autonomous task
  - unattended
  - standing task
  - background history
  - background stats
mcp_servers:
  - senclaw-background
allowed-tools:
  - background_create
  - background_list
  - background_get
  - background_update
  - background_pause
  - background_resume
  - background_delete
  - background_run_now
  - background_get_run
  - background_cancel_run
  - background_stats
---
```

### The trigger collision with `skills/schedule` — the load-bearing problem

`skills/schedule/SKILL.md` already triggers on `định kỳ`, `mỗi ngày`, `mỗi sáng`, `tự động`, `chạy tự động`, `hàng ngày`, `cron`, `đặt lịch`, `lịch sử chạy`, `recurring`, `daily`, `automate`, `periodic`. **Those are exactly the words a user reaches for when asking for a background task.** Both skills will fire on "mỗi sáng 9h hãy…", and the agent will pick whichever loaded, arbitrarily.

Narrowing the trigger lists does not fix it, because the ambiguity is real: *"mỗi sáng 9h rà khách quá hạn"* is a legitimate request for either system. The words don't carry the distinction — **the intent does**:

> **Does the user want to receive the result?**
> **Yes → `schedule`** (runs in a chat session, replies to them).
> **No → `background`** (runs autonomously, writes to a run record nobody has to read).

So the fix is not disjoint triggers but a **shared routing section, verbatim at the top of both SKILL.md files**, so that whichever one loads routes correctly:

| User says | System | Why |
|---|---|---|
| "mỗi sáng gửi tôi tóm tắt tin tức" | `schedule` | "gửi tôi" — they want the message |
| "nhắc tôi 3h chiều họp" | `schedule` | a reminder is a message |
| "mỗi sáng tự rà khách quá hạn và nhắn cho họ" | `background` | the work is the point; the user isn't the recipient |
| "6 tiếng một lần dọn dẹp tri thức" | `background` | upkeep, nobody reads a report |
| "mỗi tối tổng kết công việc rồi báo tôi" | `schedule` | "báo tôi" |
| "mỗi tối tổng kết công việc lưu vào wiki" | `background` | output goes to a system, not a person |

When genuinely ambiguous, **ask** — one question ("Anh muốn nhận kết quả trong chat, hay để nó tự chạy và xem lại ở Chạy ngầm?") is cheaper than a task in the wrong system, because the two are invisible to each other's UI.

This means a companion edit to `skills/schedule/SKILL.md` adding the same table and a `when-to-use` that explicitly excludes autonomous work. Two skills that both claim "tự động" without a routing rule is a worse outcome than either shipping alone.

### What SKILL.md must teach beyond the tool list

- **Trigger translation.** "mỗi sáng 9h" → `cron` / `0 9 * * *`. The cron parser normalizes 5-field expressions by prefixing `"0 "` and evaluates in **local** time (`src/scheduler/task_scheduler.rs:126,159`) — say the resolved time back to the user in words, because a mis-set cron is invisible until it doesn't fire.
- **Prompt authoring.** A background prompt has no human to ask for clarification and no chat history for context. It must be self-contained, state its own success condition, and say what to do when there's nothing to do. `assets/authoring.md` carries the patterns.
- **Prefer `template` over `generator`.** If the App exposes a `context_url`, the deterministic path is cheaper, skips cleanly when there's no work, and can't hallucinate its own task (§6).
- **`continuity: thread` for anything touching people.** A follow-up task that doesn't know what it did yesterday will contact the same customer twice. This is the single most common way one of these tasks embarrasses its owner.
- **Say when a task is created paused, and why.** Never report "đã tạo xong, mỗi sáng 9h sẽ chạy" for a task that is actually paused pending UI enable (guard 3). Report it as: created, paused, enable here, and here's what it will do on first run.
- **Reading failures.** `consecutive_failures` ≥ `max_failures` → `status='failed'`, auto-paused, in `attention`. Explain the backoff rather than just re-running: `background_run_now` on a task that fails deterministically just burns tokens.
- **Statistics as a judgement, not a dump.** The user's stated reason for wanting stats is to decide whether an automation is worth keeping. 100% `skipped` for a week means "delete this"; a 60% success rate means "the prompt is wrong". Say that, don't print the JSON.

---

## 12. Statistics

Plain SQL aggregates over `background_runs`; `GET /api/background/stats?window=`.

**Case convention, verified against the codebase:** REST is **snake_case** on both request and response (`serialize_schedule` at `space_server.rs:1786`, `ScheduleCreateBody` at `space.rs:590`, and the Flutter `SpaceNote`/`SpaceEvent`/`SpaceSchedule` models all read snake); **WS is camelCase** (`handlers.rs`). `/api/background/*` follows that split — don't mix them within one endpoint.

```json
{
  "window": "7d",
  "since": "2026-07-10T09:00:00+00:00",
  "totals": { "runs": 412, "success": 389, "error": 18, "timeout": 3, "cancelled": 2,
              "skipped": 71, "running": 1,
              "success_rate": 0.944, "avg_duration_ms": 24310, "p95_duration_ms": 78200,
              "tokens_in": 1840233, "tokens_out": 214880 },
  "by_task":  [ { "task_id": "…", "title": "Chăm sóc khách hàng hàng ngày",
                  "owner_kind": "app", "owner_id": "crm", "status": "active",
                  "runs": 7, "success": 6, "skipped": 0, "failures": 1,
                  "success_rate": 0.857, "avg_duration_ms": 142000,
                  "next_run": "2026-07-18T09:00:00+07:00", "consecutive_failures": 0 } ],
  "attention": [ { "task_id": "…", "title": "…", "status": "failed",
                   "consecutive_failures": 5, "last_error": "timeout after 600s" } ]
}
```

`skipped` is reported separately from `success` on purpose: a `template` task that skips because there is nothing to do is *healthy*, and folding it into either bucket would distort the rate. A task that is 100% skipped over a week, though, is one the user should probably delete — which is exactly the judgement call the statistics exist to support.

`attention` is the answer to "có lỗi gì không" and drives the UI's top band.

---

## 13. Changes required

| File | Change |
|---|---|
| `src/db/schema.rs` | +3 tables, +5 indexes (§4), next to `scheduled_tasks` (`:225`) |
| `src/db/background.rs` | **new** — accessors, mirroring `src/db/scheduled_tasks.rs` |
| `src/types.rs` | **new** — `BackgroundTask`, `BackgroundRun`, `BackgroundActivity`, enums |
| `src/background/mod.rs` | **new** — module root |
| `src/background/scheduler.rs` | **new** — tick loop, overlap, backoff, catch-up (§5) |
| `src/background/runner.rs` | **new** — prompt resolution + `run_one_shot` wrapper (§5, §6) |
| `src/background/native.rs` | **new** — native job registry (§7) |
| `src/agent/isolated_runner.rs` | **+1 field** `model_config_id: Option<String>` on `OneShotOptions` → `ZenCoreOptions` (`:171`). The one genuine gap in the existing primitives |
| `src/config.rs` | `BackgroundConfig` next to `DispatchConfig` (`:291`); `SENCLAW_BACKGROUND_*` |
| `src/lib.rs` | boot: register native jobs, start `BackgroundScheduler`, **hold the handle** (note `_task_scheduler`/`_event_notifier` are bound to `_` locals and dropped, so they have no abort path on shutdown — don't copy that) |
| `src/gateway/ui_server/background.rs` | **new** — REST handlers (§9) |
| `src/gateway/ui_server/core.rs` | +10 routes |
| `src/gateway/ui_server/space_background.rs` | **new** — manifest install/uninstall (§8) |
| `src/gateway/ui_server/space.rs` | 2 call sites: `:1933` install, `:1065` uninstall; +3 bridge actions at `:1183` |
| `src/gateway/websocket_gateway/notify.rs` | +4 `bg:*` emitters |
| `src/mcp/background_server.rs` | **new** — 11 tools, owner pinned from env (§10) |
| `src/mcp/helper.rs` | `background_mcp_config(db_path, group_folder, chat_jid)` next to `schedule_mcp_config` (`:42`) |
| `src/mcp/mod.rs` | `pub mod background_server;` |
| `src/main.rs` | `BackgroundServer` variant (`:68` block) + dispatch arm (`:194` block) |
| `src/agent/agent_pool/pool.rs` | **+1 push** `background_mcp_config(…)` at the injection block (`:1047`). The only AgentPool change, and it is one line |
| `src/agent/virtual_worker_pool.rs` | add background write tools to `VIRTUAL_EXCLUDED_TOOLS` (`:145`) — guard 2, no self-replication |
| `skills/background/SKILL.md` + `assets/` | **new** — routing, tool map, authoring, troubleshooting (§11) |
| `skills/schedule/SKILL.md` | **companion edit** — same routing table, `when-to-use` excludes autonomous work (§11) |
| `web/src/components/background/` | **new** — UI (§14) |
| `web/src/hooks/useBackground.ts` | **new** — REST + WS subscription |
| `docs/background-tasks-design.md` | this file |

No changes to `TaskScheduler`, `GroupBinding`, or `is_dynamic_system_jid`. The only `AgentPool` touch is one `mcp_servers.push(...)` line. The subsystem is additive.

---

## 14. UI

A new section, **"Chạy ngầm"**, registered in `SpaceSidebar.tsx:13,48` + `SpacePage.tsx:194` alongside the existing "Định kỳ". Ant Design + Tailwind, Vietnamese strings — matching `web/src/components/space/schedules/`, which is the closest existing analogue and worth reading before building (`SchedulesList.tsx`, `ScheduleDetailPanel.tsx` — its run-history list at `:277` and per-run error rendering at `:303` port over almost directly).

Four views:

1. **Tổng quan** — stat cards (runs/success rate/avg duration/tokens for the window), an "attention" band listing auto-paused and failing tasks, recent-runs feed live over `bg:run:*`.
2. **Danh sách** — one row per task: owner badge (Hệ thống / CRM / AI Office / Bạn), title, trigger summary in prose ("9:00 mỗi ngày"), next run, last status, 20-run sparkline, live spinner while running. Actions: pause/resume, run-now, cancel, delete. Filters by owner and status; native jobs hidden behind a "Hệ thống" toggle.
3. **Chi tiết** — config (read-only for app-owned tasks except status), run history table, statistics for this task.
4. **Phiên chạy ngầm** — the background-session viewer: one run's activity timeline (think / text / tool / tool_error / message), the resolved prompt actually sent, result or error, duration, turn count. Streams live for an in-flight run.

**Flutter parity is a real cost and should be a deliberate decision.** `desktop_app/lib/features/space/space_screen.dart:1751-1873` reimplements schedules natively in Riverpod, and the two frontends have already diverged (Flutter strings are English, web is Vietnamese; a comment at `space_screen.dart:38-39` says schedules moved to Plugins because "Space no longer exists"). Recommendation: ship React first, and for desktop either embed the React view via the existing `embeddedWebView` or accept a read-only Flutter list (view + cancel, no create). Building the full section twice is not worth it for a subsystem whose primary audience is the operator at a desk.

---

## 15. Phased plan

| Phase | Deliverable | Exit test |
|---|---|---|
| **1 — Engine** | schema, types, DB accessors, `BackgroundScheduler`, `runner` over `run_one_shot`, `model_config_id` plumb, config, boot wiring | a `static`+`interval` task registered by hand runs on time, writes a `background_runs` row with real text; no `groups` row created |
| **2 — Observability** | activity persistence, 4 WS events, REST §9, stats §12, retention prune | run history + activity timeline + stats readable over REST; an in-flight run streams `bg:run:activity` |
| **3 — Policy** | overlap policy, backoff, auto-pause, catch-up, cancel | a task that always fails auto-pauses at 5 and appears in `attention`; a 5-min task on a 1-min interval skips instead of piling up |
| **4 — UI** | React "Chạy ngầm" (4 views) | user can create, pause, run-now, cancel, read a background session, judge from stats |
| **5 — MCP + skill** | `senclaw-background` (11 tools, owner pinned, 4 guards), `skills/background`, routing edit to `skills/schedule` | from chat: *"mỗi sáng 9h tự rà khách quá hạn"* → task created **paused** (outward-facing tools), reported honestly as paused; *"task nào đang lỗi?"* → reads `attention` and explains the backoff; a request to *"nhắc tôi 3h họp"* still routes to `schedule`, not here |
| **6 — Apps** | manifest `background` key, install/uninstall, bridge actions, `template` + `contextUrl`, CRM `daily-followup` as first consumer | install CRM → task appears, paused by default; enable → runs at 9:00 against live overdue customers; uninstall → task gone, runs retained |
| **7 — Native** | native registry; migrate cognitive decay, cognitive maintenance, SOUL watcher | the three appear under "Hệ thống", pausable, with run history; watchdogs untouched |

Phases 1–3 are core and land together in practice. Phase 4 (UI) must precede phase 5 (MCP), not follow it: guard 3 makes chat-authored outward-facing tasks **paused pending a UI toggle**, so shipping the skill first would strand every task it creates with nowhere to enable them. Phase 6 is where the feature earns its keep.

---

## 16. Open questions

1. **Token/cost accounting.** `OneShotResult` exposes `turn_count` but not tokens; the chat path gets usage via the `agent:usage` event (`notify.rs:118`). Plumbing usage out of `run_one_shot` may be more than the 3-line `model_config_id` change. If it is, ship `tokens_*` as NULL in phase 2 and fill them later — the stats shape already allows for it.
2. **Retire `ContextMode::Isolated`?** It has been a stub since day one and now has a real implementation living next door. Options: (a) leave it dead, (b) make it delegate to a background task, (c) remove the variant. (b) is tempting but re-couples the two subsystems the moment it ships. Recommend (a) for now, revisit once background is proven — and note `lib.rs:1894-1904` already force-retires legacy schedules at boot, so the precedent for cleanup exists.
3. **Flutter parity** — §14. Needs a product call, not an engineering one.
4. **Bridge authentication** — §8. Pre-existing and out of scope, but background tasks raise the stakes: a standing scheduled agent run with tools is a more attractive target than a one-shot `llm.request`. Worth its own doc.
5. **`catch_up` semantics after long downtime.** If the daemon is off for a week, a daily task has 7 missed windows. Current design: one catch-up run, not 7. Is one enough for CRM follow-up, or does the App need the missed window list passed in as context? Probably the latter — `contextUrl` could receive `?since=<last_run>`, which the App can answer better than we can.
6. **Per-owner concurrency default.** Global default 3 is a guess. `dispatch.per_assignee` (`src/config.rs:291`) is the closest precedent — worth matching rather than inventing.
7. **Is the routing table enough?** (§11) Two skills claiming "tự động"/"định kỳ", separated only by whether the user wants a reply, is a real disambiguation risk — and the cost of getting it wrong is silent: a task lands in the wrong system's UI and the user never finds it. Alternatives if the table proves too weak in practice: (a) merge both into one skill that owns the routing outright and calls two MCPs, (b) have `background_create` detect reply-shaped prompts ("gửi tôi", "báo tôi", "nhắc") and refuse with a pointer to `schedule`. (b) is cheap and worth doing regardless — recommend shipping it with phase 5 and keeping (a) in reserve.
8. **Should guard 3 use PermissionBridge instead of paused-by-default?** The codebase has a working human-in-the-loop path (`form:request`/`permission:resolved`). A prompt would be lower-friction than a UI trip. The argument against: an injection that plants the task is in the same turn as the prompt the user sees, so it can shape what the prompt says; the UI toggle is out-of-band and cannot be reached. Recommend paused-by-default for v1 and revisiting once there's real usage data on how annoying the UI trip is.
9. **Tool-subset check granularity** (guard 1). `groups.allowed_tools` is a flat list, but background specs will want globs (`mcp__crm-mcp__crm_customer_*`). Does the subset check expand globs against the live tool roster, or compare patterns literally? Literal comparison is safer but will reject reasonable specs; glob expansion needs the roster at create time, which the MCP subprocess doesn't have. Leaning literal-with-a-clear-error for v1.

---

*Cross-refs:* `docs/mcp-dispatcher-design.md` (the pull-based sibling — dispatch is app-queued work, background is time-triggered work) · `docs/knowledge-cognitive-flow.md` (native job consumers) · `docs/ARCHITECTURE.md` (boot sequence) · [[ai-office-app]] [[crm-merged-app]] [[curated-memory-feature]]
