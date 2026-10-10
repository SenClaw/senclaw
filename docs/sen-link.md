# sen-link — SenClaw ↔ SenClaw over A2A

sen-link lets two SenClaw daemons (two machines, two people) delegate tasks to each other using the
[A2A protocol](https://a2a-protocol.org) v1.0, JSON-RPC binding. Code: `src/sen_link/`.

Incoming tasks become **kanban cards** on the `sen-link` board, so the existing dispatcher — a SenClaw persona or
Claude Code (assignee `claude-code`, run by the `app-claude` app worker — see `app-workers.md`) — does the work, and the result goes back as the
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

### Through the hub (NAT, no public URL)

When neither daemon can reach the other, pair through a SenClaw hub (`senclaw-hub-connector`):

1. On both: set `"hubUrl": "https://<your hub>"` in sen-link settings (no `publicUrl` needed).
2. On **B**: `POST /api/sen-link/invites` → the reply also carries `"connect": "senlink:…"`.
3. On **A**: `POST /api/sen-link/peers/pair` with `{"connect": "senlink:…"}`.

Each hub invite mints **its own hub channel** and a fresh 32-byte key; the connection string carries the hub URL,
channel id, channel access token, key and invite code — hand it over like a password. The hub admits at most two
connections per channel, so a channel is exactly one pair of daemons, and frames on it are AES-GCM encrypted with the
pair's key: the hub relays ciphertext only. Requests on the channel go through the same code as `/a2a` and
`/a2a/pair`, so peer tokens are checked exactly as over HTTP, and a peer's token is accepted only on that peer's own
channel. A peer paired this way has no URL and is always called through its channel (`viaRelay: true`).

Each request carries the sender's timestamp; one outside a 2-minute window, or an id already served within it, is
refused — a hub replaying captured frames cannot re-run a task. Answers over 5 MiB are refused with 413 (the hub drops
connections sending frames over 8 MiB), and a channel serves at most 8 requests at once. If a pairing reply is lost,
retrying the same connection string re-issues the token instead of failing. An invite code alone never claims a hub
invite over HTTP, and a hub that cannot be reached leaves a direct invite (with a `warning`).

Both daemons keep their peers' channels open and reconnect on their own (2 s → 60 s backoff). Every 30 s — and right
after removing a peer or saving settings — the open channels are reconciled with the store: an expired invite, a
removed peer or a disabled sen-link closes its socket. Hub versions before the write-deadline fix (every frame relayed
more than 10 s after the hub's last ping failed with "i/o timeout") drop such links every few seconds; update the hub.

## Trust

A remote peer never starts work on its own: its tasks land in **Triage** until a person moves them. To let a peer's
tasks run immediately, set `PATCH /api/sen-link/peers/:id {"autoRun": true}` (optionally `"assignee": "claude-code"`).
The assignee is always yours to set (per peer, else `defaultAssignee`); a peer cannot pick it.

**`autoRun` with assignee `claude-code` gives that peer a shell on this machine**: its text becomes the prompt of a
worker that runs with `Bash` as you (see `app-workers.md` and the app-claude README). Use it only for peers you
control, and prefer Triage — a person reads the task before moving it to Ready.

## Delegating

| Route (on the sending daemon) | A2A call |
|---|---|
| `POST /api/sen-link/peers/pair` `{url, code}` or `{connect}` | pairing (direct, or through a hub invite) |
| `POST /api/sen-link/peers/:id/messages` `{text, taskId?, worktree?}` | `SendMessage` (new task, or follow-up when `taskId` is set) |
| `GET /api/sen-link/peers/:id/tasks` | `ListTasks` |
| `GET /api/sen-link/peers/:id/tasks/:taskId` | `GetTask` |
| `POST /api/sen-link/peers/:id/tasks/:taskId/cancel` | `CancelTask` (queued tasks only) |

Also: `GET /api/sen-link/status`, `GET /api/sen-link/peers`, `PATCH/DELETE /api/sen-link/peers/:id`.

### From an agent (`senclaw-link` tools)

Agents delegate without the UI through the bundled `senclaw-link` MCP server (in `senclaw-core`, so every SenClaw chat
has it, and Claude Code gets it through `/mcp/core`): `link_peers`, `link_send {peer, text, worktree?, task_id?}`,
`link_task {peer, task_id}`, `link_tasks {peer}`, `link_cancel {peer, task_id}`. `peer` is a name (case-insensitive)
or id. Results are compact: `state` is one word — `submitted | working | input_required | completed | failed |
canceled` — and `results` carry the peer's summary and branch.

To be woken when a delegated task finishes, arm a watch instead of polling in the turn:

```json
{"tool": "mcp__senclaw-link__link_task", "args": {"peer": "SenClaw B", "task_id": "5"},
 "done_path": "state", "done_op": "in", "done_values": ["completed", "failed", "canceled", "input_required"]}
```

The tools are loopback calls to `/api/sen-link/*` (registered as a daemon-wide built-in, so a watch can probe them).

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
| `defaultAssignee` | — | when the peer has no assignee of its own |
| `hubUrl` | — | SenClaw hub for peers behind NAT; when set, every invite mints a hub channel |

## Not yet

Streaming (`SendStreamingMessage`), push notifications, and shared boards/memory sync.
