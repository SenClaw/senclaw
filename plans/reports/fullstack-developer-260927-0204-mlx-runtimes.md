# Phase 02 — MLX runtimes (`sen-mlx`, `sen-whisper`)

Status: **complete.** Both repos implemented, ported, tested, packaged, and live-checked
against real checkpoints. The mid-session disk crisis (see "Verification status" and
"Root-cause fix" below) is resolved — root-caused by `kongming`, fixed per its plan, and every
verification step it had blocked is now done.

## What moved

### sen-mlx (mode `model`, slot `mlx`, chat + vision, darwin-arm64)

| From (old monorepo) | To | Notes |
|---|---|---|
| `apps/mlx-lm/src/engine/{mlx_native.rs, mlx_prompt.rs, chat_template_openai.rs, image_input.rs, stream_parser.rs, thinking_parse.rs, runtime.rs}` | `sen-mlx/src/engine/*.rs` | Verbatim except 4 call sites in `mlx_native.rs` (`local_model_core::settings::*` → `crate::settings::*`) |
| `apps/mlx-lm/src/engine/mlx_lm/**` (cache, sampling, prefix_cache, error, models/*, utils/*) | `sen-mlx/src/engine/mlx_lm/**` | Verbatim (18 architecture/support files) |
| `apps/mlx-lm/src/engine/mlx_lm_utils/{error,tokenizer}.rs` | `sen-mlx/src/engine/mlx_lm_utils/*.rs` | Verbatim |
| `apps/mlx-lm/src/engine/mod.rs` | `sen-mlx/src/engine/mod.rs` | Rewritten: `local_model_core::store::has_safetensors` inlined (one small fn); added `context_length()` (was `apps/local-model-core`'s `read_config_summary`, ported as a single function, not the whole store) |
| `apps/local-model-core/src/settings.rs` | `sen-mlx/src/settings.rs` | Verbatim struct/tests — same file, same snake_case keys, same defaults |
| `apps/mlx-lm/src/provider.rs` | `sen-mlx/src/provider.rs` | Rewritten for the mode-`model` shape (see below) |
| `apps/mlx-lm/src/main.rs` | `sen-mlx/src/main.rs` | Rewritten: `serve` subcommand on `sen_runtime_sdk::server::serve`, eager load at startup |
| — | `sen-mlx/tests/manifest.rs` | New: manifest parses via the SDK, version tracks `Cargo.toml` |

**Dropped, and why:**
- `apps/mlx-lm/src/engine/models.rs` (the curated `KNOWN_MODELS` catalog + `infer_vision_from_id`) — unused by the engine itself (grepped: zero internal callers); it backed the Space App's download-suggestion UI, which is now the daemon's job (protocol §6.1). `read_model_context_length_from_dir`'s *logic* survives as `engine::context_length()`.
- `apps/local-model-core`'s `store.rs`/`download.rs`/`api.rs` (multi-model listing, HF downloader, `/api/local-models/*` REST) — explicitly daemon-owned now. sen-mlx is handed exactly one `--model` path.
- `main.rs`'s web UI serving (`ServeDir`/`ServeFile`, `ServeMlx`'s iframe), CORS layer, `senclaw-manifest.json`, `app_space_sdk::llm::publish_models` model-cache file — all Space-App registration/UI concerns; a runtime has none of that surface. `senclaw-manifest.json` itself was not carried over (superseded by `senclaw-runtime.json`).
- `mlx_lm_utils/whisper_tokenizer.rs` — dead code in the LLM engine (the one caller, `mlx_lm/models/whisper.rs`, was already removed before this port; `models/mod.rs`'s own comment confirms it). Its real home is `sen-whisper`, which has its own identical copy.
- The multi-model switching/idle-sweeper half of `MlxProvider` (`loaded: Mutex<Option<Loaded>>` keyed by model id, `spawn_idle_sweeper`) — mode `model` means one process holds exactly one model for its whole life; the daemon's own `idleTimeoutSecs` (process-level) replaces the old per-app idle-unload timer. `settings.idle_unload_secs` is still read/written faithfully (round-trip tests pass) but is not acted on operationally in this runtime — documented in code and CLAUDE.md.
- **Behavior change, not a drop:** the old Space App loaded weights *lazily* on first request (daemon's 30 s health-gate budget forced this). The runtime protocol's `health.startupTimeoutSecs` is minutes-wide, so `sen-mlx` now loads *eagerly* at startup: `Readiness::loading()` → background `spawn_blocking` load → `set_ready()`/`set_failed()`. `/health` is 503 until then. A `chat()` call arriving before ready awaits the same in-flight load via a `tokio::sync::watch` channel rather than double-loading — this only matters for a hand-started dev run racing its own first request; in production the daemon never proxies until `/health` is 200.
- No standalone bench binary exists in this checkout to port (`examples/mlx_bench`, `scripts/mlx_bench.sh`, `scripts/mlx_resource_bench.py` referenced by `docs/mlx-resource-benchmark.md` are all absent at commit `f0f31bd` — confirmed by search before concluding this). The in-engine `MLX_BENCH_EXT_DETERMINISM` determinism check inside `mlx_native.rs`'s own test module carried over as part of the verbatim port.

### sen-whisper (mode `service`, slot `asr`, darwin-arm64)

| From (old monorepo) | To | Notes |
|---|---|---|
| `crates/senclaw-media/src/{audio.rs, candle_whisper.rs}` | `sen-whisper/src/*.rs` | Verbatim |
| `crates/senclaw-media/src/mlx/{mod,mlx_serial,whisper_transcribe}.rs` | `sen-whisper/src/mlx/*.rs` | Verbatim **except one bug fix** — see below |
| `crates/senclaw-media/src/mlx/mlx_asr/{mod,error,whisper,whisper_tokenizer}.rs` | `sen-whisper/src/mlx/mlx_asr/*.rs` | Verbatim |
| `crates/senclaw-media/src/main.rs`, `routes.rs` | `sen-whisper/src/main.rs`, `src/transcribe.rs` | Rewritten: merges the sidecar's own routing with the daemon's model-resolution logic (no more HTTP hop between them — see below) |
| `src/gateway/ui_server/whisper.rs` (model management: catalog, composite download, settings CRUD) | `sen-whisper/src/models.rs`, `src/settings.rs` | Ported close to verbatim; `AppError`/`UiState` replaced with local `ApiError` (same `{"error": "..."}` wire shape) and `AppState { env: LaunchEnv }` |
| `src/gateway/ui_server/hf_validate.rs` (`check_whisper` + the Whisper half of `validate()`) | `sen-whisper/src/validate.rs` | Ported verbatim; TTS/LLM halves left behind (out of scope, belong to other runtimes) |
| `WhisperSettings` (`gateway/group_manager/types.rs`) + `load_whisper_settings`/`save_whisper_settings` (`.../llm.rs`) | `sen-whisper/src/settings.rs` | Same struct/casing (camelCase `modelId`/`language`), now seeded via `sen_runtime_sdk::legacy::load_or_import("whisperConfig")` instead of a direct `config.json` field read |
| `src/media_sidecar.rs` | *(not ported as code — see below)* | Its whole job (find the binary, spawn it, health-poll, adopt-if-running, kill-on-shutdown) is now the daemon's generic runtime launcher (phase 01). What *is* carried over: the design decisions it encoded — see "Behavior kept" below |
| — | `sen-whisper/src/api_error.rs` | New: two error shapes for two wire contracts (`ApiError` flat for `/api/whisper/*`, `OpenAiError` nested for `/v1/audio/transcriptions`) |
| — | `sen-whisper/tests/manifest.rs` | New |

**Merged, not just moved:** the old daemon called `crate::media_sidecar::ensure_running()` then made an HTTP request to a *second process* for every transcription (`transcribe_impl` in `ui_server/whisper.rs` → sidecar's `/v1/audio/transcriptions?model_dir=...`). That hop is gone: `sen-whisper/src/transcribe.rs` calls the decode engines in-process. The daemon-to-sidecar URL-building/proxying code (`transcribe_impl`, `urlencoding` query construction) was dropped as redundant, not ported.

**A real bug found and fixed, not just carried over:** `crates/senclaw-media/src/mlx/mlx_serial.rs` defines a process-wide MLX serialization lock with a doc comment explaining exactly why it's needed ("concurrent MLX work on separate threads corrupts Metal state and SIGSEGVs") — but grepping the entire old `senclaw-media` source tree shows it is **declared and never called anywhere**. Each transcription created a fresh `WhisperEngine` with only an instance-local mutex, so two concurrent transcriptions would touch MLX's shared Metal device from two OS threads with nothing serializing them — the same failure class I independently reproduced and fixed in `sen-mlx`'s test suite (see below). `sen-whisper/src/transcribe.rs::transcribe_mlx` now acquires `mlx::mlx_serial::lock()` for the whole `spawn_blocking` closure (load, decode, and the `UnloadOnExit` drop guard — the lock variable is declared *before* the engine so Rust's reverse drop order keeps it held through `unload()`). Flagged in both repos' CLAUDE.md.

**Behavior kept from `media_sidecar.rs`'s design** (the file itself is obsoleted by the daemon's generic launcher, but its decisions are re-expressed here):
- `idleTimeoutSecs: 0` in `senclaw-runtime.json` — "not reaped when idle" (the old sidecar was never killed for being idle; it drops weights after each use instead, so an idle *process* costs megabytes, not the model size). The daemon's per-`RunMode::Service` default is 300 s; this explicitly opts out, matching the old design intent, with a test pinning it (`tests/manifest.rs`).
- `/health` never reads weights — `Readiness::ready()` set immediately in `main()`.
- Symphonia probes by file extension (`write_probe_file` keeps the caller's extension on the temp file) — carried over verbatim, still load-bearing.

## Root-cause fix (mlx-sys rebuild churn)

`kongming`'s diagnosis: Cargo decides a build-script unit's debuginfo per invocation kind, not
per profile, so `mlx-sys`'s `bindgen` step fingerprinted differently under `cargo build` vs
`cargo check`/`test` and forced a fresh ~1.2 GB MLX C++ rebuild on almost every switch between
them. Applied in the order given:

1. `CMAKE=/usr/bin/false` as a routine-run guard — makes an unwanted rebuild fail fast (the
   build script's own `cmake` invocation errors immediately) instead of silently spending disk.
   Used for every probe below; never for the one real build per crate.
2. `[profile.dev.build-override] debug = false` added to both `Cargo.toml`s — the actual fix,
   pins build-script debuginfo so its fingerprint no longer depends on invocation kind.
3. Probed both crates under the guard post-fix: both immediately wanted a rebuild (expected —
   the profile itself changed, so every previously-built `mlx-sys-<hash>` was now stale
   regardless of this bug). Deleted the three now-confirmed-dead full builds (sizes 1.2–1.3 GB,
   timestamps predating the fix) plus assorted empty/partial fingerprint-only directories from
   the session's earlier churn — reclaimed disk without touching anything still referenced.
4. One real build per crate, sequential (sen-mlx first, `cargo build`, 1m54s; then sen-whisper,
   1m42s) — never both at once. Then, per crate, re-probed `cargo build` and `cargo test --no-run`
   under the `CMAKE=false` guard: both now **succeed instantly** (0.3–0.5 s for `build`, single-
   digit seconds for `test --no-run` compiling just the test harness) with no new hash created —
   confirmed by listing `debug/build/mlx-sys-*` before and after: unchanged, four directories
   total (two real builds + two small build-script-build companions, one pair per crate). The
   fix holds.
5. Two release builds (`make package`, one per crate, sequential) each triggered their own fresh
   `mlx-sys` build under the release profile (expected — the dev-profile override does not
   extend to release, and release is only ever invoked one way in this workflow, so it never hit
   the original bug). Both completed cleanly with disk to spare.
6. Docs fixed in both repos (`CLAUDE.md`, `README.md`, `Makefile` header comment): the previous
   claim — "a shared `CARGO_TARGET_DIR` shares every compiled artifact between the two repos" —
   was wrong. A shared target dir reuses the *pure-Rust* dependency compiles; it does **not**
   reuse the `mlx-sys` C++ build, because `sen-mlx` and `sen-whisper` are separate workspaces
   with separate `Cargo.lock` files and therefore separate build-script fingerprints even on the
   identical `mlx-rs`/`mlx-sys` tag. Budget one full `mlx-sys` build per repo, not one total. The
   `CMAKE=false` guard technique is documented alongside it as a way to make an unexpected
   rebuild fail loudly instead of silently, in a disk-constrained environment.

Also fixed in the same pass, found only because `make package` was actually run end to end for
the first time this session: both `Makefile`s computed `METALLIB := $(shell find ...)` with `:=`
(simple/immediate expansion), which Make evaluates once at *parse* time — before the `build`
prerequisite has run. `package` therefore always saw the pre-build state and failed with
"`mlx.metallib` not found" even on a build that had just succeeded. Changed to `=` (recursive/
lazy expansion) in both `Makefile`s so it re-evaluates when referenced inside the `package`
recipe, after `build` has actually produced the file. Confirmed by re-running `make package` for
both repos with no other change: both packaged successfully on the first retry.

## Package contents (what the daemon/clients must know)

Both `senclaw-runtime.json` manifests validated by hand against `RuntimeManifest::validate()`'s
rules (traced through every branch — slot/type agreement, format requirements, platform list,
placeholder set per mode, capability-arg gating, health path/timeout range), confirmed by
`tests/manifest.rs` passing for both, and confirmed a second time by extracting each packaged
`dist/*.tar.gz` and validating the shipped `senclaw-runtime.json` is well-formed JSON matching
every field below.

- **sen-mlx**: `type: llm-engine`, `slots: [mlx]`, `formats: [mlx]`, `capabilities: [chat, vision]`,
  `mode: model`, entry `bin/sen-mlx serve --host {host} --port {port} --model {model_path}`,
  `health.startupTimeoutSecs: 300` (large quantized checkpoints can take real time to read from
  disk), `idleTimeoutSecs` left absent (daemon's model-mode default of 900 s applies).
  `GET /v1/models` reports one model whose id is `SENCLAW_MODEL_ID` else the model directory's
  literal name (e.g. `mlx-community__Qwen2.5-0.5B-Instruct-4bit`, double underscore — matches the
  phase spec's stated fallback exactly).
- **sen-whisper**: `type: asr`, `slots: [asr]`, `capabilities: [asr]`, `mode: service`, entry
  `bin/sen-whisper serve --host {host} --port {port}`, `health.startupTimeoutSecs: 60`,
  `idleTimeoutSecs: 0` (see above). Serves the full old `/api/whisper/*` namespace verbatim plus
  `POST /v1/audio/transcriptions` (OpenAI multipart: `file`, `model?`, `language?`,
  `response_format: json|text`); an explicit `model` field is honored only if it names an
  *installed* checkpoint, else falls back to the configured/catalog default (keeps a client
  sending a placeholder like `"whisper-1"` working unmodified).
- Both packages: `mlx.metallib` (88,148,749 bytes, identical in both — same `mlx-c` source,
  same tag) copied beside the binary into `bin/` at package time; `make package` fails loudly if
  the build did not produce one under `$(CARGO_TARGET_DIR)/$(OUT_DIR)/build/*/out/build/lib/`
  (verified both the failure path, before the `METALLIB` timing fix, and the success path after).
- `sen-mlx-0.1.0-darwin-arm64.tar.gz` (32,819,096 bytes) and `sen-whisper-0.1.0-darwin-arm64.tar.gz`
  (34,021,352 bytes), each with a `.sha256` beside it, both under each repo's `dist/`. Extracted
  and listed both archives to confirm the exact layout: `bin/<binary>`, `bin/mlx.metallib`,
  `senclaw-runtime.json` at the archive root.
- Two `Makefile` bugs found and fixed before first successful `make package` (neither had been
  exercised end to end before this pass — see "Root-cause fix" above for the second one): Cargo's
  default profile is *named* `dev` but *outputs* to a `debug/` directory — `PROFILE` (user-facing)
  and `OUT_DIR` (actual path component) are separate variables so `make package PROFILE=dev`
  finds the right binary; and `METALLIB`'s `find` needed lazy (`=`), not immediate (`:=`),
  expansion so `package` sees the build's output instead of the pre-build state.

## Tests

- **sen-mlx**: `cargo test` green — **144 unit tests + 1 manifest integration test = 145 passed,
  1 ignored (needs a real checkpoint on disk), 0 failed.**
- **sen-whisper**: `cargo test` green — **25 unit tests + 1 manifest integration test = 26
  passed, 4 ignored (need `SENCLAW_WHISPER_DIR`/`say`, by design — run by hand, not in CI), 0
  failed.**
- **Found and fixed, both repos**: MLX's Metal command queue is not safe for concurrent dispatch
  from multiple OS threads in one process. `mlx_native::mlx_serial_lock` (sen-mlx) covers the
  engine's own load/generate path but not a unit test that builds an MLX `Array` directly (most
  of `engine::mlx_lm::{models,cache,sampling}::tests` — five files' test modules were still gated
  behind a now-nonexistent `feature = "local-mlx"` from the old multi-backend daemon; changed to
  plain `#[cfg(test)]`, which is what surfaced the SIGSEGV). Reproduced with a bare `cargo test`
  (crashed mid-suite), confirmed the cause by re-running with `--test-threads=1` (145/145 pass),
  then fixed at the config level rather than requiring every caller to remember a flag:
  `.cargo/config.toml` sets `RUST_TEST_THREADS=1` in both repos. Documented in both CLAUDE.md
  files as a rule, not just a workaround comment.
- `provider.rs`'s own new test (`spawn_refuses_a_directory_that_is_not_a_checkpoint`) needed
  `.err().unwrap()` instead of `.unwrap_err()` — `Arc<MlxProvider>` (and the engine it can hold)
  doesn't implement `Debug`, which `unwrap_err`'s bound requires on the `Ok` side.

## Live checks

Both run from the packaged `dist/*/bin/` binary (not a raw `cargo run`), against real installed
checkpoints under `~/.senclaw/`, read-only; all writes (the synthesized clip) went to the
session scratch dir. Both processes were started with `SENCLAW_RUNTIME_TOKEN` set and no
`SENCLAW_PARENT_PID`, and killed by PID afterward — no orphaned process left listening.

**sen-mlx**, `mlx-community__Qwen2.5-0.5B-Instruct-4bit`:
- `GET /health` with no token → `200 {"status":"ok","id":"sen-mlx","version":"0.1.0"}`.
- `GET /v1/models` with no token → `401`; with `Authorization: Bearer <token>` → `200`, one
  entry, `id: "mlx-community__Qwen2.5-0.5B-Instruct-4bit"` (the documented directory-name
  fallback), `context_length: 32768`, `tools: true`, `vision: false`.
- Streamed `POST /v1/chat/completions` ("Say the word OK and nothing else.") → proper SSE:
  a content delta (`"OK"`), a final chunk carrying `usage`, then `data: [DONE]`.

**sen-whisper**, `mlx-community__whisper-large-v3-turbo-4bit` (resolved through the legacy
`SENCLAW_LOCAL_MODELS_DIR` fallback dir, exactly as designed — no dedicated-directory install
exists for this checkpoint on this machine): clip synthesized with
`say -o clip.aiff "The quick brown fox jumps over the lazy dog"` then
`afconvert -f WAVE -d LEI16@16000 -c 1` to 16 kHz mono PCM WAV.
- `GET /health` → `200` immediately (service mode, no eager load).
- `POST /api/whisper/transcribe` with no token → `401`; with token, multipart `file` field only
  (this legacy route takes no per-request `model` field — confirmed by reading
  `read_legacy_multipart`, which treats *any* field other than `language` as the audio payload,
  after an initial test with an extra `model` field got misread as the audio content and failed
  Symphonia probing on a `.bin`-suffixed temp file — a test mistake on my part, not a code bug)
  → `{"ok":true,"text":"The quick brown fox jumps over the lazy dog."}`, exact match.
- `POST /v1/audio/transcriptions` with no token → `401`; with token, the real model id explicit
  → `{"text":"The quick brown fox jumps over the lazy dog."}`; with a placeholder id the model
  doesn't have installed (`"whisper-1"`, the OpenAI default clients often send unmodified) and
  `response_format=text` → falls back to the configured/catalog default and returns plain text
  `The quick brown fox jumps over the lazy dog.` (no JSON wrapper) — confirms both the
  placeholder-fallback and `response_format` behavior documented in "Package contents" above.

## Verification status (honest accounting)

**Update: the disk crisis is resolved and full verification is complete.** `kongming`'s
diagnosis came back (root cause: Cargo's build-script debuginfo is decided per invocation kind,
not per profile, so `bindgen` inside `mlx-sys` fingerprinted differently under `build` vs
`check`/`test` and forced a from-scratch rebuild of the whole MLX C++ core on almost every
invocation). Applied in full, in the order given — see "Root-cause fix" below — after which
both repos built, tested, and packaged cleanly with disk headroom to spare (peaked at 38 GiB
free, never dropped below 20 GiB even doing two full release builds).

Completed and verified:
- [x] sen-mlx: `cargo test` green — 144 unit + 1 manifest = 145 passed, 1 ignored, 0 failed.
- [x] sen-whisper: `cargo test` green — 25 unit + 1 manifest = 26 passed, 4 ignored (real
  checkpoint/`say` dependent, by design), 0 failed.
- [x] Both manifests hand-traced against every `RuntimeManifest::validate()` branch, and
  confirmed valid JSON matching the schema by extracting the packaged copy.
- [x] `make package` for both: release build, `mlx.metallib` bundled correctly, tarball +
  sha256 produced. Verified by extracting and listing each archive's contents.
- [x] Live check, sen-mlx: full `/health` → `/v1/models` → streamed
  `/v1/chat/completions` round trip against the real
  `mlx-community__Qwen2.5-0.5B-Instruct-4bit` checkpoint.
- [x] Live check, sen-whisper: synthesized a clip with `say` + `afconvert`, transcribed
  through both `/api/whisper/transcribe` and `/v1/audio/transcriptions` against the real
  `mlx-community__whisper-large-v3-turbo-4bit` checkpoint (found via the legacy
  `SENCLAW_LOCAL_MODELS_DIR` fallback, exactly as designed).
- [x] Bearer-token-required-except-`/health` behavior confirmed live for both (not just by the
  SDK's own generic test suite).
- [x] Two Makefile bugs found and fixed (see below), both only surfaced by actually running
  `make package` end to end, which nothing had done before this pass.

Details for every checkbox are in the sections below (Root-cause fix, Tests, Package contents,
Live checks).

## What the daemon / clients must know

- sen-mlx's `/v1/models` id is the literal model-directory name when `SENCLAW_MODEL_ID` is unset
  — clients should not assume a clean `org/repo` shape.
- sen-whisper unifies what used to be two processes (daemon proxy + sidecar) into one; nothing in
  the daemon's `/api/whisper/*` client-facing contract changes, but the daemon's *runtime
  manager* now talks to this one process directly instead of spawning a fixed-port sidecar.
- sen-whisper's `idleTimeoutSecs: 0` is deliberate (see above) — the daemon should not treat an
  ASR runtime sitting idle for a long time as anomalous.
- Both packages currently target `darwin-arm64` only, matching the runtime-protocol.md table;
  sen-whisper's Candle backend is cross-platform in source but not packaged for other platforms
  in this phase.
- `/api/whisper/transcribe` (legacy namespace) takes no per-request `model` field — only `file`
  and an optional `language`; model selection there is whatever `selected_model()` resolves
  (current settings, else first installed catalog entry). Only `/v1/audio/transcriptions`
  accepts an explicit `model`. A caller that sends an extra field the legacy route doesn't know
  about will have it silently treated as the audio payload (any field that isn't `language` is),
  so a client integration should send exactly `file` (+ optional `language`) to that route.

## Unresolved questions

Everything that was open pending the disk crisis and the `mlx-sys` rebuild-churn diagnosis is
now resolved (root cause, fix, and full verification — see "Root-cause fix" and "Verification
status" above). One item remains, low priority:

1. Whether to trim the ~117 dead-code warnings in sen-mlx's `cargo build` output (all
   "never used"/"never read" on pub items not reached from `main` — inherent to a large
   multi-architecture bin-only crate, not introduced by this port) and sen-whisper's 10 is left
   to a future pass; flagged rather than silently left out of this report.

Not left open, but worth a decision if a *third* MLX-dependent runtime joins this workspace
layout later: giving each crate its own `CARGO_TARGET_DIR` was considered as a fallback if the
rebuild churn couldn't be root-caused, but the `build-override` fix makes the shared-target
setup fully stable as-is (confirmed: `build`/`check`/`test` now settle on one stable `mlx-sys`
hash per crate, indefinitely) — no reason to change it now that it works as originally intended,
modulo the corrected understanding that the MLX C++ build itself is still one-per-repo either
way (docs now say so).
