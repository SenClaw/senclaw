# SenClaw Desktop — React/Tauri → Flutter Migration Design

**Status:** Phase 1 scaffold landed (`desktop_app/`)
**Decision date:** 2026-06-27
**Owner:** norton

This document is the design + plan for replacing the React/Tauri macOS UI with a
fresh, multi-platform Flutter app. It reflects four decisions made at kickoff:

| Decision | Choice |
|---|---|
| Codebase | **New project** (`desktop_app/`), not an extension of `channel_app/` (mobile). Targets **web + macOS + Windows + Linux** (no iOS/Android). |
| Transport | **Direct HTTP/WS to `127.0.0.1`** — no relay hub, no encryption envelope. |
| UI | **Full redesign** — new "Aurora" design language, not a port of the antd/purple web look. |
| Output | **Design doc + working scaffold** delivered together. |

---

## 1. Why this shape

The current macOS app is a **Tauri shell that loads the React `web/` bundle** from
`http://127.0.0.1:18788`. The frontend already talks to the daemon purely over
**HTTP `/api/*` + WebSocket** (`ws://127.0.0.1:18789`) — the only Tauri-native
dependency is a single folder-picker dialog (`NewChatScreen.tsx`). That means:

- A Flutter desktop app on the **same machine** can hit the **same local API
  directly**. No need for the encrypted relay tunnel the mobile `channel_app`
  uses (relay exists only because phones can't reach `127.0.0.1`).
- Static file serving, SPA fallback, `?app=1/?embed=1` query flags, and CORS all
  become irrelevant — the Flutter app **is** the client, not served by the daemon.

So the desktop transport is strictly simpler than mobile's. We keep a clean
`ApiClient` seam so a future mobile target could swap in a relay-backed impl and
reuse every feature unchanged.

### Current surface being replaced (`web/`, ~33k LOC, 110 files)

| Area | Complexity | Notes |
|---|---|---|
| Chat | High | streaming deltas, tool cards, permission/question/plan flows, attachments, voice, workbench dock |
| Settings | **Very high** | 12 subsections (LLM, channels, agents, embedding, local models, Whisper, TTS, OCR, cognitive, behavior, permissions, tool rules) |
| Cowork | Very high | teams, members, triggers, Kanban board, templates |
| Cognitive | Very high | graph search (6 modes), recall, maintenance, node/edge viz |
| Space | High | notes, calendar, schedules, embedded apps (iframes) |
| Wiki | Medium | git-backed tree, search, history, markdown+KaTeX |
| Plugins | Low–Med | skills, MCP servers, marketplace |
| Dashboard | Low | stat cards |

---

## 2. Target architecture

```
desktop_app/
  lib/
    main.dart                  # window_manager bootstrap + ProviderScope
    app/
      app.dart                 # MaterialApp.router, theme wiring, bootstrap
      router.dart              # go_router ShellRoute (rail stays mounted)
      shell.dart               # AppShell: icon rail + content pane
      nav.dart                 # nav section registry
    theme/
      tokens.dart              # design tokens + AppColors ThemeExtension
      app_theme.dart           # Material 3 ThemeData (light/dark)
    core/
      config/app_config.dart   # host/ports, --dart-define overrides
      transport/
        api_client.dart        # REST seam (HTTP today, relay-swappable)
        ws_client.dart         # persistent WS, reconnect, subscribe
        connection.dart        # Riverpod providers + /api/config bootstrap
    models/                    # GroupInfo, ChatMessage, … (grow per feature)
    features/
      dashboard/  chat/  cowork/  space/  wiki/  cognitive/  plugins/  settings/
    widgets/                   # shared UI (SectionScaffold, ComingSoon, …)
```

**State management:** Riverpod (`flutter_riverpod`). Each feature owns providers;
transport providers are global singletons. This replaces the single React
`AppContext` + `useWebSocket` mega-hook with composable, testable units.

**Routing:** `go_router` with one `ShellRoute` so the nav rail never unmounts
while content swaps. Deep links (`/chat/<jid>`) handled inside features.

**Persistence:** `shared_preferences` replaces `localStorage`
(theme, pinned jids, panel widths, tool rules, workbench cache).

### Transport seam (the important part)

```
Feature provider ──> ApiClient (REST)  ──> http://127.0.0.1:18788/api/*
                └──> WsClient (events) ──> ws://127.0.0.1:18789/
```

- `WsClient` mirrors the React `useWebSocket` lifecycle: `{type:connect}` →
  `auth:ok` → re-subscribe known groups on reconnect, exponential backoff (≤15s).
- `connectionBootstrapProvider` does `GET /api/config` once to discover the real
  `wsPort`/`token`, then opens the socket. Everything is overridable with
  `--dart-define=SENCLAW_HOST=… --dart-define=SENCLAW_UI_PORT=…`.
- **Single swap point:** to support mobile later, implement `ApiClient`/`WsClient`
  over the relay control-frame bridge (`channel_app`'s `RelayService`) behind the
  same interface. No feature code changes.

---

## 3. Design language — "Aurora"

A new system, defined entirely in `theme/tokens.dart` so it can be retuned in one
place. Not the old antd theme, not channel_app's purple.

- **Accent:** brand blue `#5B8DEF` → violet `#8B5CF6` gradient for AI/agent
  surfaces; cyan `#22D3EE` for tools/links.
- **Neutrals:** graphite scale with a faint blue tint. Full **light + dark**
  (`AppColors.dark` / `AppColors.light`), exposed via a `ThemeExtension` and read
  as `context.colors.surface` — no hard-coded colors in widgets.
- **Layout primitives:** 64px icon rail, 264px list pane, ≥320px right dock,
  4pt spacing grid, radius scale (6/10/14/20).
- **Chrome:** frameless desktop window (`window_manager`, hidden title bar) for a
  native feel on macOS/Windows/Linux; standard layout on web.

The redesign deliberately moves from web's tab-bar/page model to a **rail + list +
content + dock** desktop layout (think Linear/Slack/Zed), which suits the large
screen the macOS app actually runs on.

---

## 4. Feature mapping & dependency replacement

| React dependency | Used for | Flutter replacement |
|---|---|---|
| antd + @ant-design/icons | entire component kit | custom widgets on Material 3 + Aurora tokens; Material/Cupertino icons |
| react-markdown + remark/rehype + katex + highlight.js | message/wiki rendering, math, code | **`gpt_markdown`** (GFM + LaTeX + fenced code in one package) |
| react-router-dom | routing | `go_router` |
| qrcode.react | channel pairing QR | `qr_flutter` (add when Settings/Channels lands) |
| dayjs | date formatting | `intl` |
| xterm / @xterm/addon-fit | (declared but **unused** in `web/`) | skip; add `xterm` (Dart) only if a real terminal surfaces (e.g. ssh-manager) |
| `window.__TAURI__.dialog.open` | native folder picker | `file_picker` (`getDirectoryPath`) |

### WebSocket contract (already implemented in `ws_client.dart`)

Outbound: `connect`, `subscribe/unsubscribe`, `list:groups/channels/agents/bindings`,
`message`, `agent:control`, `permission:response`, `question:response`,
`agent:mode`, `plan:*`, entity CRUD, `notification:read`.

Inbound: `auth:ok`, `groups`, `history:load`, `agent:delta/reply/state`,
`tool:execution`, `permission:request/resolved`, `question:request/resolved`,
`dispatch:*`, `agent:todos/usage`, `workbench:*`, `plan:*`, `space:events:changed`,
`cowork:*`, `notification`.

Full REST inventory (chat/space/wiki/cowork/cognitive/llm-config/workspace/
plugins/workbench/…) is in the daemon at `src/gateway/ui_server/` — every handler
is reused as-is; the Flutter client just calls it directly.

---

## 5. Phased plan

**Phase 1 — Foundation ✅ (this scaffold)**
- New multi-platform project; Aurora design system (tokens + light/dark theme).
- Transport: `ApiClient` (REST) + `WsClient` (reconnecting WS) + config bootstrap.
- App shell: icon rail, go_router, live connection indicator.
- Dashboard (real connection/group stats) + Chat list pane wired to live
  `groups` WS event. All 8 sections navigable (rest are structured stubs).

**Phase 2 — Chat (flagship) — ✅ functionally complete**
Done & verified end-to-end against a live daemon (web build, 8 real groups
loaded over WS, no console errors):
- `conversationProvider` (autoDispose family by jid): folds `history:load`,
  `agent:delta`→`agent:reply` streaming, `agent:state`, `tool:execution`,
  `permission:request/resolved`, `question:request/resolved`, `incoming`.
- Conversation pane: header with busy spinner + Stop, scrolling message list,
  composer (Enter to send). Message bubbles render Markdown via `gpt_markdown`;
  tool cards (ok/err badge, name, title, summary); permission cards with option
  buttons that POST `permission:response` and resolve optimistically.
Added (wired to matched WS shapes, analyze-clean, app runs against the live
daemon):
- Interactive **question cards** (`widgets/question_card.dart`) — single/multi
  select + "Other" free-text → `question:response {answers, otherTexts}`.
- Global **plan-exit dialog** (`plan_provider.dart` + `widgets/plan_exit_dialog`,
  mounted via `MaterialApp.builder`) — plan markdown + start/clear/cancel →
  `plan:exit:response`.
- **Agent mode** toggle (Agent/Plan/Dag) in the conversation header →
  `agent:mode`, reflects `agent:mode:changed`.
- **Attachments** in the composer (`file_picker` → base64 data URLs) →
  `message {attachments:[{dataUrl,mimeType}]}`.

Sidebar migration (ported from React `Sidebar.tsx` / `SessionList`, verified
rendering against the live daemon):
- `session_list.dart` — New Chat + reload, **Pinned** section, organize
  (project / project-recent / chronological / flat) + sort (updated/created),
  **collapsible folder/date buckets** with counts, per-item context menu
  (pin / rename → `update:group` / copy ID / delete → `unregister:group`),
  active-state pulse dots. State persisted via `shared_preferences`
  (`prefs.dart`, same `senclaw:*` keys as the web localStorage).
- `new_chat_dialog.dart` — first message + agent picker (`list:agents`) + model
  picker (`/api/llm-config`) + code toggle & workspace folder picker
  (`file_picker`) → `register:group` + select + send first message.
- `notifications.dart` — bell + unread badge + popover, fed by `notification` /
  `space:event:*` WS events (verified: 10 live notifications received).
- `agents_provider`/`agent_states_provider` + `groups_provider` CRUD &
  incremental `group:registered/updated/unregistered` handling; selection lifted
  to `selectedJidProvider`.

Right dock (`features/dock/`) — toggled from the conversation header:
- **Agent Console** — `dispatch:update` parents/tasks + `dispatch:activity`
  sub-agent log.
- **Workbench** — `workbench:new` artifacts; file list + content via
  `GET /api/workbench/:jid/:id/read-file`; URL shown for web/backend modes.

**Phase 2 is functionally complete.** Remaining polish (later): in-app webview
for web/backend workbench artifacts + embedded Space apps; richer tool-card
detail expansion; voice input.

**Phase 3 — Cowork + Space — ✅ core done (verified rendering vs live daemon)**
- Cowork (`features/cowork/`): teams list (cards: name / manager / member role
  chips) → team detail with **Kanban board** (backlog/todo/in_progress/review/
  done/blocked) from `/api/cowork/teams/:id/tasks`. Verified: Research Bureau +
  Research squad with real members.
- Space (`features/space/`): tabbed Notes / Calendar / Schedules. Notes =
  master-detail (search + create/edit/delete, markdown viewer, robust tag parse
  for JSON-string-or-array). Calendar needs the `from,to` epoch window. Verified:
  real notes with tags rendered.
- Remaining (later): Cowork team create/from-template + member editor + messages/
  board/files; Space embedded apps (need desktop webview) + event create UI.

**Phase 4 — Wiki + Cognitive + Plugins — ✅ core done (verified vs live daemon)**
- Wiki (`features/wiki/`): recursive tree (`/api/wiki/tree`) + markdown file viewer
  (`/api/wiki/file?path=`, frontmatter stripped). Verified: real tree
  (knowledge/memories/reports/wiki/README.md).
- Cognitive (`features/cognitive/`): stats header (nodes/edges/by-kind), node
  list = top-nodes or semantic search (`POST /api/cognitive/search`), node
  summary detail. Verified: 192 nodes / 180 chunk.
- Plugins (`features/plugins/`): tabbed Skills (toggle enable/disable via
  `/api/skills/:name/:action`) + MCP servers (`/api/mcp-servers`, tool counts,
  built-in badge). Verified: agent-browser/ast-grep/clawhub/deepwiki-* skills.
- macOS title-bar fix: a 28px draggable strip at the top of the window so the
  traffic-light buttons no longer overlap the logo/rail (`app/shell.dart`).

**Phase 5 — Settings — ✅ core done (verified vs live daemon)**
- `features/settings/` — section-nav shell (General / LLM Models / Local Models /
  Embedding / Memory). General = admin-permissions + agent-behavior toggles
  (POST writes the full object, optimistic invalidate). LLM = list configs, set
  active (`/api/llm-config/active`), delete. Local Models = list +
  download/load/unload. Embedding = provider/model read. Memory = cognitive-config
  toggles (enabled / autoReflection). Verified: General toggles match live config
  exactly.
- Remaining (later): Channels + Agent-profile editors (WS CRUD), Whisper/TTS/OCR
  download managers (same pattern as Local Models), Tool-rules editor, add/test
  LLM endpoint form, theme toggle, native menu bar.

---

## Migration status (all 9 feature areas)

| Area | State |
|---|---|
| Tauri shell replaced + deleted | ✅ (supervisor + tray + diagnostics, un-sandboxed) |
| Chat (stream/tool/permission/question/plan/mode/attach/reasoning) | ✅ |
| Sidebar (SessionList: pin/organize/buckets/menu) + notifications | ✅ |
| Dashboard | ✅ |
| Cowork (teams + Kanban) | ✅ core |
| Space (notes/calendar/schedules) | ✅ core |
| Wiki (tree + viewer) | ✅ core |
| Cognitive (stats + nodes + search) | ✅ core |
| Plugins (skills + MCP) | ✅ core |
| Settings (general/LLM/local/embedding/memory) | ✅ core |

New app icon (gradient claw squircle) applied to macOS/web/windows.
~50 Dart files; `flutter analyze` clean; web + macOS builds green.

---

## 5b. Replacing & removing the Tauri shell (done)

The old `src-tauri/` was **not** a thin webview — it embedded the daemon
(`senclaw = { path = ".." }`, `run_daemon()` in-process) and owned a tray, three
windows, hide-on-close, and a diagnostics panel. Flutter can't host Rust
in-process, so the desktop app became a **daemon supervisor**:

- `core/daemon/daemon_supervisor.dart` — resolves the `senclaw` binary
  (env → next-to-exe / `Contents/Resources` → dev `target/`), spawns it
  (`senclaw start` with `SENCLAW_BIN`/port env), streams stdout/stderr to a
  2000-line ring buffer, watches exit, restarts. **Adopts** an already-running
  daemon instead of spawning a conflicting one. No-op on web.
- `core/daemon/port_tools.dart` + `features/diagnostics/` — the old diagnostics
  window: daemon status, port health, live logs, restart, kill-port. Route
  `/diagnostics`, reachable from the nav-rail status dot and the tray.
- `app/app.dart` — `tray_manager` menu (Open / Diagnostics / Open in Browser /
  Quit) + `window_manager` hide-on-close.
- **macOS entitlements:** the App Sandbox blocks outgoing localhost, subprocess
  spawn, and `~/.senclaw` writes (`errno=1`). Disabled
  `com.apple.security.app-sandbox` in both `DebugProfile`/`Release`
  entitlements (+ network client/server) — the Tauri app was un-sandboxed too.

Removal: dropped `src-tauri` from the root `Cargo.toml` workspace members;
rewired `Makefile` (`app-build` builds the daemon + `flutter build`, then bundles
the binary into `…app/Contents/Resources/senclaw`; added
`app-build-windows/linux/web`) and `.github/workflows/desktop.yml`
(`subosito/flutter-action`, no `cargo tauri`); updated the README; then
`git rm -r src-tauri`.

**Verified:** spawn mechanism (real binary on alt ports + isolated `HOME` →
`/api/config` up ~1.5 s); macOS `.app` builds (exit 0), launches, connects with
zero sandbox errors after the entitlement fix; web build serves and connects.

## 6. Risks & open questions

- **Embedded Space apps** render as iframes today. Flutter desktop has no DOM
  iframe; needs a webview (`webview_cocoa`/`flutter_inappwebview`) or a windowed
  webview. Web target can keep iframes via `HtmlElementView`. **Decision needed in
  Phase 3.**
- **Workbench artifact renderers** (static/web/backend) similarly need a webview
  story on desktop.
- **Math/code fidelity:** `gpt_markdown` covers GFM+LaTeX+code; validate against
  real agent output early in Phase 2; fall back to `flutter_math_fork` +
  `flutter_highlight` if gaps appear.
- **Cutover:** run Flutter app alongside the Tauri app against the same daemon
  during phases 2–5; flip the default once Chat+Settings reach parity.
- **`window_manager` on web** compiles (calls guarded by `kIsWeb`) but verify the
  web build stays green in CI.

---

## 7. How to run the scaffold

```bash
cd desktop_app
flutter pub get
flutter run -d macos      # or: -d chrome / -d windows / -d linux
# point at a non-default daemon:
flutter run -d macos --dart-define=SENCLAW_UI_PORT=18788
```

The daemon must be running locally (`cargo run`) so `/api/config` + the WS gateway
are reachable; otherwise the app shows "Offline" and retries with backoff.
