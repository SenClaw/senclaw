# SLink — SenClaw ↔ SenClaw on the local network

SLink (code name `sen-link`: the API paths, the `senLink` config key, the `~/.senclaw/sen-link/` folder and the
`senclaw-link` MCP server keep it) lets two SenClaw daemons on the same network — two machines, two people — delegate
tasks to each other using the [A2A protocol](https://a2a-protocol.org) v1.0, JSON-RPC binding. Code: `src/sen_link/`.

Incoming tasks become **kanban cards** on the `SLink` board, so the existing dispatcher — a SenClaw persona or
Claude Code (assignee `claude-code`, run by the `app-claude` app worker — see `app-workers.md`) — does the work, and the result goes back as the
A2A task's artifacts.

There is no relay and no public URL. Two daemons talk to each other directly, only inside a private network, and
everything they say after pairing is sealed (see [What is private](#what-is-private)).

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
   serves `GET /.well-known/agent-card.json`, `POST /a2a` and `POST /a2a/pair/begin|finish`, and nothing more — no `/api`, no MCP
   bridge, no WebSocket gateway. `SENCLAW_UI_BIND_HOST` no longer matters for SLink.
2. On **B**: `POST /api/sen-link/invites` → `{"code": "K7MQ2XRP", "expiresAt": …, "port": 18791, "addresses": ["192.168.1.20", …]}`
   — a one-time code (10 minutes) and this machine's LAN addresses to type on the other side.
3. On **A**: *Check* the address, then pair with the code.

   ```
   POST /api/sen-link/probe        {"addr": "192.168.1.20"}                          # where does it land, is it a SLink?
   POST /api/sen-link/peers/pair   {"addr": "192.168.1.20", "code": "K7MQ2XRP"}
   ```

### Finding peers without typing

While SLink is on, the daemon finds other SenClaws by itself, and a person can ask it to look harder.

**Announcements (mDNS / DNS-SD).** While enabled, the daemon browses `_senclaw-link._tcp` on the LAN. While it is also
`discoverable` and listening on a real interface (not loopback), it announces itself: the TXT record carries `v=1`,
`id` (its daemon id), `name` and `pair` (`1` while an invite can be redeemed), and the SRV record the listener's port.
No Bonjour or Avahi is needed. Only interfaces with a LAN address are used. Turning SLink off sends a goodbye, so the
others drop it at once. A daemon that is not heard from again is dropped after five minutes. An unchanged announcement is
not repeated, so a daemon that is still there is asked rather than waited for: once its last sighting is over 2½ minutes
old, the daemon requests its Agent Card at the addresses it was seen at (every 15 s, at most four daemons at a time, 3 s
each) and a card with the same id counts as a sighting. One that does not answer, or answers as another daemon, is
dropped on schedule. `GET /api/sen-link/discovered` lists what has been seen:

```json
{"discovered": [{"daemonId": "3b1b316d-…", "name": "Mac A", "addrs": ["192.168.0.82"], "port": 51791,
                 "pairable": true, "sources": ["mdns"], "seenSecsAgo": 3}],
 "browsing": true, "advertising": true, "error": null}
```

An entry is a *claim*: anyone on the network can announce any name. What proves who is on the other end is the pairing
code, not the list. Pair from the list with `{"daemonId": "…", "code": "…"}` instead of an address; the daemon tries each
address the id was seen at, reads the Agent Card first and refuses to send the code to a daemon with a different id.

**Scan.** `POST /api/sen-link/scan` walks subnets looking for SLinks, and only when asked — it never runs by itself,
because a TCP sweep can set off an IDS on a managed network. By default it walks the subnets this machine is on (a
subnet wider than a /22 is narrowed to the /24 around the address, and the reply says so). Point-to-point interfaces — a
VPN, Tailscale — are left out, so an unaimed scan never sweeps the far end of a tunnel; type its range if you want it. `{"cidr": "192.168.1.0/24", "port": 18791}` walks a
range of your choice instead: it must be inside the LAN and hold at most 1024 addresses. IPv4 only. Each address gets a
TCP connect (400 ms) and, if the port is open, an Agent Card request (1.5 s); only a SenClaw card with the SLink
extension counts. 128 at a time, so a /24 takes about a second. One scan at a time (a second start is a 409); poll
`GET /api/sen-link/scan/:id` for `{state: running|done|canceled, scanned, total, ranges, notes, found}` and stop it with
`POST /api/sen-link/scan/:id/cancel`. Hits join the `discovered` list for five minutes, and stay while they keep answering. A tailnet cannot be walked
(~4 million addresses): type a Tailscale address or MagicDNS name instead.

`discoverable: false` hides this daemon from both: no announcement, no Agent Card. It can still be paired by a typed address.

macOS 15 asks once for permission to use the local network, and shows the text from the app's `Info.plist`
(`NSLocalNetworkUsageDescription`, with `NSBonjourServices` = `_senclaw-link._tcp`); the desktop app carries both. Where
multicast does not cross (a guest Wi-Fi that isolates clients, a VLAN, a Docker bridge), announcements find nothing —
scan, or type the address.

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

The address is stored **as typed**, so a name keeps following DNS when the machine's IP changes. An IP literal that stops
answering follows the daemon instead: if the network has seen the same daemon id elsewhere (an announcement or a scan),
the call is repeated there and the stored address is updated. A name is never rewritten. An announcement is only a
claim, but following it is safe: the request is sealed under the pair key, so whatever answers at the new address either
holds the key or learns nothing and cannot answer.

`probe` answers `{ok, code, addr, resolved, reachable, hidden, card, error}` with `code` one of `ok`, `bad_address`,
`unresolved`, `not_lan`, `unreachable`, `not_slink`, and an `error` sentence for a person. `card` carries the peer's
name, id and whether it has a live invite; `hidden` means the port is a SLink that does not show its card
(`discoverable: false`) — it can still be paired by typed address.

### How pairing works

The invite code is a *password*, and pairing is a password-authenticated key exchange (SPAKE2, over Ed25519), so the
code is never sent and nothing sent can be tested against guesses offline:

1. **A** types B's code. `POST /a2a/pair/begin` carries A's half of the exchange; B answers with its half and a session id.
2. Each side now holds a key that is the same on both sides *only if the codes were*. A seals its offer — who it is, the
   port it listens on, its id, and the token B will call it with — under that key and sends `POST /a2a/pair/finish`.
3. If B can open the offer, A knew the code (a wrong code gives a different key and nothing opens). B records A, uses up
   the invite, and answers with its own id and the token A will call it with, sealed too. If A can open the answer, B
   knew the code. Both then derive the **pair key** that seals every later request (`~/.senclaw/sen-link/peers.json`, mode 0600).

Someone who does not know the code gets one guess per handshake and learns only that it was wrong. **There is one invite
at a time** (a new code replaces the last), and it **stops working after three wrong codes**. The side that issued the
invite learns the other's address from the connection itself (the source address plus the port in the offer), so it can
send tasks back with nothing typed. Each daemon has an id (`~/.senclaw/sen-link/identity.json`); pairing again with the
same daemon replaces its record, keeping the peer id its tasks are filed under and the trust you gave it — and retires
the old pair key.

## What is private

After pairing, every `/a2a` request and answer is one AES-256-GCM envelope under the pair key. The associated data names
the direction and both daemons, so an envelope cannot be turned into an answer or carried to a third daemon. A request
carries an id and the sender's clock: one outside a two-minute window, or an id already served, is refused, so a
captured request cannot run a task twice (**keep the two machines' clocks within two minutes**). An answer carries the id
of the request it answers. The token given at pairing travels *inside* the envelope, as a second lock.

On the wire someone watching the network sees: that two machines talk, when, how much, and each sender's daemon id
(`X-SLink-From`). They do not see the task, the answer, the token or the key — the test suite sniffs a pairing and a
task through a relay and asserts none of them appears. The Agent Card and the mDNS announcement (name, id, whether an
invite is open) are plain by design; `discoverable: false` hides both. What the pair key does not protect: someone who
reads `peers.json` has the key, and someone who is given the invite code in time can pair themselves — handing out the
code is what trusts them.

## Trust

A remote peer never starts work on its own: its tasks land in **Triage** until a person moves them. To let a peer's
tasks run immediately, set `PATCH /api/sen-link/peers/:id {"autoRun": true}` (optionally `"assignee": "claude-code"`).
The assignee is always yours to set (per peer, else `defaultAssignee`); a peer cannot pick it.

**`autoRun` with assignee `claude-code` gives that peer a shell on this machine**: its text becomes the prompt of a
worker that runs with `Bash` as you (see `app-workers.md` and the app-claude README). Use it only for peers you
control, and prefer Triage — a person reads the task before moving it to Ready.

## Sharing what this SenClaw knows

A paired peer can **ask** this SenClaw a question and get the best few matches from its facts and notes. Nothing is
copied to the peer and nothing is written on either side. It is **closed by default**: until you open it for a peer,
every question is refused with "has not shared its knowledge with you".

`PATCH /api/sen-link/peers/:id {"knowledge": {"facts": true, "notes": true, "profiles": ["main"], "noteTypes": ["project", "reference"]}}`

| Field | Meaning |
|---|---|
| `profiles` | Profile folders (the agent's name on disk) the switches below apply to. None named = nothing shared. Must exist; `GET …/knowledge` lists the choices |
| `facts` | Facts of those profiles that belong to the whole agent: not said in one particular chat, not replaced |
| `notes` | Curated notes of those profiles (`profiles/<name>/memory/*.md` with frontmatter) of the chosen types |
| `noteTypes` | A subset of `project`, `reference`, `feedback`; empty means `project` + `reference`. `user` (notes about you) can never be shared |

Whatever the policy says, these **never** leave: raw day logs (`YYYY-MM-DD.md`), `conversation-summary-*` notes, notes
written in one particular chat (`originSessionId` is a chat JID), notes of type `user`, files that are links or larger
than 256 KB, and the profile files (`USER.md`, `AGENTS.md`, SOUL). Secrets in a text are redacted before it goes out.
In the apps it is the **Knowledge** panel of a peer's row (Settings → SLink, web and desktop), which also has a box to ask the peer. `GET /api/sen-link/peers/:id/knowledge` shows the policy and a **preview computed by the same rules as an answer**: counts
of what would be shared per profile, five sample titles, and how many items each excluded class holds. Read it before
switching a peer on: a real install had a customer's email address in an ordinary reference note.

Asking: `POST /api/sen-link/peers/:id/recall {query, limit?, profile?}` (the `link_recall` tool does the same for an
agent). Search is by words, no model and no embedding, so a question never costs the answering machine a model call.
At most 10 hits, each text cut at 600 characters, 30 questions a minute per peer. Answers carry `untrusted: true` and
`from`: they are text another machine wrote, to weigh and not to obey. A peer that predates this (or has it off) answers
"update SenClaw on that machine"; one that has not shared says so.

## Syncing a Space App's data

Two SenClaws that both have the same Space App can keep its data the same — for ssh-manager: hosts, keychain, port
forwarding rules and snippets. The app does not talk to the other machine; the daemons do. The app says what it holds
(`sync` block in `senclaw-manifest.json`) and answers four local calls (`/schema`, `/state`, `/read`, `/apply` under the
declared `endpoint`); the daemon carries them across the sealed channel as `appsync/*`. The app's side of the contract is in
its repo (ssh-manager's README); the research and the reasoning behind this are in `plans/reports/research-261022-1156-slink-app-data-sync.md`.

**Closed by default, and it takes both ends.** What a peer may do with an app is a policy per peer and app, stored with the peer:

`PUT /api/sen-link/peers/:id/app-sync/:app {"policy": {"collections": {"hosts": "both", "keychain": "send"}, "secrets": false, "adoptMatches": false}}`

| Field | Meaning |
|---|---|
| `collections` | collection → `send` (this machine hands it over), `receive` (takes it), `both`; absent or `off` = nothing. Only collections the app declares |
| `secrets` | Let the fields the app calls secret (passwords, keys) cross, both ways. Off: they are cut from what leaves **and** from what arrives, and a secret already here is never replaced. Turning it on needs `"confirmSecrets": true` in the body |
| `adoptMatches` | Fold a record that arrives under another id into the one already here when they mean the same (same host, port and user; same name). Off: it is reported as a `duplicate` and nothing is added |

A collection moves from A to B only if A **sends** it and B **receives** it, each judged by that machine's own policy when the
call arrives — a bug or a lie on the calling side does not widen what the other allows. Fields the app declares secret
(`secretFields`) or machine-local (`localFields`, e.g. whether a tunnel is open) are cut by the daemon from the manifest, whether or
not the app did it. An app is not synced at all when it answers its sync routes without its access token (the daemon asks its own
port, with no token, and expects 401), when its live `/schema` differs from the manifest's declaration, or when it is not installed.

| Route (on the sending daemon) | |
|---|---|
| `GET /api/sen-link/peers/:id/app-sync` | Syncable apps on this machine with this peer's policy, pending changes and last sync; `remote` lists what the peer has opened to this machine |
| `GET` / `PUT …/app-sync/:app` | One app's policy |
| `POST …/app-sync/:app/preview` | What a sync would do on both machines (`dryRun`); changes nothing |
| `POST …/app-sync/:app/run` `{"approve": [{"collection", "id"}], "collections": [...]}` | Sync now: fetch what the peer has newer (collections this machine receives and the peer sends), send what this machine has newer (sends / peer receives). Both apps pick the newer stamp, so repeating or interrupting a run is harmless |

**Risky changes wait for a person.** An app can hold a change back (`held`) — ssh-manager does for a change that would send a stored
credential somewhere new. Nothing a peer sends can release one: `allowRisky` and `adoptMatches` in a peer's call are not read. The
held changes are in `status.pending` (kept in memory; the next preview finds them again) and are released by running
`approve` from the machine that holds them, which fetches the record again from the peer and applies it with the flag.

The wire methods: `appsync/apps` (what this machine has opened to the caller), `appsync/describe`, `appsync/state`, `appsync/read`,
`appsync/apply`; errors `-32010` (not open to you), `-32011` (over 240 calls a minute), `-32012` (app not installed, not syncing, not
running or not safe), `-32013` (schemas differ). Batches stay under about 300 KB and 500 records, so records that point at each other
settle together; a peer that only receives shows no digest, so everything is sent to it each run and its app keeps what it has newer.
Not done yet: syncing on a timer or when the app changes, an inbox that survives a restart, and the clients' UI.

## Delegating

| Route (on the sending daemon) | A2A call |
|---|---|
| `POST /api/sen-link/probe` `{addr}` | — (checks an address) |
| `GET /api/sen-link/discovered` | — (what announcements and scans have shown) |
| `POST /api/sen-link/scan` `{cidr?, port?}`, `GET /api/sen-link/scan/:id`, `POST /api/sen-link/scan/:id/cancel` | — (walks subnets) |
| `POST /api/sen-link/peers/pair` `{addr \| daemonId, code}` | pairing |
| `POST /api/sen-link/peers/:id/messages` `{text, taskId?, worktree?}` | `SendMessage` (new task, or follow-up when `taskId` is set) |
| `POST /api/sen-link/peers/:id/recall` `{query, limit?, profile?}` | `knowledge/recall` (see above) |
| `GET /api/sen-link/peers/:id/knowledge` | — (policy, preview, profiles to choose from; local) |
| `GET /api/sen-link/peers/:id/tasks` | `ListTasks` |
| `GET /api/sen-link/peers/:id/tasks/:taskId` | `GetTask` |
| `POST /api/sen-link/peers/:id/tasks/:taskId/cancel` | `CancelTask` (queued tasks only) |

Also: `GET /api/sen-link/status` (settings, this daemon's id and LAN addresses, where it listens and why not, whether it
is browsing and announcing, peers dropped by the upgrade), `GET /api/sen-link/peers` (each with `online`: the network has
seen it lately), `PATCH/DELETE /api/sen-link/peers/:id`.

### From an agent (`senclaw-link` tools)

Agents delegate without the UI through the bundled `senclaw-link` MCP server (in `senclaw-core`, so every SenClaw chat
has it, and Claude Code gets it through `/mcp/core`): `link_peers`, `link_send {peer, text, worktree?, task_id?}`,
`link_task {peer, task_id}`, `link_tasks {peer}`, `link_cancel {peer, task_id}`, `link_recall {peer, query, limit?, profile?}`. `peer` is a name (case-insensitive)
or id. Results are compact: `state` is one word — `submitted | working | input_required | completed | failed |
canceled` — and `results` carry the peer's summary and branch. `link_peers` also says which peers are `online`.

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
| `POST /a2a` | a sealed envelope under the pair key, sender named in `X-SLink-From`, token inside | `SendMessage`, `GetTask`, `ListTasks`, `CancelTask` (pre-1.0 `message/send`, `tasks/*` accepted), `knowledge/recall` (error `-32010` when not shared, `-32011` over the limit), `appsync/*` (above); anything that does not open, is stale or replayed, or carries the wrong token is a bare 401 |
| `POST /a2a/pair/begin` | the live invite's code (as the handshake's password) | `{msg}` → `{sid, msg}`; 403 with no live invite, 429 with too many handshakes open |
| `POST /a2a/pair/finish` | the sealed offer | `{sid, sealed}` → the sealed answer; a wrong code is a 403 and counts against the invite |

All of them answer 404 while `senLink.enabled` is false. No streaming or push notifications yet: poll `GetTask`.

The Agent Card says who is behind it in an A2A extension, `capabilities.extensions[]` with uri
`https://github.com/SenClaw/senclaw/sen-link/v1` and `params {daemonId, pairable, knowledge, appSync}` (`knowledge` and `appSync` are the versions of the knowledge and app-sync methods it answers; absent on older builds); that is how `probe` recognises a
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
| `discoverable` | true | false: no announcement and no Agent Card, so probes, scans and listeners find nothing; pairing by typed address still works |
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
- **Peers paired by an earlier build cannot be used until they are paired again.** They have no pair key (and those
  from 0.2.0 an old UI-port address), and nothing is sent or accepted unsealed, so there is no way to be downgraded to
  the old protocol. They are marked `legacy`; pairing the same daemon again replaces the record and keeps its tasks and
  trust. Both sides must run this version.
- The kanban board `sen-link` is renamed to `SLink` in place, cards and all. New cards carry the label `SLink`; old ones
  keep theirs.

## Not yet

Streaming (`SendStreamingMessage`), push notifications, copying knowledge between machines (a read-only mirror per peer is the design, see `plans/reports/research-261020-2247-slink-knowledge-sync.md`), and a limit on how many handshakes one
machine may open (a flood can fill the 16 handshake slots for a minute; it cannot pair).
