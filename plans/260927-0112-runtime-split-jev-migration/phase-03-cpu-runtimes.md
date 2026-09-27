# Phase 03 — CPU runtimes: `sen-sysone`, `sen-ocr`, `sen-tts`

Owner: one agent. Repos: `/Users/benji/Projects/SenClaw/sen-sysone`, `sen-ocr`, `sen-tts` (only).
Contract: `senclaw/docs/runtime-protocol.md` §1–4 (esp. §4.3, §4.4, §4.6), SDK `senclaw/crates/sen-runtime-sdk` (read-only).
Repo shape, Makefile targets and packaging rules: the "Common runtime repo shape" section of `phase-02-mlx-runtimes.md`
(dev ports: sen-sysone 4961, sen-ocr 4962, sen-tts 4964). `CARGO_BUILD_JOBS=4`.

All three are mode `service`: one process, started with `serve --host {host} --port {port}`, `/health` ready before any
weights load, settings in `<SENCLAW_RUNTIME_DATA_DIR>/settings.json` seeded once from the old `config.json` key via
`sen_runtime_sdk::legacy::load_or_import`. Each serves the old daemon namespace **verbatim** — same paths, request and
response bodies, status codes, masking rules — because desktop and web keep calling those paths through the daemon.
Where an old handler used the daemon's `Config`/`UiState`, derive the same values from `LaunchEnv` + the same env
overrides (`SENCLAW_*_MODELS_DIR`, …) with defaults under `SENCLAW_HOME`.

## sen-sysone (slot `decision`, capability decision, all six platforms)

Source: old `src/decision/{laya/**, online.rs, types.rs, json.rs, settings.rs}` + the model/ask/settings/online-test
handlers of `src/gateway/ui_server/decision.rs` + `laya/testdata`.

- Routes: §4.3 list + `POST /v1/systemone` (same answer as `/api/decision/ask`).
- Settings = `backend`, `local`, `online` (import from `decisionConfig`, ignoring `gate`/`skills`, which stay in the daemon).
  `GET` masks the key (`hasApiKey`), `same_key_scope` + `clearApiKey` rules unchanged, Cloudflare nested `input` shape
  unchanged, online answers serialized from `AskResponse` directly.
- Keep every rule from the old CLAUDE.md "Typed decisions: Laya on ONNX" that concerns the engine: order-preserving
  `Json`, parity fixture + ignored parity tests, graph detection, pinned community exports + manifest-last install,
  Vietnamese routing to the multilingual checkpoint (409 refusal branch), single-flight detached load with `DeleteGuard`,
  load on request never at boot, idle unload from the last request start. They go into this repo's CLAUDE.md.
- ONNX Runtime via `ort` (download-binaries): check with `otool -L`/`ldd` whether the binary needs a shared
  onnxruntime library; if so the package carries it beside the binary and the manifest/launch works from the package dir.
- Checkpoints stay in `<SENCLAW_LOCAL_MODELS_DIR>/laya/`. Docs: move `laya-decisions.md` here from the old repo.
- Live check: the real machine has `~/.senclaw/local-models/laya` (1.2 GB). Run the parity tests against it and one
  `/v1/systemone` ask. If any code path you exercise writes into the model root, work on a scratch copy instead.

## sen-ocr (slot `ocr`, capability ocr; darwin-arm64 with metal+coreml, linux-x64 and windows-x64 on CPU)

Source: old `src/local_model/ocr/**`, `src/gateway/ui_server/ocr.rs` (routes, engine cache, catalog downloads, custom
models, `ocr_text_from_bytes`), `examples/ocr_roundtrip.rs` (keep as this repo's round-trip harness), the `ocr-rs` git
dependency and its features (`ocr-paddle` / `ocr-paddle-metal`; MNN builds with cmake — heavy).

- Routes §4.4 verbatim; `POST /api/ocr/recognize` is also what the daemon's internal client and MCP server call.
- Settings from `ocrConfig`; models in `SENCLAW_OCR_MODELS_DIR` (default `$SENCLAW_HOME/ocr-models`).
- Live check: render a line of Vietnamese + English text to PNG, recognize it with the existing
  `~/.senclaw/ocr-models/PP-OCRv5_mobile_latin` (read-only).

## sen-tts (slot `tts`, capability tts, all six platforms; `say` presets on macOS only)

Source: old `src/tts/**` (VieNeu ONNX engine, vendored sea_g2p, voices, chunking, macOS `say`), `src/gateway/ui_server/tts.rs`,
the TTS part of `ui_server/hf_validate.rs`, `TtsSettings` load/save from `gateway/group_manager/{types,llm}.rs`.

- Routes §4.6 verbatim + `POST /v1/audio/speech` (WAV).
- Keep: removed voices degrade to the macOS voice with `fallback_reason` / `X-TTS-Fallback` (never 400), the `Unsupported`
  catch-all backend, arena-off memory behavior, chunking.
- Settings from `ttsConfig`; models in `SENCLAW_TTS_MODELS_DIR` (default `$SENCLAW_HOME/tts-models`).
- Live check: synthesize through both routes with the `say` fallback (no model needed) and, if a VieNeu model exists in
  `~/.senclaw/tts-models`, with it (read-only).

## Acceptance

Each repo: `cargo test` green (ported tests included), `make package` yields a valid archive, hand-started with a token
env → `/health` open and every other route 401 without the bearer, the §4 routes answer with the old shapes, live checks
pass. Report per repo what moved, what changed shape (should be nothing), and anything the daemon must know.
