# Social App — Unified Design (Facebook · X · Threads · Instagram · TikTok)

> Status: **AUTHORITATIVE — consolidates all prior social research/design** (2026-07-20).
> This document unifies two divergent efforts into one development plan:
> - [`social-app-extension-design.md`](social-app-extension-design.md) — the **BUILT** `apps/social` (port 4520, token-capture MV3 extension, self-contained Rust). ✅ compiles, 18 tests, real FB/X/Threads posting.
> - [`social-extension-multiplatform.md`](social-extension-multiplatform.md) — the **richer research** (9 platforms, 2-tier model, PlatformAdapter, autonomy gate, per-platform token/CSRF matrices).
>
> Both are now **superseded by this file** for the 5 platforms in scope. Their per-platform research is folded in below; where they disagreed on architecture, the decisions here win.
> Related still-live: [`youtube-app-research.md`](youtube-app-research.md) (separate `apps/youtube`, kept), [`shopee-app-research.md`](shopee-app-research.md) (official-only philosophy), [`mini-browser-app-design.md`](mini-browser-app-design.md) (stealth), [`apps/video-flow`](../apps/video-flow) (extbridge pattern).

## Scope

Five platforms, chosen because they cluster as **session-heavy, Meta+X+TikTok**: **Facebook, X (Twitter), Threads, Instagram, TikTok**. (Reddit/LinkedIn/Zalo from the 9-platform research are out of this unification.)

**YouTube** is a deliberate sixth: deep YouTube work lives in the standalone `apps/youtube`, but `Platform::Youtube` is accepted here, so it gets a light adapter too. Half-wiring it is exactly the drift the parity test now forbids — a platform is either fully present on **both** sides (Rust `Platform` + `extension/adapters/<id>.js`) or not at all.

---

## 1. The one architectural decision (how the two designs reconcile)

The two efforts diverged on five axes. Unified verdicts:

| Axis | Built (`apps/social` 4520) | Research (`4510`) | **Unified decision** |
|---|---|---|---|
| Backend base | self-contained (video-cloner-style) | moltbook connector | **Keep self-contained** — it already builds & tests. Don't rebase to moltbook. |
| Extension | bundled MV3 `apps/social/extension/` + `extbridge` :9224 | extend `senclaw-extension-chrome` (WXT) + core `BrowserBridge` | **Keep the bundled MV3 + extbridge** — self-contained, no coupling to core protocol. **Adopt the research's `PlatformAdapter` registry** *inside* it. |
| Write governance | `cadence.rs` (min-gap + daily cap + jitter) | autonomy gate draft→approve→live | **Both** — cadence stays; **add the draft→approve→live gate** on top for every write. |
| Two-tier model | implicit (official stubs + extension ops) | explicit | **Make it explicit** — every platform is an adapter with an official tier and a session tier. |
| Ports | HTTP 4520, ext-WS 9224 | HTTP 4510 | **4520 / 9224** (already running; 4510 was never built). |

**Net:** the BUILT app is the foundation; this design grafts the research's three best ideas onto it — explicit two-tier adapters, a PlatformAdapter registry in the extension, and a draft→approve→live autonomy gate.

---

## 2. Unified capability matrix (the 5 platforms)

✅ official API (personal acct) · 🟡 Business/Page/Creator or gated · 🌐 web-session only · ❌ none.

| Platform | Post | DM | Search | Browse others' feed | Main blocker | Ban risk |
|---|---|---|---|---|---|---|
| **X** | ✅ API (paid) · 🌐 | 🟡 API murky · 🌐 | ✅ 7-day · 🌐 deep | ✅ cheap read · 🌐 | `ct0`/csrf, rotating GraphQL ids | Medium |
| **Facebook** | ❌ profile · 🟡 Page · 🌐 | 🟡 Page Messenger (24h) · 🌐 personal | 🌐 | 🌐 (Groups API killed 2024) | `fb_dtsg` + `doc_id` | High |
| **Threads** | ✅ **API** (~250/24h, replies) | ❌ (no DM) | ✅ **keyword search API** | 🟡 read-own · 🌐 broad | write via API/mobile; shares IG auth | High (shared IG identity) |
| **Instagram** | 🟡 publish (Business, ~50/24h) · 🌐 | 🟡 24h Business window · 🌐 | ❌ · 🌐 | 🌐 | `X-IG-App-ID`, `X-IG-WWW-Claim` | **Very high** |
| **TikTok** | 🟡 Direct Post (audited) / Upload-inbox · 🌐 | ❌ (no API) · 🌐 | ❌ (Research=academic) · 🌐 | 🌐 | `X-Bogus`/`X-Gnarly`/`msToken` (page-signed) | High |

**Design consequence (unified):** posting on **X and Threads is genuinely official-API-clean**; **Facebook Page** posting is official; everything else (personal FB, IG feed/DM, all TikTok read/DM, deep search) is **session-tier only**. So the app leans official for X/Threads/FB-Page and session for the rest — never the reverse.

---

## 3. Unified per-platform auth capture (session tier)

HttpOnly cookies (`sessionid`, `c_user/xs`, `auth_token`) are **not** readable via `document.cookie` → captured in the background via `chrome.cookies.get` (needs `cookies` permission + host). In-page CSRF/sign tokens are scraped by the content/MAIN-world script or sniffed off network requests. **Tokens never leave the machine** — the extension holds them and replays; the app only learns "session present".

| Platform | Session cookie (HttpOnly) | CSRF / sign token | Internal endpoint |
|---|---|---|---|
| X | `auth_token` | `ct0` (=`x-csrf-token`), hardcoded bearer in JS | `x.com/i/api/graphql/*` |
| Facebook | `c_user`, `xs`, `datr` | `fb_dtsg`, `jazoest`, `lsd` | `facebook.com/api/graphql/` (`doc_id`) |
| Instagram | `sessionid`, `ds_user_id` | `csrftoken`→`X-CSRFToken`, `X-IG-App-ID: 936619743392459`, `X-IG-WWW-Claim` | `i.instagram.com/api/v1/`, `www.instagram.com/graphql/query` |
| Threads | `sessionid` (shared with IG) | `X-IG-App-ID: 238260118697367` | web read-only; write via official API |
| TikTok | `sessionid` | `tt_csrf_token`, **`msToken`**, signed **`X-Bogus`/`X-Gnarly`** (webmssdk VM) | `tiktok.com/api/*` |

Numbers to **verify live** before relying on them: TikTok `webmssdk`/`X-Gnarly` version (rotates — never hardcode the signer, let the page sign), IG ~50 post/24h + action thresholds, Threads ~250/24h, X DM rate-limits.

---

## 4. Extension: PlatformAdapter registry (grafted onto the built MV3)

The built `background.js` dispatches `ReplayApi` per-op. **Unify it into a registry** so per-site logic isn't one growing switch:

```
apps/social/extension/
  background.js          # WS + registry router: pick adapter by tab host, call capability
  content.js             # relay + DOM hooks
  injected.js            # MAIN-world signer (page self-signs TikTok; reads Meta fb_dtsg)
  adapters/
    base.js              # { id, matches(host), capabilities(), captureAuth(), post(), dm(), search(), browse() }
    x.js  facebook.js  instagram.js  threads.js  tiktok.js
```

Each adapter chooses one of **two execution strategies** per capability (from the matrix §2):
- **Replay internal request** — use captured credentials, call the internal endpoint directly. Fast, but must match headers/CSRF/signature. **Let the page self-sign** where a VM exists (TikTok) by injecting into the page world rather than reimplementing `X-Bogus`.
- **Drive the DOM** — type into the composer, click Post via the existing action executor. Slower but most natural; the fallback when signing is unavailable.

The Rust `web_ops::run` already carries `{platform, op}` to the extension — the registry keys off `platform`. No Rust protocol change needed (the built extbridge stays).

---

## 5. Backend: two explicit tiers + autonomy gate

`apps/social` (Rust, axum :4520, ext-WS :9224) — **unchanged foundation**, with two additions:

1. **Explicit official tier** — `channels/<platform>.rs` already are the official-API modules. **Fully wired today: Facebook Page `/{page_id}/feed`, X `/2/tweets`, Threads 2-step publish.** TikTok/Instagram stay stubs (need a media-upload pipeline). All go through `official_post()`.
2. **Session tier** — `web_ops::run` → cadence → `extbridge` → extension adapter. Used for search/feed/groups(FB)/DM.
3. **Write governance (unify cadence + autonomy gate):** every `post`/`dm` becomes a **draft** first (moltbook-style observe/draft/live), then a human approves, then it goes live — *and* the live action still passes the `cadence` governor (min-gap + daily cap). Draft→approve→live is both a safety control and a human-like pacer.

Audit is already real: every post → `post_log` (`social_post_log` / `GET /api/logs`), DM out → `inbox`. Keep that; add a `drafts` table for the gate.

MCP tools (current 12) stay `social_<verb>` with a `platform` arg — simpler than the research's `social_<platform>_<verb>` fan-out and already shipped. Threads is now a valid `platform` value.

---

## 6. Anti-block principles (unified, apply to all 5)

From `apps/mini-browser/src/stealth.rs` — *don't spoof; use the real identity, fix only what's actually wrong.*

1. **Top priority: drive the user's real Chrome via the extension** — home IP, trusted fingerprint, valid self-rotating cookies. A server/headless-on-VPS is the fastest-flagged combo.
2. If it must run with Chrome closed → CDP mini-browser style (keep `Sec-CH-UA`, `navigator.webdriver=false`, site-isolation, human-like input).
3. **Human cadence** — randomized delays (seconds→minutes, never fixed), daily caps under folklore thresholds, ramp new accounts, respect active hours, no bursts, back off immediately on `challenge_required`/checkpoint, never parallelize actions on one account. (This is what `cadence.rs` enforces centrally.)
4. **Read a lot, write a little** — posting your own content at human pace is the lowest-risk zone.
5. **TikTok specifically** — let the page self-sign (inject into page world); never reimplement `X-Bogus` (rotates); avoid a shared sign-server (one detection burns the whole library).
6. **Meta (FB/IG/Threads)** — a Threads write shares the IG identity; a flag on one can cascade. Keep DM reactive-only.

**No promise of non-detection.** The app *reduces* risk; it cannot guarantee "won't get blocked."

---

## 7. Compliance boundary (read carefully)

- **Official tier** (X post, Threads API, FB/IG Page publish) is **ToS-clean** for your own account.
- **Session-riding tier** (personal DM, deep search/browse, personal profile/group posting) **violates each platform's ToS** even on your own account — it's *logged-in* automated interaction. Realistic downside: throttle → checkpoint → **real account lock**. The user is betting their personal account.
- **Mandatory**: draft→approve→live for every write; conservative cadence; read-heavy/write-light; surface the risk to the user plainly. **Refused**: mass-DM, fake engagement, bulk scraping — the exact behaviors platforms target hardest.

---

## 8. Unified roadmap (what's done ✅ / next ⏳)

| # | Step | State |
|---|---|---|
| 0 | Scaffold `apps/social` (self-contained Rust, MCP, extbridge, cadence) | ✅ built, 18 tests |
| 1 | Official tier: **X, Threads, FB Page** posting (reqwest) | ✅ wired |
| 2 | Audit: `post_log` + `inbox` + REST/MCP readback | ✅ done |
| 3 | Extension: bundled MV3 (token capture + ReplayApi + heartbeat) | ✅ built |
| 4 | End-to-end extension round-trip proof | ✅ unit test (fake-ext harness) |
| 5 | **PlatformAdapter registry** in the extension (`adapters/base.js` + x/facebook/instagram/threads/tiktok/youtube; `importScripts`; per-capability strategy official/replay/page-sign/dom/none; heartbeat carries the capability map). Rust↔extension parity is **enforced by a test** (`every_platform_has_an_extension_adapter`) after a real drift shipped once. | ✅ done, JS syntax-checked |
| 6 | **Autonomy gate draft→approve→live** (`gate.rs` + `drafts` table + `social_autonomy/drafts/approve/reject` MCP + REST + UI) | ✅ done, verified (21 tests + browser) |
| 7 | Per-platform signers in `injected.js` (TikTok page-self-sign first, then Meta) + concrete signed `ReplayApi` endpoints | ⏳ **needs live browser session — cannot be done offline** |
| 8 | Media-upload posting for TikTok/IG | ⏳ needs media pipeline |

**Order:** validate the framework on the **official-clean** platforms first (X → Threads → FB Page ✅), then the adapter registry (step 5) — the autonomy gate (step 6) is now in place — then the high-churn signers **on a real logged-in session** (start TikTok, page-self-signed).

### Per-platform capability enforcement — as built

An audit found the app treated all platforms identically: asking to DM on **Threads/TikTok/YouTube** (which have *no DM at all*) returned "Extension chưa kết nối", sending the user to install an extension that could never help. The capability matrix existed only in the extension adapters; Rust ignored it.

Now `Platform::capability(cap) -> Capability{Official|Replay|PageSign|Dom|None}` is **authoritative in Rust** and enforced:
- `web_ops::run` refuses a `None` capability up front (before the extension check *and* before spending cadence), with the platform's own reason.
- `gate::submit` refuses a DM draft for a no-DM platform.
- `social_search` **routes by strategy**: Threads/YouTube go to their real official search API (`channels::official_search` — Threads `keyword_search`, YouTube `search.list`), everything else to the extension.
- The full matrix is published in `social_status.capabilities` so an agent never asks for something a platform lacks.

A test (`rust_capability_table_matches_the_extension_adapters`) parses each `adapters/*.js` `capabilities` block and asserts it equals the Rust table, so the two can never drift again.

### Autonomy gate — as built (step 6)

`app_settings.autonomy` ∈ `observe|draft|live` (default **draft**). `gate::submit()` routes every `social_post`/`social_send_dm`: observe → refused; draft → row in `drafts` (pending) returning `draft_id`; live → `gate::execute_write()` immediately (post→official API, reply→extension DM), both still through `cadence`. `social_approve` runs `execute_write` for a pending draft and, on failure (e.g. missing token), **leaves it pending with the error in `detail`** so it can be fixed and retried. `social_reject` marks it rejected. Mirrored REST: `GET /api/drafts`, `POST /api/drafts/:id/{approve,reject}`; the static UI shows the mode selector + a pending-drafts table with Duyệt/Bỏ buttons.
