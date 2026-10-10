# Plan: move the Claude Code worker into the background Space App `app-claude`

Status: implemented on feat/claude-code-cowork (+ new repo app-claude) · E2E verified with a fake `claude` · Research: `../reports/research-261010-1036-claude-code-to-app-claude.md`

## Decisions (recommended defaults; user said "continue")
1. Generic **app-worker protocol** (manifest `workers`), not app polling kanban.
2. Remove the Claude Code sections from desktop/web; the app ships its own UI (Space Apps iframe).
3. `/mcp` HTTP bridge stays in the daemon (shared by every external client).
4. Distribution: zip via `scripts/pack.sh` (marketplace style), not bundled with desktop.
5. Built-in worker removed from the daemon (feature unreleased → no data migration).

## Phases
| # | Phase | Repo | Status |
|---|---|---|---|
| 1 | SDK `worker` module: job/status types, `Worker` trait, `worker_router` (POST/GET/DELETE `/runs`), daemon-token check | senclaw/app-space-sdk | done |
| 2 | Daemon: manifest `workers`, resolver over enabled apps, dispatcher → app over loopback (start/poll/cancel), dispatch tokens for `/mcp` (bridge moved to `src/mcp/http_bridge`) | senclaw | done |
| 3 | `app-claude` repo: port runner/verify/worker/hooks/registry/settings, REST + embedded UI, manifest (background), pack.sh | app-claude | done |
| 4 | Remove `src/claude_code` (except bridge), `claudeCode` config block, `/api/claude-code/*`; remove desktop/web sections; docs | senclaw, desktop, web-app | done |
| 5 | E2E: install app-claude into an isolated daemon, card `claude-code` → app → Done/Blocked; sen-link task over hub → app | all | done |

## Acceptance
- A card assigned to `claude-code` is executed by app-claude (fake `claude`), verified, reviewed, closed — same outcomes as the built-in worker had.
- Daemon has no Claude-specific code left; `cargo test`, flutter test, web build pass.
- App down/not installed → card Blocked with a clear reason (not a silent persona run).

## Result (2026-10-10)
- Daemon: generic app-worker protocol (`src/apps/workers.rs`, `src/agent/mcp_dispatch/app_worker.rs`), `/mcp` bridge
  moved to `src/mcp/http_bridge.rs` with dispatch tokens; `src/claude_code/` and the `claudeCode` config block removed.
  `cargo test --lib` 2764 pass.
- SDK: `app_space_sdk::worker` (Job, RunStatus, Worker, token-only `worker_router`).
- app-claude: new repo, 20 tests; zip via `scripts/pack.sh`.
- Clients: Claude Code sections removed from web and desktop (360 desktop tests pass, web builds).
- E2E (isolated daemon, fake `claude`): install zip → background app up, direct calls 401 / proxy OK; card → app →
  Done with Stop-hook gate, verify, review; alias `claude`; worker's MCP via dispatch token (80 + 20 tools), token 401
  after; app stopped → card Blocked with reason; app stop mid-run kills claude and its children; sen-link task from a
  peer over the hub executed by the app.
- Review fixes: process groups killed on cancel/shutdown, dispatch tokens pinned to the daemon's chat (no `?chat=`),
  persona precedence + assignee conflicts blocked, tokenless app refused (both sides), token revoked in a drop guard
  and its bridge sessions reaped, deadline grace, protocol-error cap, API-version header.
- Known: reinstalling an app zip does not restart a running background app (existing daemon behaviour) — stop/start
  after an update.
