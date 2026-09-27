# Pairing: how a Telegram chat earns a binding

## The hole this closes

A Telegram message from a chat with no binding used to **complete the channel's
pending binding on sight**. Whoever messaged the bot first owned it — no time
limit, no human in the loop, and no check on who they were. Three details made
that worse than it sounds:

- The **web UI only ever creates pending bindings.** `AgentSettings.tsx` calls
  `onRegisterBinding({ agentId, channelId })` with no `jid`, and the Telegram
  channel form has no Chat JID field at all. So the first-come path was not an
  edge case — it was the only way to bind Telegram from the UI.
- **An empty stored bot token matched any incoming token.** The Settings form
  omits `botToken` entirely when the field is blank ("Leave empty to use the
  global default bot from .env"), so `creds["botToken"]` read as `""` and a
  message arriving through a completely unrelated bot completed the binding.
- **The claimer got everything.** A UI-created agent has no `allowed_tools`, and
  `None => Vec::new() // empty = no filter (all tools)` means Bash and Write.
  The permission prompt is delivered to `chat_jid` — the same chat — so they
  approved their own tool calls.

`ADMIN_TELEGRAM_USER_ID`, `TelegramBotConfig.admin_user_id` and a documented
`senclaw channel telegram add --user <id>` all suggested an allowlist existed.
None of them was read by anything; the CLI subcommand did not exist at all.
That is the dangerous part: the system *looked* authenticated, so nobody
re-checked it.

## The flow now

```
unknown chat ──message──▶ router
                            │  no binding
                            ▼
                    channel_pairings row  ──▶ bot replies with an 8-char code
                            │                  (and processes nothing)
                            │
                     a person approves
                     (Settings, or `senclaw pairing approve <CODE>`)
                            │
                            ▼
                         binding
```

The two halves live in different modules on purpose:

| | file | trust |
|---|---|---|
| request | [`src/gateway/pairing.rs`](../src/gateway/pairing.rs) `request` | runs on untrusted input; may only **record** |
| approval | same file, `approve` / `reject` | behind the daemon's authenticated API |

`MessageRouter::challenge_unbound_telegram` returns `Option<GroupBinding>` and
**always returns `None`**. Nothing reachable from an arriving message can grant
access.

## Rules for Claude

- **A pairing row is a request, never a grant.** Only `approve` writes a
  binding. Any future code path that turns a `channel_pairings` row into access
  without a human has reintroduced the original bug in a new shape.
- **An empty stored `botToken` means the configured default bot, not any bot.**
  `resolve_telegram_channel` compares against `config.telegram.bot_token` and
  matches nothing when that is also empty. Restoring the old
  `|| channel_token.is_empty()` shortcut reopens cross-bot claiming, and it
  reads like a harmless fallback.
- **Re-serve the live code; do not mint a new one per message.** Someone who
  says "hello" three times while walking to their laptop is one request. Three
  rows read as three strangers knocking, and the UI cannot tell which code the
  person is holding.
- **Announce on a new code, never "once per chat".** `request` returns
  `Requested { pairing, is_new }` and the router speaks only when `is_new`. The
  flood guard falls out of row reuse: repeat messages inside the hour re-serve
  the same row and say nothing. Keying it to the *chat* instead looks equivalent
  and is not — once the code expires the next message mints a replacement, and a
  chat-keyed flag goes silent exactly then, leaving the user holding a dead code
  with no way to learn the new one.
- **The bot must say something.** Silence is indistinguishable from a broken
  bot, which is the pressure that produced auto-claiming in the first place.
  `render_challenge` names the code, says where to approve it, and states
  plainly that nothing was processed.
- **`resolve_pairing` is narrowed to `status='pending'` and returns the row
  count.** An `Ok` on a zero-row update would let two approvers both believe
  they were the one who let this chat in, and would make a rejected code
  re-approvable.
- **Approving a group lets in every member of that group.** There is still no
  per-sender check inside a bound chat — `should_trigger` looks at
  `is_from_me`, chat type and `@mention`, never at `sender_jid`. The UI says so
  before the click; do not describe group pairing as authenticating a person.
- **Expiry is one hour and unreadable stamps count as expired.** A row whose
  deadline cannot be parsed must not stay approvable forever.
- **The first approval fills the channel's pending binding; later ones create
  their own** onto the same agent (`list_bindings_for_channel` → first row).
  Without that second branch a channel could only ever admit one chat, because
  nothing else creates pending rows.

## Surfaces

| | |
|---|---|
| REST | `GET /api/pairings[?all=true]`, `POST /api/pairings/:id/{approve,reject}`, `POST /api/pairings/approve-code` |
| WS | `pairing:requested`, `pairing:resolved` (broadcast to all clients) |
| Web UI | Settings → Channels → *Pairing* ([`PairingCard.tsx`](../web/src/components/settings/PairingCard.tsx)) |
| Desktop | Settings → Channels → *Pairing* ([`pairing_section.dart`](../desktop_app/lib/features/settings/pairing_section.dart)) |
| Chat | `pair` · `pair approve <CODE>` · `pair reject <ID>` |
| CLI | `senclaw pairing list \| approve <CODE> \| reject <ID>` |

**The chat commands live in `dispatch_command`, which both surfaces call** — the
message router (channel chats) and `handle_message_send` (web/desktop chat
sessions) — so one implementation covers Telegram and the app's own chat. Both
callers are already gated: an unbound chat is answered with a code and returns
*before* command dispatch, so it can never approve itself, and the WebSocket
handler runs `require_auth` first. The existing model still applies, though —
every bound chat has full admin, so any of them can approve any pending request.

**Never point a user at the CLI from inside the product.** `render_challenge`
used to say `senclaw pairing approve <CODE>`. That binary resolves through
`PATH`, and a months-old copy in `~/.local/bin` answers
`error: unrecognized subcommand 'pairing'` — which reads as the pairing feature
being broken rather than the binary being stale. The challenge now names only
routes served by the daemon that sent the message: the Settings page and
`pair approve <CODE>` typed into an already-connected chat. A test asserts the
CLI string is absent.

The CLI talks to the daemon over loopback rather than opening the DB: approval
has to reach the waiting chat with "you're in", and the channel connection lives
in the daemon's process.

**A 200 from the daemon is not proof the route exists.** The SPA fallback
answers any unknown path with `index.html` and a 200, so a CLI that read a
missing body as an empty result printed *"no chats waiting"* at a daemon that
had never heard of pairing — an operator would leave a real request sitting
unapproved. `send` therefore fails on a success status whose body is not JSON
and names the likely cause (daemon older than the CLI). Found by running the
command against the developer's live daemon, not by any test.

## The silent-send trap (fixed 2026-09-09)

A live install paired correctly — row created, code minted, challenge persisted
to `group_messages` — and **the user received nothing, with nothing in the log**.
The pairing code was not at fault; the reply never left the process.

`run_daemon` pushes a default `TelegramChannel` **unconditionally**
([`lib.rs`](../src/lib.rs) 3a), token or not, and then builds another adapter per
DB channel. With no `TELEGRAM_BOT_TOKEN` in the environment the first adapter is
tokenless, unconnected, and holds an empty `bots` map. `owns_jid` answered
`chat_jid.starts_with("tg:")`, so it claimed every Telegram chat — including
chats belonging to a bot it had never heard of. The reply loop
(`for c in channels { if c.owns_jid(&jid) { send; break } }`) stopped at it,
`resolve_bot` returned `None`, `send_message` returned `Err("Bot not found")` —
and the call site was `let _ = …`, so the error was discarded.

Both halves were required for the silence, and both are fixed:

- **`owns_jid` is bot-aware.** `TelegramChannel` keeps a
  `std::sync::RwLock<HashSet<u64>>` of the bot user ids it holds — a *sync* set,
  because `bots` is behind a tokio mutex and `owns_jid` is a sync trait method,
  which is why it could not consult it in the first place. An adapter holding no
  bot now owns nothing; `tg:{bot_user_id}:…` is claimed only by the adapter that
  has that bot; bot-less legacy jids (`tg:user:{id}`) are served by any adapter
  that has one.
- **The reply path reports failures.** A send error is logged with the channel
  id, and a jid no adapter claimed is logged too — the agent has already spent a
  turn producing that answer.

Rules for Claude:

- **Never `let _ =` a channel send.** This path is the only thing between a
  finished agent turn and the user; discarding its error produces a bug with no
  symptom except "the bot ignores me".
- **`owns_jid` is routing, not a platform check.** Answering "is this a
  Telegram jid" instead of "is this *my* bot's chat" is wrong the moment a
  second adapter exists, and the daemon creates one per DB channel.
- **A channel with no binding hands out un-approvable codes.** The router now
  warns at request time; `approve` still refuses, but by then the operator has
  already been staring at a dead code.

## Announcing on a cooldown, not once (fixed 2026-09-09)

The send fix above was necessary and not sufficient. A challenge that failed to
send still left a `pending` row live for its full hour, so every later message
re-served it with `is_new == false` — and the router, announcing only on
`is_new`, stayed mute. The user held no code, had no way to ask for one, and
nothing was written to the DB either (an unbound chat's inbound message is never
stored: `store_message` runs after the binding is resolved). From every angle it
looked like the message had not arrived.

`is_new` tracks "did we mint a row", never "did the user receive it", and the
send is fire-and-forget so the router cannot learn the outcome. The condition is
now `is_new || cooled_down`, with a 60-second window per chat: a chat that keeps
typing gets one reply a minute, and a code that never arrived can be asked for
again.

- **`challenge_cooled_down` is read-only and paired with `stamp_challenge`.**
  The condition short-circuits on `is_new`, so a self-stamping check would never
  run for a freshly minted code — leaving no timestamp and letting the very next
  message repeat it.
- **`None` (never announced) must answer "yes".** That covers the first message
  *and*, after a restart, a chat whose only challenge failed to send.

## Two things that made this hard to diagnose

- **`channels.connection_state` is never written.** It appears only in `SELECT`
  column lists; the schema default is `'disconnected'`, so the Channels page
  badge reads "disconnected" for a bot that is polling perfectly. It is not a
  signal — do not use it to judge whether a channel works, and do not tell a
  user to. (Still unfixed; see below.)
- **The daemon has no log file.** `main.rs` initialises
  `tracing_subscriber::fmt()`, which writes to stdout; under the desktop app
  stdout is a pipe to the parent and lands nowhere on disk. To read the log,
  stop the app's daemon and run `senclaw start` from a terminal
  (`RUST_LOG=debug` for more).

## What is still open

- **Feishu, QQ and WeChat keep their own pending-binding paths.** Only Telegram
  was changed. `complete_pending_feishu_binding` and friends still complete on
  first message.
- **No per-sender allowlist inside a bound group.** Pairing gates the chat, not
  each member.
- `TelegramBotConfig.admin_user_id` and the whole `telegram_bots` section of
  `config.json` are still written and never read (`get_telegram_bots` has no
  callers). `ADMIN_TELEGRAM_USER_ID` was removed.
- **`channels.connection_state` is a dead column** that the Channels UI renders
  as a status badge. Making it truthful means having the adapters report state
  back, which the WebSocket gateway has no handle on today.
- **The daemon logs only to stdout.** There is no file sink and no
  `SENCLAW_LOG_FILE`, so a desktop install is undiagnosable without restarting
  it in a terminal.
