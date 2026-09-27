# Phase 03 — CPU runtimes: sen-sysone, sen-ocr, sen-tts

Status: **DONE** — all three repos fully verified end to end: `cargo test`
green, release build, `make package` producing a valid, checksummed archive,
and live checks against real models/backends for every one of them. The
`dist/*.tar.gz` archives are kept on disk (not deleted) for the lead's
install-local end-to-end test, per instruction.

## Environment note: shared disk crisis (resolved)

Partway through this phase the shared build machine ran out of disk space
(`/dev/disk3s1s1` hit **0 bytes free**, and my own `df -h` calls started
failing with `ENOSPC`). This was **not** caused by these three repos alone —
`/System/Volumes/Data` was at 436 GB/460 GB used before any of my builds ran.
I switched all three repos to a **shared** `CARGO_TARGET_DIR`
(`/Users/benji/Projects/SenClaw/.cargo-target-cpu`, common deps compiled once)
and deleted every per-repo `target/`/`dist/` I made. Disk then oscillated
between ~120 MB and ~1.8 GB for a long stretch, correlated with other agents'
concurrent builds; I paused, escalated with status updates, made two cautious
probe attempts (both tipped the machine back into `ENOSPC`, killed + cleaned
up both times), and tried a NAS-volume workaround (technically writable but
impractically slow over SMB — killed + cleaned up, nothing left behind).

The user then cleared old build caches and freed **38 GB**. The lead had me
resume with instructions to keep ≥5 GB free and pause again if it dropped
below that. It never came close: disk stayed at **30-38 GB** free through
every remaining build, test, and package step below — the heaviest single
step (sen-ocr's `imageproc`/`rav1e` dev-dependency chain, the thing that
caused the original crash) only ever cost ~1-2 GB. All remaining verification
for sen-ocr and sen-tts is now complete; see their sections below for full
results.

## What moved (all three repos)

Ported by copying the old daemon's files **byte-for-byte** (diffed against
`/Users/benji/Projects/SemaClaw` to confirm) wherever the module had no
daemon-internal dependency, and writing new glue only for: the CLI entry
(`main.rs`), the HTTP router + `AppState`/`AppError` (`http.rs`, replacing
`UiState`/`AppError` from the daemon's `ui_server/core.rs`), and settings
persistence (`settings_store.rs`, replacing `group_manager`'s
`load_*_settings`/`save_*_settings` against the daemon's whole `config.json`
with `sen_runtime_sdk::legacy::load_or_import` + an atomic write of this
runtime's own `<data_dir>/settings.json`). **No shape change** to any
request/response body, status code, or route path — see per-repo sections.

Common repo shape followed exactly as phase-02 specified: own single-crate
workspace, `Makefile` (`build`/`test`/`package`/`install-local`/`run-dev`),
`senclaw-runtime.json` + a test asserting it parses via the SDK and
`version == CARGO_PKG_VERSION`, `.github/workflows/release.yml`,
`README.md` + `CLAUDE.md` carrying the relevant old "Rules for Claude",
`.gitignore` (`target/`, `dist/`). Dev ports: sen-sysone 4961, sen-ocr 4962,
sen-tts 4964, matching the daemon's launch args. `CARGO_TARGET_DIR` defaults
to the shared `../.cargo-target-cpu` in every Makefile (override-able), each
with `cargo clean -p <id>` (not a blanket `cargo clean`) so cleaning one repo
never evicts the others' cached shared deps.

Nothing was committed anywhere (per instructions). Nothing was written under
`senclaw/` except this report.

---

## sen-sysone (decision runtime)

**Fully done and verified.**

### What moved
- `src/decision/{json.rs, types.rs, online.rs, laya/**}` from the old repo —
  copied unchanged (confirmed via `diff`), same module path
  (`crate::decision::…`), so zero internal-import changes were needed.
- `src/decision/settings.rs` — copied then **trimmed**: `DecisionSettings`
  now holds only `backend`/`local`/`online` (`gate`/`skills` removed, along
  with `GateSettings`/`SkillRouteSettings`/`FeatureMode`/`GateQuestions`) —
  those stay in the daemon's control plane per the plan. `validated()` no
  longer touches a gate threshold. Settings-only tests kept; the one
  gate-specific test was dropped, others unaffected.
- `src/gateway/ui_server/decision.rs`'s model/ask/settings/online-test
  handlers → `src/http.rs`, minus `gate`/`skills` handlers (daemon-only).
  `AppState { env: LaunchEnv }` replaces `UiState`; `root()`/`settings()`
  helpers now read `LaunchEnv.models_dir.join("laya")` and
  `settings_store::load/save` instead of `Config.paths`/`group_manager`.
- New route: `POST /v1/systemone` mounted at the **same handler** as
  `POST /api/decision/ask` (`AskRequest.backend` is `#[serde(default)]`, so a
  Jev-shaped body with no `backend` deserializes identically) — no
  duplicated logic.
- `docs/laya-decisions.md` moved, then **edited** (not just moved): about
  half the old file documented the tool-call gate and pre-skill router as if
  they lived here (`../src/decision/gate/` links etc.) — those explicitly
  stayed in the daemon. Trimmed those sections to a short pointer, fixed
  "runs inside the daemon"/`config.json` framing to describe this runtime,
  fixed the parity test command to what actually works here, and corrected
  one factual regression (this port's settings write is atomic; the old
  daemon's `config.json` write was not).

### Shape change
None to the wire contract. Internal: `DecisionSettings` no longer carries
`gate`/`skills` (by design — the daemon merges its own into what
`GET /api/decision/settings` shows; verified this is a proxy-layer concern,
not this repo's).

### Tests
`cargo test`: **57 passed, 0 failed, 1 ignored** (the Laya/Python parity
fixture, needs real checkpoints — see live check). Includes a new
`tests/manifest.rs` asserting the SDK parses `senclaw-runtime.json` and its
version matches `CARGO_PKG_VERSION`, and new `http.rs` tests for the
settings/models routes and path-traversal rejection.

### Live checks (all passed)
- `SENCLAW_LAYA_PARITY_ROOT=~/.senclaw/local-models/laya cargo test --features decision-laya parity -- --ignored --nocapture`
  against the real 1.2 GB `multilingual` checkpoint (read-only): **all 6
  fixture cases matched** Python token-for-token and within 1e-3 probability
  (english checkpoint skipped, not installed on this machine).
- Started the release binary with `SENCLAW_LOCAL_MODELS_DIR` pointed at the
  real `~/.senclaw/local-models` (read-only) and everything else
  (`SENCLAW_HOME`, `SENCLAW_RUNTIME_DATA_DIR`, `SENCLAW_CONFIG_PATH`) at a
  scratch dir: `GET /health` → 200 with no token; `GET /api/decision/settings`
  → 401 with no/wrong token, 200 with the right one; `GET /api/decision/models`
  correctly lists the real `multilingual` checkpoint as installed;
  `POST /v1/systemone` with a Vietnamese cancellation request correctly
  routed to the multilingual checkpoint ("non-ASCII text → a multilingual
  checkpoint; loaded on demand"), hot-loaded it (1180 ms), and answered a
  `choice` + `noul` question correctly; `POST /runtime/shutdown` → 202 and
  the process exited cleanly. Process stopped, nothing left in
  `~/.senclaw` (all writes were scoped to the scratch dir).
- `make package` produced a valid `.tar.gz` (manifest at top level + `bin/`),
  extracted it to `/tmp` and ran the binary from there (`serve --help`
  printed correctly and exits 1 by design — see the sen-ocr section for why),
  confirmed no bundled onnxruntime dylib is needed (`otool -L` shows only
  system frameworks — `ort` with `download-binaries` statically links on this
  platform/version). **Re-run after disk recovered** (this repo's binary was
  unchanged, so `cargo build --release` was a 0.19s no-op): checksum verified
  (`shasum -a 256 -c`), archive re-extracted and re-run, same result. Archive
  kept at `sen-sysone/dist/sen-sysone-0.1.0-darwin-arm64.tar.gz` per the
  lead's final instruction (not deleted this time).
- `make install-local` fallback path (extract into `~/.senclaw/runtimes`)
  verified against a **scratch** `HOME` (never the real one): manifest byte
  matches the source, binary runs. (A pre-existing, pre-migration `senclaw`
  binary happens to be on this machine's real `PATH` at
  `~/.local/bin/senclaw` — harmless for the Makefile logic since it just
  errors on the missing `runtime` subcommand rather than touching anything,
  but noting it in case it is stale and worth removing.)

### Package contents
`bin/sen-sysone` (34 MB release binary) + `senclaw-runtime.json`. No shared
library needs bundling on darwin-arm64.

### What the daemon must know
- Settings shape at `GET/PUT /api/decision/settings` no longer includes
  `gate`/`skills` — the daemon's proxy must inject/strip those itself
  (per `runtime-protocol.md` §5.2, already specified, now confirmed as the
  actual behavior on this side).
- `POST /v1/systemone` is live and answers identically to
  `POST /api/decision/ask`.
- Checkpoints are read from `<SENCLAW_LOCAL_MODELS_DIR>/laya/<id>/` exactly
  as before — nothing to re-download.

---

## sen-ocr (OCR runtime)

**Fully done and verified.**

### What moved
- `src/local_model/ocr/{catalog.rs, engine.rs}` — copied unchanged (`diff`
  confirmed) into `src/ocr/`. **Fixed one real bug while porting**: the old
  daemon gated the *whole* `local_model::ocr` module (catalog **and** engine)
  behind `#[cfg(feature = "ocr-paddle")]` at `local_model/mod.rs`, and
  `ui_server/ocr.rs` carried a duplicate hand-written `stub` module so the
  catalog shape was still available without the feature. Since `catalog.rs`
  has zero dependency on `ocr-rs`, I instead gate only `pub mod engine;` in
  `src/ocr/mod.rs`, so the catalog compiles unconditionally and `http.rs`
  needs no stub duplication — simpler, same behavior (`recognize` still
  answers `501` without the feature).
- `src/gateway/ui_server/ocr.rs` → `src/http.rs` verbatim logic, with
  `UiState`/`Config.paths.ocr_models_dir` replaced by `AppState { env }` +
  a `models_root()` helper reading `SENCLAW_OCR_MODELS_DIR` directly (this
  is an engine-private override, not one of `LaunchEnv`'s shared fields —
  matches how the protocol doc describes it in §6.1).
- `examples/ocr_roundtrip.rs` copied and kept as this repo's round-trip
  harness (per the phase doc), only its `use` line changed
  (`senclaw::local_model::OcrEngine` → `sen_ocr::ocr::OcrEngine`). Needed
  adding `src/lib.rs` (`pub mod http; pub mod ocr; pub mod settings_store;`)
  so the example and `main.rs` both consume the same code — `main.rs` no
  longer declares its own `mod` tree, it calls into the library.
- `settings_store.rs` — new, `OcrSettings{model_id, language}`, legacy key
  `"ocrConfig"`, same atomic-write pattern as sen-sysone.

### Shape change
None. `GET /api/ocr/models` catalog shape, `POST /api/ocr/recognize`
multipart handling, settings auto-promotion logic — all copied verbatim.

### Tests
- `cargo check` (both `--features ocr-paddle-metal` and plain default):
  **clean**.
- `cargo test --bin sen-ocr --features ocr-paddle-metal`: 0 tests (correct —
  all of this crate's tests live in the library, not `main.rs`; this command
  turned out **not** to skip `imageproc`/`ab_glyph`/`rav1e` either way — see
  cross-repo notes for why `--bin` doesn't avoid them).
- `cargo test --features ocr-paddle-metal` (full, unscoped): **14 passed, 0
  failed** — 13 in `src/lib.rs` (catalog, engine, settings_store, http) + 1
  `tests/manifest.rs`. Compiled cleanly through the full `imageproc`/`rav1e`
  chain with disk at 37 GB free, using only ~1-2 GB — confirms the original
  crash was purely a timing collision with the shared-machine disk crisis,
  not an inherently disk-hungry build.
- `cargo test --features ocr-paddle-metal --examples`: example builds and
  its (empty, by design — it's a manual smoke-test harness, not `#[test]`s)
  test harness runs clean.

### Live check (passed)
`cargo build --release --features ocr-paddle-metal --example ocr_roundtrip`,
then ran the resulting binary directly against the real, read-only
`~/.senclaw/ocr-models/PP-OCRv5_mobile_latin`:
- **English** (`--lang en`): 4/4 phrases passed, **100% mean recall**, exit 0.
  Confirms the full pipeline end to end — mmap'd MNN model load, Metal/CoreML
  backend, det+rec inference, text decode — is correct.
- **Vietnamese** (`--lang vi`): 1/4 passed, **54.5% mean recall**, exit 3.
  This is **expected, not a defect**: this repo's own `CLAUDE.md` documents
  the bundled latin model's charset as missing precomposed stacked-tone
  vowels, "verified at ~58% word recall" — and the failures here are exactly
  that (`"thế"→"th"`, `"giới"→"gii"`, `"Việt"→"Vit"`, tone-marked vowel
  dropped, everything else correct). 54.5% here vs. the documented ~58% is
  the same finding within phrase-sample noise. Both runs were deterministic
  across `--iters 2` and ran at 200-900 ms per phrase (Metal backend).

### `make package` (verified, archive kept)
`sen-ocr needs no bundled shared library (MNN is statically linked)`.
`sen-ocr-0.1.0-darwin-arm64.tar.gz` — checksum verified
(`shasum -a 256 -c`), extracted to scratch, contents confirmed
(`senclaw-runtime.json` at top level + `bin/sen-ocr`), binary runs (`serve
--help` prints usage and exits 1 — intentional: `usage()` is `-> !` and
always calls `std::process::exit(1)`, by design, same in sen-tts). `otool -L`
confirms only system frameworks (Metal, CoreML, CoreVideo, Foundation) —
matches the "no bundled lib" claim. Kept at
`sen-ocr/dist/sen-ocr-0.1.0-darwin-arm64.tar.gz` (not deleted) for the lead's
install-local test.

### Package contents
`bin/sen-ocr` (5.6 MB archive) + `senclaw-runtime.json`. MNN's default build
mode uses a **precompiled** library per platform, confirmed empirically now:
no bundled dylib needed, behaves exactly like `ort`'s download-binaries.

### What the daemon must know
Nothing behavior-affecting beyond the contract as written — routes, bodies
and the `/api/ocr/recognize` multipart shape are unchanged from the old
daemon, and the live check confirms the port behaves identically to the old
in-daemon code for both languages the bundled model supports.

---

## sen-tts (TTS runtime)

**Fully done and verified.**

### What moved
- `src/tts/{chunk.rs, macos.rs, vieneu/**}` and `src/safe_log.rs` — copied
  unchanged (`diff` confirmed), same module path (`crate::tts::…`,
  `crate::safe_eprintln!`). `vieneu/mod.rs`'s own internal
  `#[cfg(feature = "tts-vieneu")]` gating on `engine`/`npz`/`phonemize`/
  `sea_g2p`/`voices` was already correct as copied — no fix needed here
  (unlike sen-ocr's engine, this one already isolated its native-dep modules
  properly in the original code).
- `src/tts/mod.rs` — copied then had its doc comments updated (old file
  described itself relative to the daemon: `crate::gateway::ui_server::tts`
  calls it, "no reason [MLX backends] belong here [i.e. not the daemon]" —
  reworded to describe this repo directly); logic is unchanged.
- `src/gateway/ui_server/tts.rs` + the TTS half of `ui_server/hf_validate.rs`
  → `src/http.rs`: model listing/download/settings/synthesize routes verbatim,
  plus `check_tts` (HF pre-download validation) merged in as one shared
  `normalize_hf_id` instead of the old duplicate copy in `hf_validate.rs`.
- **New route**: `POST /v1/audio/speech` (OpenAI `{model?, input, voice?,
  response_format, speed?}` → WAV). Implemented as a thin body-parsing
  wrapper around the **same** `synthesize_response()` function
  `/api/tts/synthesize` calls, so fallback/header behavior (`X-TTS-Fallback`,
  degrade-not-400) is identical on both routes by construction, not by
  copy-paste. Rejects `response_format` other than `"wav"` with 400.
- `settings_store.rs` — new, `TtsSettings`, legacy key `"ttsConfig"` (the
  struct shape itself is unchanged from `group_manager::types::TtsSettings`).

### Shape change
None to existing routes. `POST /v1/audio/speech` is additive, per the
protocol doc §4.6.

### Tests
`cargo test` (re-run in full after disk recovered): **36 passed, 1 ignored,
0 failed** in the bin test binary + **1 passed** in `tests/manifest.rs` = 37
total. This confirms `openai_audio_speech_route_speaks_macos_speech` (the one
failure seen during the disk crisis, HTTP 200 expected got 500) was indeed
the environmental flake it looked like — it now passes in the same full,
parallel run with no isolation needed. The 1 ignored test is VieNeu native
synthesis gated on a downloaded model dir (exercised for real below via
HTTP instead). Added test coverage beyond the original: `normalize_hf_id`'s
own test (previously only in the daemon's separate `hf_validate.rs`, now
merged), both `check_tts` cases the daemon had (`VitsModelForPreTraining`
accept, `XttsModel`/multi-speaker reject), and a fallback-degrades-not-400
test for the OpenAI route specifically.

### Release build + `otool -L`
`cargo build --release`: clean, 50s. `otool -L` on the resulting 35 MB
binary: only system frameworks (`libc++`, Foundation, CoreFoundation,
CoreML, libobjc, libSystem, libiconv) — **no onnxruntime dylib**, confirming
`ort`'s `download-binaries` statically links here exactly as it does for
sen-sysone (same `=2.0.0-rc.12` pin, same platform). `make package` printed
the same conclusion independently: "sen-tts needs no bundled onnxruntime
library (statically linked or system-found)".

### Live check (passed, real VieNeu model)
Started the release binary in a scratch `SENCLAW_HOME`/
`SENCLAW_RUNTIME_DATA_DIR` with `SENCLAW_TTS_MODELS_DIR` pointed at the real,
read-only `~/.senclaw/tts-models` and a `SENCLAW_RUNTIME_TOKEN` set:
- Auth: `GET /health` → 200 with no token; `GET /api/tts/models` → 401
  with no/wrong token, 200 with the right one, correctly listing
  `pnnbao-ump/VieNeu-TTS-v3-Turbo` as `installed: true` at its real on-disk
  path.
- **`say` fallback**, both routes: `POST /api/tts/synthesize
  {"text":"Xin chào","model_id":"macos-speech"}` → 200, valid 16-bit/22050 Hz
  WAV, `X-TTS-Backend: macos-speech`. `POST /v1/audio/speech
  {"input":"Hello...","model":"macos-speech-en","response_format":"wav"}` →
  200, valid WAV, `X-TTS-Backend: macos-speech-en`.
- **Real VieNeu-TTS v3 Turbo model**, both routes, two different voice
  presets: legacy route with voice "Phạm Tuyên" → 200, valid 48 kHz WAV
  (284 KB, ~1.4 s round trip), `X-TTS-Backend: pnnbao-ump/VieNeu-TTS-v3-Turbo`
  (not a fallback). OpenAI route with voice "Ngọc Linh" → 200, valid 48 kHz
  WAV (200 KB), same backend header. Confirms the full ONNX inference chain —
  tokenizer, sea_g2p normalize + phonemize, npz weight loading, ONNX Runtime
  sampling, codec decode — works end to end against the real checkpoint on
  both routes.
- Validation: `response_format: "mp3"` on `/v1/audio/speech` → 400, as
  designed.
- `POST /runtime/shutdown` → 202, process exited cleanly. Nothing left in
  `~/.senclaw` (all writes scoped to the scratch dir; the model dir was
  opened read-only).

### `make package` (verified, archive kept)
`sen-tts-0.1.0-darwin-arm64.tar.gz` — checksum verified (`shasum -a 256 -c`),
contents confirmed (`senclaw-runtime.json` + `bin/sen-tts`). Kept at
`sen-tts/dist/sen-tts-0.1.0-darwin-arm64.tar.gz` (not deleted) for the lead's
install-local test.

### Package contents
`bin/sen-tts` (35 MB) + `senclaw-runtime.json`; no bundled onnxruntime
library needed (confirmed, see above).

### What the daemon must know
`POST /v1/audio/speech` is new, live, and now verified end to end against
both the `say` fallback and the real VieNeu model on this machine. Everything
else is unchanged from the old daemon namespace.

---

## Cross-repo notes

- All three pin `ort = "=2.0.0-rc.12"` (exact), not the loose
  `"2.0.0-rc.10"` the old monorepo's `Cargo.toml` *declared* — that loose
  requirement let a fresh resolve pick `rc.13`, which renamed
  `ort::execution_providers::CPUExecutionProvider` and broke the copied
  `LayaEngine`/`VieNeuEngine` code. The old repo's actual `Cargo.lock` had
  resolved to `rc.12`, which is what the copied engine code was written
  against and validated (parity test) — so `=2.0.0-rc.12` is the correct pin,
  not an arbitrary choice.
- `sen-runtime-sdk` needed its own `[workspace]` table (fixed by the lead
  session, not me) — without it, Cargo inferred `senclaw/` as its workspace
  root via ancestor-directory search (since `senclaw/Cargo.toml` originally
  listed `crates/sen-runtime-sdk` as a member), so any `cargo build/test` in
  a sen-* repo needed `senclaw/Cargo.toml` to parse cleanly — it did not,
  transiently, while phase-01 was mid-edit removing OCR/TTS/decision-laya. I
  never wrote to `senclaw/` myself (confirmed via my own bash history: only
  `ls`/`grep`/one failed-before-touching-anything `cargo metadata` with a
  typo'd flag).
- `Cargo.lock` is present and committed-ready in all three (generated during
  successful builds) — pins the exact working dependency set including the
  `ort` downgrade.
- `cargo test --bin <name>` does **not** skip a package's dev-dependencies,
  contrary to what I originally assumed and reported. Verified empirically on
  sen-ocr: `cargo test --bin sen-ocr --features ocr-paddle-metal` still
  compiled the full `imageproc`/`ab_glyph`/`rav1e` chain (0 tests ran, since
  all of sen-ocr's tests live in the library, not `main.rs`) before I got to
  the unscoped `cargo test`, which is what actually ran them. Cargo resolves
  and builds the whole test-profile dependency graph for the package
  regardless of `--bin` scoping; only `--examples`/`--example <name>` control
  whether the *example itself* gets built. Worth knowing for future disk/CI
  planning — there is no cheap way to test just the binary target when the
  package also declares dev-dependencies for an example.
- Every package's own `usage()`/`--help` path calls `std::process::exit(1)`
  unconditionally (by design, `-> !` return type) — seen on both sen-ocr and
  sen-sysone. Not a bug: these binaries are launched by the daemon with
  `serve --host … --port …`, not used interactively; the usage text still
  prints correctly, it just isn't a "0 means help was shown" CLI.

## Unresolved questions

1. `~/.local/bin/senclaw` on this machine is a pre-migration build with no
   `runtime` subcommand — worth confirming with whoever owns the daemon
   phase whether that is expected to still be there or should be replaced/
   removed once the new daemon ships its `runtime install-local` command.
2. sen-ocr's `ocr_roundtrip` example's dev-dependency weight (`imageproc`/
   `ab_glyph` → `rav1e`/`av1-grain`/`ring`/`tar`) is real (confirmed ~1-2 GB,
   ~1 minute release-build time) and worth knowing about for CI sizing, even
   though I did not change it (the phase doc requires keeping the example,
   and I did not add these dependencies — they were already in the old
   monorepo's `Cargo.toml`, just cheap there because of overlap with the
   rest of that huge dependency graph).
3. All three `dist/*.tar.gz` archives are being kept on disk (per the lead's
   final instruction, for the install-local end-to-end test) rather than
   deleted after verification like earlier in the session — flagging so
   whoever runs that test knows where they are:
   `sen-sysone/dist/sen-sysone-0.1.0-darwin-arm64.tar.gz`,
   `sen-ocr/dist/sen-ocr-0.1.0-darwin-arm64.tar.gz`,
   `sen-tts/dist/sen-tts-0.1.0-darwin-arm64.tar.gz`.

Status: DONE
Summary: All three CPU runtimes (sen-sysone, sen-ocr, sen-tts) are fully ported, tested, and verified end to end — `cargo test` green, release builds, `otool -L` shared-lib checks, `make package` producing valid checksummed archives, and live checks against real models (Laya multilingual parity, OCR round-trip in English and Vietnamese, TTS via both `say` fallback and the real VieNeu-TTS model) all passed.
Concerns/Blockers: None remaining. The disk-space crisis documented in this report (not caused by these three repos) fully resolved once the user cleared build caches; all deferred verification steps completed afterward with no code defects found.
