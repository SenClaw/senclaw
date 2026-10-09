# sen-link — SenClaw ↔ SenClaw over A2A

sen-link lets two SenClaw daemons (two machines, two people) delegate tasks to each other using the
[A2A protocol](https://a2a-protocol.org) v1.0, JSON-RPC binding. Code: `src/sen_link/`.

Incoming tasks become **kanban cards** on the `sen-link` board, so the existing dispatcher — a SenClaw persona or
Claude Code (assignee `claude-code`, see `claude-code-cowork.md`) — does the work, and the result goes back as the
A2A task's artifacts.

## Enable and pair

Both daemons must reach each other over HTTP (same LAN, Tailscale, or a TLS proxy). Bind the UI server beyond
loopback (`SENCLAW_UI_BIND_HOST`) — the API stays token-gated — and set the public URL.

1. On both: `PUT /api/sen-link/settings` with `{"enabled": true, "name": "Benji's SenClaw", "publicUrl": "http://100.64.0.2:18788"}`.
2. On **B**: `POST /api/sen-link/invites` → `{"code": "K7MQ2XRP", "expiresAt": …}` (one-time, 10 min).
3. On **A**: `POST /api/sen-link/peers/pair` with `{"url": "http://<B>:18788", "code": "K7MQ2XRP"}`.

Pairing exchanges two random tokens: each side stores the token it presents to the other, and only the SHA-256 of the
token it accepts (`~/.senclaw/sen-link/peers.json`, mode 0600). A daemon without a `publicUrl` still answers `/a2a`
to anyone who can reach it with a valid peer token; its peers just have no URL to send it tasks with.

## Trust

A remote peer never starts work on its own: its tasks land in **Triage** until a person moves them. To let a peer's
tasks run immediately, set `PATCH /api/sen-link/peers/:id {"autoRun": true}` (optionally `"assignee": "claude-code"`).
The assignee is always yours to set (per peer, else `defaultAssignee`); a peer cannot pick it.

**`autoRun` with assignee `claude-code` gives that peer a shell on this machine**: its text becomes the prompt of a
worker that runs with `Bash` as you (see the trust boundary in `claude-code-cowork.md`). Use it only for peers you
control, and prefer Triage — a person reads the task before moving it to Ready.

## Delegating

| Route (on the sending daemon) | A2A call |
|---|---|
| `POST /api/sen-link/peers/:id/messages` `{text, taskId?, worktree?}` | `SendMessage` (new task, or follow-up when `taskId` is set) |
| `GET /api/sen-link/peers/:id/tasks` | `ListTasks` |
| `GET /api/sen-link/peers/:id/tasks/:taskId` | `GetTask` |
| `POST /api/sen-link/peers/:id/tasks/:taskId/cancel` | `CancelTask` (queued tasks only) |

Also: `GET /api/sen-link/status`, `GET /api/sen-link/peers`, `PATCH/DELETE /api/sen-link/peers/:id`.

## Wire surface (on the receiving daemon)

| Path | Auth | |
|---|---|---|
| `GET /.well-known/agent-card.json` | none | Agent Card (JSON-RPC interface at `<publicUrl>/a2a`, bearer scheme) |
| `POST /a2a` | peer bearer token | `SendMessage`, `GetTask`, `ListTasks`, `CancelTask` (pre-1.0 `message/send`, `tasks/*` accepted) |
| `POST /a2a/pair` | one-time invite code | pairing handshake |

All three answer 404 while `senLink.enabled` is false. No streaming or push notifications yet: poll `GetTask`.

### Task mapping

| Card column | Task state |
|---|---|
| Triage / Todo / Ready | `TASK_STATE_SUBMITTED` |
| In Progress | `TASK_STATE_WORKING` |
| Blocked | `TASK_STATE_INPUT_REQUIRED` (a follow-up message requeues it) · `TASK_STATE_FAILED` (dispatcher gave up) · `TASK_STATE_CANCELED` (final) |
| Done | `TASK_STATE_COMPLETED` |

Artifacts are the card's `complete` and `worktree` comments; the status message is the latest comment. A peer only
sees cards it created (card tenant `sen-link:<peer-id>`).

## Settings — `config.json` → `senLink`

| Key | Default | |
|---|---|---|
| `enabled` | false | |
| `name`, `description` | `SenClaw`, … | shown in the Agent Card and to peers |
| `publicUrl` | — | how peers reach this daemon |
| `boardTitle` | `sen-link` | board incoming tasks land on (created on first use) |
| `workspaceDir` | — | that board's workspace dir (where workers run) |
| `defaultAssignee` | — | when neither peer nor message names one |

## Not yet

Relay through `senclaw-hub-connector` for peers behind NAT, streaming (`SendStreamingMessage`), push notifications,
shared boards/memory sync, and desktop/web UI screens.
