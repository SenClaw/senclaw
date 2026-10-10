# App workers and the `/mcp` bridge

A **app worker** is an installed Space App that *executes* dispatched work items. The first one is
[`app-claude`](https://github.com/SenClaw/app-claude): kanban cards assigned to `claude-code` run in the user's own
Claude Code CLI, verified by the app before the card closes. The daemon holds no Claude-specific code.

```
kanban card (assignee X) ─► MCPDispatcher ─► worktree + dispatch token ─► app declaring worker X
                                 ▲                                         POST /api/worker/runs
                                 └──── poll GET /runs/{id} until outcome ◄─┘ (app runs its agent, verifies)
```

## Declaring a worker

In `senclaw-manifest.json` (the app must be `runtime.mode: "background"` — supervised, always up):

```json
"workers": [{ "assignee": "claude-code", "aliases": ["claude"], "path": "/api/worker" }]
```

Mount `app_space_sdk::worker::worker_router` at that path and implement `Worker` (`start`, `status`, `cancel`).
The router answers only the app's own access token, and refuses everything when the app has none (started outside
SenClaw) — starting a run executes an agent, which must never be anonymous. A worker's children should lead their
own process groups and be killed on cancel and on shutdown: the supervisor's stop signal reaches only the app's group.

## What the dispatcher does (`src/agent/mcp_dispatch/app_worker.rs`)

For a claimed item whose assignee an **enabled** app declares (`src/apps/workers.rs`):

- A SenClaw persona with that name always wins (an app cannot take over a persona's work by declaring it).
- Two enabled apps declaring the same assignee → the item is **Blocked** (`declared by several apps`), never routed
  to whichever row the database returns first.
- An app without an access token is refused (the token is how the app tells the daemon from any local process).

Then it:

1. Creates the workspace as for any worker (a git worktree for cards labelled `worktree`).
2. Mints a **dispatch token** (`mdt_…`, pinned to the daemon's chat — the first chat group —, valid for the item
   timeout + 5 min, revoked when the item ends however it ends) and sends
   `POST {path}/runs` a `Job`: item id, assignee, prompt, `working_dir`, `worktree {path, branch, base}`, timeout,
   and `mcp` — the daemon's `/mcp/core` and `/mcp/kanban` with `Authorization: Bearer <dispatch token>`, plus any
   HTTP MCP servers the source declared.
3. Polls `GET {path}/runs/{id}` every 3 s while keeping the item's lease alive; finalizes the item with the
   `Outcome` the app reports (worktree note added as for every worktree run). Requests carry the app token and
   `x-senclaw-api-version`.
4. Deadline + 30 s (so the app's own timeout outcome arrives first) → `DELETE` the run, `TimedOut`. Unknown run
   (`404`, e.g. the app restarted), 20 unreachable polls or 5 unreadable answers → `Failed`. App not reachable at start → `Failed` (card Blocked with `gave_up: app '…' is not reachable`). A
   session-mode app declaring a worker → `Blocked` with that reason.

Items with no matching app worker take the persona path as before.

## The `/mcp` bridge (`src/mcp/http_bridge.rs`)

`/mcp/core` and `/mcp/kanban` serve SenClaw's own stdio MCP servers over MCP Streamable HTTP (JSON responses;
`GET` answers 405) to external agents: an interactive Claude Code (`claude mcp add`) or an app worker's agent.

- Every request needs a bearer token, loopback included: the daemon API token or a live dispatch token.
- A session (`Mcp-Session-Id`) answers only the token that opened it; idle sessions close after 30 min; each token
  holds at most 8 sessions (32 in total, least recently used closed first).
- `core` is scoped to one chat: a dispatch token's pinned chat (its `?chat=` is ignored), or `?chat=<jid>` with the
  API token, else the first chat group. Sessions opened with a dispatch token close once it is revoked. The `initialize` reply tells the model which `group_folder` / `chat_jid` to pass to tools that ask.
- `core` hosts schedule, background, usage, send, dispatch, memory, wiki, space (calendar, notes), profile and
  sen-link (`BRIDGE_CORE_SERVERS`); the ones duplicating a coding agent's own tools are left out.

```bash
export SENCLAW_TOKEN="$(cat ~/.senclaw/api_token)"
claude mcp add --transport http --scope user senclaw-core http://127.0.0.1:18788/mcp/core --header "Authorization: Bearer ${SENCLAW_TOKEN}"
```

## Trust

An app worker's agent typically runs as the user with a shell (app-claude's default allows `Bash`). A card reaching
Ready with such an assignee means its text gets a shell on this machine — which is why sen-link peers' tasks land in
Triage unless the peer may auto-run.
