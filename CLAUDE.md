# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project overview

SenClaw is a general-purpose framework for personal AI agents — multi-channel messaging gateway, agent orchestration, memory, scheduling, wiki, and Web UI. This repository is the **daemon** (`senclaw`, Rust). It links **no inference code**: every engine is a *runtime* it installs, launches as a child process and reaches over loopback HTTP (see "Runtimes" below and [docs/runtime-protocol.md](docs/runtime-protocol.md)). The agent loop is organized around the JEV v2.2 control plane (see "Control plane" below and [docs/control-plane.md](docs/control-plane.md)).

Sibling repositories (checked out next to this one, `../<name>`):

| Repo | What |
|---|---|
| `desktop` | Flutter desktop console; bundles and supervises this daemon |
| `web-app` | React web UI the daemon serves (dev fallback `../web-app/dist`) |
| `sen-mlx` | MLX LLM runtime (model mode) |
| `sen-turbo-fieldfare` | TurboFieldfare LLM runtime for `.gturbo` models (model mode, Apple Silicon) |
| `sen-sysone` | Laya / Jev typed-decision runtime |
| `sen-browser` | Browser runtime (managed Chrome + extension relay) for the browser engine v2 |
| `sen-ocr`, `sen-whisper`, `sen-tts` | OCR, speech-to-text, text-to-speech runtimes |
| upstream llama.cpp | GGUF runtime, installed by the daemon from ggml-org releases |

Two SDKs live here but are their own Cargo workspaces (other repos depend on them by path): `crates/sen-runtime-sdk` (the runtime contract in code) and `app-space-sdk` (Space Apps). The migration record is `plans/260927-0112-runtime-split-jev-migration/`.

## Build & run

```bash
cargo build                  # daemon
cargo test --workspace       # daemon tests (the SDKs are not members)
make test-sdks               # app-space-sdk + sen-runtime-sdk tests
cargo run -- start           # run the daemon (UI 18788, WS 18789 by default)
senclaw runtime list         # installed runtimes / slots (see `senclaw runtime --help`)
```

Never develop against a live install: use `HOME=<scratch dir>` (every path derives from it) plus
`SENCLAW_UI_PORT`/`SENCLAW_WS_PORT` on other ports, and pin client tools to them explicitly — `senclaw acp` takes
`--gateway ws://127.0.0.1:<ws port>` and does not read `SENCLAW_WS_PORT`.

The web UI is built in `../web-app` (`npm run build`); the daemon serves `SENCLAW_WEB_DIST`, else `./web/dist`, else
`../web-app/dist`.

The Antigravity and Gemini CLI sign-ins use Google's desktop OAuth clients, built into
`src/providers/oauth/provider.rs`. `SENCLAW_{ANTIGRAVITY,GEMINI_CLI}_OAUTH_CLIENT_{ID,SECRET}` override them — at runtime
from the daemon's environment, or at build time (release CI passes them from repository secrets when set). GitHub push
protection flags those two values; allow them there when a push is blocked, never obfuscate them to get past it.

Releases are tag-driven (`vX.Y.Z` → `.github/workflows/release.yml` publishes `senclaw-<target>` binaries, which
`scripts/install.sh`/`install.ps1` and `senclaw update` download). Bump `Cargo.toml`'s version first and refresh
`docs/openapi-daemon.yaml` with `SENCLAW_WRITE_OPENAPI=1 cargo test --lib committed_spec_matches_the_routers`.

## Architecture

### Startup sequence (daemon)

`src/lib.rs::run_daemon()` boots in this order (runtime manager: see "Runtimes"):

1. SQLite init (WAL, schema, memory tables)
2. GroupManager — load group bindings from DB + config.json
3. Channel adapters connect (Telegram → Feishu → WeChat), each graceful on failure
4. AgentPool + GroupQueue created, wired with sendReply callback
5. MessageRouter starts — routes incoming messages to AgentPool via GroupQueue (per-group FIFO)
6. TaskScheduler starts — polls for due cron/interval/once tasks
7. DispatchBridge, PersonaRegistry, VirtualWorkerPool — DAG team orchestration
8. WebSocketGateway + UIServer (axum) — serves React Web UI + WS events
9. WikiManager — git-driven knowledge base
10. Graceful shutdown on SIGINT/SIGTERM

### Key layers

- **`agent/`** — Agent lifecycle, multi-agent pool with per-group concurrency limits, permission bridging (human-in-the-loop), persona registry, DAG-based virtual worker dispatch
- **`gateway/`** — Message routing, group binding management, trigger/mention detection, command dispatch, WebSocket push events, HTTP/WS UI server
- **`channels/`** — Telegram (teloxide), Feishu/Lark (REST + WS long-connection), WeChat (iLink long-polling) adapters. QQ was removed on 2026-09-12 (never ported past a stub)
- **`mcp/`** — MCP servers exposed to agents: admin, dispatch, memory, schedule, send, virtual worker, workspace, local Wiki (git)
- **`memory/`** — FTS5 full-text search + vector similarity (sqlite-vec, not yet wired in Rust). Chunking, embedding cache, query rewrite, daily log indexing. Providers: OpenAI, OpenRouter, Ollama, local (a GGUF embedding model run by the llama.cpp runtime)
- **`scheduler/`** — Cron/interval/once task execution with five context modes: `isolated` (fresh session), `group` (shared chat context), `notify` (push-only), `script` (shell), `script-agent` (shell output fed to agent)
- **`db/`** — rusqlite wrapper (Mutex-protected connection). Tables: `groups`, `channel_messages` (FIFO), `scheduled_tasks`, `task_run_logs`, `router_state`. Memory tables in `memory::schema`
- **`wiki/`** — Git-backed knowledge base that converts agent outputs into structured, searchable entries
- **`clawhub/`** — ClawHub skill marketplace (auth, lockfile, signal protocol)
- **`skills/`** — Bundled skill definitions (bot-channels, clawhub, wiki)
- **`cli/`** — Subcommands: `skills`, `clawhub`, `wiki`, `channel`
- **`config.rs`** — Single `Config::from_env()` read at startup. All paths default under `~/.senclaw/`

### Web UI

React 18 + Vite 6 + Tailwind 3, in the sibling `web-app` repository (entry points `main.tsx` and `wiki-main.tsx`), served by the daemon's axum server from `SENCLAW_WEB_DIST` / `web/dist` / `../web-app/dist`.

## Chat attachments: images (vision, else OCR) and documents

Everything attached to a chat message — from the web composer, the desktop
picker, a paste, or a channel adapter that downloaded media — travels as
`attachments: [{dataUrl, mimeType, name?}]` and is one type end to end:
[`types::MessageAttachment`](src/types.rs). `is_image()` splits the two routes in
[`AgentPool::prepare_turn_input`](src/agent/agent_pool/pool.rs); image turns
bypass the engine's text-only mid-turn pending queue because the per-group queue
is what serializes them ([`src/lib.rs`](src/lib.rs) `enqueue_and_process`).

**Documents** ([`src/agent/documents.rs`](src/agent/documents.rs)) are saved
under `~/.senclaw/uploads/<sanitized jid>/<stamp>-<name>` and their text pulled
out (`text/*` and code by MIME *or* extension, `.docx` by unzipping
`word/document.xml`; no PDF extractor is built in). `append_document_context`
inlines up to 20k characters **and always states the saved path**, so the agent
can Read/grep the rest — or the whole file when the format is one we can't parse.
An unreadable file is reported as such, never silently dropped, and the block
tells the model not to invent contents.

**Images** go through `build_agent_input`, which resolves every source (local
path, http(s) download, `data:` URL) to base64 and returns interleaved blocks.
`split_input` then separates the text from the images, and
`AgentPool::dispatch_user_input` picks one of two routes:

- **Vision model** → the images travel as real `ContentBlock::Image` blocks,
  placed *ahead* of the prompt text in the user turn
  ([`ZenEngine::start_query`](src/zen_core/engine.rs)), and serialize as
  `image_url` data URLs (OpenAI) or `source.data` raw base64 (Anthropic).
- **Text-only model** → each image is transcribed by the built-in OCR engine and
  `append_ocr_context` folds the result into the prompt, labelled as a
  transcription. When OCR yields nothing the prompt tells the model to say so
  and **not** guess — an unanswerable "describe this image" is otherwise
  answered with an invented one.

Rules for Claude:

- **The capability check must go through
  [`ZenEngine::model_accepts_images`](src/zen_core/engine.rs).** It wraps the
  *same* `resolve_model_profile_at` the turn itself uses, including its
  fallbacks (unknown per-group override → active config → first config). A
  second lookup with its own resolution disagrees exactly on those edges and
  routes a vision model's images through OCR.
- **Never send image blocks on a maybe.** No config resolved → treat as
  vision-less. A text-only endpoint answers an image block with a hard 400 that
  fails the whole turn; OCR only degrades it.
- **[`src/zen_core/vision.rs`](src/zen_core/vision.rs) patterns are
  load-bearing, and the web copy in `LLMSettings.tsx` must match.** They were
  pinned to the model generations that existed when written, which silently
  demoted each new release to the OCR path. Generation digits are open-ended
  (`claude-[3-9]`, `gpt-[5-9]`, `gemini-[2-9]`) for that reason. The explicit
  `vision` toggle in Settings → Models always wins over inference.
- **Save the document before extracting from it.** The path is the fallback for
  every format we can't parse; an extractor that returns `Err` without a saved
  file leaves the agent with nothing to open.
- **Only Telegram downloads channel media** (photos and image-typed documents,
  in [`src/channels/telegram.rs`](src/channels/telegram.rs) `download_media`).
  Feishu/WeChat construct `attachments: Vec::new()` — adding media there means
  filling that field, not just parsing the event. A channel turn is rebuilt from
  DB history, so an adapter's attachments must reach
  `StoredMessage::attachments` or `run_agent` can never see them.
- Clients cap an image's long edge at 1568px before upload (`MAX_IMAGE_EDGE` in
  `ChatView.tsx`, `kMaxImageEdge` in
  `desktop_app/lib/features/chat/image_attachment.dart`) — a phone photo
  otherwise base64s past Anthropic's 5 MB per-image limit. Documents are capped
  at 32 MB on both ends (`MAX_DOC_BYTES`).

## Testing

- Rust: `cargo test` — unit tests co-located in `#[cfg(test)]` modules at the bottom of each source file
- The old TS code has three test files at the repo root: `test-comprehensive.ts`, `test-multi-model.ts`, `test-regression.ts`

## Code conventions

(Carried over from the TypeScript → Rust port; the TypeScript source is no longer in any repository.)
- Filenames: `snake_case.rs`. Module declarations in `mod.rs` files
- `anyhow::Result` for fallible functions, `thiserror` for library error types
- SQLite access through `Db::with_conn()` / `Db::with_conn_mut()` closures (Mutex guard)
- Config is read once via `Config::from_env()` — do not call `env::var()` directly in library code

## SenClaw MCP naming convention

All SenClaw-bundled MCP servers follow a strict three-level naming pattern. Skills, docs, and any code referencing MCP tools by name MUST use the canonical form below — never invent shortened or "stripped" variants.

### Pattern

```
mcp__senclaw-<domain>__<tool-prefix>_<verb>[_<modifier>]
└────┬─────┘└────┬───┘└────────┬──────────┘
   protocol  server name      tool name
   prefix    (kebab-case)     (snake_case)
```

1. **Server name** — `senclaw-<domain>` (kebab-case). The string passed as the first arg of `McpServerConfig::new(...)` in [`src/mcp/helper.rs`](src/mcp/helper.rs). One server per domain.
2. **Tool name** — `<tool-prefix>_<verb>[_<modifier>]` (snake_case). The Rust method name under `#[rmcp::tool]` inside `src/mcp/<domain>_server.rs`. The `<tool-prefix>` is usually the same word as `<domain>`, with a few historical exceptions (see table).
3. **Full identifier from Claude Code** — concatenate: `mcp__` + server name + `__` + tool name. This is what `ToolSearch select:...` and direct tool calls expect.

### Canonical registry

| Domain | Server name | Tool prefix | Example full tool |
|---|---|---|---|
| browser | `senclaw-browser` | `browser_` | `mcp__senclaw-browser__browser_navigate` |
| memory | `senclaw-memory` | `memory_` | `mcp__senclaw-memory__memory_search` |
| schedule | `senclaw-schedule` | `schedule_` | `mcp__senclaw-schedule__schedule_task` |
| wiki | `senclaw-wiki` | `wiki_` | `mcp__senclaw-wiki__wiki_write` |
| dispatch | `senclaw-dispatch` | `dispatch_` | `mcp__senclaw-dispatch__dispatch_task` |
| send | `senclaw-send` | `send_` | `mcp__senclaw-send__send_message` |
| workspace | `senclaw-workspace` | `workspace_` | `mcp__senclaw-workspace__workspace_*` |
| virtual | `senclaw-virtual` | `run_` / `virtual_` | `mcp__senclaw-virtual__run_persona` |
| space | `senclaw-space` | `space_` | `mcp__senclaw-space__space_schedule_activity` |
| ocr | `senclaw-ocr` | `ocr_` | `mcp__senclaw-ocr__ocr_*` |
| litho | `senclaw-litho` | `litho_` | `mcp__senclaw-litho__litho_generate` |
| js | `senclaw-js` | `js_` | `mcp__senclaw-js__js_eval` |
| **cognitive** | `senclaw-cognitive` | **`cog_`** (not `cognitive_`) | `mcp__senclaw-cognitive__cog_search` |
| **sandbox** | `senclaw-sandbox` | **`sbx_`** (not `sandbox_`) | `mcp__senclaw-sandbox__sbx_run` |
| usage | `senclaw-usage` | `usage_` | `mcp__senclaw-usage__usage_overview` |
| admin | `senclaw-admin` | (varies) | — |

Source of truth: server names live in [`src/mcp/helper.rs`](src/mcp/helper.rs) `*_mcp_config()` builders; tool names are the `#[rmcp::tool] async fn <name>` definitions in `src/mcp/*_server.rs`.

### Space-App MCP servers

Space Apps register their own MCP servers with a different pattern: `mcp__<mcp.name>__<tool>`, where `<mcp.name>` is the `mcp.name` field of the app's `senclaw-manifest.json` (usually `<app-id>-mcp`, e.g. `ssh-manager-mcp` → `mcp__ssh-manager-mcp__ssh_execute_command`, but not always — luna-calendar registers `luna-mcp`). Never derive the server name from the app id; read the manifest. Tool names live in the app's `apps/<app>/src/mcp.rs` `tools/list`. Runtime check: `GET http://127.0.0.1:18788/api/mcp-servers` lists every registered server with its status. Full lookup + troubleshooting guide (including the `groups.allowed_tools` whitelist trap that empties a session's tool roster): [docs/tool-skill-name-lookup.md](docs/tool-skill-name-lookup.md).

### Built-in MCP servers run in ONE process (`senclaw-core`)

`AgentPool` used to spawn fourteen MCP subprocesses per chat session — one per
built-in server. It now spawns a single `senclaw core-server` that hosts them
all in-process and merges their tool tables, so `wiki_*`, `workspace_*`,
`memory_*` … all arrive over one stdio connection. Each server keeps its own
subcommand (`senclaw wiki-server`, …) for debugging one in isolation, and
`mcp.bundled = false` (env `SENCLAW_MCP_BUNDLED`) restores the per-server spawn.
Adding a server means giving it `from_env() -> Result<Option<Self>>` plus
`vis = "pub"` on its `#[rmcp::tool_router]` — the aggregator never re-declares a
tool. Full design, limits, and a verified stdio transcript:
[docs/mcp-core-bundled.md](docs/mcp-core-bundled.md).

### MCP tool aliases (Plugins → Alias)

Users (and Space Apps via `mcp.toolAliases` in `senclaw-manifest.json`) can rename an
MCP tool or override it with another tool. Mapping `alias → target` lives in the
`mcp_tool_aliases` table, resolves at stage 0 of `resolve_tool_by_name`
([src/tools/tool_search.rs](src/tools/tool_search.rs)) and decorates the roster via
[src/tools/tool_alias.rs](src/tools/tool_alias.rs). App-declared aliases import
**disabled** — the user must enable them in Plugins → Alias. When a tool name doesn't
behave as documented, check this table first (`GET /api/tool-aliases`). Full guide:
[docs/mcp-tool-alias.md](docs/mcp-tool-alias.md).

### Space-App external links

Links in a Space App UI must open in the **system browser**, never navigate the embedded desktop webview. Flow (JS hook `openExternal` → webview safety net → daemon `POST /api/ui/open-url`), canonical helper (`apps/zeach/web/src/openExternal.ts`), and per-app adoption checklist: [docs/space-app-open-external.md](docs/space-app-open-external.md).

### Rules for Claude

- **Never invent a "short" tool name.** There is no `mcp__browser__*` resolver in plain Claude Code. The form `mcp__senclaw-<domain>__<prefix>_<verb>` is the only one that resolves **for an externally-registered server** — see the next bullet for what a SenClaw agent actually sees.
- **Inside a SenClaw agent the bundled names differ, and this registry is not what resolves.** With `mcp.bundled = true` (the default) one `senclaw-core` process hosts every built-in, and
  [`engine.rs`](src/zen_core/engine.rs) strips the `senclaw-` prefix from the *server* — so every built-in tool reaches the model as **`mcp__core__<tool>`**: `mcp__core__schedule_watch`, `mcp__core__dispatch_status`, `mcp__core__browser_click`. Only with `SENCLAW_MCP_BUNDLED=false` do the per-domain names in the table above appear. For a month after bundling shipped, the registry spelling named nothing on a default install: 43 of the 53 `select:` calls that loaded nothing named a tool registered under its bundled name, and an agent told to arm a watch promised a notification it had no way to send. **`resolve_across_layouts` now bridges all three spellings** (`mcp__senclaw-browser__browser_search`, `mcp__browser__search`, `mcp__core__browser_search`) for `select:` and direct calls alike — never remove that stage. The full registry name is therefore the portable one; a **bare** name is not, because an app may reuse it (`mini-browser-mcp` reuses 18 `browser_*` names, so bare `browser_navigate` resolves to nothing).
- **A skill's own tools are pre-loaded when it loads — its text must not force a `ToolSearch` first.** `apply_skill_activation` resolves every full `mcp__…` name the skill writes (through the cascade above) and every tool verb it mentions. A skill that says "always run ToolSearch before any call" costs every turn one LLM round trip (median 6.6 s) for tools already in the list: word the step as "only if they are not in your tool list", as `agent-browser` and `web-research` do.
- **Tool discovery is not a job for the decision engine.** Measured on 78 real tool-needing prompts and 36 that needed none (`plans/reports/research-260926-1254-jev-tool-discovery.md`): Laya picked the used tool family 20 times, said "none" 3 times out of 32, and could not tell tool-needing requests from the rest. What wasted discovery round trips was naming, fixed deterministically. The engine reaches discovery only through the pre-skill router's co-signed load.
- **Never substitute another MCP server** (e.g. Playwright plugin, `Claude_in_Chrome`) when a SenClaw skill references a SenClaw server. SenClaw skills assume their own server semantics, return shapes, and side effects — substituting a different browser MCP silently breaks the skill's contract.
- **Verify the server is registered before suggesting the user run it.** Check `.mcp.json` at project root and the Claude Code MCP list. If absent, the fix is to register the server in `.mcp.json` (stdio command pointing to the `senclaw` binary with the matching `<domain>-server` subcommand), not to rewrite the skill.
- **Match the registry above when writing or updating a SKILL.md.** When in doubt, run `grep -n '#\[rmcp::tool' src/mcp/<domain>_server.rs -A 2` to confirm the exact `async fn <name>` and use that verbatim.
- **Two prefix exceptions** — server `senclaw-cognitive` has tool prefix `cog_*`, and server `senclaw-sandbox` has tool prefix `sbx_*`. Do not "normalize" them to `cognitive_*` / `sandbox_*`.
- **`senclaw-sandbox` is the built-in OS-sandbox engine** (`src/sandbox`, ported from `apps/sandbox` which still exists as a standalone Space App with server `sandbox-mcp`). The built-in engine's data lives in `~/.senclaw/sandbox/`; the Space App keeps its own under `~/.senclaw/space-app-data/sandbox/`. Enforcement switches (agent Bash exec / Python / Node.js / scheduler scripts through the sandbox) live at `/api/sandbox/exec-policy` and the Plugins → Sandbox Web UI page.

### Registering a SenClaw MCP server for Claude Code

Project-level [`.mcp.json`](.mcp.json) template (one entry per server needed):

```json
{
  "mcpServers": {
    "senclaw-<domain>": {
      "type": "stdio",
      "command": "/absolute/path/to/target/release/senclaw",
      "args": ["<domain>-server"],
      "env": { "SENCLAW_WS_PORT": "18789" }
    }
  }
}
```

The `<domain>-server` subcommand list is in `src/main.rs` (e.g. `browser-server`, `memory-server`, `schedule-server`, ...). After editing `.mcp.json`, the user must restart Claude Code and approve the server in the prompt.

## Runtimes (`src/runtime/`, `src/local_models/`)

The daemon links **no inference code**. MLX, llama.cpp, the decision model
(Laya / a hosted Jev-compatible backend), OCR, Whisper and TTS are all
*runtimes*: separate programs the daemon installs from a GitHub-release
package, launches as a child process and reaches over loopback HTTP — the way
LM Studio runs its engines. Full contract:
[docs/runtime-protocol.md](docs/runtime-protocol.md) +
[crates/sen-runtime-sdk](crates/sen-runtime-sdk) (manifest types, launch
environment, the server scaffold every `sen-*` runtime mounts).

| capability | runtime | reached at |
|---|---|---|
| GGUF chat / embedding / vision | upstream llama.cpp (`llama.cpp-{metal,cpu,vulkan,cuda}`) | `local:<key>` LLM config → `/api/runtimes/models/:key/v1/*` |
| MLX chat / vision | `sen-mlx` | same, `local:<key>` |
| Gemma 4 `.gturbo` chat / vision | `sen-turbo-fieldfare` | same, `local:<key>` |
| Typed decisions | `sen-sysone` | `/api/decision/*` (proxied, control-plane routes excepted) |
| OCR | `sen-ocr` | `/api/ocr/*` (proxied) |
| Speech to text | `sen-whisper` | `/api/whisper/*` (proxied) |
| Text to speech | `sen-tts` | `/api/tts/*` (proxied) |
| Browser (observe, guarded act) | `sen-browser` | `/api/browser-agent/*` (the daemon's loop), `/api/browser/*` (GET-only proxy) |

`src/runtime/`: `store` (installed packages; install-local from a directory or
`.tar.gz`/`.zip` with traversal guards), `settings` (slot selections, update
channel, idle timeouts), `index` (the runtime catalog, fetched/cached/bundled),
`llamacpp` (resolves and installs an upstream `llama.cpp` build, **generating**
its manifest — there is none upstream), `jobs` (background installs with
progress), `supervisor` (spawn, health-gate, idle-sweep, crash accounting,
orphan cleanup at boot, stop-all at shutdown), `manager` (`RuntimeManager`, the
one seam everything else goes through), `clients` (internal typed calls: the
decision gate and skill router, OCR for text-only models), `proxy` (the
legacy-namespace reverse proxy + the local-model route), `rest`
(`/api/runtimes/*`). `src/local_models/`: what GGUF/MLX checkpoints are on
disk, their stable keys and capabilities (a minimal GGUF metadata reader —
only the key/value header, never tensor data), HF downloads, and
`/api/local-models/*`.

Rules for Claude:

- **A legacy namespace (`/api/ocr`, `/api/tts`, `/api/whisper`, and
  `/api/decision` outside the control-plane routes) is a *generic* reverse
  proxy, not a route list the daemon maintains.** `src/runtime/proxy.rs`
  forwards method + path + query + a streamed body both ways to whatever
  runtime fills that slot; adding a route to a `sen-*` runtime needs no daemon
  change. Static routes win over the wildcard (axum/matchit: more specific
  first) — that is what keeps `/api/decision/gate` etc. in the daemon while
  everything else under `/api/decision/*` proxies through.
- **No runtime installed or selected is a 503 with a machine-readable `code`**
  (`runtime_not_installed` / `runtime_not_selected` / `runtime_start_failed`),
  never a hard failure the caller has to special-case. The decision gate and
  skill router degrade to their pre-engine behaviour (gate shows the prompt,
  router falls back to keywords); OCR for a text-only model degrades to no
  transcription (the existing "OCR yielded nothing, don't guess" prompt path).
- **Never parse a decision request into `serde_json::Value`.** Unchanged from
  before the split: `crate::decision::json::Json` preserves the key order a
  `choice`'s option markers and an object state's encoded text depend on.
  `crate::decision::client::ask` builds the wire text from an `AskRequest`
  (whose `state`/`questions` are `Json`) and sends it verbatim to whichever
  process fills the `decision` slot — never through the daemon's own HTTP
  server. The *response* side may freely go through `serde_json::Value`, since
  the probabilities coming back have no order-dependent meaning.
- **The daemon owns only the tool-call gate and the pre-turn skill router of
  `decisionConfig`** (`crate::decision::settings::DecisionSettings` — just
  `gate` + `skills` now; `Backend`, `LocalSettings`, `OnlineSettings` moved
  with the engine). `backend`/`local`/`online` are `sen-sysone`'s own sub-keys
  of the same JSON object; the daemon reads/writes `decisionConfig` as raw
  `serde_json::Value` precisely so an unrelated save (LLM configs, gate
  settings, anything) round-trips those sub-keys untouched instead of
  collapsing them the moment they are not part of a typed Rust struct. The
  same rule applies to `ocrConfig`/`ttsConfig`/`whisperConfig` — kept only so
  a runtime's first start can still import them
  (`sen_runtime_sdk::legacy::load_or_import`).
- **A `local:<key>` model is merged into `load_llm_configs` the same way an
  app-provided model is** — scanned fresh off `local-models/` on every read,
  never persisted to `config.json` (`save_llm_config` refuses both `app:` and
  `local:` ids), and exempted from the "empty API key means unconfigured"
  check for the same reason: it is reached through the daemon's own loopback
  proxy, which needs no credential of its own. See "Space Apps that serve
  models" below for the shared mechanics (the single merge seam inside
  `load_llm_configs`, no total request timeout for a loopback provider) —
  `crate::local_models::is_local_config` is the same kind of check as
  `is_app_config`.
- **The supervisor's idle sweep never stops a process mid-request.** An
  in-flight call increments `RunningProcess::in_flight` before proxying and
  decrements it after; the sweep skips any process with a nonzero count
  regardless of how stale `last_used_at` looks.
- **A start is single-flight per process key** (`service:<runtime-id>` or
  `model:<model-key>`): concurrent callers for the same key serialize on that
  key's lock and re-check for an already-healthy process once they get it, so
  only the first caller actually spawns anything.
- **Orphan cleanup at boot only stops a `running.json` entry it can verify.**
  A pid can be alive under a *different* process after a reboot recycles it;
  the check is the same one `space_mcp::reclaim_app_port` uses for Space
  Apps — the process's working directory must match the package directory it
  was launched from. "Cannot verify" (Windows; no `lsof`) means "leave it
  alone", never "kill it anyway".
- **A local model's embedding dimensions are a name-based guess until the
  first real answer.** `LocalProvider` (`src/memory/embedding_providers.rs`)
  seeds `dimensions()` from the model key's slug (`large`/`base`/else, same as
  before the split), then overwrites it with the checkpoint's actual output
  length once a request succeeds — the pattern `OllamaProvider` already used.

## Browser engine v2 (`src/browser_agent/`)

The agent's browser tools (`senclaw-browser`) run on one of two engines:
`legacy` (the extension's content-script executor over the WS gateway) or `v2`
— a decision loop in the daemon (**rule → Jev → LLM → person**) driving the
`sen-browser` runtime. `browserAgent.engine` in `config.json` picks it; `auto`
(default) means v2 once `sen-browser` is installed. `browser_mcp_config` sets
`SENCLAW_BROWSER_ENGINE=v2` and exactly one of `McpBrowserServer` /
`McpBrowserAgentServer` builds. One decision request per step asks the
operation and every operation's target at once; the joint confidence
p(op) × p(target) picks act / LLM fallback / review. Design record:
`plans/260929-0143-sen-browser-runtime/design.html`.

Rules for Claude:

- **Risk tiers are decided in code (`policy::risk_tier`), never by a model.**
  A click on a purchase / send / delete control (EN and VI keywords, word
  boundaries) or a confirm dialog parks the task as `needs_approval`; typing
  into a credential or one-time-code field is `needs_user` (hand the tab
  over). `browser_do` refuses both with 403 rather than executing.
- **`browser_approve` is confirmed per call** (`PER_CALL_MCP_TOOLS` in
  `zen_core/permissions.rs`): no "never ask again", no saved grant honoured,
  and the prompt shows the pending action (`run::describe_approval`), not the
  id the agent sent. It is checked **before** the skip flags: a session that
  asks nobody (workflow steps and background runs set every skip flag, and
  so does the chat's own skip toggle) gets a refusal, and the action stays
  paused for the person — `GET /api/browser-agent/approvals`, shown in
  Settings → Browser on web and desktop.
- **A step names an element of the observation the LLM was shown.**
  `rest::SHOWN` keeps it per tab; re-observing before mapping an index
  recreates the old stale-index bug. The runtime refuses a changed page with
  409 and nothing is sent.
- **`/browser/ext` checks the Origin before the upgrade** (only
  `chrome-extension://<32 a-p>`) and then pairs: an 8-character code, approved
  with `pair approve <CODE>` in chat or the REST route; the token is stored as
  a SHA-256 only. Never add a write pass-through under `/api/browser/*` — it
  would skip the risk tiers.
- **Never parse the decision request into `serde_json::Value`** (same rule as
  the decision client): `encoder` builds `Json` so option markers keep their
  order. `encoder` tests pin jev-full and laya-v3 against the upstream Python.
- **One job, one tool call.** A step of the loop costs ~0.35 s (decision
  ~270 ms, risk check ~45 ms, action ~50 ms); a tool call costs the chat model
  a whole turn — 6–11 s measured on a real install (a 100k-character system
  prompt, 49 tools, no prompt cache). Driving a page with `browser_open` →
  `browser_do` → `browser_read` is that turn four times; `browser_task` and
  `browser_read {url}` are one. The tool descriptions and the v2 skill text
  say so — keep them saying so.
- **A skill that teaches the browser tools has a text per engine.**
  `SKILL.browser-v2.md` beside `SKILL.md` is what engine v2 reads
  (`skills::scan::with_variant`, resolved the way the tool set is). The legacy
  text names tools engine v2 does not have, which cost every browser chat a
  `ToolSearch` turn and then a page driven one step per turn.
  `the_engines_skills_name_only_its_tools` fails a v2 text that names a tool
  the v2 server does not register.
- **When a page is ready is the runtime's call** (`wait_ready` in
  `sen-browser`): the load event is late on pages that stream or track, and
  early on app shells. Never add a sleep or a second observe in the daemon to
  wait for a page — an action that starts a navigation already comes back with
  the page it brings.
- **The loop says where its time went and why it was slow.**
  `stats.timing` splits a task into open / decide / llm / risk / text / act /
  page / verify / answer; `notes` names a decision model that could not answer
  (every step is then an LLM call). The checkpoint being absent is also on
  `GET /api/browser-agent/settings` (`decisionModel.installed`) and shown in
  Settings → Browser — a missing `laya-browser` once made every step take
  seconds with nothing saying why.
- **Model calls a task will need start before it needs them**: completion
  criteria are written while the task runs (`write_criteria`), and the local
  checkpoints load while the first page opens (`warm_decision_models`) — one
  after the other, because the decision runtime can fail a checkpoint when two
  load at the same moment. An unsure DONE is checked against the page before an
  LLM is asked; the check is what accepts a DONE whoever proposes it.
- **A click on something that is already on is preceded by that same check**
  (`already_on` in `run::drive`). Asked to like a clip that is already liked,
  `laya-browser` clicks the like anyway — which takes it back. The snapshot
  reports a toggle's `aria-pressed` as `checked`, the way a checkbox's is.

## Space Apps that serve models (`llm` manifest block)

An app declaring an `llm` block becomes an LLM provider: its models appear in
the same picker as OpenAI and Anthropic, and turns route to its own
`/v1/chat/completions` over loopback.

```json
"llm": { "autoRegister": true, "path": "/v1", "adapt": "openai", "displayName": "MLX" }
```

The app speaks **OpenAI** — `GET /v1/models`, `POST /v1/chat/completions` (SSE
and JSON) — so the daemon reuses `adapt: "openai"` and needs **no new adapter**.
`app_space_sdk::llm::openai_router` renders the wire from a semantic
`LlmProvider` trait, so an app emits `Chunk::{Text, Reasoning, ToolCall, Usage}`
and never hand-writes the JSON.

Registration mirrors `mcp.autoRegister` exactly: session apps are addressed
through `/api/space/apps/<id>/proxy/v1`, and the model list is cached at
`<app>/.senclaw/llm-models.json` so a **stopped** app still has models in the
picker — without which nobody would select one, so nothing would call the app,
so it would never start.

Rules for Claude:

- **`LlmDecl::parse` returns `Result`, unlike every other parser in
  `src/apps/manifest.rs`.** The others fall back to a default because the
  failure is survivable. Here it is not: an `adapt` the daemon does not route
  means every turn gets an OpenAI body and fails upstream with an error naming
  neither the app nor the field, and `adapt: "local-mlx"` routes the turn to an
  in-process engine so the app is registered and *never called*.
- **`APP_DECLARABLE_ADAPTERS` is narrower than `ROUTED_ADAPTERS`** — `openai`
  and `anthropic` only. Do not widen it to whatever `query_llm` happens to
  dispatch.
- **App configs are never written to `config.json`.** They are rebuilt from
  `space_app_llm_providers` on every `load_llm_configs`. `save_llm_config`
  refuses an `app:` id; a frozen copy would outlive the app.
- **Merging happens inside `load_llm_configs`, not at the HTTP layer.** That
  function is the single seam the picker, `resolve_model_profile_at` and
  `model_accepts_images` all go through.
- **`REQUEST_TIMEOUT` no longer applies to a loopback endpoint.**
  `DEFAULT_MAX_NEW_TOKENS` is 8192, which at ~60 tok/s is over two minutes of
  legitimate output — a total deadline would cut it mid-sentence. Stalls are
  caught by the client's `read_timeout`, which resets on every byte. Never
  reintroduce a total timeout for a local provider.

Model in [`src/apps/llm_provider.rs`](src/apps/llm_provider.rs) and
[`src/apps/manifest.rs`](src/apps/manifest.rs) `LlmDecl`; SDK in
[`app-space-sdk/src/llm.rs`](app-space-sdk/src/llm.rs); registration in
[`src/gateway/ui_server/space_mcp.rs`](src/gateway/ui_server/space_mcp.rs)
`register_llm`. Design record:
[docs/space-app-llm-provider-sdk.md](docs/space-app-llm-provider-sdk.md).

## Gemma 4 on the native MLX path

Moved to the `sen-mlx` runtime with the rest of native MLX inference
(`docs/runtime-protocol.md`; this repo compiles no MLX at all now). The
sliding-window KV ring, sampling defaults from the checkpoint's own
`generation_config.json`, and the TurboQuant / long-context measurements live
in `sen-mlx`'s own docs.

## Zen Patterns (`src/patterns/`)

A **pattern** is a named system prompt for one text transform: text in, text
out, **one LLM call, no tools, no loop**. The on-disk format is
[Fabric](https://github.com/danielmiessler/fabric)'s (`<name>/system.md`, with
`# IDENTITY and PURPOSE` → `# STEPS` → `# OUTPUT INSTRUCTIONS` → `# INPUT`), so
its ~250-pattern library imports with no converter. Storage is
`~/.senclaw/patterns/`: `sources.json` (the ledger), `user/` (local, resolved
**first**), `sources/<id>/` (git checkouts), `strategies/` (cot/tot/reflexion —
two-field JSON appended to the system prompt).

**255 Fabric patterns + all 9 strategies are vendored into the repo** under
`assets/patterns` / `assets/strategies` (MIT, pinned at `v1.4.470`), alongside 6
SenClaw wrote for Vietnamese. `build.rs` walks them into `BUNDLED_PATTERNS` /
`BUNDLED_STRATEGIES`, so a fresh install has a working library **offline on
first launch** — `GET /api/patterns/catalog` offers it as one tap, next to git
presets for SenClaw's own repo and Fabric upstream.

Reached from chat through **one** bundled skill (`skills/pattern`) plus **four**
MCP tools on `senclaw-patterns` (`pattern_list`, `pattern_get`, `pattern_run`,
`pattern_sync`). Managed at **Plugins → Patterns** (browse, run, import zip,
add git source, sync). Installable via a Zen Kit's `patterns` /
`patternSources` blocks; the bundled **Fabric Patterns** kit
([`assets/kits/fabric.json`](assets/kits/fabric.json), served by
[`src/kits/builtin.rs`](src/kits/builtin.rs)) is one tap with no marketplace.

Rules for Claude:

- **Never turn patterns into skills.** [`src/skills/scan.rs`](src/skills/scan.rs)
  loads every skill into one registry and each contributes `triggers` to the
  pre-turn matcher; a few hundred entries drown `web-research` /
  `agent-browser` and flood the slash-command namespace. The whole design is
  N patterns behind a constant-size tool surface.
- **A pattern lands in the system-prompt position of a real LLM call**, so a
  source tracking a branch lets an upstream commit rewrite instructions the
  agent obeys. `SourceSyncOutcome::pinned` reports tag/sha vs branch and the UI
  says so; the shipped Fabric kit pins a tag, and a test enforces that.
- **`sanitize_name` is the single choke point** for names arriving from a git
  directory listing, a kit manifest and a UI field. The zip importer only ever
  writes `system.md`/`user.md` under one sanitized component.
- **The input is never sent twice.** A pattern containing `{{input}}` gets it
  interpolated and an empty user message; one without gets it as the user
  message. Appending unconditionally doubles the bill on a long transcript.
- **An unknown `{{placeholder}}` stays verbatim** (same rule as
  [`src/scaffold/`](src/scaffold/)) and comes back in `unresolved`. Blanking
  silently deletes an instruction.
- **Fabric patterns pin English output** in their own `# OUTPUT INSTRUCTIONS`.
  The language rule is appended at *render* time, after them so it wins — never
  patched into the checkout, which the next sync reverts. Pass
  `language: "auto"` for Vietnamese input.
- **The kit installer registers git sources but never clones.** Cloning is
  network I/O and happens in `kits.rs::sync_kit_pattern_sources`, the same
  split Space App installs use — it is what keeps `cargo test` offline.
- **Never hand-edit `assets/patterns/*` that came from Fabric.** Re-vendoring
  replaces the tree wholesale. Only the 6 SenClaw-authored ones are held to the
  full `# IDENTITY → # STEPS → # OUTPUT INSTRUCTIONS → # INPUT` convention by
  tests; the vendored 255 are verbatim upstream and a handful end differently.
- **The bundled table is generated, never listed.** `build.rs`
  `emit_bundled_patterns` walks `assets/patterns` + `assets/strategies`; adding
  or re-vendoring is dropping files in, with nothing to update by hand.
- **Kit patterns go to a `kit-<id>` source, never `user`.** Uninstall is then a
  directory delete that cannot take a hand-written pattern with it.
- **Pattern checkouts are shallow (`git_sync::clone_shallow`, depth 1) and a
  refresh re-clones.** Measured on Fabric: **402 s** full vs **32 s** shallow,
  identical 255 patterns. `clone_or_pull` stays full for marketplace sources —
  a shallow repo cannot be deepened and its `pull_existing` assumes history.
- **`/summarize` in the composer resolves as a pattern** once no skill claims
  the token ([`src/agent/prompt_directives.rs`](src/agent/prompt_directives.rs)).
  Skills always win a shared name, and the lookup uses `PatternRegistry::names`
  — `list` reads every `system.md`, which is 255 file reads per slash-bearing
  message.

Full guide: [docs/zen-patterns.md](docs/zen-patterns.md).

## Waiting on long jobs: watch mode (`src/scheduler/watch.rs`)

An agent that delegated to something slow had no way to come back on its own. It
`sleep`ed, polled once, and told the user to ask again later — and
`background_*` cannot close that loop because a background run **has no chat by
design** ("no reply to anybody"; its only reach is an OS notification).

A **watch** is the third thing: `ContextMode::Watch` on a `scheduled_tasks` row
that re-checks a condition every interval and, the moment it holds, dispatches a
prompt into the *originating chat* — the same `dispatch_into_chat` seam
`ContextMode::Group` uses. The agent arms it with `schedule_watch` and ends its
turn.

**The tick is one MCP tool call, no LLM.** `WatchConfig` names the tool, its
args, and a declarative `DoneWhen` evaluated in Rust, so an hour of waiting costs
tool calls and nothing else; an agent turn is spent only when the condition
resolves or the watch gives up.

Rules for Claude:

- **Unwrap the MCP envelope before testing the condition.** A tool answers
  `{"content":[{"type":"text","text":"…"}]}` and that text is usually itself
  JSON. Test `done_when` against the envelope and every sane path (`status`,
  `data.state`) resolves to nothing — so the watch polls to its deadline against
  a job that finished on the first tick. `normalize_tool_result` is that step.
- **A string compares as its text, not as quoted JSON.** `render_value` exists
  so a `"done"` status does not compare against `"\"done\""` and never match.
- **A single probe error must not end a watch.** A stopped `session` Space App is
  the resting state, not a fault; tolerance is `MAX_ERROR_STREAK`, and a success
  clears the streak.
- **A watch must never end silently.** Deadline, check ceiling and error streak
  all terminate it *by dispatching into the chat* — `render_timeout` has no empty
  branch. Ending quietly is indistinguishable to the user from the giving-up
  behaviour watch mode replaces, and its default text tells the model not to
  invent a result.
- **Retire the row before the resume turn, not after.** The poll loop advances
  `next_run` *before* handing to the executor, so a watch that merely returns
  `Ok` is re-armed; `finish_watch` marks it completed first so a slow agent turn
  cannot let the next tick pick the same watch up again.
- **An unreadable `deadline_at` or `watch_json` ends the watch.** Neither is
  fixable by polling again, and the alternative is a row that polls forever.
- **Ownership comes from the chat env, never a tool parameter** — unlike the
  older `schedule_task` beside it. A watch speaks into a conversation; a
  caller-supplied jid would let one session speak into another.
- **`interval_secs` and `timeout_secs` are clamped, not rejected** (15–3600 and
  ≤86400). A model that asks for a 2-second poll should get a sane watch, not an
  error it has to recover from mid-turn.
- Omitting `tool` is the **agent-turn fallback**, for jobs no single call can
  check — it still works, it just costs a turn per interval. `mcp_manager: None`
  degrades every watch to it rather than failing.

A watch is **visible and stoppable**: `GET /api/watches?chatJid=…` backs a strip
above the composer in all three clients (`WatchStrip` in web / desktop /
channel_app) showing what is being watched and its check count, with a Stop
button (`POST /api/watches/:id/stop`). `schedule_watch_list` / `schedule_watch_stop`
give the agent the same two operations.

More rules for Claude:

- **A path that resolves to nothing is "not reported yet", never "done".**
  Without that guard the negated operators invert an absent value into a match,
  so a mistyped path finishes the watch on its first probe. That shipped: a
  watch on `status` (the real field being `task.status`) woke the chat every
  60 s, each wake spending an agent turn to say "still running" and arm another
  identically-wrong watch. `Exists` is the deliberate exception — absence *is*
  its answer.
- **A watch is armed against the chat's own folder, never a `schedule_` one.**
  `run_daemon`'s legacy-schedule sweep retires every active row whose folder
  does not match `schedule\_%`, so it must exclude `context_mode = 'watch'` or
  each restart silently kills every in-flight watch — nothing logged, nothing
  to see, the chat simply never hears back.
- **Persist the counters on the resolving tick too.** `Done` and `GaveUp` retire
  the row, and without a `persist_watch` first the probe that answered is never
  written back — a watch that resolved on check 1 is recorded as having made
  zero, which is exactly what made a premature match look like it never ran.
- **Stopping is silent.** The user just cancelled it; waking the chat to
  announce that is noise. The row is completed, not deleted, because it is the
  only record the wait happened.
- **`watch_stop` is scoped by chat, not by id alone** — an id-only lookup would
  let one conversation cancel another's wait.
- **Stopping must report whether a row actually changed.** `update_task_status`
  returns `Ok` for an id that matched nothing, so both the endpoint and the MCP
  tool once confirmed a stop that never happened. `Db::stop_watch` narrows to
  `context_mode = 'watch' AND status = 'active'` and returns the row count —
  which also stops this path retiring an ordinary schedule.
- **A `Query<T>` field name is the wire name.** `#[serde(alias = "chat_jid")]`
  on a field already called `chat_jid` renames nothing: every client's
  `?chatJid=` was rejected with a 400 and the strip silently never loaded. It
  needs `rename = "chatJid"` with the snake_case spelling kept as the alias.
  Neither this nor the stop bug above is visible to `cargo check` or to a unit
  test — both were found by calling the running daemon.

Guide (Vietnamese, PHẦN D):
[docs/background-schedule-tasks-guide.md](docs/background-schedule-tasks-guide.md).
Agent-facing instructions live in [`skills/schedule/SKILL.md`](skills/schedule/SKILL.md)
and in the `schedule_watch` tool description — the description is what actually
reaches a model mid-turn, so it carries the anti-patterns (`sleep`, "ask me
later", in-turn polling loops) explicitly.

## DAG dispatch: finishing, and retrying what failed

Three properties the dispatch bridge must keep, all of which failed silently
before:

**A DAG that cannot progress is failed, not left running.** The timeout sweep in
`process_pending` only watches `processing` tasks, so a task that never *starts*
has no deadline at all — and `can_start_task` returns false forever for a
virtual task whose persona is missing from the registry. The parent then sits
`active` with nothing running and nothing runnable until the daemon restarts,
which from the chat is indistinguishable from a DAG still working.
`sweep_stalled_parents` closes exactly that case.

**Waiting for a DAG should not block the turn.** `dispatch_all_tasks` and
`create_parent_and_run` poll in-turn until every task is terminal, with a 900 s
default. A DAG that outlives the deadline returns a failure while the daemon
keeps working — historically the manager then re-dispatched the whole graph.
`dispatch_status` is the non-blocking snapshot; combined with `schedule_watch`
(see the watch-mode section above) a long DAG costs no turn while it runs.

**A person looking at a failure can re-run it.** `retry_task` /
`retry_parent_failed`, reached at `POST /api/dispatch/tasks/:task_id/retry` and
`POST /api/dispatch/parents/:parent_id/retry`, surfaced in all three clients
(`web/src/components/InlineDispatchCard.tsx`, desktop
`message_widgets.dart::InlineDispatchCard`, `channel_app` dispatch screen).

Rules for Claude:

- **Reviving a task means reviving its parent.** A DAG whose last task failed is
  already `"done"`, and the scheduler skips non-active parents — so re-queueing
  a task without flipping the parent back to `"active"` produces a task that is
  never picked up. A test pins this.
- **The stall condition is "nothing `processing` **and** nothing startable".**
  Not "nothing processing": a task blocked only by a concurrency slot is waiting
  on a peer that *is* running, and killing it would break every DAG wider than
  its own limit. The narrow form is sound because the only thing that unblocks a
  task is another task finishing.
- **The stall sweep needs its grace period.** At boot and at parent creation the
  persona registry and virtual-worker pool are briefly unwired, so every task is
  legitimately un-startable for a moment. `STALL_GRACE_SECONDS` covers it;
  without it a fresh DAG is killed on the first tick.
- **User retry is not the infra-retry budget.** `MAX_INFRA_RETRIES` is automatic,
  capped at 1, and gated on `is_retryable_infra_error`. `retry_task` is a person
  deciding, so it is uncapped and does not inspect the cause. Do not fold them
  together.
- **Clear the previous attempt's verdict on retry** — `verification_result`,
  `file_changes`, and checklist item statuses. Left behind, they are read as
  belonging to the new run.
- **A retry refusal is a message for a person.** "already running", "finished
  successfully" — the endpoints return it as a `400` body and all three clients
  show it verbatim. A generic "retry failed" just makes the user click again.
- **`retry_task` kicks `process_next_pending` itself** rather than waiting for
  the 300 ms tick, because someone is watching the button.
- **Route params in this crate are axum 0.7 `:name`, never `{name}`.** Braces
  compile fine and are matched as a *literal* segment, so the route silently
  never fires. Every one of the ~112 param routes in
  [`core.rs`](src/gateway/ui_server/core.rs) uses the colon form.
- **`dispatch_all_tasks` waits for all tasks, and its description used to say
  "stops on first error".** It was changed to wait-all in July 2026 and the
  description was not; a model reading the stale text plans around a failure
  mode that no longer exists.

## Scaffolding: `senclaw create`

`senclaw create app|skill|sub-agent <name>` renders a working project from a
template. Templates live in a git repo (`NortonBen/senclaw-templates`, cloned to
`~/.senclaw/templates/repo`) **and** are compiled into the binary from
`assets/templates/` — git wins when reachable, the bundled copy is what keeps the
command working offline. Four app languages: `rust` (default), `go`, `node`,
`python`. Engine in [`src/scaffold/`](src/scaffold/), CLI in
[`src/cli/commands/create.rs`](src/cli/commands/create.rs).

Rules for Claude:

- **The rendered project is validated before anything is written**, and the
  checks are the silent-failure ones: a misspelled `runtime.mode` (falls back to
  `session`, so a background poller quietly stops), a `runtime.kind` that is not
  exactly `"server"` or a missing `start` (the app installs and never launches),
  a wrong-typed field (`as_str`/`as_u64` read a wrong type exactly like an absent
  one, so `"port": "4800"` would reach the daemon as port 0), and a port above
  65535 (cast `as u16`, so 70000 becomes 4464). Adding a template means those
  checks must still pass — `cargo test` renders every bundled template and runs
  them ([`src/scaffold/create.rs`](src/scaffold/create.rs) `validate`).
- **The wildcard-bind rule runs on the template's own source, not the rendered
  output** (`check_bind_host`). After substitution a user's `--desc "never binds
  0.0.0.0"` is indistinguishable from code. It catches the literal *and* the
  hostless forms that contain no literal — `server.listen(PORT)`, `":"+port`,
  `(("", PORT))`, bare `next start` — and strips comments first, since every
  template documents the rule. This is now the only in-repo enforcement of the
  bind-loopback rule for Space App code, since `apps/` moved to its own repo.
- **Adding a bundled template is dropping a directory into `assets/templates/`.**
  `build.rs` walks it into an `include_bytes!` table; there is no list to update.
- **The render syntax is `{{lower_snake}}` only.** Everything else in braces
  (`{{.Name}}`, `{{ item.title }}`, `{{#each}}`) passes through untouched, so a
  template may ship Go/Vue/handlebars syntax. An unknown `{{placeholder}}` is
  left verbatim and warned about, never blanked. **Substituted values are
  escaped for their destination** — JSON, a markdown file's YAML frontmatter
  (but not its prose), or HTML — because `--desc`/`--icon`/`--var` are arbitrary
  user text: unescaped, one can inject a second `id` key that serde prefers, or
  a second `name:` that the persona registry prefers.
- **A template that calls `/bridge` must send `action`, not `capability`**, and
  must treat a `200` carrying `{"status":"error"}` as a failure. Both are
  enforced by tests over the bundled templates
  ([`src/scaffold/bundled.rs`](src/scaffold/bundled.rs)) because both fail in a
  way that looks exactly like the app degrading gracefully with no daemon.
- **Only the id folds diacritics.** `"Quản lý Kho"` → id `quan-ly-kho`, display
  name still `Quản lý Kho`. Folding shares the table in
  [`src/security/replication.rs`](src/security/replication.rs) `fold`.
- Ports auto-pick from **4800** (bundled apps own 4300–4799), skipping ports
  declared by installed manifests *and* ports currently listening.
- `postCreate` steps are **printed, never executed**.

Full guide, including how to author a template for the repo:
[docs/senclaw-create.md](docs/senclaw-create.md).

## Space App lifecycle: background vs session

Every server Space App is one of two things, declared as `runtime.mode`:
**`background`** (started with the daemon, supervised, restarted when it dies)
or **`session`** — the **default** — started when the app is opened or one of
its MCP tools is called, and stopped once idle for `runtime.idleTimeoutSecs`
(60s default, 15s floor). Session is the default because the old behaviour
launched all ~50 installed apps at boot and kept them forever.

The mechanism that makes on-demand MCP work: a session app's MCP server is
registered against **the daemon's app proxy**
(`/api/space/apps/<id>/proxy<mcp.path>`), not the app's own port, and its tool
list comes from `<app>/.senclaw/mcp-tools.json` cached at the last successful
connection. So its tools stay in every agent's roster while it is stopped, and
the first call connects → proxy → spawn → answer. Without both halves nothing
would ever call a stopped app and it would never start.

Rules for Claude:

- **Never assume "not running" is a fault.** For a session app it is the resting
  state; only background apps are supervised.
- **A misspelled `mode` is silent** — it falls back to `session`, so an app that
  must poll a channel quietly stops.
  [`tests/space_app_lifecycle_manifests.rs`](tests/space_app_lifecycle_manifests.rs)
  enforces the spelling *and* scans each app's Rust for autonomous-startup
  markers (`extbridge::serve_ws`, `spawn_heartbeat`, `spawn_scheduler`,
  `spawn_poller`, `run_supervisor`, `spawn_janitor`); an app that gains one must
  be declared `background` or `cargo test` goes red.
- **Adding a background loop to an app means editing its manifest too.**

Manifest also carries `requires` (what the machine must have: `node`, `python`,
`bin`, `env`, `os` — checked at install *and* before every launch, a hard miss
refuses the launch with the reason) and `sandbox` (the confinement the app asks
for itself; `force: true` means the settings dialog cannot turn it off, and a
non-forced declaration never overrides a choice the user already saved).

`runtime.runner` (`binary` | `node` | `python` | `shell`, inferred from `start`)
drives a one-off prepare step: `npm ci`/`npm install` for Node, and for Python a
**virtualenv at `<app>/.venv`** plus `pip install -r requirements.txt` into it —
never the user's system Python. The stamp hashes file *content*, not mtimes, so
extracting an update does not reinstall.

Model in [`src/apps/`](src/apps/) (`manifest.rs`, `requirements.rs`,
`prepare.rs`, `sandbox_decl.rs`); process lifecycle in
[`src/gateway/ui_server/space_mcp.rs`](src/gateway/ui_server/space_mcp.rs).
Endpoints: `POST /api/space/apps/:id/{stop,start}`,
`GET /api/space/apps/:id/requirements`, plus `lifecycle` + `requirements` blocks
on `/runtime`. Knobs: `SENCLAW_SPACE_SUPERVISE_SECS` (20),
`SENCLAW_SPACE_IDLE_SWEEP_SECS` (10). Full guide:
[docs/space-app-lifecycle.md](docs/space-app-lifecycle.md).

### Managing Space Apps from chat (`space_app_*`)

`senclaw-space` carries five tools so an agent can do from a conversation what
Settings → Space Apps does: `space_app_list` (installed apps + running state,
filterable, `probe` for a real health check), `space_app_start`,
`space_app_stop`, `space_app_restart`, and `space_app_mcp_list` (which MCP
server each app registers, its status and tool count — the way to look up the
full `mcp__<mcpName>__<tool>` name).

They live in [`src/mcp/space_apps.rs`](src/mcp/space_apps.rs) and are **loopback
HTTP calls back into the daemon**, unlike the notes/calendar half of the same
server which opens the DB directly. The reason is not style: an app's process
lives in the daemon's `SpaceMcpLauncher` — a child-process map, a user-stopped
set, a launch counter, all in memory in *another* process. A second launcher in
the MCP subprocess would fight the first one for ports. `space_mcp_config` sets
`SENCLAW_SPACE_API_URL`; loopback peers are exempt from the daemon's API token,
and the app-token gate covers only an app's *data* routes, never `/start` and
`/stop` — so the local case needs no credential.

Rules for Claude:

- **`GET /api/space/apps/status` is a literal sibling of `:id`.** Adding a route
  like it means adding the segment to `app_auth::split_app_path`'s literal list,
  or it is parsed as an app named "status".
- **Never report a stopped `session` app as broken.** It is the resting state;
  only `background` apps are supervised. The tool descriptions say so because an
  agent that "fixes" it restarts something working as designed.
- **Stopping a `background` app stops whatever it was watching** (channel polls,
  schedules) until someone starts it again — confirm with the user first.
- **A client timeout on `space_app_start` is not a failure.** A first start can
  be minutes (`npm ci`, venv); the daemon keeps going after the client gives up,
  so the answer is "check again with `space_app_list`", never a retry.
- **The MCP registry is enrichment, not the answer.** `space_app_mcp_list` reads
  `mcpName` from the manifest and only *decorates* it with live status; when the
  registry is unreadable it degrades (`registered: null` + `degraded` note)
  rather than failing or claiming `registered: false`.

### Space App SDKs

Four, one per language an app can be written in — all documented against the
same manifest:

| | |
|---|---|
| Rust | [`app-space-sdk/`](app-space-sdk/) |
| Node / TypeScript | [`senclaw-sdk/senclaw-app-sdk/`](senclaw-sdk/senclaw-app-sdk/) — `@senclaw/space-sdk` on npm, subpaths `/mcp` and `/lifecycle` |
| Python | [`senclaw-sdk/senclaw-app-sdk-python/`](senclaw-sdk/senclaw-app-sdk-python/) — `senclaw-space-sdk` on PyPI, `senclaw_space`, standard library only |
| Go | [`senclaw-sdk/senclaw-app-sdk-go/`](senclaw-sdk/senclaw-app-sdk-go/) — `go get github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go`, package `senclaw` + subpackages `manifest` / `dispatch`, standard library only |

Each SDK carries its own runnable minimal app under `examples/`, and each
exposes a manifest validator that catches the silent-failure spellings
(`python -m senclaw_space.manifest <file>` / `validateManifest()` /
`go run …/cmd/senclaw-manifest <file>`).

**A Go app has no install step.** `runtime.install` runs for the `node` and
`python` runners only ([`src/apps/prepare.rs`](src/apps/prepare.rs) returns
early for `binary` and `shell`), so a Go app either ships a built binary
(`start: "./app"`, runner inferred `binary`) or compiles in `start`
(`go run .`, runner `shell`, `requires.bin: ["go"]`, within the daemon's 30s
health budget). Declaring `install: "go build …"` is silently skipped — the Go
SDK's `manifest.Validate` is the only thing that flags it.

## Per-app Space App sandbox

Each Space App can be run inside the OS sandbox from Plugins → Space Apps → the
**Sandbox** button (web + desktop): write-jail to its own folders, a read mode
(`open` / `strict`), extra folders, and a network mode — everything / nothing /
**only these sites**. Per-site egress cannot be an OS rule (Seatbelt accepts only
`*` or `localhost` as a remote host), so it is an allowlisting proxy on loopback
with the sandbox given no direct egress: a client that ignores `HTTP_PROXY`
reaches nothing rather than everything. Config in
[`src/sandbox/app_policy.rs`](src/sandbox/app_policy.rs), launch wrapping in
[`src/sandbox/app_launch.rs`](src/sandbox/app_launch.rs), proxy in
[`src/sandbox/proxy.rs`](src/sandbox/proxy.rs), REST at
`/api/space/apps/:id/sandbox`. Enforcement differs by platform (macOS full, Linux
folders only, Windows none) and the UI says so up front. Traps — `strict` breaks
apps whose runtime lives under `$HOME` (nvm), granting a folder is not enough when
its *parent* is denied (SQLite dies with `SQLITE_CANTOPEN`), `npm start` wants
`registry.npmjs.org`, paths are never remapped — plus the measured before/after
table: [docs/space-app-sandbox.md](docs/space-app-sandbox.md). Process lifecycle
(daemon must catch SIGTERM, shutdown signals all apps at once, a healthy port is
*reclaimed* rather than adopted — only when the process's cwd proves it is that
app's): [docs/sandbox-app-design.md](docs/sandbox-app-design.md).

## Space App runtime monitor

Plugins → Space Apps → **Details & logs** carries a live panel for any
`runtime.kind == "server"` app: running/answering state (a health check, not just
"tracked"), pid, port, uptime, **launch count** (a number climbing on its own is
the only visible signature of a crash loop), CPU/RAM by *process group* (`npm →
node`), open sockets via `lsof`, the allowlist proxy's allowed/denied counters,
and the cwd + start command + env needed to rerun the app by hand. One endpoint,
`GET /api/space/apps/:id/runtime`
([src/gateway/ui_server/space_runtime.rs](src/gateway/ui_server/space_runtime.rs)),
best-effort everywhere — a missing `lsof` or a timed-out health check becomes a
note in the payload, never a failed request. Plugins → Sandbox additionally
carries the fleet view (`GET /api/space/apps/sandbox-overview`, one `ps` for the
whole list), whose first column is *what the running process actually got* — the
only place the "configured confined, running unconfined" gap is visible, since a
profile is fixed at launch. Guide:
[docs/space-app-monitor.md](docs/space-app-monitor.md).

## Space App network binding

Space Apps under `apps/*` have **no authentication of their own**. Their REST API
and their MCP endpoint are wide open to anyone who can reach the port: the trust
boundary is the loopback interface, not the app. The daemon reaches every app at
`http://127.0.0.1:<port>` — health checks (`src/gateway/ui_server/space_mcp.rs`
`health_url`), the MCP origin, and the UI proxy all hardcode loopback — so
binding loopback costs nothing operationally.

Every app bootstrap MUST therefore resolve its bind host from the env, never
hardcode one:

```rust
// Loopback by default. A Space App authenticates nothing of its own — the
// daemon reaches it over 127.0.0.1 and the UI is same-origin — so binding
// 0.0.0.0 hands the whole REST + MCP surface to anyone on the LAN. Set
// SENCLAW_BIND_HOST=0.0.0.0 to opt in to that explicitly.
let host = std::env::var("SENCLAW_BIND_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await.unwrap();
```

Rules:

- **Never write `bind("0.0.0.0:...")`.** The bootstrap is copy-pasted between apps,
  so one bad copy re-exposes the fleet. This was a real exposure (fixed 2026-07-31):
  nearly every app was serving customer data on `*:PORT`.
- **Same knob for extension-bridge WebSockets** (`apps/*/src/extbridge.rs`). The
  Chrome extensions all dial `ws://127.0.0.1:<port>`, so loopback is correct there too.
- **Node apps** read `process.env.SENCLAW_BIND_HOST || '127.0.0.1'` and pass it as
  the `app.listen(PORT, HOST, ...)` host argument.
- **Next.js apps** must pass `-H ${SENCLAW_BIND_HOST:-127.0.0.1}`; bare `next start`
  binds `0.0.0.0`.
- `apps/rule-engine` keeps its older `RULE_ENGINE_BIND` override, checked before
  `SENCLAW_BIND_HOST`.

[`tests/space_app_bind_loopback.rs`](tests/space_app_bind_loopback.rs) enforces all
of the above on every `cargo test`.

## Space App access token & API version

Loopback is a boundary around the *machine*, not around an *app*: knowing an
app's id (public) used to be enough for any local process — including another
Space App — to drive its `/bridge` (a full tool-enabled agent), read its
`/config` (API keys) and query its SQLite. So the daemon now mints **one access
token per installed app** (`sca_<64 hex>`, table `space_app_tokens`), hands it to
the app's process in **`SENCLAW_TOKEN_ACCESS_APP`**, and treats it as the app's
name: a token presented against another id is **403 in every mode**.

- **`SENCLAW_APP_TOKEN_MODE`** = `strict` (**default** — a tokenless call to an
  app's data route is refused unless the caller is the daemon's own UI) | `warn`
  (served + one log line per app, the way to find what would break) | `off`
  (served as before; the escape hatch for an app with a hand-rolled HTTP
  client). Strict only gates the app's *data* routes (`/bridge`, `/config`,
  `/sqlite/query`, `/mcp/register`, `/env`, `/token`) — never management routes
  or `/proxy`, `/static`. An unrecognised value falls back to `strict`, never to
  `off`: a typo must not silently disable app isolation.
- **Changeable from the UI** at Settings → Space Apps (web + desktop), via
  `GET`/`PUT /api/space/app-token-mode`. The choice lives in `router_state`
  (`space:appTokenMode`), **overrides** the env var, and needs no daemon restart
  — the middleware reads it per request. That route is deliberately not under
  `/api/space/apps/`: `app_auth` gates everything there per app id, which under
  strict would lock the operator out of the switch that turns strict off.
- **`SENCLAW_API_VERSION`** — Space-App contract version (now **2**). Injected
  into every app, stamped on every app-scoped response, sent by every SDK. Older
  contracts are served; a newer one gets **426**.
- **Inbound**: the proxy stamps the token on everything it forwards (and strips
  a client's copy), and MCP configs carry it in `headers`/`env` so a *background*
  app — whose MCP client dials its port directly — still authenticates. Each SDK
  ships an opt-in guard (`RequireAppToken` / `require_app_token` /
  `requireAppToken`) that closes the app's own port to everything but the daemon.

Rules for Claude:

- **Strict mode is not a boundary against local malware.** Anything that can read
  `~/.senclaw/senclaw.db` reads every token in it. The feature is app-vs-app
  isolation, and it is only a real boundary combined with the per-app sandbox.
- **Never turn the relay's `TrustedOperator` marker into a header.** It is a
  request extension precisely because nothing on the network can forge one.
- **Never drop the `sca_` prefix check** in `presented_token` — the daemon's own
  API token arrives through the same `Authorization: Bearer` header.
- `/env` must never carry the token: it feeds the app's *browser* UI.

Model in [`src/apps/token.rs`](src/apps/token.rs), enforcement in
[`src/gateway/ui_server/app_auth.rs`](src/gateway/ui_server/app_auth.rs). Full
guide: [docs/space-app-api-token.md](docs/space-app-api-token.md).

## Telegram access: pairing, not first-come

A Telegram message from a chat with **no binding** used to *complete* the
channel's pending binding on sight — so whoever messaged the bot first owned it,
uncapped in time and unchecked as to who they were. It was not an edge case: the
web UI's "Add Agent" calls `onRegisterBinding({agentId, channelId})` with no
`jid`, and the Telegram channel form has no Chat JID field, so the pending path
was the *only* way to bind Telegram from the UI. The claimer got every tool
(a UI-created agent has no `allowed_tools`, and empty means no filter) and
approved its own permission prompts, because those are delivered to `chat_jid`.

Now the bot answers an unknown chat with an **8-character code** and processes
nothing. A person turns that code into a binding — Settings → Channels →
Pairing, or `senclaw pairing approve <CODE>`.

Rules for Claude:

- **A `channel_pairings` row is a request, never a grant.**
  `challenge_unbound_telegram` returns `Option<GroupBinding>` and *always*
  returns `None`; only [`pairing::approve`](src/gateway/pairing.rs) writes a
  binding. Any path that turns a row into access without a human is the original
  bug in a new shape.
- **An empty stored `botToken` means the configured default bot, not any bot.**
  The old condition was `channel_token == bot_token || channel_token.is_empty()`,
  and the Settings form omits the key entirely when the field is blank — so a
  message through an unrelated bot completed this channel's binding.
  `resolve_telegram_channel` compares against `config.telegram.bot_token` and
  matches nothing when that is empty too.
- **Announce on a new code (`Requested::is_new`), never "once per chat".**
  Re-serving the live row is what stops the flood — three "hello"s are one
  request. A chat-keyed "already challenged" flag looks equivalent and silently
  breaks expiry: the next message after the hour mints a replacement code that
  the user is then never told.
- **`resolve_pairing` is narrowed to `status='pending'` and returns the row
  count.** An `Ok` on a zero-row update lets two approvers both think they were
  the one who let the chat in, and makes a rejected code re-approvable.
- **Approving a group admits every member.** `should_trigger` still never looks
  at `sender_jid` — pairing gates the chat, not the person inside it.
- **Only Telegram changed.** Feishu/WeChat keep their first-message pending
  completion.
- **Approval has four surfaces, and the chat one is `dispatch_command`.** Both
  the message router and the WS `handle_message_send` call it, so
  `pair approve <CODE>` works from Telegram and from the app's own chat with one
  implementation. Never advertise the `senclaw pairing` CLI from inside the
  product — it resolves through `PATH`, and a stale binary answers
  "unrecognized subcommand 'pairing'", which reads as pairing being broken.
- `ADMIN_TELEGRAM_USER_ID` is **gone** — parsed and never read, a knob that
  looked like an allowlist and enforced nothing. `TelegramBotConfig.admin_user_id`
  and the `telegram_bots` config.json section are still write-only
  (`get_telegram_bots` has no callers).

- **Never `let _ =` a channel send, and `owns_jid` must answer "is this *my*
  bot's chat", not "is this a Telegram jid".** `run_daemon` pushes a default
  `TelegramChannel` unconditionally — tokenless and unconnected when no
  `TELEGRAM_BOT_TOKEN` is set — then builds another per DB channel. A
  prefix-only `owns_jid` let the dead one claim every chat, the reply loop broke
  on it, `send_message` returned `Err("Bot not found")`, and `let _ =` ate it:
  paired chats got no reply and the log said nothing. `TelegramChannel` keeps a
  *sync* `RwLock<HashSet<u64>>` of its bot ids because `owns_jid` is sync and
  `bots` sits behind a tokio mutex.

Full record, including the three details that made the old behaviour worse than
it sounds, and the silent-send trap: [docs/telegram-pairing.md](docs/telegram-pairing.md).

## Daemon network binding & API token

The daemon's own surface (UI HTTP 18788 + WS gateway 18789) binds `127.0.0.1`
by default via `SENCLAW_UI_BIND_HOST` — a knob deliberately **separate** from
the Space-App `SENCLAW_BIND_HOST` (apps have no auth; the env would propagate
to them). Desktop users flip it at **Settings → General → Network access**
(Private `127.0.0.1` / Public `0.0.0.0`); the choice is persisted in prefs and
handed to the daemon at spawn time, so it needs a daemon restart to take
effect. The token itself is `SENCLAW_API_TOKEN`, else auto-generated
`~/.senclaw/api_token` (0600), presented via `Authorization: Bearer`,
`X-SenClaw-Token`, `?token=`, or the `senclaw_token` cookie minted by
`POST /api/auth/login`. `/api/auth/status` and `/api/auth/login` are the only
open API paths; CORS is loopback-origin-only (never reintroduce
`CorsLayer::permissive()` — it leaked `/api/llm-config` keys to any website).

**Whether the token is demanded is `SENCLAW_AUTH_MODE`, a tri-state** — not a
boolean derived from the bind host:

| | required when | for |
|---|---|---|
| `auto` (default) | bind host non-loopback, peer non-loopback | laptop, desktop install |
| `always` | every peer, loopback included | cloud, Docker, behind a reverse proxy |
| `off` | never | an ingress that already authenticates |

Live override at `GET`/`PUT /api/auth/mode` (stored in `router_state`
`auth:mode`, wins over the env, no restart), surfaced at Settings → General →
Access token in both web and desktop.

Rules for Claude:

- **`always` is not a hardened `auto`, it is the only correct setting behind a
  same-host reverse proxy.** nginx/Caddy terminating TLS on the daemon's box
  makes every Internet client arrive from `127.0.0.1`, and `auto` exempts
  exactly those. Chosen over trusting `X-Forwarded-For` because it needs no
  trusted-proxy list and has no spoofable surface.
- **The loopback exemption in `authorize()` must stay *after* the mode check.**
  An early return on a loopback peer makes `always` indistinguishable from
  `auto` — and it looks like it works. `tests/daemon_auth_guard.rs` pins the
  order.
- **Local trust needs a local name and no foreign page** (`local_request`):
  `Host` loopback (or absent) and `Origin` absent or loopback
  (`chrome-extension://` only on `/browser*`). Without it DNS rebinding and
  aliases like `lvh.me` rode the exemption, and any web page — the agent's own
  browser included — could open the WS gateway (no CORS for WebSockets) and
  answer permission prompts. Native clients send no `Origin`.
- **An unrecognised `SENCLAW_AUTH_MODE` falls back to `auto`, never `off`.**
- **`/api/auth/mode` is gated.** It is the switch that turns the gate off;
  putting it in `OPEN_API_PATHS` hands it to anonymous remote clients.
- **`always` means the daemon's own loopback callers need the token too.**
  `run_daemon` publishes it into its own env (children inherit it) and
  in-process callers attach it through
  [`util::internal_auth::header_for`](src/util/internal_auth.rs) — MCP
  `space`/`patterns`/`ocr`, kanban's `llm_info`, and the Space-App LLM proxy,
  whose OpenAI endpoint *is* a daemon route (`/api/space/apps/<id>/proxy/v1`)
  and whose `api_key` is empty by design.
- **The session cookie's `Secure` flag is conditional** (`X-Forwarded-Proto`,
  or `SENCLAW_AUTH_COOKIE_SECURE`). Both mistakes are silent: `Secure` on plain
  HTTP makes the browser discard a cookie the login just minted.

**Docker** — `Dockerfile` + `docker-compose.yml` at the repo root. The image
sets `SENCLAW_UI_BIND_HOST=0.0.0.0` (the container's own namespace; `-p`
decides reachability) and `SENCLAW_AUTH_MODE=always` (with `--network host` or
a sidecar proxy, `auto` would exempt everyone). Traps, all enforced or
documented: the healthcheck must hit `/api/auth/status` — every other route
401s under `always` and flaps the container unhealthy forever; `~/.senclaw`
**must** be a volume or the token regenerates each restart and every saved
login breaks; never set `SENCLAW_BIND_HOST=0.0.0.0` in the image (that is the
Space-App knob — apps have no auth); publish both 18788 and 18789 because the
web UI dials the WS gateway at the same hostname. A Linux container compiles
**no** MLX/Metal, so it has no local models, no Whisper ASR and unaccelerated
OCR — same as the Linux CI target.

Full guide: [docs/remote-access-security.md](docs/remote-access-security.md).

## Code v2: checkpoints, repo map, LSP, worktrees, edit formats, ACP, trajectories

Coding runs **inside the ordinary chat agent** (the separate code engine was
removed in 83e9720). The layers added on 2026-09-12
(`plans/260912-0141-code-v2/`), each with its own doc:

| Layer | Where | Doc |
|---|---|---|
| Shadow-git checkpoints after every writing tool; restore/diff/explain | `src/checkpoints/`, `/api/chats/:jid/checkpoints*` | [docs/code-session-api.md](docs/code-session-api.md) |
| Code sessions = chats with `group_type = "code"` (mobile contract) | `src/gateway/ui_server/code_sessions.rs` | same |
| Repo map (tree-sitter + PageRank) in the prompt + `find_symbol`/`find_references`/`symbol_body`/`repo_map` | `src/repo_map/`, `src/tools/repo_map_tools.rs` | [docs/repo-map.md](docs/repo-map.md) |
| LSP diagnostics appended to Edit/Write results | `src/lsp/`, hook in `zen_core/run_tools.rs` | [docs/lsp-diagnostics.md](docs/lsp-diagnostics.md) |
| Git worktrees for `isolation: "worktree"` DAG tasks, `Task` subagents, kanban cards labelled `worktree` | `src/worktree/`, `/api/worktrees*` | [docs/worktree-isolation.md](docs/worktree-isolation.md) |
| Per-model `EditFormat` (exact/fuzzy/udiff/whole) | `zen_core::EditFormat`, `src/tools/edit_apply.rs` | [docs/edit-formats.md](docs/edit-formats.md) |
| `senclaw acp` — ACP agent on stdio for Zed/JetBrains | `src/acp/` | [docs/acp.md](docs/acp.md) |
| Trajectory JSONL per turn + replay + `scripts/evals/run.py` | `src/trajectory/` | [docs/evals.md](docs/evals.md) |

Rules for Claude:

- **Checkpoints never touch the project's `.git`.** The shadow repo is
  `~/.senclaw/checkpoints/<chat>/` with the working dir as work tree. Only
  git repositories and code sessions are crawled — never `$HOME`. A restore
  snapshots the current tree first and is itself a checkpoint.
- **The repo map must never block a turn.** `map_for_prompt` answers from
  the process cache and kicks a background refresh; the first turn in a
  fresh tree gets no map. Names defined in > 3 files (`Error`, `new`) carry
  no edges — they made every crate's error enum rank first.
- **LSP is a hook, not a tool.** Only servers already on PATH; `didChange`
  is always sent for an open document; diagnostics are filtered by URI; a
  server that fails twice is disabled for the workspace. `~/.cargo/bin/
  rust-analyzer` can be the rustup proxy with no component — it exits at
  once, which is why the client keeps stderr.
- **Worktree isolation falls back loudly.** Outside a git repository the
  task runs in the shared directory and its result says
  `isolation=worktree ignored: …`. Nothing auto-merges or auto-PRs.
- **`EditFormat` flows profile → `RunContext.hook_profile` → `ToolContext`.**
  Adding a field to `ToolContext`/`ModelProfile`/`LlmConfig` means every
  struct literal in `src/`, `tests/`, `examples/` — a naïve patcher also
  matched `fn f() -> ModelProfile {` signatures and corrupted function
  bodies; check `-> Type {` sites.
- **`senclaw acp` is a translator, not an engine.** It needs the daemon's
  WS gateway; logs go to stderr because stdout is the protocol.
- **Trajectories are off by default** and 0600: they contain the files the
  agent read.
- **Migrations that add a column to a Space table must be guarded by the
  table existing** — `apply_space_tables` runs *after* `run_migrations`, and
  an unguarded ALTER took 177 tests down with "no such table".

## Failure ledger: recording what breaks, learning nothing (yet)

Every tool failure in a chat lands in `failure_episodes` — one row per run of
consecutive failures of the *same tool*, plus the outcome: the tool eventually
worked and a user message had arrived first (`fix_source = user` — only a
remembered lesson could have skipped that detour), it worked with no user
message in between (`model` — the existing `TOOL_ERROR_NUDGE` already handles
it), or it never worked (`gave_up`). Written by
[`src/failures/`](src/failures/mod.rs) off the same per-chat event seam as the
trajectory; read at `GET /api/failures{,/summary}`.

It **only records**. Nothing is distilled into a lesson and nothing reaches a
prompt: the gate for building that is the ledger's own numbers (repeat rate
≥ ~15%, and who supplied the fix). Design:
[docs/error-learning-research.md](docs/error-learning-research.md); what
shipped: [docs/failure-ledger.md](docs/failure-ledger.md).

Rules for Claude:

- **It is not `tool_executions`.** That table is the chat's replay buffer and is
  FIFO-trimmed with the message cap, so the *oldest* failures — exactly the ones
  that prove a failure repeats — are deleted first. The ledger is durable and
  bounded by row count instead.
- **Shape, never values.** `args_shape` is `name -> JSON type`; argument values
  are user data and this table outlives the chat. `string:empty` stays distinct
  from `string` because "passed `host` empty" and "omitted `host`" are different
  mistakes.
- **A permission denial is not a tool failure.** The deny paths in
  `run_tools.rs` emit no `ToolExecutionError`, so they never reach the ledger —
  keep it that way.
- **Only the main agent's final message ends a turn.** Subagents finish on the
  same bus; counting theirs retires the chat's episodes a turn or two early. An
  episode survives two turn-ends before it is `gave_up`, because a user's
  correction lands in the turn after the one that failed.
- **The open-episode set is seeded from the DB at boot.** It filters the hot
  path (every *successful* tool call); unseeded, rows left `open` by a restart
  can never close.
- **`SessionError{tool_error_loop}` carries `details.toolSig`.** Read that —
  parsing the human sentence in `message` breaks the first time it is reworded.

Evals can now **run** a case, not only score one: `OneShotOptions.trajectory_jid`
records a one-shot turn (`trajectory::force_enable`, process-local, never
written to `enabled.json`), and `scripts/evals/run.py` builds a fresh workspace
per case with its own `SENCLAW_TRAJECTORIES_DIR` — `HOME` deliberately stays
real, because the run needs the machine's model config. Cases grade the
**outcome** (`expect`: what the workspace and final answer look like
afterwards), not the tool path: a path-graded case fails an agent that found a
different correct route and passes one that made every right call and still
broke the file. See [docs/evals.md](docs/evals.md).

## Where a model-supplied path is resolved

`Read`/`Edit`/`Write`/`NotebookEdit` used to hand the input `file_path` straight
to `std::fs`, so a **relative** path resolved against the *daemon process* cwd
— which on a desktop install is the app bundle's `Contents/Resources` (the
Flutter supervisor starts the daemon next to its binary; confirmed with `lsof`
on a live daemon). `Write` therefore created files **inside the signed bundle**,
silently and successfully; `Read` either failed with "File not found" on a file
sitting in the project or, worse, *succeeded* against an unrelated file that
happens to exist there (`Cargo.toml`, `senclaw`, `mlx.metallib`). `Glob` and
`Grep` had the same bug for an explicit `path` argument.

That was not an exotic case: `Glob` **returns** paths relative to the working
dir and `Grep` displays them that way, so the `Glob` → `Read` loop always
failed — even though all four schemas ask for an absolute path.

Now: [`util::paths::resolve_in_workspace`](src/util/paths.rs) (`~` first,
absolute as given, relative joined onto the working dir, empty working dir left
alone), applied by [`run_tools::resolve_path_inputs`](src/zen_core/run_tools.rs)
**before anything reads the input**, over the fields each tool declares in
`Tool::path_fields()` — `file_path`, `notebook_path`, or `path`.

Rules for Claude:

- **Resolve in `run_tools`, not in the tool.** `validate_input` only *borrows*
  the input (`&Value` → `Result<(), String>`), so a resolution there reaches
  neither the permission layer nor `call`. And `gen_tool_permission` — which
  builds the approval card the user clicks — receives **no `ToolContext`**, so
  resolving inside `call` would have the user approving one path while another
  is written. The mirror mistake is as bad: fixing only `call` leaves
  `validate_input` refusing valid files, because it runs first.
- **The per-tool call is a deliberate second layer, not duplication.** It
  covers the two paths `run_tools` does not: tests that call a tool directly,
  and a `PreToolUse` hook returning `updated_input` with a relative path.
  Resolving twice is safe only because a **relative** `working_dir` is treated
  as no anchor at all and the path is returned untouched — join it and the
  result is still relative, so the second pass joins again (`"src"` +
  `"a.txt"` → `"src/a.txt"` → `"src/src/a.txt"`). `workspace_switch` persists
  whatever string it is handed, so that input shape is reachable.
- **A new tool that takes a path must declare `path_fields`, and a wrapper
  must forward it.** `AliasedTool` delegates everything else; when it did not
  forward this one, an aliased `Edit`/`Write` skipped the seam and the approval
  card showed the raw path while the tool's second layer wrote elsewhere. The
  in-tool layer means a missing declaration breaks *nothing that fails* — only
  `tools::path_field_tests` catches it.
- **Never resolve by field name across all tools.** An MCP tool's `path` may be
  a wiki page or a URL path; rewriting it as a filesystem path corrupts the
  call. That is why `path_fields` is declared per tool.
- **It resolves; it does not confine.** Nothing rejects `..`, on purpose: a
  chat's working dir defaults to the user's HOME, `/tmp` is a symlink to
  `/private/tmp`, and `Path::starts_with` does not normalise `..`
  (`/a/b/../x` starts_with `/a/b` is true) — a jail written that way refuses
  legitimate paths and stops nothing.
- **An error must name the resolved path.** "File not found: app.py" hides the
  directory, which was the entire bug.
- **Do not make producers emit absolute paths.** `Glob`/`Grep`/repo-map return
  relative by design, and the repo-map tests pin that shape.

Three findings from the same investigation, deliberately **not** fixed — full
notes in [docs/tool-path-resolution.md](docs/tool-path-resolution.md):
`allowed_paths` is a dead knob (stored, merged, editable in the Web UI, and
compared against nothing), and `NotebookEdit` emits `notebook_path`, which the
LSP hook's `data["path"] || data["file_path"]` lookup never reads.

The third one — an **alias** over `Edit`/`Write` escaping the file-edit prompt
entirely — **is fixed**: every permission classification now goes through
`Tool::permission_name()`, the name of the tool that will actually execute. As
`tool.name()`, an aliased `Edit` matched no branch of the checker at all and
landed on "other non-readonly tool, default allow", writing files with no
prompt. A wrapper tool must forward `permission_name` for the same reason it
forwards `path_fields`: neither omission fails anything — one silently skips
the resolution seam, the other silently skips the prompt.

## Control plane (JEV v2.2 P0/P1, `src/control_plane/`)

A typed decision layer above the existing decision gate and pre-skill router:
rule first, then Jev (a probability-answering decision model reached through
[`decision::client`](src/decision/client.rs) → the `decision` runtime), then
the LLM, then a human. Full design, switches and REST:
[docs/control-plane.md](docs/control-plane.md).

Rules for Claude:

- **`route.skill` and `tool.risk` in the spec registry are descriptive, not
  authoritative.** They mirror `decisionConfig.skills`/`decisionConfig.gate`
  live — change behaviour at `/api/decision/skills` / `/api/decision/gate` as
  before. `SpecRegistry::set_mode` refuses both on purpose; if it ever stops
  refusing, the registry and the real settings can silently diverge.
- **A new spec is shadow (never acts) until an eval proves it, and shadow
  itself is opt-in.** `controlPlane.shadow` (default off) gates whether any
  `shadow`-mode spec is called at all — a spec's own `mode: active` is the
  only thing that bypasses that gate. This is deliberate: a default install
  must not start `sen-sysone` (a 1.2–1.7 GB Laya load) on every chat turn just
  to collect labels. `SENCLAW_JEV_OFF=1` / `controlPlane.jevOff` beats both,
  for the ablation baseline.
- **The trace (`GET /api/traces`) is metadata only — spec, answer, confidence,
  band, token counts, tool names and error classes, never a message or a
  file's content.** The one exception is `controlPlane.recordDecisionInputs`
  (default off), which writes the exact `state`/`questions` sent to a spec to
  a *separate* file for G1 replay — never merged into the trace itself. Adding
  anything content-shaped to `control_plane::trace::Trace` breaks that
  guarantee for everyone, not just the caller who needed it.
- **`policy_gate::may_auto_approve`'s floor (`risk_tier >= 3 && !reversible`
  from `tool_registry`, or unparseable) is stricter than what the existing
  tool-call gate does today, on purpose.** The gate asks Jev per Bash command
  and skips the prompt only on a confident, per-call reversible verdict; the
  floor is the ceiling a *future*, less careful engine must respect, not a
  rewrite of the gate's own logic.
  `policy_gate::tests::the_existing_gate_never_exceeds_the_floor` is the
  non-regression check — if it ever fails, the gate started auto-approving
  something the floor disagrees with.
- **`<agent_status>` is appended to a per-call CLONE, never to the persisted
  history — and `controlPlane.agentStatus` defaults OFF.** It lands on a
  different message each call, so it defeats a local engine's exact-prefix KV
  reuse (every sen-mlx agent step re-prefilled the whole prompt). Keep it off
  by default. `zen_core::conversation::outgoing_messages` returns
  `Cow::Borrowed(messages)` untouched when `controlPlane.agentStatus` is off,
  or a cloned `Vec` with one `ContentBlock::Text` pushed onto the *last
  user-role* message when on — never mutates `messages` itself, which is what
  the loop returns, what compaction reads and what the trajectory records.
  `summarize_history` (compaction's own LLM call) deliberately does not go
  through this function. Both Anthropic and OpenAI adapters already render a
  trailing text block in a user message correctly with no provider-specific
  code (`query_llm.rs::anthropic_content_blocks` puts every block type in one
  content array; `openai_messages_for_api` turns a `Text` block *after*
  `ToolResult` blocks into a separate trailing `role:"user"` message,
  following the `role:"tool"` ones it just emitted) — verified by reading both
  before wiring this in, not assumed.
- **L1 offload (`workspace::write_artifact`) is gated on
  `controlPlane.workspace.substituteToolOutput` (default off) at the exact
  point in `zen_core::run_tools` where a tool's result string is about to
  become a `ContentBlock::ToolResult`.** The length check runs first and is
  in-memory only, so a normal-sized result never touches `config.json`. Keyed
  by `RunContext::agent_data_dir` (already on the struct — already unique per
  chat), not by adding a new field to `RunContext`/`ToolContext`, which
  CLAUDE.md's own "Code v2" section already flags as high blast radius.
- **Tool metadata (`tool_registry::metadata_for`) is a side table keyed by
  name, not new `Tool` trait methods** — the trait has ~30 implementors, and
  turning 9 descriptive fields into required methods would touch every one of
  them for a concern that changes nothing about what a tool does. Look a
  wrapped tool up by `Tool::permission_name()`, not `Tool::name()`, the same
  rule permission classification already follows.
