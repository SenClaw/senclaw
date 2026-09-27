# Phase 02 — MLX runtimes: `sen-mlx`, `sen-whisper`

Owner: one agent. Repos: `/Users/benji/Projects/SenClaw/sen-mlx`, `/Users/benji/Projects/SenClaw/sen-whisper` (only).
Contract: `senclaw/docs/runtime-protocol.md` §1–4 (esp. §4.2, §4.5), SDK `senclaw/crates/sen-runtime-sdk` (read-only).

## Common runtime repo shape (also used by phase 03)

```
Cargo.toml             own workspace root; sen-runtime-sdk = { path = "../senclaw/crates/sen-runtime-sdk" }
src/main.rs            `serve` subcommand on sen_runtime_sdk::server::serve (+ any existing tools as subcommands)
senclaw-runtime.json   manifest; a test asserts it parses with the SDK and version == CARGO_PKG_VERSION
Makefile               build · test · package (dist/<id>-<version>-<platform>.tar.gz + .sha256) · install-local · run-dev
.github/workflows/release.yml   tag v* → build per platform → package → release assets (checks out SenClaw/senclaw as a sibling)
README.md · CLAUDE.md (carry the old "Rules for Claude" that apply) · docs/ · .gitignore (target/, dist/)
```

- `install-local`: `senclaw runtime install-local dist/<archive>` when a `senclaw` binary is on PATH, else extract into
  `$HOME/.senclaw/runtimes/<id>/<version>/` writing the manifest last. Never run it against the real home in this phase.
- `run-dev`: standalone serve on a fixed dev port (sen-mlx 4970, sen-whisper 4963) — no token, no watchdog.
- Share one MLX build between both repos while developing: `CARGO_TARGET_DIR=/Users/benji/Projects/SenClaw/.cargo-target-mlx`
  (mlx-rs/mlx-sys tag v0.25.3 in both, identical features). `CARGO_BUILD_JOBS=4`.
- The package must contain `mlx.metallib` beside the binary (MLX resolves it relative to the executable); `make package`
  fails loudly if it is missing (find it under the target dir's `build/*/out`), as the old `make app-build` did.

## sen-mlx (mode `model`, slot `mlx`, capabilities chat + vision, platform darwin-arm64)

Source: old `apps/mlx-lm/**` (engine, provider, models, sampling, caches, bench) + what it needs from
`apps/local-model-core` (settings; not the downloader/store/REST — the daemon owns the model library now) + the
OpenAI wire from `app-space-sdk::llm` (path dep `../senclaw/app-space-sdk`, reuse — do not re-implement the wire).

- `sen-mlx serve --host {host} --port {port} --model {model_path}` (model also from `SENCLAW_MODEL_PATH`). Loads that one
  model at startup: `Readiness::loading()` → `set_ready()` / `set_failed()`; `/health` 503 until loaded.
- OpenAI `GET /v1/models` (the one model; id = `SENCLAW_MODEL_ID` else the directory name), `POST /v1/chat/completions`
  JSON + SSE, tools, usage; Gemma-4 vision input as before. `ModelCard.vision` from the checkpoint config.
- Sampling/KV/prefill settings from `<SENCLAW_LOCAL_MODELS_DIR>/settings.json` — the same snake_case file and struct
  (`local_model_core::settings::Settings`), read as before; never `rename_all`.
- Drop Space-App parts: `senclaw-manifest.json`, the model-management web UI, `/api/local-models/*`, app registration.
- Keep: the process-wide MLX serial lock, prefix cache, sampling precedence (user → checkpoint → off), KV ring, TurboQuant
  exceptions, bench harness (as a subcommand or example), all existing unit tests.
- Docs moved here from the old repo: `gemma4-local-optimizations.md`, `mlx-resource-benchmark.md`,
  `mlx-rs-turboquant-native-runtime.md`, `local-gemma-mlx-runtime.md`, `local-model-space-app-extraction.md`; CLAUDE.md
  carries the old "Gemma 4 on the native MLX path" rules and the MLX rules from "Local models left the daemon".
- Live check (read-only use of an existing model): `SENCLAW_LOCAL_MODELS_DIR=<scratch copy with settings.json>`
  `sen-mlx serve --port 4970 --model ~/.senclaw/local-models/mlx-community__Qwen2.5-0.5B-Instruct-4bit`, then `/health`,
  `/v1/models`, a streamed `/v1/chat/completions`. Stop it afterwards.

## sen-whisper (mode `service`, slot `asr`, capability asr, platform darwin-arm64)

Source: old `crates/senclaw-media/**` (routes, audio front-end, MLX Whisper, anything else it has) + model management and
settings that lived in the daemon: `src/gateway/ui_server/whisper.rs`, the STT part of `ui_server/hf_validate.rs`,
`WhisperSettings` load/save in `gateway/group_manager/{types,llm}.rs`, and `src/media_sidecar.rs` behavior worth keeping.

- Serve the old `/api/whisper/*` namespace **verbatim** (paths, bodies, status codes) + `POST /v1/audio/transcriptions`
  (OpenAI multipart) — both share one transcription path.
- Settings in `<SENCLAW_RUNTIME_DATA_DIR>/settings.json`, seeded from `config.json[whisperConfig]` via
  `sen_runtime_sdk::legacy::load_or_import`. Model locations unchanged (§6.1): honor `SENCLAW_WHISPER_MODELS_DIR` and the
  paths the old code resolved (the real machine has `~/.senclaw/local-models/mlx-community__whisper-large-v3-turbo-4bit`).
- Keep: extension-based Symphonia probing (`filename` → temp file extension), weights dropped after each use, `/health`
  answering before any weight is read.
- Live check: synthesize a short clip (`say -o <scratch>/a.aiff "xin chào các bạn"` → `afconvert -f WAVE -d LEI16 …`),
  transcribe through both routes with the existing model (read-only). Stop the process afterwards.

## Acceptance

Both repos: `cargo test` green (ported tests included), `make package` yields a valid archive (manifest + binary +
`mlx.metallib`), started by hand with a token env set → `/health` open, everything else 401 without the bearer; live
checks above pass. Report per repo what moved, what was dropped and why.
