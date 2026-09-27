# Social App — Next-Direction Research (tests · ops docs · CRM inbox)

> Status: **RESEARCH + Direction 3 IMPLEMENTED** (2026-07-20). Assesses three go-forward directions for the built `apps/social`; the CRM-inbox integration (Direction 3, Option A) is now built and verified end-to-end. Companion to [`social-unified-design.md`](social-unified-design.md).

## ✅ Implemented: social → CRM inbox (Direction 3, Option A pull)

Built and runtime-verified 2026-07-20:
- **Social side** — inbound is now captured & persisted: `social_inbox_poll` parses the extension reply (`messages[]` → `{external_id|thread_id, sender|sender_name|from, text}`), dedups on (platform, external_id, text), and stores each as `direction='in'` (added `sender` column + additive migration). New cursor feed `GET /api/inbox?since=<id>` (inbound-only, id-ascending) + operator reply `POST /api/inbox/reply` (routes through the autonomy gate + cadence). Verified: feed returns inbound only, excludes outbound, cursor advances.
- **CRM side** — `apps/crm/src/channels/social.rs` adapter (`poll`/`send`/`health_check`): `poll()` GETs `{base_url}/api/inbox?since={cursor}`, namespaces `external_id` as `"{platform}:{id}"` (so cross-platform thread ids don't collide as CRM conversations), advances the channel cursor by the last row id, and returns `Inbound{external_id, customer_name: sender, text}` — the existing `ingest()` pipeline (thread upsert → `resolve_customer` → `add_conv_message` → bus event → `sale::on_inbound`) does the rest. `send()` splits the namespace back and POSTs to social's reply endpoint. Wired into `CHANNEL_KINDS` + `poll_scheduler` + `send_raw` + `probe`. Config: `{ "base_url": "http://127.0.0.1:4520" }`.

**Both apps compile; social 34 tests, CRM social-adapter 2 tests, end-to-end HTTP contract verified.** To deploy: rebuild both apps; create a CRM inbox channel of kind `social`; run `social_inbox_poll` (agent/scheduled) so social captures inbound, then CRM's 15s poll pulls it into the unified inbox. Operator replies from CRM respect social's autonomy mode (draft by default → a social draft to approve).

**Not yet done:** real inbound only arrives once the extension's inbox signer (roadmap step 7) is wired — until then `social_inbox_poll` returns `not_wired`. The plumbing above is complete and tested with seeded data.

---

## Original research (all three directions)

Three candidate directions, researched below with a recommendation and a concrete design for each. All three are **fully doable offline** (no live browser session needed — unlike the blocked signer/media work).

---

## Direction 1 — Integration tests for REST/SPA

### What exists
30 unit tests, but **zero over-the-wire tests**: every test calls a function or an axum handler directly (verified — no `TcpListener`/`reqwest`/`axum::serve` in any test). That leaves a real gap: the **wiring** in `main.rs` is untested — `nest("/api", …)`, the SPA fallback, and CORS. This is exactly the bug class that bit us mid-build: after a stale binary, `GET /api/drafts` returned the **SPA index HTML instead of JSON** because the route didn't match and fell through to the fallback. A unit test can't catch that; an integration test can.

### Design (recommended)
An in-crate integration test that builds the **real** router and drives it over HTTP:

```rust
// build the same Router main.rs builds (extract a `build_app(state) -> Router`
// helper so main.rs and the test share it), bind 127.0.0.1:0, reqwest against it.
```
Assertions that pay for themselves:
- `GET /api/status` → 200 + `application/json` with the expected keys (not HTML).
- `GET /api/drafts` → JSON `{drafts:[…]}` (the exact regression: **must not** be the SPA shell).
- `GET /` and `GET /some/spa/route` → 200 + `text/html` (SPA fallback works).
- Full lifecycle over HTTP: `POST /api/mcp/message` social_connect → social_post (draft) → `GET /api/drafts` shows it → `POST /api/drafts/:id/reject` → gone.
- `POST /api/ext/callback` with a wrong secret → 401.

**Prereq refactor:** extract `main.rs`'s router assembly into `fn build_app(state) -> Router` so the test and the binary use the identical wiring (otherwise the test tests a copy).

**Effort:** low (~1 test file + a small refactor). **Value:** high — locks the route/fallback wiring that unit tests structurally cannot reach. **Offline:** yes.

---

## Direction 2 — End-user operations doc (runbook)

### What exists
Skills/persona teach the *agent*; there is no doc for a *human operator*. The static UI hints at setup but there's no single runbook.

### Design
`docs/social-operations.md` (or `apps/social/README.md`) covering:
1. **What it is / is not** — hybrid official-API + session-riding; the honest risk statement; what's ToS-clean vs not.
2. **Install** — load `apps/social/extension` unpacked in Chrome; run the app; confirm `social_ext_status` shows `connected` + `hosts_ready`.
3. **Connect accounts** — the per-platform `official_config` key table (already in the `social-manage` skill: fb `page_id+access_token`, x `access_token`, threads `threads_user_id+access_token`, ig `ig_user_id+access_token`, tiktok `access_token`).
4. **Autonomy modes** — observe/draft/live; the draft→approve→reject flow; why draft is the default.
5. **Monitoring** — the status summary, session history, and API-audit views (UI + `social_post_log`/`social_action_log`/`social_sessions`).
6. **Troubleshooting** — "extension chưa kết nối", quota `blocked`, a draft stuck pending with an error detail, signer-not-wired responses.
7. **Compliance/limits** — daily caps, reactive-DM-only, no mass send.

**Effort:** low. **Value:** medium-high (this is what a real operator needs). **Offline:** yes.

---

## Direction 3 — Social → CRM inbox integration

The ambitious one. Researched against CRM's actual architecture.

### The two apps are complementary, not redundant
- **CRM** (`apps/crm`, port 4390, `crm-mcp`) has a **poll-based** multi-channel inbox. Its `facebook`/`zalo`/`telegram` `poll()` adapters are **real** (Graph Page conversations, Zalo OA, Telegram Bot API); `tiktok` is an inert scaffold. So CRM reaches the **official** surfaces: FB **Page** inbox, Zalo **OA**, Telegram bot.
- **Social** reaches what those official APIs **cannot**: personal FB Messenger, IG DM, TikTok DM, X DM — via session-riding.

So social feeding CRM = **one unified customer inbox** spanning official + session-riding capture. That's the real value.

### Blocker A (CRM side): CRM has no inbound-injection seam
Confirmed by grep across `api.rs`/`api_inbox.rs`/`mcp.rs`/`mcp_ext.rs`: **no webhook, no `/ingest` route, no MCP ingest tool.** Every inbound `conv_messages` row is written **only** by `ChannelManager::ingest(ch, Vec<Inbound>)` — a *private* method fed *only* by the poll loops. The write-facing REST (`POST /conversations`, `/conversations/:id/send`) only ever writes **outbound** operator messages and tries to push them out the live channel. An external process cannot inject an inbound message today.

The reusable machinery downstream of `ingest()` is exactly what we'd want, though:
`get_or_create_conversation` (upsert thread, `UNIQUE(channel_kind, external_id)`) → `resolve_customer` (link to a contact via `customer_channels`, or `customer_id=0` if unknown) → `add_conv_message(direction='inbound')` → `emit("message")` on the bus → `sale::on_inbound` (sales handoff). `Inbound` is tiny: `{ external_id, customer_name, text }`.

### Blocker B (social side): social doesn't capture/persist inbound
`social_inbox_poll` returns the extension's **raw** result and persists nothing; only `send_dm` writes an `inbox` row (direction `out`). So social's `inbox` table holds only outbound today. Before it can feed anyone, social must **parse the extension inbox result into structured messages and persist them** (direction `in`, with `external_id` + sender name).

### Two integration architectures

**Option A — Pull (CRM polls social). RECOMMENDED.**
Fits CRM's poll-only design; no new push/webhook attack surface on CRM.
1. *Social*: capture+persist inbound (Blocker B), expose `GET /api/inbox?since=<cursor>` returning new inbound messages `{external_id, sender_name, text, created_at}`.
2. *CRM*: add channel `kind = "social"` to `CHANNEL_KINDS` (`db.rs:757`) and a `channels/social.rs` `poll()` adapter that GETs social's feed (base URL + cursor in the channel `config` JSON), maps rows to `Inbound{external_id, customer_name: sender_name, text}`, and returns them — the existing `ingest()` does the rest. `send()` maps back to social's `social_send_dm` for operator replies.
- **Touch:** ~1 new CRM adapter + 1 enum entry + social inbound-capture. CRM's cursor/`last_sync_at` handles dedup. Downstream (threads, identity, sales) reused as-is.

**Option B — Push (social posts to CRM).**
1. *CRM*: new seam — `POST /api/inbox/channels/:id/ingest` taking `{external_id, customer_name, text}`, constructing an `Inbound` and calling the existing pipeline.
2. *Social*: on capture, POST to CRM (CRM base URL + channel id in social settings).
- More real-time, but adds an unauthenticated-ish ingest endpoint to CRM (needs a shared secret) and a new push path CRM's design deliberately avoids.

**Recommendation: Option A (pull).** It respects CRM's existing poll architecture, adds the least new surface, and reuses CRM's dedup cursor. Both options share the same social-side prerequisite (Blocker B), so **capturing+persisting inbound in social is step 1 regardless.**

### Sequenced plan for Direction 3
1. Social: structured inbound capture + persist (parse `inbox_poll` result → `inbox` rows, direction `in`).  *(also improves social standalone — the inbox UI/`social_inbox_list` becomes real)*
2. Social: `GET /api/inbox?since=` cursor feed.
3. CRM: `"social"` channel kind + `channels/social.rs` poll/send adapter.
4. Wire operator replies (CRM `send` → social `social_send_dm`, which already goes through the autonomy gate + cadence).

**Effort:** medium (spans two apps). **Value:** high (unified inbox). **Offline:** yes for steps 1-3; step 4's real send still needs the extension live, but the plumbing is testable with the fake-extension harness.

---

## Overall recommendation (sequencing)

1. **Direction 1 (integration tests)** — do first. Cheap, offline, and it protects every change after it (including the CRM work). The `build_app` refactor also makes the app easier to test forever.
2. **Direction 2 (ops doc)** — do next. Cheap, offline, unblocks a human actually running it.
3. **Direction 3 (CRM inbox)** — the strategic one, but it's multi-app and its own step 1 (social inbound capture) is a real feature. Design is settled (Option A pull); schedule it deliberately.

A natural first increment that serves both #3 and social standalone: **make social actually capture and persist inbound messages** (Blocker B). That's valuable on its own (social's inbox becomes real) and is the prerequisite for any CRM integration.
