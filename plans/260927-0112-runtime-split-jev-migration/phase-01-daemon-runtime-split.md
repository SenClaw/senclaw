# Phase 01 — Daemon runtime split (`senclaw/`)

Owner: one agent. Repo: `/Users/benji/Projects/SenClaw/senclaw` (only). Contract: `docs/runtime-protocol.md` §2, §3, §5, §6, §7, §8, §9.

## Context

The migrated daemon compiles and is the old code at `f0f31bd`. It still contains every in-process engine. This phase
removes them and adds the runtime manager, so the daemon keeps only HTTP clients of the engines. Phases 02–05 build
the runtimes and clients against the same contract in parallel; phase 06 (control plane) starts from your result.

## Remove (with their tests; the runtimes carry them over from the old repo)

- `src/tts/`, `src/local_model/`, `src/media_sidecar.rs`, `src/decision/laya/`, `src/decision/online.rs`.
- Engine code in `src/gateway/ui_server/{ocr,tts,whisper,decision,hf_validate,embedding_models}.rs` — keep only what the
  daemon still serves (decision gate/skills routes, embedding config UI routes rewired to runtimes).
- Candle local embedding in `src/memory/` (`local-embed*`), OCR engine calls, whisper/tts settings persistence.
- Cargo features `local-embed`, `local-embed-metal`, `tts-vieneu`, `decision-laya`, `ocr-paddle`, `ocr-paddle-metal`; deps
  `candle-*`, `ort`, `ocr-rs`, `memmap2`, `bytemuck`, and `tokenizers`/`minijinja*`/`image`/`fancy-regex` **only if** nothing
  else uses them; unused `[workspace.dependencies]` (mlx-rs, mlx-sys, symphonia, rubato, realfft, half, safetensors).
- `examples/ocr_roundtrip.rs` and the `[[example]]` entry; docs that moved to runtimes
  (`laya-decisions.md` → sen-sysone; `gemma4-local-optimizations.md`, `mlx-resource-benchmark.md`,
  `mlx-rs-turboquant-native-runtime.md`, `local-gemma-mlx-runtime.md`, `local-model-space-app-extraction.md` → sen-mlx).
  Runtime agents copy them from the old repo; you delete them here.

## Keep in the daemon (clients + control plane)

- `src/decision/{gate, skill_route, json, types, settings}`: the gate and skill router are control plane (phase 06 moves
  them under `control_plane`). `settings` shrinks to the daemon-owned part (`gate`, `skills`); read `decisionConfig` as
  JSON and preserve `backend/local/online` sub-keys untouched when saving (they seed sen-sysone, §8).
- New `decision::client`: `ask(request_json_text) -> AskResponse` over HTTP to the `decision` slot
  (`POST /api/decision/ask`), key order preserved end to end (build with `decision::json`, never `serde_json::Value`).
  Gate (8 s) and router (`ROUTE_TIMEOUT` 1.5 s) timeouts unchanged; an unavailable runtime = engine error path
  (gate shows the prompt, router falls back to keywords) — never a hard failure.
- OCR for text-only models (`agent_pool/pool.rs::ocr_images`) → `runtime` client → `POST /api/ocr/recognize` on the
  `ocr` slot; no runtime → no text → the existing "OCR yielded nothing, don't guess" prompt path.
- `mcp/ocr_server.rs` (it already calls the daemon REST) keeps working through the proxy.

## Add

1. **`src/runtime/`** — runtime manager (depends on `sen-runtime-sdk` with `default-features = false`):
   `store` (scan user + bundled roots, install from dir / `.tar.gz` / `.zip` with traversal guards, uninstall),
   `settings` (`runtimes/settings.json`), `index` (fetch/cache/bundled `runtimes/index.json` via `include_str!`, channels),
   `llamacpp` (upstream resolver: GitHub releases API, `b<N>` tags, asset per platform, sha256 from `digest`,
   generated manifest §7.2, `extraAssets`), `jobs` (install jobs + progress + cancel), `supervisor` (spawn, env §3.1,
   health gate, single-flight start, idle sweep, crash accounting, logs with rotation, `running.json` orphan cleanup,
   stop all on shutdown), `proxy` (axum handlers: legacy namespaces §5.2 incl. decision settings merge, model route
   `/api/runtimes/models/:key/*`), `clients` (internal typed calls), `updates` (check + auto-update §7.3), REST §5.1.
2. **`runtimes/index.json`** — real entries only: `llama.cpp-{metal,cpu,vulkan,cuda}` (stable `b11201`, beta `latest`;
   asset names verified from the b11201 release: `llama-{version}-bin-macos-arm64.tar.gz`, `…-macos-x64.tar.gz`,
   `…-ubuntu-x64.tar.gz`, `…-ubuntu-arm64.tar.gz`, `…-ubuntu-vulkan-x64.tar.gz`, `…-ubuntu-vulkan-arm64.tar.gz`,
   `…-ubuntu-cuda-12.8-x64.tar.gz` (+ `cudart-llama-{version}-bin-ubuntu-cuda-12.8-x64.tar.gz`), `…-win-cpu-x64.zip`,
   `…-win-cpu-arm64.zip`, `…-win-vulkan-x64.zip`, `…-win-cuda-12.4-x64.zip` (+ `cudart-llama-bin-win-cuda-12.4-x64.zip`));
   the five `sen-*` with `releases: []` until published. The macOS tarball extracts to `llama-b11201/llama-server` + dylibs.
3. **`src/local_models/`** — model library §5.3/§6: scan (MLX dirs with `config.json` + safetensors; GGUF files; skip
   engine-private folders), capabilities (chat / embedding / vision from GGUF metadata or `mmproj` presence; MLX
   `config.json`), stable keys, HF downloads (port `apps/local-model-core/src/{download,store}.rs` from the old repo;
   resumable, progress, cancel), `hf-files`, shared `settings.json` passthrough (snake_case — never `rename_all`), load/unload
   through the supervisor.
4. **LLM configs**: `local:<key>` merged in `gateway/group_manager/llm.rs::load_llm_configs`; `save_llm_config` refuses
   `local:`; extend every `is_app_config` empty-key exemption to local configs; map a saved
   `app:mlx-lm:<model>` active id to the matching `local:` model when one exists (else leave it). No total timeout.
5. **Embeddings**: memory's `local` provider → OpenAI embeddings on a GGUF embedding model via the model route; the
   embedding settings REST lists local embedding-capable models (keep route paths; document shape changes in your report).
6. **CLI** `senclaw runtime list | install <id>[@version] | install-local <path> | uninstall <id> <version> | select <slot> <id>[@version] | update | logs <id>`
   (talks to the running daemon's REST, or operates on the store directly when no daemon answers — say which in help).
7. **Config/paths**: `runtimes_dir`, `runtime_data_dir`, `runtime_logs_dir`, bundled runtimes dir (`SENCLAW_BUNDLED_RUNTIMES_DIR`
   else `<exe_dir>/runtimes`), index URL — through `Config::from_env()` only.
8. **Boot/shutdown**: wire the manager into `run_daemon` (orphan cleanup at boot, idle sweeper, stop-all on shutdown).
9. **Web dist**: dev fallback `CARGO_MANIFEST_DIR/../web-app/dist` (sibling repo) after `SENCLAW_WEB_DIST` and cwd.
10. **Distribution**: `cli/commands/distrib.rs` — no media sidecar download or bundle requirement, bundle check = daemon only,
    repos `SenClaw/senclaw` (daemon), `SenClaw/web-app` (web dist asset). `Makefile`: daemon targets only (desktop targets
    move to the desktop repo). `Dockerfile`: no model features.
11. **Tests**: unit tests per module (manifest selection & slot rules, archive traversal guard, env/args rendering, proxy
    path mapping + auth header replacement + legacy decision exceptions, idle/crash/single-flight logic, orphan file,
    model scan + keys + mmproj pairing, config merge/refusal, index channel resolution incl. `latest`), plus an
    **end-to-end test** with a real child process: an example binary `examples/echo_runtime.rs` built on
    `sen-runtime-sdk` (server feature as a dev-dependency) installed via install-local into a temp home, started by the
    supervisor, health-gated, called through the proxy with the token, idled out, and cleaned up.
12. **Docs**: README(s) and CLAUDE.md — replace "Local models left the daemon", "Typed decisions: Laya on ONNX", the
    media-sidecar text and build instructions with a "Runtimes" section (link the protocol doc) and the rules that still
    apply to the daemon side (decision gate/router rules, app-provider rules, empty-key exemption, no total timeout,
    single-seam config merge). Keep every other section.

## Acceptance

- `cargo test --workspace` green (report counts vs baseline log `…/scratchpad/baseline-test.log` if you need it: ask the lead).
- `cargo tree -p senclaw | grep -E "mlx|candle|ort |ocr-rs|mnn"` empty.
- Space App tests (`tests/space_app_*`) green; register/register-local/install-zip untouched in behavior.
- A live smoke on an isolated home (`HOME=$scratch SENCLAW_UI_PORT=28788 SENCLAW_WS_PORT=28789`): daemon boots, `GET /api/runtimes`
  answers, install-local of the echo runtime works, `/api/ocr/models` returns the 503 `runtime_not_installed` body.
  Stop the daemon afterwards.

## Risks

- Hidden users of removed modules (grep `local_model`, `tts::`, `media_sidecar`, `decision::laya`, `decision::online`, `candle`).
- Key-order loss on the decision path (serde_json `Value`) silently changes answers — keep the raw text path.
- Proxy buffering an SSE stream breaks streaming; use body streaming both ways.
