# SenClaw Runtime Protocol (v1)

The daemon links **no inference code**. Every engine — MLX, llama.cpp, the Laya
decision model, OCR, Whisper, TTS — is a *runtime*: a separate program the
daemon installs, launches as a child process and talks to over loopback HTTP,
the way LM Studio runs its engines. This document is the contract between the
daemon (`senclaw`), the runtimes (`sen-*`, upstream llama.cpp) and the clients
(`desktop`, `web-app`).

Code form of the contract: [`crates/sen-runtime-sdk`](../crates/sen-runtime-sdk)
(manifest types + validation, launch environment, server scaffold). When this
document and the SDK disagree, the SDK's tests are the tie-breaker — fix the
document.

## 1. Vocabulary

| Term | Meaning |
|---|---|
| **Runtime** | An engine program plus its `senclaw-runtime.json`, installed as a *package*. |
| **Package** | `~/.senclaw/runtimes/<id>/<version>/` — immutable once installed. |
| **Slot** | What a runtime is selected *for*: `gguf`, `mlx`, `gturbo` (model formats) or `decision`, `ocr`, `asr`, `tts`, `browser` (capabilities). One selection per slot. |
| **Mode** | `service` — one process per runtime, it manages its own models. `model` — one process per loaded model, passed on the command line. |
| **Process** | A running launch of a runtime, owned and supervised by the daemon. |

### Runtimes

| id | repo | type | slots | mode | capabilities | platforms | replaces (old repo) |
|---|---|---|---|---|---|---|---|
| `sen-mlx` | SenClaw/sen-mlx | llm-engine | mlx | model | chat, vision | darwin-arm64 | `apps/mlx-lm` + `apps/local-model-core` (Space App) |
| `sen-turbo-fieldfare` | SenClaw/sen-turbo-fieldfare | llm-engine | gturbo | model | chat, vision | darwin-arm64 | `turbo-fieldfare-senclaw` Space App |
| `llama.cpp-metal` | upstream ggml-org/llama.cpp | llm-engine | gguf | model | chat, embedding, vision | darwin-arm64 | `apps/candle`, candle `local-embed` |
| `llama.cpp-cpu` | upstream | llm-engine | gguf | model | chat, embedding, vision | darwin-x64, linux-x64, linux-arm64, windows-x64, windows-arm64 | — |
| `llama.cpp-vulkan` | upstream | llm-engine | gguf | model | chat, embedding, vision | linux-x64, linux-arm64, windows-x64 | — |
| `llama.cpp-cuda` | upstream | llm-engine | gguf | model | chat, embedding, vision | linux-x64, windows-x64 | — |
| `sen-sysone` | SenClaw/sen-sysone | decision | decision | service | decision | all six | `src/decision/laya`, `src/decision/online.rs` |
| `sen-ocr` | SenClaw/sen-ocr | ocr | ocr | service | ocr | darwin-arm64, linux-x64, windows-x64 | `src/local_model/ocr`, `ui_server/ocr.rs` |
| `sen-whisper` | SenClaw/sen-whisper | asr | asr | service | asr | darwin-arm64 | `crates/senclaw-media`, `media_sidecar.rs`, `ui_server/whisper.rs` |
| `sen-tts` | SenClaw/sen-tts | tts | tts | service | tts | all six | `src/tts`, `ui_server/tts.rs` |
| `sen-browser` | SenClaw/sen-browser | browser | browser | service | browser | all six (needs Chrome) | the extension's content-script executor, `src/browser` bridge |

The daemon keeps only *clients* of these engines: the decision gate and skill
router (control plane), OCR of images for text-only models, the `senclaw-ocr`
MCP server, voice endpoints and the local-model LLM providers — all over HTTP.

## 2. Packages

### 2.1 Layout

```
~/.senclaw/runtimes/
  settings.json                 selections, autoUpdate, channel, idle timeouts
  running.json                  processes the daemon launched (orphan cleanup)
  index-cache.json              last fetched runtime index
  <id>/<version>/               one installed package
    senclaw-runtime.json        written LAST — its absence = interrupted install
    bin/<program>               (or whatever entry.command names)
    …                           resources (mlx.metallib, onnxruntime dylib, …)
~/.senclaw/runtime-data/<id>/   persistent per-runtime data (settings.json, caches)
~/.senclaw/local-models/        shared model root (unchanged layout, see §6.1)
~/.senclaw/logs/runtimes/       <id>.log, <id>--<model-key>.log (5 MB, 1 rotation)
```

A second, read-only root is scanned for **bundled** packages: `SENCLAW_BUNDLED_RUNTIMES_DIR`,
else `<dir of current_exe>/runtimes/`. The desktop app ships default runtimes
there. A bundled package appears with `source: "bundled"`; a user-installed
copy of the same id+version wins.

### 2.2 Manifest — `senclaw-runtime.json`

camelCase JSON. Parsed and validated by `sen_runtime_sdk::manifest::RuntimeManifest::parse`.

```json
{
  "schemaVersion": 1,
  "id": "sen-ocr",
  "name": "SenClaw OCR",
  "version": "0.1.0",
  "description": "PaddleOCR PP-OCRv4/v5 on MNN",
  "type": "ocr",
  "slots": ["ocr"],
  "formats": [],
  "capabilities": ["ocr"],
  "platforms": ["darwin-arm64", "linux-x64", "windows-x64"],
  "accelerator": "metal",
  "mode": "service",
  "entry": {
    "command": "bin/sen-ocr",
    "args": ["serve", "--host", "{host}", "--port", "{port}"],
    "env": {},
    "capabilityArgs": {}
  },
  "health": { "path": "/health", "startupTimeoutSecs": 60 },
  "idleTimeoutSecs": 300,
  "api": { "openaiBase": "/v1" },
  "homepage": "https://github.com/SenClaw/sen-ocr",
  "releaseNotesUrl": "https://github.com/SenClaw/sen-ocr/releases/tag/v0.1.0",
  "license": "MIT"
}
```

Rules enforced by the SDK (all tested):

- `type` ↔ `slots` must agree; `llm-engine` ⇔ `mode: "model"` ⇔ non-empty `formats` covering its slots.
- Misspelt enum values and **unknown `{placeholders}`** are errors. Unknown top-level keys are *warnings* (surfaced in the Runtime screen).
- `entry.command` is relative to the package and may not contain `..` or be absolute: a package runs only what it ships.
- Placeholders: `{host} {port} {token} {data_dir} {models_dir} {package_dir} {parent_pid}`; model mode adds `{model_path} {model_id} {mmproj_path} {context_length}`. `{{`/`}}` are literal braces; JSON inside an argument is left alone.
- `capabilityArgs` (model mode) are appended, in key order, when the model being launched has that capability — llama-server gets `--embedding` for an embedding model and `--mmproj {mmproj_path}` for a vision model.
- `idleTimeoutSecs`: absent → daemon default for the mode; `0` → never stop.

### 2.3 Release archives

`<id>-<version>-<platform>.tar.gz` (`.zip` for Windows), containing the package
root (manifest at the top level of the archive or inside a single top-level
directory — installers accept both), plus `<archive>.sha256` in `shasum -a 256`
output format (`<hex>  <file name>`, so `shasum -a 256 -c` verifies it). Each
`sen-*` repo builds them with `make package` and publishes them on its GitHub
release `v<version>`. The catalog's `sha256` is that same digest; when a
package omits it, the installer downloads `<archive>.sha256` from the release
and refuses to extract if that file is missing or does not match.

## 3. Launch

### 3.1 Environment

Set by the daemon on every launch (constants in `sen_runtime_sdk::env`), applied
after the manifest's own `entry.env` so they always win:

| Variable | Value |
|---|---|
| `SENCLAW_RUNTIME_ID` / `SENCLAW_RUNTIME_VERSION` | the package |
| `SENCLAW_RUNTIME_HOST` | `127.0.0.1` (always) |
| `SENCLAW_RUNTIME_PORT` | free loopback port picked for this launch |
| `SENCLAW_RUNTIME_TOKEN` | 32 random bytes, hex, new per launch |
| `SENCLAW_RUNTIME_DATA_DIR` | `~/.senclaw/runtime-data/<id>/` |
| `SENCLAW_LOCAL_MODELS_DIR` | `~/.senclaw/local-models/` |
| `SENCLAW_PARENT_PID` | the daemon's pid |
| `SENCLAW_CONFIG_PATH` | the daemon's `config.json` (read-only; §8) |
| `SENCLAW_HOME` | `~/.senclaw` |
| `SENCLAW_MODEL_PATH` / `SENCLAW_MODEL_ID` | model mode only |

Every variable has a standalone default in `LaunchEnv::from_env`, so a runtime
can be started by hand for development (`sen-ocr serve --port 4960`): no token →
no auth, no parent → no watchdog.

### 3.2 Lifecycle (daemon side)

1. **Resolve** the package for the slot (service) or the model's format slot (model).
2. **Port + token**: bind `127.0.0.1:0`, read the port, release it, pass it on.
3. **Spawn** `entry.command` with rendered `args`, cwd = package dir, stdout+stderr → the log file.
   Record `{pid, port, id, version, key, command, startedAt}` in `running.json`.
4. **Health gate**: `GET health.path` every 250 ms until 200, within `startupTimeoutSecs`.
   500 = failed at once. A process that exits before healthy = failed; its log tail is in the error.
5. **Serve**: every request is proxied with `Authorization: Bearer <token>`; `lastUsedAt` is touched when a request *starts*.
6. **Idle stop** after `idleTimeoutSecs` without a request (sweep every 10 s, re-checked under the lock; an in-flight request keeps the process).
   Defaults: service 300 s, model 900 s.
7. **Stop**: `POST /runtime/shutdown` (upstream llama.cpp: skip), `SIGTERM` after 3 s, `SIGKILL` after 10 s.
   Daemon shutdown stops every process it launched.
8. **Orphans**: at daemon start, each `running.json` entry whose pid is alive **and** whose command line still matches is stopped, then the file is reset. Runtimes built on the SDK also exit on their own within ~2 s of the daemon disappearing (parent watchdog).
9. **Concurrency**: a start is single-flight per process key (every waiter awaits the same start); a crash (process exit while in use) is recorded, counted in `launches`, and the next request starts it again.

Process keys: `service:<runtime-id>` and `model:<model-key>`.

## 4. Runtime HTTP API

### 4.1 Common — every runtime

Provided by `sen_runtime_sdk::server::serve` for `sen-*` runtimes.

| Route | Auth | Body |
|---|---|---|
| `GET /health` | none | `{"status":"ok"\|"loading"\|"failed","id","version"}` — 200 / 503 / 500 |
| `GET /runtime/info` | bearer | `{"id","version","mode","capabilities","pid","detail":{…}}` |
| `POST /runtime/shutdown` | bearer | 202, graceful exit |

- **Bind loopback only** (non-loopback refused unless `SENCLAW_RUNTIME_ALLOW_REMOTE=1`, which the daemon never sets).
- **Bearer auth on every route except `/health`** when `SENCLAW_RUNTIME_TOKEN` is set (constant-time compare). Upstream llama-server gets the same token through `--api-key {token}`.
- **Errors** are `{"error": "<message>", "code"?: "<code>"}` — the daemon's own error shape, so clients reading `body.error` keep working. Codes in `sen_runtime_sdk::api::codes`.
- Set `DefaultBodyLimit` explicitly on upload routes (axum's default is 2 MB): OCR images and audio are larger.

### 4.2 LLM engines (model mode)

OpenAI-compatible, relative to `api.openaiBase` (`/v1`):
`GET /v1/models`, `POST /v1/chat/completions` (JSON and SSE, tools, `stream_options.include_usage`),
`POST /v1/embeddings` (embedding models). `/health` answers **503 while the model loads** and 200 once it can generate.

- **sen-mlx**: `sen-mlx serve --host {host} --port {port} --model {model_path}`; loads the model at startup (`Readiness::loading` → `set_ready`), serves exactly that model. Sampling/KV settings come from the shared `<models_dir>/settings.json` (snake_case, the file `local-model-core` used; never rename its fields). Vision (Gemma-4) supported as before.
- **sen-turbo-fieldfare**: `sen-turbo-fieldfare serve --host {host} --port {port} --model {model_path} --max-context {context_length}`. The process spawns `TurboFieldfareServer` (Swift + Metal, from [drumih/turbo-fieldfare](https://github.com/drumih/turbo-fieldfare)) against one completed `.gturbo` directory and proxies `/v1`. `/health` is 503 until that engine binds. `{context_length}` is snapped to 4096, 8192, 16384, 32768, or 65536. The OpenAI `model` field must be the daemon's model key (`SENCLAW_MODEL_ID`). A sibling `<stem>.vision.gturbo` pack enables image input.
- **llama.cpp**: `llama-server -m {model_path} --host {host} --port {port} --api-key {token} -c {context_length}` + `capabilityArgs`. The daemon generates this manifest at install time (§7.2).

### 4.3 Decision — `sen-sysone` (service)

Serves the old daemon REST namespace **verbatim** — same paths, bodies, status codes as `src/gateway/ui_server/decision.rs` before the split:

`GET /api/decision/models` · `POST /api/decision/models/custom` · `POST /api/decision/models/import` ·
`POST /api/decision/models/:id/download` · `POST /api/decision/models/:id/cancel` ·
`POST /api/decision/models/:id/load` · `POST /api/decision/models/:id/unload` ·
`DELETE /api/decision/models/:id` · `POST /api/decision/ask` ·
`GET|PUT /api/decision/settings` · `POST /api/decision/online/test`

plus `POST /v1/systemone` (Jev-compatible `{model?, state, questions}` → same answer as `/api/decision/ask`).

- Owns backend selection (local Laya / online Jev: TypeSafe, Cloudflare, custom) and its API key. `GET /api/decision/settings` masks the key; the key-scope rules (`same_key_scope`, `clearApiKey`) are unchanged.
- `settings` here covers `backend`, `local`, `online` only. **`gate` and `skills` stay in the daemon** (control plane) — see §5.2.
- **Key order is part of the contract**: requests are parsed with the order-preserving `Json` type, answers serialized from `AskResponse` directly, never through `serde_json::Value`/`json!`.
- Laya checkpoints stay under `<models_dir>/laya/` (existing downloads are reused).

### 4.4 OCR — `sen-ocr` (service)

Old namespace verbatim (`ui_server/ocr.rs`): `GET /api/ocr/models` · `GET|PUT /api/ocr/settings` ·
`POST /api/ocr/models/:id/download` · `POST /api/ocr/models/custom` · `GET /api/ocr/models/:id/status` ·
`POST /api/ocr/models/:id/cancel` · `DELETE /api/ocr/models/:id` · `POST /api/ocr/recognize`.
Models stay where they were (§6.1).

### 4.5 Speech to text — `sen-whisper` (service)

Old namespace verbatim (`ui_server/whisper.rs`): `GET /api/whisper/models` · `GET|PUT /api/whisper/settings` ·
`GET /api/whisper/models/:id/validate` · `POST /api/whisper/models/:id/download` · `GET /api/whisper/models/:id/status` ·
`POST /api/whisper/models/:id/cancel` · `DELETE /api/whisper/models/:id` · `POST /api/whisper/transcribe`,
plus OpenAI `POST /v1/audio/transcriptions` (multipart `file`, `model?`, `language?`, `response_format` json|text).
Symphonia probes by extension: keep the `filename` → temp-file extension rule. Weights drop after each use (as the sidecar did); the process may stay up.

### 4.6 Text to speech — `sen-tts` (service)

Old namespace verbatim (`ui_server/tts.rs`): `GET /api/tts/models` · `GET|PUT /api/tts/settings` ·
`GET /api/tts/models/:id/validate` · `POST /api/tts/models/:id/download` · `GET /api/tts/models/:id/status` ·
`POST /api/tts/models/:id/cancel` · `DELETE /api/tts/models/:id` · `POST /api/tts/synthesize`,
plus OpenAI `POST /v1/audio/speech` (`{model?, input, voice?, response_format: "wav", speed?}`).
A removed voice still degrades to the macOS `say` preset with `X-TTS-Fallback` / `fallback_reason` — never a 400.

### 4.7 Browser — `sen-browser` (service)

The *hands and eyes* of the browser engine v2: it observes a page, executes one guarded action with trusted CDP input,
and hands a tab to the person. It never sees the goal and never calls a model — the decision loop (Jev + LLM) is the
daemon's `src/browser_agent`. Two drivers share one core: **managed** (a Chrome it launches with its own profile under
`<data_dir>/profiles/<name>`) and **extension** (the person's Chrome through the SenClaw extension's `chrome.debugger`,
relayed by the daemon — the extension connects to the daemon's WS gateway at `/browser/ext`, never to the runtime).

`GET /v1/status` · `GET /v1/scripts` · `GET|POST /v1/sessions` · `DELETE /v1/sessions/:sid` ·
`GET|POST /v1/sessions/:sid/tabs` · `DELETE /v1/tabs/:tid` · `POST /v1/tabs/:tid/{observe,act,navigate,read,screenshot}` ·
`POST|DELETE /v1/tabs/:tid/handover` · `GET /v1/drivers/extension` (WebSocket, the daemon's relay).

An action names an id from the latest observation; it is refused before any input when the page changed
(`stale_page`, 409), another element covers the target (`target_covered`, 409) or the id is unknown
(`invalid_target`, 422). A mutation is never retried (`uncertain_mutation`, 500). Only approved scripts run in a page,
in an isolated world; their SHA-256 bundle (`sen-browser scripts`) is what the extension enforces. Full API and
safety model: the `sen-browser` README.

## 5. Daemon REST API

All routes below are served by the daemon on its UI port and go through its normal API auth.

### 5.1 Runtime management — `/api/runtimes`

| Route | Purpose |
|---|---|
| `GET /api/runtimes` | platform, settings, slots with selections, installed runtimes, processes |
| `GET /api/runtimes/catalog?refresh=1` | runtime index for the current channel, merged with install state |
| `POST /api/runtimes/check-updates` | fetch the index; with `autoUpdate`, start installs for updates of *selected* runtimes |
| `POST /api/runtimes/install` `{id, version?}` | 202 `{jobId, id, version}` — background download + install |
| `POST /api/runtimes/install-local` `{path}` | install from a directory holding `senclaw-runtime.json`, or a `.tar.gz`/`.zip`; `source: "local"` |
| `GET /api/runtimes/jobs` · `GET /api/runtimes/jobs/:jobId` · `POST /api/runtimes/jobs/:jobId/cancel` | install progress |
| `DELETE /api/runtimes/:id/versions/:version?force=1` | uninstall; 409 while running unless `force` (stops it first) |
| `PUT /api/runtimes/selections` `{slot, id\|null, version\|null}` | select (version null = newest installed); returns the `GET /api/runtimes` body |
| `GET\|PUT /api/runtimes/settings` `{autoUpdate, channel, idleTimeoutSecs:{service, model}}` | |
| `POST /api/runtimes/slots/:slot/start` | warm up a service slot; returns the process |
| `POST /api/runtimes/processes/:key/stop` | stop one process (key URL-encoded) |
| `GET /api/runtimes/:id/logs?lines=200` | `{path, lines:[…]}` |

`GET /api/runtimes`:

```json
{
  "platform": "darwin-arm64",
  "settings": { "autoUpdate": true, "channel": "stable", "idleTimeoutSecs": { "service": 300, "model": 900 } },
  "slots": [
    { "slot": "gguf", "label": "GGUF", "kind": "format",
      "selected": { "id": "llama.cpp-metal", "version": "b11201", "name": "Metal llama.cpp" },
      "candidates": [ { "id": "llama.cpp-metal", "version": "b11201", "name": "Metal llama.cpp" } ] },
    { "slot": "ocr", "label": "OCR", "kind": "capability", "selected": null, "candidates": [] }
  ],
  "installed": [
    { "id": "llama.cpp-metal", "name": "Metal llama.cpp", "version": "b11201", "versions": ["b11201"],
      "type": "llm-engine", "slots": ["gguf"], "formats": ["gguf"], "capabilities": ["chat", "embedding", "vision"],
      "platforms": ["darwin-arm64"], "accelerator": "metal", "mode": "model", "compatible": true,
      "source": "index", "description": "…", "releaseNotesUrl": "…", "warnings": [] }
  ],
  "processes": [
    { "key": "model:gguf-qwen2-5-7b-instruct-q4-k-m-1a2b3c4d", "runtimeId": "llama.cpp-metal", "version": "b11201",
      "slot": "gguf", "modelKey": "gguf-qwen2-5-7b-instruct-q4-k-m-1a2b3c4d", "pid": 1234, "port": 41234,
      "state": "ready", "startedAt": 1790000000000, "lastUsedAt": 1790000100000, "launches": 1, "error": null }
  ]
}
```

`state`: `starting | ready | stopping | failed`. A process appears as `starting` **as soon as it is launched** (while the
health gate runs), so clients can show a load in progress. `version` in `installed` is the newest installed; `versions`
lists all, newest first. Versions compare numerically (`b11201` > `b9999`; semver for `0.x.y`), never as plain strings.
A slot with nothing selected but exactly one compatible candidate installed auto-selects it on first use.

**One process object everywhere** — `GET /api/runtimes` `processes[]`, `POST /api/local-models/:key/load`,
`POST /api/runtimes/slots/:slot/start` and `LocalModel.process` all carry the same fields:
`{key, runtimeId, version, slot, modelKey|null, pid, port, state, startedAt, lastUsedAt, launches, error|null}`.

**Selections.** `PUT /api/runtimes/selections {slot, id|null, version|null}`: `id: null` clears the slot;
`version: null` means *track the newest installed version of `id`* and is what clients send by default; an explicit
`version` pins the slot. When an auto-update install of the selected runtime finishes, a pinned slot moves to the new
version once no process of the old version is in use (§7.3); a `null` slot follows automatically.

**Settings.** `PUT /api/runtimes/settings` is a partial merge of camelCase fields — any subset of
`{autoUpdate, channel, idleTimeoutSecs: {service?, model?}}` — and answers the full settings object.

**Other bodies.** `POST /api/runtimes/install` and `POST /api/local-models/download` answer **202**.
`POST /api/runtimes/install-local` answers 200 with the installed entry (the same shape as an element of `installed[]`).
`POST /api/runtimes/check-updates` answers `{checkedAt, channel, updates: [{id, from, to}], started: [{jobId, id, version}], error|null}`.
`GET /api/runtimes/:id/logs?lines=200&key=<process key>` reads that process's log; without `key` it reads the runtime's
most recent log (`<id>.log` for a service, the newest `<id>--<model>.log` for a model runtime).

`GET /api/runtimes/catalog`:

```json
{ "channel": "stable", "fetchedAt": 1790000000000, "source": "https://…/index.json", "error": null,
  "entries": [
    { "id": "sen-ocr", "name": "SenClaw OCR", "description": "…", "type": "ocr", "slots": ["ocr"], "formats": [],
      "capabilities": ["ocr"], "accelerator": "metal", "platforms": ["darwin-arm64", "linux-x64", "windows-x64"],
      "compatible": true, "available": true, "latestVersion": "0.1.0", "installedVersion": null,
      "updateAvailable": false, "releaseNotesUrl": "…", "downloadSize": 18000000 }
  ] }
```

`available: false` = listed but no package published for this platform/channel yet (install from a local build).
`available` is `compatible` **and** a package exists for this platform on this channel — for an `upstream` entry, an
asset mapped for this platform counts. `error` carries the fetch failure when the remote index could not be read (the
response then comes from the cache or the bundled copy, named in `source`); `releaseNotesUrl` and `downloadSize` come
from the channel's release (upstream llama.cpp: the release page URL and the asset size from the release API).

Install job: `{jobId, id, version, state: "queued"|"downloading"|"verifying"|"extracting"|"done"|"failed"|"cancelled", receivedBytes, totalBytes|null, percent|null, error|null, startedAt, finishedAt|null}`.

### 5.2 Legacy namespaces → slot runtimes

`/api/ocr/*` → `ocr`, `/api/tts/*` → `tts`, `/api/whisper/*` → `asr`, `/api/decision/*` → `decision`,
**except** the control-plane routes the daemon keeps: `GET|PUT /api/decision/gate`, `POST /api/decision/gate/check`,
`GET|PUT /api/decision/skills`, `POST /api/decision/skills/check`. `GET /api/decision/settings` is proxied and the
daemon adds its `gate` and `skills` into `settings` of the response so existing clients render unchanged; `PUT` forwards
only `backend/local/online` and keeps the stored gate.

The proxy starts the slot's runtime on demand, forwards method, path, query, body and response (streamed;
no total timeout — read timeout only), replacing any client `Authorization` with the runtime token. A runtime's own
error passes through with **its** status and body unchanged — the proxy never rewraps it (the decision-settings merge
only touches a 2xx body).
No runtime for the slot → **503** `{"error": "No OCR runtime is installed. Install one in Settings → Runtime.", "code": "runtime_not_installed", "slot": "ocr"}`
(`runtime_not_selected` when installed but unselected, `runtime_start_failed` with the log tail when it would not start).
Clients show that state with a link to Settings → Runtime.

`/api/browser/*` → `browser` is **GET only** (status, tabs): acting on a page goes through the daemon's own
`/api/browser-agent/*`, which applies the browser engine's risk tiers — a write pass-through would skip them.
`sen-browser` serves only `/v1/*`, so this one namespace is stripped: `GET /api/browser/v1/sessions` reaches the
runtime's `GET /v1/sessions`.

### 5.3 Local models — `/api/local-models`

| Route | Purpose |
|---|---|
| `GET /api/local-models` | `{root, models:[LocalModel], downloads:[Download]}` |
| `GET /api/local-models/hf-files?repo=` | `{repo, format: "gguf"\|"mlx"\|"unknown", files:[{name, size, quant\|null, mmproj}]}` |
| `POST /api/local-models/download` `{repo?, file?, mmproj?, revision?, format?, vision?}` | 202 `{downloadId}` — GGUF needs `file`; MLX downloads the snapshot; `format: "gturbo"` runs `TurboFieldfareRepack` from the installed `sen-turbo-fieldfare` package for `mlx-community/gemma-4-26b-a4b-it-4bit` at revision `0d77464eeb233a2da68ebf9d7dc4edaac7db956d` into `gemma4.gturbo` (`vision: true` installs the image sibling). Any other repo or revision is rejected. |
| `GET /api/local-models/downloads` · `GET …/downloads/:id` · `POST …/downloads/:id/cancel` | progress |
| `DELETE /api/local-models/:key?force=1` | 409 while loaded unless `force` |
| `POST /api/local-models/:key/load` `{contextLength?}` | start (or reuse) its process; returns the process |
| `POST /api/local-models/:key/unload` | stop it |
| `GET\|PUT /api/local-models/settings` | `{defaultContextLength, engine:{…shared settings.json, snake_case…}}` |

`LocalModel`: `{key, name, format, path, sizeBytes, capabilities, vision, embedding, mmprojPath, quant, repo,
contextLength, runtime:{slot, selected|null}, process|null}`. `name` is human-readable (the repo id for a snapshot, the
file name without `.gguf` for a GGUF file — never the bare architecture). Download: `{downloadId, repo, files, format,
state, receivedBytes, totalBytes, percent, error, startedAt, finishedAt}` with `state` one of
`queued | listing | downloading | done | failed | cancelled` (the first three are active).

- `POST /api/local-models/:key/load` takes an **optional** body — none, `{}`, or `{contextLength}` — and answers the
  process object once it is ready. A failure answers the same structured 503 as the proxy (`code`, `slot`, `error`).
- Launch context: the request's `contextLength`, else `min(model maximum, defaultContextLength)`, where
  `defaultContextLength` defaults to **32768** — never the model's full maximum by default (llama-server would allocate
  a 128K KV cache), and not 8K either: an agent turn opens with ~15.7K tokens of system prompt + tool schemas, so an 8K
  window fails every agent turn on a local model. The `local:` LLM config reports the effective value as its
  `contextLength`.
- `GET|PUT /api/local-models/settings` is `{defaultContextLength, engine}` (camelCase outside, `engine` passed through
  snake_case untouched); `PUT` accepts a partial body and keeps what it does not name.
- Deleting a GGUF file keeps a `mmproj-*.gguf` that another model in the same folder still uses.

Model keys are URL-safe and stable: `<format>-<slug>-<8 hex of sha256(format + relative path)>`.

### 5.4 Models as LLM providers

Every chat-capable local model whose format slot has a runtime appears in the model picker as config id
`local:<key>`, adapter `openai`, empty `api_key`, base URL
`http://127.0.0.1:<ui_port>/api/runtimes/models/<key>/v1`. That route loads the model on first use
(JIT, like a session Space App) and proxies to its process. Rules carried over from app providers:
merged inside `load_llm_configs` (the single seam), never written to `config.json` (`save_llm_config`
refuses `local:`), the empty `api_key` is exempted wherever configs are filtered, **no total request
timeout** for a loopback provider. `vision` comes from the model (GGUF: an mmproj file; MLX: `vision_config`
in `config.json`), never from its name.

### 5.5 Embeddings

The memory/cognitive `local` embedding provider calls a GGUF embedding model through the same
`/api/runtimes/models/<key>/v1/embeddings` route. No candle in the daemon. In `/api/embedding-config`, the local
provider's `modelName` is the **bare model key** (a leading `local:` is accepted and stripped); an empty `modelName`
means "not chosen yet". `GET /api/embedding/features` → `{local, modelsDir}`; `GET /api/embedding/models` lists
installed embedding-capable local models `{key, name, repo, quant, sizeBytes, contextLength}`;
`POST /api/embedding/download-model` answers 410 pointing at `POST /api/local-models/download`.

## 6. Models on disk

### 6.1 Layout (unchanged — nothing is re-downloaded)

- MLX / safetensors snapshots: `<local-models>/<org>__<repo>/` (the `local-model-core` layout).
- GGUF: `<local-models>/gguf/<org>__<repo>/<file>.gguf` (+ `mmproj-*.gguf` beside it).
- TurboFieldfare: `<local-models>/<name>.gturbo/` (`manifest.json` magic `GTURBO` plus `model_weights.bin`). The optional image pack is the sibling `<name>.vision.gturbo/` and is not listed as its own model.
- Engine-private stores stay exactly where the in-daemon engines put them, and each runtime derives the same
  defaults from `SENCLAW_HOME` and honors the same override variables the daemon's `Config` read:
  `<local-models>/laya/` (Laya), `<local-models>/hf-cache/`, `SENCLAW_WHISPER_MODELS_DIR` (default
  `~/.senclaw/whisper-models`) and any whisper snapshot the old code resolved under `<local-models>`,
  `SENCLAW_TTS_MODELS_DIR` (`~/.senclaw/tts-models`), `SENCLAW_OCR_MODELS_DIR` (`~/.senclaw/ocr-models`).
  The daemon passes these variables through to runtimes when set. The model library never lists
  engine-private folders (laya, hf-cache, whisper/ASR checkpoints) as LLMs.

## 7. Runtime index and updates

### 7.1 Index

`runtimes/index.json` in the senclaw repo; fetched from `SENCLAW_RUNTIME_INDEX_URL`
(default `https://raw.githubusercontent.com/SenClaw/senclaw/main/runtimes/index.json`, `file://` allowed),
cached in `index-cache.json`, with the bundled copy compiled into the daemon as the offline fallback.

```json
{
  "schemaVersion": 1,
  "runtimes": [
    { "id": "sen-ocr", "name": "SenClaw OCR", "type": "ocr", "description": "…",
      "slots": ["ocr"], "formats": [], "capabilities": ["ocr"], "accelerator": "metal",
      "channels": { "stable": "0.1.0", "beta": "0.1.0" },
      "releases": [
        { "version": "0.1.0", "notesUrl": "…",
          "packages": [ { "platform": "darwin-arm64", "url": "https://github.com/SenClaw/sen-ocr/releases/download/v0.1.0/sen-ocr-0.1.0-darwin-arm64.tar.gz", "sha256": "…", "size": 18000000 } ] } ] },
    { "id": "llama.cpp-metal", "name": "Metal llama.cpp", "type": "llm-engine", "description": "…",
      "slots": ["gguf"], "formats": ["gguf"], "capabilities": ["chat", "embedding", "vision"], "accelerator": "metal",
      "channels": { "stable": "b11201", "beta": "latest" },
      "upstream": { "kind": "llama.cpp", "repo": "ggml-org/llama.cpp", "binary": "llama-server",
                    "assets": { "darwin-arm64": { "asset": "llama-{version}-bin-macos-arm64.tar.gz" } } } }
  ]
}
```

Only real, published packages are listed — an unpublished `sen-*` runtime has `releases: []` and shows as
`available: false`. `channels.<name>` = the version that channel installs; `"latest"` (upstream only) =
newest `b<digits>` release carrying the asset.

### 7.2 llama.cpp (upstream builds)

Assets come from `https://github.com/ggml-org/llama.cpp/releases` (prereleases tagged `b<N>`; the
"latest" release is not a build). SHA-256 comes from the release API's asset `digest`. After extraction the
installer finds `llama-server` (`.exe` on Windows) in the tree and **generates** `senclaw-runtime.json`:
mode `model`, command = the found path, args
`["-m","{model_path}","--host","{host}","--port","{port}","--api-key","{token}","-c","{context_length}"]`,
`capabilityArgs {"embedding":["--embedding"],"vision":["--mmproj","{mmproj_path}"]}`, health `/health`
with `startupTimeoutSecs: 600`. Windows CUDA needs the `cudart-llama-bin-win-cuda-*.zip` asset extracted into
the same directory (`extraAssets`).

### 7.3 Updates

LM Studio semantics: the channel (`stable`/`beta`) picks versions; **Check for updates** refreshes the index;
with **auto-update** on, updates of runtimes that fill a slot are installed in the background and the slot
moves to the new version once no process of the old one is in use (the old version stays installed until
the user removes it). With **auto-update off**, a pinned slot never moves on its own, even when a newer
version is already installed alongside it — a user who wants to stay on an older version (e.g. to roll back a
regression) turns auto-update off; turning it back on is what lets that slot advance again.

## 8. Settings migration

Before the split the daemon kept engine settings in `config.json` (`ocrConfig`, `ttsConfig`, `whisperConfig`,
`decisionConfig`). On first start a runtime seeds `<data_dir>/settings.json` from its old key
(`sen_runtime_sdk::legacy::load_or_import`) and owns it from then on. The daemon never writes those keys
again; it still reads `decisionConfig.gate` and `decisionConfig.skills` (control plane) and must preserve
the other sub-keys when it saves them.

## 9. Security rules

- Runtimes bind loopback; every route but `/health` needs the per-launch token; the daemon strips client
  credentials and injects the token. Runtime ports are never exposed to clients — clients reach runtimes
  only through the daemon, behind its own auth.
- Downloads are HTTPS; SHA-256 is verified when the index provides it; extraction rejects absolute paths,
  `..`, and links escaping the package; the package is extracted to a temp dir, the manifest written or
  checked last, then renamed into place.
- `install-local` copies (never symlinks) into the runtimes dir.
- A package executes only `entry.command` inside its own directory.
- Tests never touch the real `~/.senclaw` or ports 18788/18789/18790: they set `HOME` (or the explicit
  `SENCLAW_*` path variables) to a temp dir and use ephemeral ports.
