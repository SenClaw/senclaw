# Claude Code cowork

SenClaw can hand kanban work to **Claude Code** (the user's own `claude` CLI), verify the result itself, and let
Claude Code call back into SenClaw's tools over MCP. Code: `src/claude_code/`.

```
kanban card (assignee: claude-code) ──► MCPDispatcher ──► ClaudeCodeWorker
                                                            │ spawn `claude -p --output-format stream-json …`
                                                            │   cwd = card worktree, MCP = senclaw-kanban + senclaw-core
                                                            │ Stop hook ──► POST /hooks/claude-code/stop (verify gate)
                                                            │ post-run verify ─ reviewer session (read-only) ─ fix rounds (--resume)
                                                            ▼
                                       Outcome → card Done (with branch note) | Blocked (reason) 
```

## Delegating a card

1. Turn dispatch on (Settings → Dispatch, or `SENCLAW_DISPATCH_ENABLED=1` on first boot).
2. On a board with a **workspace dir**, create a card with **assignee `claude-code`** (or `claude`) and move it to
   **Ready**. Add the label `worktree` to run on its own branch `senclaw/card-<id>` (recommended for code).
3. Set `claudeCode.verifyCommand` (e.g. `cargo test`). It is only ever read from settings — never from card text,
   which can come from a chat or a sen-link peer and must not choose what runs in a shell.

The worker session:

- runs headless in the card's directory with `--permission-mode` / `--allowedTools` from settings, and
  `--strict-mcp-config` — it sees exactly SenClaw's servers, none of the user's own;
- gets `senclaw-kanban` (read the card, comment) and `senclaw-core` (schedule, background, memory, wiki, space/calendar,
  dispatch, send, usage, profile) scoped to `claudeCode.mcpChatJid` (default: the first chat);
- may **not** call `kanban_complete` / `kanban_block` — SenClaw closes the card;
- must end with a structured result (`--json-schema`): `status` `done|blocked`, `summary`, `files_changed`, …

## Verification

The model's report can only lower the outcome (`blocked`); it never closes a card by itself. When no independent
check can run — no `verifyCommand` and no worktree diff to review — the card is **Blocked** as `review-required`.

| Layer | What runs | Effect |
|---|---|---|
| Stop hook | verify command, when Claude tries to finish | failing → `{"decision":"block"}` with the output; Claude keeps working (max `maxStopBlocks`) |
| Post-run verify | verify command again, by SenClaw | failing → feedback round |
| Review | a second Claude Code session, `--permission-mode plan`, read-only tools, over the worktree diff | `changes_requested` → feedback round; no verdict = not a pass |
| Fix rounds | `--resume <session>` with the failures | after `maxFixRounds` the card is **Blocked** with the findings |

On success the card moves to Done with a comment: summary, verify/review results, session id, cost, and the worktree
branch to merge or open a PR from (`/api/worktrees/*`).

## Claude Code → SenClaw (MCP)

The daemon serves MCP **Streamable HTTP** at `/mcp/core` and `/mcp/kanban` (JSON responses, no server stream; `GET`
answers 405). Every request needs a bearer token — loopback included — either the daemon API token
(`~/.senclaw/api_token`) or a live run token. A session (`Mcp-Session-Id`) answers only the token that opened it;
idle sessions are reaped after 30 min, and each token holds at most 8 (32 in total; the least recently used is
closed). `?chat=<jid>` picks the chat `core` is scoped to (API token only; a run token stays on its run's chat). The
`initialize` reply tells the model which `group_folder` / `chat_jid` to pass to tools that ask for them.

```bash
export SENCLAW_TOKEN="$(cat ~/.senclaw/api_token)"
claude mcp add --transport http --scope user senclaw-core http://127.0.0.1:18788/mcp/core --header "Authorization: Bearer ${SENCLAW_TOKEN}"
claude mcp add --transport http --scope user senclaw-kanban http://127.0.0.1:18788/mcp/kanban --header "Authorization: Bearer ${SENCLAW_TOKEN}"
```

`GET /api/claude-code/status` returns these commands for the running daemon.

## Settings — `config.json` → `claudeCode`

| Key | Default | Meaning |
|---|---|---|
| `binary` | `claude` | executable (bare name → `PATH`) |
| `model` | — | `--model` |
| `permissionMode` | `acceptEdits` | worker `--permission-mode` |
| `allowedTools` | `Bash, Read, Edit, Write` | worker `--allowedTools` (SenClaw MCP servers added automatically) |
| `maxTurns` | 80 | per session |
| `maxBudgetUsd` | — | per session (API-key billing) |
| `verifyCommand` | — | check that must exit 0 |
| `verifyTimeoutSecs` | 900 | |
| `maxStopBlocks` | 3 | Stop-hook send-backs per run |
| `review` | true | reviewer session |
| `maxFixRounds` | 2 | `--resume` feedback rounds |
| `mcpChatJid` | — | chat scope for `senclaw-core` |

## REST

| Route | |
|---|---|
| `GET /api/claude-code/status` | binary found, version, MCP endpoints, `claude mcp add` commands |
| `GET /api/claude-code/runs` | live and last 50 runs: phase, session, fix round, last activity, cost |
| `GET/PUT /api/claude-code/settings` | the `claudeCode` block (validated) |
| `POST /hooks/claude-code/stop` | worker Stop hook (run token only) |

## Accounts and policy

SenClaw only spawns the user's own, already signed-in `claude` binary on the user's machine. It never reads, stores, or
forwards Claude credentials. Subscription plans assume individual use — long automated loops may hit plan limits;
API-key auth with `maxBudgetUsd` is the predictable option.

## Trust boundary

The worker is **not sandboxed from the daemon**. It runs as the user; with `Bash` allowed (the default) it can do
anything the user can, including reading `~/.senclaw/api_token`, and the `senclaw-core` server it gets carries the
daemon token in its environment. Not inheriting `SENCLAW_API_TOKEN` / `SENCLAW_WS_TOKEN` is defense in depth only.
The worktree protects the user's checkout, not the machine. Treat "a card assigned to `claude-code` reaches Ready" as
"this text gets a shell on my machine" — which is why sen-link peers land in Triage unless `autoRun` is set.
The session's process group (background jobs, servers it started) is killed when the session or verify ends.
