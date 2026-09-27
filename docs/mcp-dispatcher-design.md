# MCP Dispatcher: Autonomous work-dispatch for SenClaw + Rust SDK

**Status:** ✅ Implemented (all phases, 2026-07-12) · **Supersedes:** the earlier Kanban-only draft.
**Scope:** a reusable core dispatcher (`MCPDispatcher`) + a Rust SDK (`app-space-sdk::dispatch`),
with the **Kanban Space App** (`apps/kanban`) as the first consumer.

## 0. Implementation status

All phases landed. What was built:
- **SDK** `app-space-sdk/src/dispatch/` — `DispatchSource`/`DispatchProvider` traits, `WorkItem`/`Outcome`/`McpServerSpec`/`Capacity`/`Workspace` types, `HttpDispatchSource` client, `dispatch_router` (axum), `run_mcp_bridge` (stdio↔HTTP).
- **Kanban** `apps/kanban/src/dispatch.rs` + `db.rs` — `claimed_by`/`lease_until` columns, `dispatch_claim`/`heartbeat`/`reclaim`/`clear_claim`, `KanbanDispatchProvider`, `/api/dispatch/*` mounted in `main.rs`.
- **Core** `src/agent/mcp_dispatch/mod.rs` — `MCPDispatcher` engine; `src/main.rs` `mcp-bridge <url>` subcommand; `src/config.rs` `DispatchConfig` (env `SENCLAW_DISPATCH_*`); boot in `src/lib.rs` (gated on `SENCLAW_DISPATCH_ENABLED`).

Verified: SDK + kanban compile & the four `/api/dispatch/*` endpoints work end-to-end via curl (poll→claim→heartbeat→finalize→Done, reclaim); `senclaw mcp-bridge` proxies stdio→kanban HTTP MCP (20 tools); core compiles and the daemon logs `[mcp-dispatch] started — 1 source(s)` at boot. Not yet exercised live: a full autonomous agent run (dispatcher → `run_one_shot` → worker calls `kanban_complete`), which needs a running daemon with `SENCLAW_DISPATCH_ENABLED=true` + a configured LLM + a Ready task.

**Enable it:** run the daemon with `SENCLAW_DISPATCH_ENABLED=true` and `SENCLAW_DISPATCH_KANBAN_URL=http://127.0.0.1:4400` (tune `SENCLAW_DISPATCH_{INTERVAL_SECS,MAX_CONCURRENT,PER_ASSIGNEE,MAX_TURNS,TIMEOUT_SECS}`). Put a card in a board's **Ready** column with an `assignee` matching a persona; the dispatcher claims it, runs a worker, and the worker resolves it with `kanban_complete`/`kanban_block`.

## 1. Goal

Give SenClaw a **generic, reusable** way to autonomously run work through persona agents —
like [Hermes Agent's Kanban dispatcher](https://hermes-agent.nousresearch.com/docs/user-guide/features/kanban),
but not tied to Kanban. Any MCP-exposed work source (a Kanban board, a review queue, an incident
board, an email-triage queue, a research pipeline) can plug into **one** dispatcher engine and get:
watch → claim → spawn a tool-enabled worker agent → run to completion → report back, with **no
human in the loop per task**.

Two deliverables:
1. **`MCPDispatcher`** — one core poll-loop engine that drives any registered *dispatch source*.
2. **A Rust SDK** — the `DispatchSource` abstraction + an HTTP dispatch contract + client/server
   helpers, so a source (in-process, or a remote Space App) becomes dispatchable by implementing a
   small trait.

Kanban is the first consumer and the worked example throughout.

> **Naming.** The original design named a Kanban-specific `KanbanDispatcher`. This is generalized
> to `MCPDispatcher` + a `DispatchSource` trait so the same engine serves every MCP that needs
> autonomous execution. Kanban becomes one `DispatchSource` implementation.

## 2. Design principles

- **One engine, many sources.** The poll/claim/spawn/reclaim loop is generic; everything
  source-specific is behind the `DispatchSource` trait.
- **Reuse the core runner.** Spawning a worker = `isolated_runner::run_one_shot` (already a full
  tool-enabled agent loop). The dispatcher never re-implements agent execution.
- **The SDK is the contract.** A source (Rust in-process, or a Space App over HTTP) implements a
  small trait / REST contract; the SDK carries the shared types and glue.
- **MCP is how the worker acts.** Each work item declares the MCP server(s) the worker needs
  (including the source's own tools) so the worker can update its own item.
- **Dispatcher lives in core.** Only core can spawn a tool-enabled agent; a Space App can only make
  one-shot `llm.request` calls. Sources may be remote, but the engine is core.

## 3. What already exists in core (verified against code)

The load-bearing primitive is a **full tool-enabled agent loop**, not a one-shot LLM call.

| Primitive | Entry point | Notes |
|---|---|---|
| One-shot agent run | `isolated_runner::run_one_shot(OneShotOptions{…})` (`src/agent/isolated_runner.rs:162`) | Runs a prompt to idle. Takes `system_prompt`, `custom_rules`, `use_tools` (whitelist), **`mcp_configs: Vec<McpInject{config, scope}>`** (per-call MCP injection), `skip_permissions`, `max_agent_turns`, `timeout`. Returns `OneShotResult{ text, errored, timed_out }`. |
| Concurrency-gated worker | `VirtualWorkerPool::run(&persona, prompt, cwd, task_id, timeout, …)` (`src/agent/virtual_worker_pool.rs:249`) | Wraps the same `ZenEngine` loop; gates on `persona.max_concurrent`. Caveat: MCP injection is global (`set_extra_mcp_servers`, `:215`), not per-task. |
| DAG dispatcher (a poll loop already!) | `DispatchBridge::enqueue_parent(…)` (`src/agent/dispatch_bridge/bridge.rs:196`) + `::start` (`:343`) | 300 ms poll loop; dep gating via `can_start_task`/`is_ready` (`:989`); per-persona concurrency; timeouts. **The pattern `MCPDispatcher` mirrors.** |
| Persona resolution | `PersonaRegistry::get(name) -> &PersonaConfig` (`src/agent/persona_registry.rs:113`) | `.md` frontmatter `name/description/tools/max_concurrent` + body = system prompt; plus builtins. |
| MCP injection | `McpServerConfig{ name, command, args, env, request_timeout_secs }` (`src/mcp/helper.rs:13`) | stdio subprocesses; `run_one_shot` takes them per-call. |
| Tool whitelist | `use_tools` = `persona.tools` ∥ `GroupBinding.allowed_tools` ∥ `OneShotOptions.use_tools` | Empty = all. See [[senclaw-allowed-tools-trap]]. |
| Workflow engine (alt runner) | `WorkflowService::start_run(…)` (`src/workflow/service.rs:416`), agent steps → `run_one_shot` | `StepKind::Agent|Script`, `depends_on`, `trigger:"schedule"`. |

Not usable as the runner: the `scheduler` has no persona field and its `Isolated`/`ScriptAgent`
agent arms are stubs (`src/scheduler/executor.rs:47`, `:162`). Core also has an internal
`cowork_team_tasks` board (`src/db/cowork_tasks.rs`) with no MCP surface and no auto-dispatch
(`cowork_runtime.rs:14` "no agent dispatch yet") — a candidate future in-process `DispatchSource`.

## 4. What is missing (must be built)

1. **`MCPDispatcher`** — the generic poll-loop engine (does not exist).
2. **The Rust SDK** — `DispatchSource` trait, shared types, HTTP client/server helpers.
3. **Worker access to a source's HTTP MCP** — Space App MCPs are **HTTP/SSE**; agent injection is
   **stdio**. A generic stdio↔HTTP bridge is required.
4. **Atomic claim/lease + crash reclaim** on each source (Kanban needs new columns + endpoints).

## 5. Architecture

```
   sources (implement DispatchSource, via the SDK)
   ┌──────────────────────┐   ┌───────────────────────────┐
   │ apps/kanban (remote) │   │ core cowork (in-process)  │   … future sources
   │ HttpDispatchSource ──┼───┤ impl DispatchSource       │
   └──────────┬───────────┘   └────────────┬──────────────┘
              │  REST dispatch contract     │  direct Db
              ▼                             ▼
        ┌───────────────────────────────────────────────┐
        │  MCPDispatcher (core)  — one poll loop ~30s    │
        │  for each source: reclaim · poll_ready ·       │
        │  spawn worker · finalize                       │
        └───────────────────────┬───────────────────────┘
                                │ run_one_shot(persona = item.assignee,
                                │   mcp = item.mcp, system += item.guidance)
                                ▼
                Worker agent (one session per item)
                item.mcp tools → do work → Outcome::Completed | Blocked
                                │
        ┌───────────────────────┘  finalize(item_id, outcome)
        ▼
   source updates its own record (kanban_complete / block / …)
```

The worker reaches a remote source's HTTP MCP through the SDK's **stdio↔HTTP bridge**
(a spawnable `senclaw mcp-bridge <url>` proxy) so `run_one_shot` can inject it like any
`McpServerConfig`.

## 6. The abstraction (SDK core)

```rust
/// A source of dispatchable work. Implemented in-process (Rust + Db) or, for a
/// remote Space App, by the SDK's HttpDispatchSource over the REST contract (§8).
#[async_trait]
pub trait DispatchSource: Send + Sync {
    /// Stable id, e.g. "kanban" or "kanban:board-3".
    fn id(&self) -> &str;

    /// Atomically claim up to `capacity` ready items (deps satisfied, under WIP,
    /// per-assignee limits). Claiming sets a lease so a crash can be reclaimed.
    async fn poll_ready(&self, capacity: Capacity) -> Result<Vec<WorkItem>>;

    /// Extend the lease on an in-flight item (called while the worker runs).
    async fn heartbeat(&self, item_id: &str) -> Result<()>;

    /// Return items whose worker died / lease expired to the ready state.
    async fn reclaim(&self) -> Result<Vec<String>>;

    /// Record the terminal outcome (source maps it to its own semantics).
    async fn finalize(&self, item_id: &str, outcome: Outcome) -> Result<()>;
}

pub struct Capacity { pub total: usize, pub per_assignee: usize }

pub struct WorkItem {
    pub id: String,
    pub assignee: Option<String>,       // → persona (None = source/default persona)
    pub prompt: String,                 // the task to run
    pub guidance: Option<String>,       // source-specific system-prompt block
    pub mcp: Vec<McpServerSpec>,        // tools the worker needs (incl. the source's own)
    pub workspace: Workspace,           // Scratch | Dir(path) | Worktree{repo, branch}
    pub depends_on: Vec<String>,
    pub priority: i32,
    pub timeout: Option<Duration>,
}

pub enum McpServerSpec {
    Stdio(McpServerConfig),             // native stdio server
    Http { name: String, url: String },// bridged to stdio by the SDK at spawn time
}

pub enum Outcome {
    Completed { summary: String, metadata: serde_json::Value },
    Blocked   { reason: String },
    Failed    { error: String },
    TimedOut,
}
```

`WorkItem` is deliberately runner-shaped: the dispatcher can build a `run_one_shot` call from it
without knowing anything about Kanban.

## 7. The Rust SDK (`app-space-sdk::dispatch`)

New module in the existing workspace SDK crate (`app-space-sdk/src/dispatch/`). Four layers:

1. **Shared types + trait** — `DispatchSource`, `WorkItem`, `Outcome`, `Workspace`,
   `McpServerSpec`, `Capacity`. Depended on by both core and apps.
2. **Client (core-side)** — `HttpDispatchSource::new(base_url, source_id)` implements
   `DispatchSource` by calling the REST contract (§8). This is how `MCPDispatcher` drives any
   remote Space App with zero source-specific code.
3. **Server helpers (app-side)** — a `DispatchProvider` trait an app implements over its own data,
   plus `dispatch_router::<P>()` returning an axum `Router` that mounts the standard endpoints. An
   app implements a handful of methods and gets a dispatch-compatible HTTP surface for free.
   ```rust
   #[async_trait]
   pub trait DispatchProvider: Send + Sync {
       async fn claim_ready(&self, cap: Capacity) -> Result<Vec<WorkItem>>;
       async fn heartbeat(&self, id: &str) -> Result<()>;
       async fn reclaim(&self) -> Result<Vec<String>>;
       async fn finalize(&self, id: &str, outcome: Outcome) -> Result<()>;
   }
   ```
4. **stdio↔HTTP MCP bridge** — an SDK helper (`mcp_bridge::to_stdio(McpServerSpec::Http{..})`)
   and a `senclaw mcp-bridge <url>` subcommand that proxies stdio MCP to an app's `/api/mcp`, so
   any `McpServerSpec::Http` is turned into a spawnable `McpServerConfig` at worker launch. This is
   generic — it works for **any** Space App MCP, not just Kanban ("dùng cho toàn MCP cần thiết").

The SDK is the reusable surface: a new dispatchable feature implements `DispatchProvider`
(if remote) or `DispatchSource` (if in-process), and nothing in the engine changes.

## 8. HTTP dispatch contract (what a remote source exposes)

Mounted by `dispatch_router` under `/api/dispatch`:

| Method + path | Body → Response | Purpose |
|---|---|---|
| `POST /api/dispatch/poll` | `{capacity}` → `[WorkItem]` | Atomically claim & return ready items |
| `POST /api/dispatch/heartbeat` | `{item_id}` → `{ok}` | Extend lease |
| `POST /api/dispatch/reclaim` | `{}` → `[item_id]` | Return dead-worker items to ready |
| `POST /api/dispatch/finalize` | `{item_id, outcome}` → `{ok}` | Record complete/block/fail |

An app-space provider auth-scopes these to the daemon (same trust model as the existing bridge).

## 9. The engine — `MCPDispatcher` (core, `src/agent/mcp_dispatch/`)

```rust
pub struct MCPDispatcher {
    sources: Vec<Arc<dyn DispatchSource>>,
    personas: Arc<PersonaRegistry>,
    budget: Concurrency,      // total + per-source + per-assignee
    interval: Duration,       // poll tick, e.g. 30s
    failure_limit: u32,       // circuit breaker
}
```

Per tick, for each source:
1. `source.reclaim()` — dead/expired workers → ready (+ a `stale`/`crashed` note via finalize path).
2. `source.poll_ready(capacity)` — atomically claim items within budget.
3. For each `WorkItem`: resolve `persona = personas.get(item.assignee)`; resolve `item.mcp` (bridge
   any `Http` spec to stdio); `run_one_shot(OneShotOptions{ prompt: item.prompt, system_prompt:
   persona.system_prompt + item.guidance, working_dir: item.workspace, use_tools: persona.tools,
   mcp_configs: item.mcp, skip_permissions: true, timeout: item.timeout, .. })`.
4. On finish: map `OneShotResult` → `Outcome` (the worker's own `complete`/`block` tool call is the
   source of truth; a silent exit or `errored`/`timed_out` → `Failed`/`TimedOut`) and call
   `source.finalize(item.id, outcome)`. Circuit-break after `failure_limit` consecutive failures.

Registration at boot (`src/lib.rs`): register in-process sources directly and remote sources via
`HttpDispatchSource::new(app_base_url, "kanban")`. Config-gated on/off with the budget/interval knobs.

For items with children, the engine may instead route through `DispatchBridge::enqueue_parent` to
reuse existing dependency fan-out.

## 10. Kanban as the first consumer

`apps/kanban` implements `DispatchProvider` over `kanban.db` and mounts `dispatch_router`:
- `claim_ready` — cards in the `ready`-role column with `open_deps == 0` (already computed in
  `board_full`), under WIP + per-assignee=1; sets `claimed_by`+`lease_until` in a transaction.
  Maps each to a `WorkItem { assignee, prompt: title+description, guidance: KANBAN_GUIDANCE,
  mcp: [Http{ "senclaw-kanban", "<app>/api/mcp" }], workspace }`.
- `heartbeat` — extend `lease_until`.
- `reclaim` — cards whose `lease_until` passed → back to `ready`, comment `stale`.
- `finalize` — `Completed → kanban_complete` (move to Done + summary comment);
  `Blocked → kanban_block` (→ Blocked + reason); `Failed/TimedOut → kanban_block("gave_up: …")`.

The worker gets the `kanban_*` tools (via the bridged `senclaw-kanban` MCP) so it can `kanban_show`
its card, then the engine finalizes based on the worker's terminal `complete`/`block` call.

## 11. Worker run + guidance + workspace

Guidance is per-source; Kanban's `KANBAN_GUIDANCE(id)`:

> You are running task `#{id}`. (1) `kanban_show({id})` to read the description, comments, and
> parent summaries. (2) Do the work with your tools. (3) You **must** finish with exactly one of
> `kanban_complete(summary, metadata)` or `kanban_block(reason)`; never exit without one. For
> code-changing tasks, `kanban_block("review-required: …")` + `kanban_comment(metadata)`.

Workspaces mirror Hermes: **scratch** (temp, discarded), **dir** (persistent path), **worktree**
(git worktree for coding tasks). Carried on `WorkItem.workspace`.

## 12. Task lifecycle (Kanban mapping)

```
Triage ─(decompose)─▶ Todo ─(open_deps==0)─▶ Ready ─(dispatcher claim+spawn)─▶ In Progress
                                                                                   │
                                    ┌───────────────────────────────────────────────┤
                              kanban_complete                                  kanban_block
                                    ▼                                                ▼
                                  Done                                           Blocked ─(unblock)─▶ Ready
```

Dispatcher: `Todo→Ready`, `Ready→In Progress`, reclaim `In Progress→Ready`. Worker: `→Done`,
`→Blocked`. Human/agent: `Blocked→Ready`.

## 13. Changes required

**SDK (`app-space-sdk::dispatch`):** shared types + `DispatchSource`/`DispatchProvider` traits +
`HttpDispatchSource` client + `dispatch_router` + `mcp_bridge` (and the `senclaw mcp-bridge`
subcommand).

**Core:** `MCPDispatcher` engine (`src/agent/mcp_dispatch/`), boot registration + config, wiring to
`run_one_shot`/`PersonaRegistry`.

**Kanban app (Phase 0):** `claimed_by`/`lease_until` columns; implement `DispatchProvider`; mount
`dispatch_router`; declare its MCP as an `Http` spec on each `WorkItem`.

## 14. Phased plan

- **Phase 0 — SDK skeleton + Kanban provider (app):** `app-space-sdk::dispatch` types/traits +
  `dispatch_router` + `mcp_bridge`; Kanban implements `DispatchProvider` (claim/lease/finalize) and
  mounts the router. No behavior change yet.
- **Phase 1 — Engine, single-task autonomous run (core):** `MCPDispatcher` with one source
  (`HttpDispatchSource("kanban")`), poll → claim → `run_one_shot(persona, kanban-mcp, guidance)` →
  finalize. **The "agent runs the task by itself" milestone.**
- **Phase 2 — Full semantics:** dependency promotion, board-wide + per-assignee WIP, crash reclaim
  via lease/PID, circuit breaker; multi-source registry.
- **Phase 3 — Parity extras:** auto-decompose, review-gate, terminal-event notifications,
  goal-mode; add a second `DispatchSource` (e.g. core cowork or a review queue) to prove reuse.

## 15. Open questions

- **Bridge vs. native:** ship the stdio↔HTTP `mcp_bridge` for remote sources, or push sources to
  provide native stdio MCP? (Recommendation: bridge — it makes *every* Space App dispatchable.)
- **SDK home:** `app-space-sdk::dispatch` (already in the workspace, apps depend on it) vs. a
  standalone `dispatch-sdk` crate if non-space in-process sources want it without the space bits.
- **Workspace policy:** default scratch, or require explicit `dir`/`worktree` per source/item?
- **Permissioning:** workers run unattended (`skip_permissions: true`) — which tool classes (shell,
  send) stay human-gated even for autonomous workers?
- **Model routing:** daemon active model for all workers, or per-persona model selection?

---

*Cross-refs:* [[kanban-app]] · [[cowork-dag-verification-bugs]] · [[workflow-feature-port]] ·
[[senclaw-allowed-tools-trap]] · [[senclaw-mcp-naming]] · [[space-mcp-sdk-harness]]. Companion
analysis: `docs/DISPATCH_COWORK_FLOW_ANALYSIS.md`, `docs/DAG_Team.md`, `docs/cowork-flow.md`.
