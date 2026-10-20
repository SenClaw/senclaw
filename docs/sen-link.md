# SLink — SenClaw ↔ SenClaw on the local network

SLink (code name `sen-link`: the API paths, the `senLink` config key, the `~/.senclaw/sen-link/` folder and the
`senclaw-link` MCP server keep it) lets two SenClaw daemons on the same network — two machines, two people — delegate
tasks to each other using the [A2A protocol](https://a2a-protocol.org) v1.0, JSON-RPC binding. Code: `src/sen_link/`.

Incoming tasks become **kanban cards** on the `SLink` board, so the existing dispatcher — a SenClaw persona or
Claude Code (assignee `claude-code`, run by the `app-claude` app worker — see `app-workers.md`) — does the work, and the result goes back as the
A2A task's artifacts.

There is no relay and no public URL. Two daemons talk to each other directly, and only inside a private network.

## What counts as the LAN

`lan::is_lan_addr` is the rule, and it is enforced, not just advised:

| Range | |
|---|---|
| `127.0.0.0/8`, `::1` | loopback (one machine, tests) |
| `10/8`, `172.16/12`, `192.168/16` | private IPv4 |
| `169.254/16`, `fe80::/10` | link-local |
| `fc00::/7` | unique-local IPv6 (includes Tailscale's `fd7a:115c:a1e0::/48`) |
| `100.64.0.0/10` | CGNAT — **Tailscale's IPv4 block**, so a tailnet counts as LAN |

Anything else is refused: a connection *from* a public address gets 403 before its request is read (a router that
forwards the port shows its public side as the source, so a forwarded port stays closed), and the client will not
*connect to* one — an IP literal is rejected up front, and a DNS name is used only through the LAN addresses it
resolves to (no LAN address → error; no redirects; no system proxy). "LAN" means a private network, not a trusted one.

## Turn it on and pair

1. On both machines: Settings → **SLink** → enable, and give each a name. (`PUT /api/sen-link/settings` with
   `{"enabled": true, "name": "Benji's SenClaw"}`.) This opens the **LAN listener**, port `18791` by default
   (`listenPort`, bound to `listenHost`, `0.0.0.0` by default). Nothing else about the daemon is exposed: the listener
   serves `GET /.well-known/agent-card.json`, `POST /a2a` and `POST /a2a/pair`, and nothing more — no `/api`, no MCP
   bridge, no WebSocket gateway. `SENCLAW_UI_BIND_HOST` no longer matters for SLink.
2. On **B**: `POST /api/sen-link/invites` → `{"code": "K7MQ2XRP", "expiresAt": …, "port": 18791, "addresses": ["192.168.1.20", …]}`
   — a one-time code (10 minutes) and this machine's LAN addresses to type on the other side.
3. On **A**: *Check* the address, then pair with the code.

   ```
   POST /api/sen-link/probe        {"addr": "192.168.1.20"}                          # where does it land, is it a SLink?
   POST /api/sen-link/peers/pair   {"addr": "192.168.1.20", "code": "K7MQ2XRP"}
   ```

### Typing the address

`addr` is `host[:port]` — port `18791` if omitted:

| Form | Example |
|---|---|
| IPv4 | `192.168.1.20`, `192.168.1.20:18791` |
| IPv6 in brackets | `[fd00::5]:18791` (scope ids such as `fe80::1%en0` are not supported: use the IPv4 address or a name) |
| A name | `mac-mini.local`, `nas.lan`, `mac-mini`, a name from `/etc/hosts` or your DNS |
| Tailscale | `100.101.102.103`, `macbook` or `macbook.<tailnet>.ts.net` (MagicDNS) |

A pasted `http://host:port/` is accepted. A name is judged by what it resolves to, not by how it looks: a public name
that points into `192.168.x.x` works; any name that resolves only outside the LAN is refused. `.local` names go through
the operating system's resolver (built in on macOS; Linux needs `nss-mdns`).

The address is stored **as typed**, so a name keeps following DNS when the machine's IP changes; an IP literal does not
(re-pair, or type the name).

`probe` answers `{ok, code, addr, resolved, reachable, hidden, card, error}` with `code` one of `ok`, `bad_address`,
`unresolved`, `not_lan`, `unreachable`, `not_slink`, and an `error` sentence for a person. `card` carries the peer's
name, id and whether it has a live invite; `hidden` means the port is a SLink that does not show its card
(`discoverable: false`) — it can still be paired by typed address.

### What pairing records

Pairing exchanges two random tokens: each side stores the token it presents to the other, and only the SHA-256 of the
token it accepts (`~/.senclaw/sen-link/peers.json`, mode 0600). The side that issued the invite learns the other's
address from the connection itself (the source address plus the port the caller says it listens on), so it can send
tasks back with nothing typed. Each daemon has an id (`~/.senclaw/sen-link/identity.json`); pairing again with the
same daemon replaces its record, keeping the peer id its tasks are filed under and the trust you gave it.

> **The link is plain HTTP with bearer tokens.** Anyone who can watch traffic on your network segment can read the
> tasks and the tokens. That is acceptable on a home network or inside Tailscale (which encrypts), and not on a
> coffee-shop Wi-Fi. Turn SLink off when you do not need it; with it off nothing listens. A sealed channel is the next
> piece of work.

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
| `POST /api/sen-link/probe` `{addr}` | — (checks an address) |
| `POST /api/sen-link/peers/pair` `{addr, code}` | pairing |
| `POST /api/sen-link/peers/:id/messages` `{text, taskId?, worktree?}` | `SendMessage` (new task, or follow-up when `taskId` is set) |
| `GET /api/sen-link/peers/:id/tasks` | `ListTasks` |
| `GET /api/sen-link/peers/:id/tasks/:taskId` | `GetTask` |
| `POST /api/sen-link/peers/:id/tasks/:taskId/cancel` | `CancelTask` (queued tasks only) |

Also: `GET /api/sen-link/status` (settings, this daemon's id and LAN addresses, where it listens and why not, peers
dropped by the upgrade), `GET /api/sen-link/peers`, `PATCH/DELETE /api/sen-link/peers/:id`.

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

## Wire surface (on the receiving daemon, LAN listener only)

| Path | Auth | |
|---|---|---|
| `GET /.well-known/agent-card.json` | none | Agent Card; `supportedInterfaces` names the address the caller used. 404 while SLink is off or `discoverable` is false |
| `POST /a2a` | peer bearer token | `SendMessage`, `GetTask`, `ListTasks`, `CancelTask` (pre-1.0 `message/send`, `tasks/*` accepted) |
| `POST /a2a/pair` | one-time invite code | pairing handshake: `{code, name, port, daemonId, token}` |

`/a2a` and `/a2a/pair` answer 404 while `senLink.enabled` is false. No streaming or push notifications yet: poll `GetTask`.

The Agent Card says who is behind it in an A2A extension, `capabilities.extensions[]` with uri
`https://github.com/SenClaw/senclaw/sen-link/v1` and `params {daemonId, pairable}`; that is how `probe` recognises a
SLink and tells it from any other web server.

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
| `enabled` | false | opens the LAN listener |
| `name`, `description` | `SenClaw`, … | shown in the Agent Card and to peers |
| `listenHost` | `0.0.0.0` | an IP address; which interface(s) the listener binds |
| `listenPort` | `18791` | |
| `discoverable` | true | false: no Agent Card, so probes and scans find nothing; pairing by typed address still works |
| `boardTitle` | `SLink` | board incoming tasks land on (created on first use); an empty value or the old default `sen-link` means `SLink` |
| `workspaceDir` | — | that board's workspace dir (where workers run) |
| `defaultAssignee` | — | when the peer has no assignee of its own |

The listener follows these settings without a restart: saving them opens, moves or closes the socket. If the port is
taken, `GET /api/sen-link/status` reports `listenError` and the listener retries every ten seconds.

## Upgrading from the hub build (0.2.0)

- `publicUrl` and `hubUrl` are gone; old values in `config.json` are ignored and dropped on the next save. Peers no
  longer need anything to be reachable but the LAN listener — `SENCLAW_UI_BIND_HOST` can go back to loopback.
- **Peers that only a hub reached are removed** the first time the new daemon starts; `peers.json` is copied to
  `peers.json.bak-<unix time>` first, and `GET /api/sen-link/status` lists their names under `migrated`. Pair them again.
- **Peers paired directly keep working in one direction at most.** Their stored address is the other side's old UI port
  (`…:18788`), and the new daemon no longer serves `/a2a` there. They are marked `legacy` and should be paired again
  over the LAN (pairing the same daemon replaces the record and keeps its tasks and trust).
- The kanban board `sen-link` is renamed to `SLink` in place, cards and all. New cards carry the label `SLink`; old ones
  keep theirs.

## Not yet

Finding peers without typing an address (mDNS, a network scan), a sealed channel, streaming
(`SendStreamingMessage`), push notifications, and shared boards/memory sync.
