> ⚠️ **SUPERSEDED for platform scope by [`social-unified-design.md`](social-unified-design.md)** (2026-07-20), which consolidates this doc and `social-extension-multiplatform.md` into one plan for Facebook/X/Threads/Instagram/TikTok. This file remains the accurate record of the **built `apps/social` implementation** (structure, ports, what compiles). Read the unified doc for the go-forward architecture.

# Social App + Shared Multi-Host Extension — Design

> Status: **Scaffold built (compiles + tests green), signers & official-API wiring pending** — 2026-07-20
> Scope decided with user: one Space App `apps/social` covering **Facebook + TikTok + X + Instagram + YouTube**, integrated via **one shared MV3 extension that captures the session token and replays platform web-APIs** (the `apps/video-flow` pattern), driven remotely by the SenClaw daemon. This **supersedes the standalone `apps/youtube` plan** in [`youtube-app-research.md`](youtube-app-research.md) — YouTube folds in here.
> Related: [`mini-browser-app-design.md`](mini-browser-app-design.md), [`senclaw-extension-design.md`](senclaw-extension-design.md), [`shopee-app-research.md`](shopee-app-research.md), [`youtube-app-research.md`](youtube-app-research.md), [`apps/video-flow`](../apps/video-flow).

## What exists now (built + runtime-verified)

`apps/social/` — workspace member, `cargo build -p social` clean, `cargo test -p social` = **15 green**, boots and serves REST+MCP.
- Backend: `main.rs` (axum **:4520** + ext-WS **:9224** — 4490/4491/9223 were taken), `config.rs`, `db.rs` (+schema.sql: accounts/inbox/post_log/action_log/settings), `state.rs`, `extbridge.rs` (ported from video-flow), `cadence.rs` (human-cadence governor: per-account min-gap + daily cap + jitter), `web_ops.rs` (extension-backed ops through cadence), `api.rs` (REST: status/settings/accounts/logs/inbox/ext), `mcp.rs` (social-mcp, **12 tools**), `channels/{mod,sign,facebook,tiktok,x,instagram,youtube}.rs`.
- **Official-API posting is REAL for Facebook (Page `/{page_id}/feed`) and X (v2 `/2/tweets`)** — a valid `official_config` posts for real; TikTok/IG/YouTube stay documented stubs (need a media-upload pipeline). Every post attempt is written to `post_log` (verify via `social_post_log` / `GET /api/logs`), so success can be checked instead of trusted.
- Extension: `apps/social/extension/` (MV3) — `manifest.json` (5 hosts), `background.js` (WS + token capture + ReplayApi + heartbeat/hosts_ready), `content.js` + `injected.js` (MAIN-world signer stubs), `README.md`.
- `senclaw-manifest.json`, `skills/{social-manage,social-engage}`, `personas/social-manager.md`, static `web/dist/index.html` status page, `scripts/pack.sh` (bundles binary+skills+personas+extension+web_dist).

**Genuinely still blocked (needs live, rotating per-platform RE — cannot be done offline):** the `injected.js` signers (TikTok msToken/X-Bogus, FB fb_dtsg, X ct0), the concrete signed `ReplayApi` endpoints per op, and the media-upload posting for TikTok/IG/YouTube. Insertion points are explicit; the generic `ReplayApi` (credentialed fetch with an explicit `url`) already works. See build order §8.

---

## 1. Goal & non-goals

**Goal.** Let a SenClaw agent operate a user's TikTok and Facebook accounts for *legitimate* CRM/CSKH and content work:
- Post content (TikTok video/photo; Facebook post).
- Read/scroll feed and search for brand/keyword monitoring.
- Read and reply to messages (TikTok Shop / Facebook Page inbox where allowed).
- Facebook: browse groups the user belongs to.

**Non-goals (explicitly out).** Mass-DM, fake engagement (like/follow farms), scraping at scale, or anything whose purpose is to evade enforcement for spam. Those are the exact behaviors both platforms target hardest; the design deliberately does not optimize for them. Rate-limits and human-cadence throttling below are **product requirements**, not optional.

---

## 2. Reality constraints that shape the design

### 2.1 Feature availability (why a hybrid is unavoidable)

| Capability | TikTok official API | Facebook official API | Extension (web session) |
|---|---|---|---|
| Post | ✅ Content Posting API, `video.publish`, ~15–25/day, 6 req/min | ✅ Pages publish (Graph) | ✅ via UI |
| DM / messages | ❌ no third-party DM; Business Messaging = TikTok Shop only, blocked in US/EU/UK | ⚠️ Page inbox only (Messenger Platform); personal DM ❌ | ⚠️ via UI, spam-flag risk |
| Search | ❌ Research API = academics only, 1–7 day latency | ❌ heavily gated | ⚠️ scrape SERP/feed |
| Browse feed/posts | ⚠️ Display API = own videos only | ⚠️ limited | ⚠️ scroll+scrape |
| Browse groups | — (TikTok has no groups) | ❌ Groups API deprecated | ⚠️ via UI |

**Consequence:** posting rides the official API where possible; everything else rides the extension. The app is a **hybrid** by necessity.

### 2.2 Anti-bot reality (the "không bị chặn" question)

There is **no guarantee of non-detection.** Design accepts this and minimizes exposure instead of promising evasion:
- TikTok scores 40+ device/behavior signals per session; every web-API call needs `msToken` + a rotating `X-Bogus`/`X-Gnarly` signature generated by obfuscated page JS (must run in the page's MAIN world). TikTok rotates the signing algorithm periodically — **the signing path will break in waves and needs maintenance.**
- Facebook uses `fb_dtsg`/`lsd`/`jazoest` tokens per session and its own behavioral detection.
- Both platforms' ToS forbid automated interaction. Realistic downside = shadowban → feature lock → account lock.

**Mitigations baked into the design (reduce, not eliminate):**
1. Ride the user's **real logged-in Chrome session** via the extension (fingerprint = genuine Chrome, the least divergent option). This is why the extension pattern beats headless CDP for this use case.
2. Never spoof gratuitously — mirror `mini-browser/src/stealth.rs`: correct only what is actually false, keep the rest genuine.
3. **Human-cadence queue** — randomized delays, respect platform rate-limits, no bursts. Enforced centrally (§5.3).
4. Prefer the official API where it exists (posting).
5. Documented per-account risk warning in the UI; recommend non-critical accounts.

---

## 3. High-level architecture

```
                 ┌───────────────────────────── SenClaw daemon (18788) ───────────┐
                 │  space_apps registry · SpaceMcp supervisor · LLM/REST bridge   │
                 └───────────────▲───────────────────────────▲────────────────────┘
                                 │ SENCLAW_BASE_URL           │ MCP autoRegister
                                 │ (llm.request, space.rest)  │ (/api/mcp/sse)
        ┌────────────────────────┴────────────────────────────┴───────────────────┐
        │                       apps/social  (axum, port 4490)                     │
        │  api.rs (REST) · mcp.rs (JSON-RPC) · db.rs (sqlite) · llm.rs             │
        │  channels/tiktok.rs  channels/facebook.rs   (official-API path, signed)  │
        │  extbridge.rs  ── WS server on 9223 (app-owned, NOT CDP) ───────┐        │
        └────────────────────────────────────────────────────────────────┼────────┘
                                                                          │ ws://127.0.0.1:9223
                    ┌─────────────────────────────────────────────────────┴──────┐
                    │        Shared MV3 extension (extension/)                     │
                    │  background.js  — WS client, command dispatch, token store   │
                    │  webRequest.onBeforeSendHeaders → capture session tokens     │
                    │  content.js     — DOM read/act on tiktok.com / facebook.com  │
                    │  injected.js    — MAIN world: read page globals, sign calls  │
                    │  hosts: *.tiktok.com, *.facebook.com                         │
                    └──────────────────────────────────────────────────────────────┘
                              (runs inside the user's own logged-in Chrome)
```

Two integration surfaces, one app:
- **Official API surface** — `channels/*.rs`, server-to-server, signed, low risk. Used for posting and (TikTok Shop / FB Page) inbox where available.
- **Extension surface** — `extbridge.rs` ⇄ extension, rides the user session, used for search/feed/groups/DM-via-UI.

---

## 4. Reused patterns (what to copy, from where)

| New piece | Copy from | Notes |
|---|---|---|
| App skeleton (`main.rs`, `api.rs`, `mcp.rs`, `db.rs`, manifest, `web/`) | `apps/mindmap` or `apps/video-cloner` | video-cloner if a job/progress-WS model is wanted |
| `extbridge.rs` (WS server + `call(method,params,timeout)` RPC, `callback_secret`, "last connection wins") | `apps/video-flow/src/extbridge.rs` | change default port 9222 → **9223** to avoid colliding with video-flow's ext-WS |
| Extension shell (`manifest.json`, `background.js`, `injected.js`, `rules.json`) | `apps/video-flow/extension/` | plain MV3 (not the WXT `senclaw-extension-chrome`). **The WXT extension does NOT capture tokens — do not use it here.** |
| Token capture (`webRequest.onBeforeSendHeaders` → stash bearer → replay) | `apps/video-flow/extension/background.js:146-170,479,689,1062` | generalize from single Google bearer to per-host token sets |
| Stealth philosophy (correct-not-spoof) | `apps/mini-browser/src/stealth.rs` | applies if a CDP fallback is later added |
| Official-API request signer (HMAC-SHA256, unit-tested) | `apps/crm/src/channels/tiktok.rs` `sign()` | already wired into a `ChannelManager` polling loop |
| MCP JSON-RPC server (manual, `/api/mcp/sse` + `/api/mcp/message`) | `apps/mini-browser/src/mcp.rs` | `initialize`/`tools/list`/`tools/call`, broadcast `mcp_tx` |
| Daemon registration | `apps/mindmap/senclaw-manifest.json` | `runtime.port`, `mcp.autoRegister`, `skills[]`, `personas[]` |

**Port map (avoid collisions):** daemon `18788`, browser-WS `18789`(+`18790`), mindmap `4350`, mini-browser `4360`, video-flow HTTP `4460`/ext-WS `9222`. → **social HTTP `4490`, ext-WS `9223`.**

---

## 5. Component design

### 5.1 Extension (shared, multi-host)

`manifest.json` permissions: `storage, alarms, tabs, webRequest, scripting, declarativeNetRequest, sidePanel`; `host_permissions: ["*://*.tiktok.com/*", "*://*.facebook.com/*", "http://127.0.0.1/*"]`. Content scripts per host; `injected.js` web-accessible MAIN-world; `rules.json` for any header rewrites (DNR).

`background.js` responsibilities:
- **WS client** to `ws://127.0.0.1:9223` with backoff reconnect + heartbeat (mirror video-flow / `ws-client.ts`).
- **Token capture** — `chrome.webRequest.onBeforeSendHeaders` with `extraHeaders`, filtered per host:
  - TikTok: `msToken` (cookie + query), `X-Bogus`/`X-Gnarly` (observe, then reproduce via `injected.js`), `sessionid` cookie, `Authorization` if present.
  - Facebook: `fb_dtsg`, `lsd`, `jazoest`, `c_user`/`xs` cookies.
  - Stash in `chrome.storage.session` keyed by host; forward a **redacted presence flag** (not the raw token) to the app for status; raw tokens stay in the extension and are used to **replay calls locally**.
- **Command dispatch** — daemon → `extbridge` → WS → `background.js`: `Navigate/Click/Type/Scroll/ExtractText/…` (reuse the video-flow command shape) plus high-level `ReplayApi{host, endpoint, params}` that fires an authenticated `fetch` from the page context.
- **MAIN-world `injected.js`** — read `window`-scoped globals (TikTok signer, FB `require('DTSGInitialData')`), compute `X-Bogus`/`X-Gnarly` for a given URL+body, return via custom events (`SIGN_REQUEST`→`SIGN_RESULT`), same channel pattern as video-flow's captcha bridge.

Security note: tokens never leave the local machine — extension holds them, app only sees presence/expiry. No token in URL/query to the app. Matches the repo's privacy posture.

### 5.2 `extbridge.rs` (app ⇄ extension)

Direct copy of `apps/video-flow/src/extbridge.rs`: axum WS `/`, `ExtBridge::call(method, params, timeout)` with `register_pending`/`complete_callback` oneshot correlation, `callback_secret` handed on connect, `POST /api/ext/callback` fallback, `is_connected()`/`stats()`. Started from `main.rs` on its own port (9223), separate from the app HTTP server. "Last connection wins."

### 5.3 App backend (`apps/social`)

- `db.rs` tables: `accounts(platform, handle, session_present, token_expiry)`, `jobs(kind, status, cadence)`, `inbox(platform, thread_id, msg, direction)`, `feed_cache`, `post_log`.
- **Cadence governor** (the mitigation, centralized): every extension action goes through a per-account queue with randomized inter-action delay and hard daily caps (posting ≤ platform limit; DM/search conservative). One choke point so limits can't be bypassed per-tool.
- `channels/tiktok.rs`, `channels/facebook.rs`: official-API path (finish the existing TikTok signer scaffold; add FB Graph for Pages).
- `llm.rs`: content drafting, message reply drafting via `SENCLAW_BASE_URL` (`llm.request`).

### 5.4 MCP tools (agent-facing)

`social_post`, `social_search`, `social_feed`, `social_send_dm`, `social_inbox_poll`, `social_groups` (FB only), `social_account_status`, `social_ext_status`. Each takes `platform: "tiktok"|"facebook"`. All mutating tools route through the cadence governor.

### 5.5 Web UI (`web/`)

React + Vite (mirror mindmap): Accounts/Connect screen (with the risk warning + "extension connected" indicator from `social_ext_status`), Composer, Inbox, Feed viewer, Settings (ports, cadence).

---

## 6. Data & auth flow (extension surface)

```
1. User installs extension, logs into TikTok/FB normally in their Chrome.
2. Extension connects ws://127.0.0.1:9223, sends heartbeat {hosts_ready, token_present}.
3. Agent calls MCP social_search{platform:"tiktok", q:"..."}.
4. app → cadence governor (delay) → extbridge.call("ReplayApi", {...}).
5. background.js builds the request; injected.js signs it (msToken+X-Bogus); fetch() runs
   in page context with the user's own cookies.
6. Response → callback → app parses → MCP result to agent.
   Raw tokens never traverse the app boundary.
```

---

## 7. Risks & open questions

- **Signing maintenance** — TikTok `X-Bogus`/`X-Gnarly` and FB `fb_dtsg` rotate; treat `injected.js` signers as high-churn. Add health checks that flag "signing broken" fast (like video-flow's `vf_status`).
- **DM legitimacy** — keep DM strictly reactive (reply to inbound), never outbound-cold, to stay defensible and reduce flag risk.
- **US/EU/UK** — TikTok Business Messaging is unavailable there; the extension DM path is the only option, at higher risk. Surface this per-account.
- **ToS** — document clearly in-app; this is a user operating their own account, not a bulk service.
- **Open:** should posting default to official API (safer, capped) or extension (uncapped, riskier)? → Recommend official API default, extension fallback.
- **Open:** one extension with both hosts vs two — recommend **one** (shared WS, per-host modules) for a single connection point.

---

## 8. Build order (when we move to code)

1. Scaffold `apps/social` from `apps/mindmap`; manifest on port 4490; empty MCP + REST + web shell; register with daemon; health-check green.
2. Port `extbridge.rs` from video-flow on 9223; stub extension that only connects + heartbeats; verify round-trip `social_ext_status`.
3. Extension token-capture for **one** host (TikTok) end-to-end: capture → `ReplayApi` → one read-only call (e.g. own profile). Prove the signer.
4. Cadence governor + `social_search`/`social_feed` (read-only).
5. Facebook host module (groups browse, page inbox).
6. Posting: finish `channels/tiktok.rs` official path + FB Graph; wire `social_post`.
7. DM (reactive only) + Inbox UI.
8. Skills + personas + polish.

Each step is independently verifiable before the next.
