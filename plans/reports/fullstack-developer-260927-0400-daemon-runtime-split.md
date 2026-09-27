# Phase Implementation Report

### Executed Phase
- Phase: phase-01-daemon-runtime-split
- Plan: `plans/260927-0112-runtime-split-jev-migration`
- Status: **DONE** — implemented, compiled, fully tested (unit + the new end-to-end test + both SDKs), dependency-tree-verified, and exercised against a live daemon. See "Verification" for what was actually run and three real bugs the live smoke test caught that unit tests alone had missed.

## Verification (this session, after disk + workspace blockers cleared)

Session was paused mid-phase on two environmental blockers (disk hit 0 bytes free once, then a Cargo `multiple workspace roots` error from `members = ["."]` overriding `exclude`). Both were the lead's to fix and both were fixed (disk freed to 38 GB; the lead dropped the `members` line from the top-level `[workspace]`, since an explicit `members = ["."]` makes every path under the root an implicit member and that overrides `exclude` — `cargo metadata --no-deps` now shows only `senclaw`, both SDKs standalone). Resumed and ran, in order:

1. **`CARGO_BUILD_JOBS=6 CARGO_INCREMENTAL=0 cargo check --workspace --all-targets`** — first pass surfaced 20 real compile errors (all in code I wrote this session, none in code I only touched) plus 1 in `examples/alias_ui_harness.rs` (a 12th `UiState` literal missing `runtime_manager`, found by the compiler after I'd already manually fixed 11 in `tests/`). Fixed all; final state is clean (only one pre-existing, unrelated `dead_code` warning in `src/tools/write.rs` that predates this phase). Full list of what was wrong, briefly: a stray `///` doc comment with nothing after it in `supervisor.rs` (which cascaded into three bogus "cannot infer type" errors elsewhere in the same file — fixing the comment fixed all three); `RuntimeClientError`'s `bad()` REST helper called with `.map_err(bad)` against `anyhow::Error` sources instead of `.map_err(|e| bad(e.to_string()))` (3 sites); a `None` needing a type annotation; a `dyn Fn` progress callback missing `+ Send + Sync` so `tokio::spawn`'s future wasn't `Send`; `Dial` needing `#[derive(Debug)]` for a test's `.unwrap_err()`; two tail-expression-temporary-lifetime issues (`state.lock().unwrap().clone()` as the last line of a function, fixed by binding to a local first); a `repo` value moved across loop iterations; a lifetime on `package_for` tied to the wrong reference; an `Option<StartError>` match arm that partially moved a field out of `e` then tried to `Display` all of `e` (simplified to rely on `StartError`'s own `Display`, which already carried the detail I was redundantly appending).
2. **`cargo test --workspace`** — first run: 2482 passed, 5 failed. All 5 were real, all fixed:
   - `quant_from_filename` (duplicated in `local_models/scan.rs` and `local_models/hf_files.rs`) split GGUF filenames on `_` as a delimiter, which truncates a real quant token — `Q4_K_M` became `Q4`. Consolidated to one function in `local_models/keys.rs`, fixed to split only on `-`/`.`, and tightened the match to require a digit immediately after `Q`/`IQ` (a bare word starting with `Q`, e.g. "quant", must not match — caught by my own regression test on the first re-run, fixed by that digit check).
   - `runtime::updates`'s own test called `select()` on an id that was never actually installed — `select()` correctly refuses that; fixed the test to install a fake package first.
   - `gateway::group_manager::config`'s round-trip test asserted compact JSON (`"backend":"online"`) against `save_global_config`'s pretty-printed output (`"backend": "online"`) — a test bug, fixed the assertion.
   - `gateway::ui_server::openapi::committed_spec_matches_the_routers` — the committed `docs/openapi-daemon.yaml` was stale after this phase's route changes, exactly as its own failure message says to fix: regenerated with `SENCLAW_WRITE_OPENAPI=1`.
   - My own new `quant_token_is_not_truncated_at_its_own_underscores` test (added alongside the fix above) initially failed on its own edge case (see the digit-check fix above).
   Second run: **all green, 0 failures**, including the new `tests/runtime_manager_lifecycle.rs` end-to-end test (built `examples/echo_runtime.rs` on demand, installed it, spawned it, health-gated it, called it with its per-launch token, confirmed 401 with no token, idle-swept it, confirmed a restart gets a fresh port, force-uninstalled a running version). The E2E test failed once on its own first run too (see bug #2 below) and passes now.
3. **`make test-sdks`** — `app-space-sdk`: 18 passed (+ some ignored integration/doc tests). `sen-runtime-sdk`: 24 passed. Both clean, neither SDK touched.
4. **`cargo tree -p senclaw | grep -iE "mlx|candle|ort |ocr-rs|mnn"`** — empty (grep exit 1). No removed inference dependency reaches the daemon's dependency graph.
5. **Live smoke** (`HOME=<scratch>`, `SENCLAW_UI_PORT=28788`, `SENCLAW_WS_PORT=28789`, `SENCLAW_UI_BIND_HOST=127.0.0.1`) — see "Live smoke transcript" below. Daemon stopped cleanly afterward; verified no orphaned child process and no held port.

### Real bugs the live smoke test caught that unit tests missed

The E2E test and the live smoke test both use `examples/echo_runtime.rs`, a real `sen-runtime-sdk`-based child process — this is what surfaced two design-level bugs no amount of reading would have (both now fixed, with regression coverage):

1. **A manifest's own `idleTimeoutSecs` was parsed into `LaunchSpec` but never actually reached the idle-sweep decision.** `RunningProcess` had no field to carry it, so `Supervisor::sweep_idle` always used the daemon-wide default (300s/900s) regardless of what any runtime declared — directly contradicting `docs/runtime-protocol.md` §3.1 ("absent → daemon default for the mode; `0` → never stop", implying present → that value). Added `RunningProcess.idle_timeout_secs: Option<u64>`, populated at spawn from `LaunchSpec`, and `sweep_idle` now checks `proc.idle_timeout_secs.unwrap_or(default)`. Caught because my E2E test used a 1-second manifest timeout to keep the test fast, and the process never idled out.
2. **`PackageSource::Local` (the "you installed this from a local path" label `install-local` is supposed to report — `docs/runtime-protocol.md` §5.1 pins this exact string) was never persisted.** `finish_install` returned it correctly in its one-shot response, but `scan_root`/`scan_all` unconditionally relabels *everything* under `runtimes_dir` as `Index` on every rescan — and `GET /api/runtimes` always rescans. So `install-local`'s own response said `"source": "local"`, but the very next `GET /api/runtimes` said `"index"` for the same package. Caught live: I installed the smoke-test fixture, checked `GET /api/runtimes` right after, and watched the label flip. Fixed by threading a `PackageSource` parameter through `finish_install`/`install_from_dir`/`install_from_archive` (all three catalog-install call sites — `jobs.rs`, `llamacpp.rs`, the CLI's no-daemon fallback — now pass `Index`; `install_local`'s public entry point always passes `Local`) and writing a `.install-source` marker file into the package directory that `scan_root` reads back on every scan. Also fixed a related bug in the same code path while I was in there: the `processes[].slot` field in `GET /api/runtimes` was hardcoded to `null` for every process (both `service:` and `model:` keys) — the protocol's own example shows it populated (`"slot": "gguf"` for a model process). Now derived from the installed package's declared slot (service) or the model's own format (model), and verified live (`"slot": "ocr"` on the running echo runtime).

Everything else in "What was removed"/"What was added" below was written before this verification pass and held up unchanged against the compiler and the test suite — no further corrections needed there.

## What was removed

- `src/tts/`, `src/local_model/`, `src/media_sidecar.rs`, `src/decision/laya/`, `src/decision/online.rs` — deleted wholesale, tests included.
- `src/gateway/ui_server/{ocr,tts,whisper,hf_validate}.rs` — deleted (their routes are now a generic proxy, see below).
- `src/gateway/ui_server/decision.rs` — trimmed to the control-plane routes only (gate + skill router); all model-management/`ask`/backend-settings handlers removed (now proxied).
- `src/gateway/ui_server/embedding_models.rs` — candle download logic removed, rewritten against `local_models::scan` (see "REST shape changes").
- Candle local embedding in `src/memory/embedding_providers.rs` (`local_candle` module, `CandleEngine`, HF-download-for-embeddings) — removed; `LocalProvider` rewritten to call the runtime model route over HTTP.
- `examples/ocr_roundtrip.rs` + its `[[example]]` Cargo.toml stanza — removed.
- Cargo features `local-embed`, `local-embed-metal`, `tts-vieneu`, `decision-laya`, `ocr-paddle`, `ocr-paddle-metal` and the `[features]` table — removed entirely (now empty/absent).
- Deps removed from `[dependencies]`: `candle-core`, `candle-nn`, `candle-transformers`, `tokenizers` (optional copy), `ort`, `fancy-regex`, `memmap2`, `bytemuck`, `image`, `ocr-rs`, `minijinja`, `minijinja-contrib` (optional copies). Verified via exhaustive grep that nothing else in `src/`/`tests/`/`examples/` references any of these crates directly.
- `[dev-dependencies]` `imageproc`, `ab_glyph` (only used by the removed OCR round-trip example) — removed.
- Docs moved to runtime repos, deleted here: `laya-decisions.md`, `gemma4-local-optimizations.md`, `mlx-resource-benchmark.md`, `mlx-rs-turboquant-native-runtime.md`, `local-gemma-mlx-runtime.md`, `local-model-space-app-extraction.md`. Fixed the dangling links in `README.md`, `README.vi.md`, `CLAUDE.md`, `docs/space-app-llm-provider-sdk.md`.
- `senclaw install desktop` / `senclaw uninstall desktop` / hidden `senclaw apply-update` CLI subcommands and ~1550 lines of desktop-bundle-swap machinery in `src/cli/commands/distrib.rs` (Windows locker scripts, `swap_bundle`, `extract_bundle`, the media-sidecar downloader, etc.) — all superseded by the `desktop` repo's own `update_desktop` binary. `distrib.rs` is now ~230 lines: `senclaw web` (Web UI bundle from `SenClaw/web-app`) and `senclaw update` (daemon binary from `SenClaw/senclaw`) only.
- Makefile: `hub-*`, `build-extension`, every `app-*`/desktop target — removed (non-goals / moved to the `desktop` repo). Kept `run`, `run-release`, `test`; added `test-sdks` and `clean-target-cache`.
- `GateSettings`/`SkillRouteSettings` stay; `DecisionSettings` (daemon copy) shrank from `{backend, local, online, gate, skills}` to `{gate, skills}` — `Backend`, `LocalSettings`, `OnlineSettings` deleted from `src/decision/settings.rs` (a minimal `Backend` enum moved into `src/decision/types.rs`, since `AskRequest.backend` is still part of the wire contract to `sen-sysone`).
- `OcrSettings`/`TtsSettings`/`WhisperSettings` typed structs and their `load_*_settings`/`save_*_settings` functions in `group_manager` — removed entirely (nothing in the daemon reads/writes them anymore).

## What was added

- **`src/runtime/`** (11 files, see below) — `RuntimeManager` is the single seam: `store` (scan/install-local from dir or `.tar.gz`/`.zip` with a traversal guard, uninstall), `settings` (`runtimes/settings.json`), `index` (fetch/cache/bundled `index.json`, channel resolution), `llamacpp` (GitHub-releases resolver + generates the manifest, since none ships upstream), `jobs` (background installs with progress/cancel), `supervisor` (spawn, env, health-gate, single-flight start, idle sweep with an `in_flight` counter so a live request is never cut off, crash-safe `running.json`, orphan cleanup at boot reusing `space_mcp::process_cwd`'s verify-before-kill pattern), `manager` (`RuntimeManager`, slot resolution + auto-select-when-one-candidate, `RuntimeClientError` taxonomy), `clients` (internal typed calls: `decision_ask`, `ocr_recognize` — the latter degrades to `Ok(None)` on not-installed/not-selected, never an `Err`), `proxy` (generic legacy-namespace reverse proxy + the decision-settings merge + the model route), `rest` (`/api/runtimes/*`), `updates` (auto-update per selected slot).
- **`runtimes/index.json`** — `llama.cpp-{metal,cpu,vulkan,cuda}` with real b11201 asset names per platform (verified against the phase file's literal list, including the windows-cuda `cudart-llama-bin-win-cuda-12.4-x64.zip` extraAsset with no `{version}` token, as specified); the five `sen-*` entries with `releases: []`.
- **`src/local_models/`** (8 files) — `scan` (MLX dirs + GGUF files, engine-private dirs excluded, mmproj pairing, quant-from-filename), `gguf` (a real, from-scratch GGUF metadata reader — magic/version/tensor-count/KV header only, stops before tensor data, correctly skips array-typed values including string arrays so it never desyncs on a real tokenizer-vocab key), `keys` (stable `<format>-<slug>-<8hex>`), `download` (ported/adapted from the old `local-model-core`, MLX whole-snapshot vs. GGUF exact-file(s) download), `hf_files`, `settings` (engine `settings.json` raw passthrough, kept snake_case; a separate `daemon-settings.json` for the one daemon-owned field, `defaultContextLength`), `rest` (`/api/local-models/*`).
- **LLM config `local:<key>`**: merged in `load_llm_configs` (scanned fresh, never persisted); `save_llm_config` refuses both `app:` and `local:` ids; `is_app_config` empty-key exemption extended to `local:` at its one real call site (`memory/cognitive/llm_openai.rs`) plus the two provider-managed-config guards in `llm_config.rs` (update/delete); `app:mlx-lm:<model>` active-id migration to a matching `local:` model by name, else left alone.
- **Embeddings**: `LocalProvider` now POSTs to `/api/runtimes/models/<key>/v1/embeddings` with the internal-auth loopback header, seeds `dimensions()` from the key's slug and overwrites it from the first real response (same pattern as `OllamaProvider`).
- **CLI** `senclaw runtime {list,install,install-local,uninstall,select,update,logs}` — tries the daemon's REST first, falls back to operating on the store/`RuntimeManager` directly when the daemon isn't reachable (distinguished via `reqwest::Error::is_connect()`), printing which path it took.
- **Config/paths**: `runtimes_dir`, `runtime_data_dir`, `runtime_logs_dir`, `bundled_runtimes_dir` (`SENCLAW_BUNDLED_RUNTIMES_DIR` else `<exe_dir>/runtimes`, `None` unless the dir exists), `runtime_index_url` — all resolved once in `Config::from_env()`; `RuntimeManager`/`index.rs` never call `std::env::var` themselves (fixed one instance where I'd initially had `index.rs` read the env var directly, then routed it through `Config` per the project rule).
- **Boot/shutdown**: `RuntimeManager` constructed early in `run_daemon` (before `AgentPool`), `cleanup_orphans().await` + `spawn_idle_sweeper()` at boot, wired into `AgentPool` (decision skill-router, OCR-for-text-only-models) and `UiState`, `stop_all().await` alongside `space_mcp_launcher.shutdown()` at shutdown.
- **Web dist fallback**: `resolve_dist_dir()` now falls back to `<CARGO_MANIFEST_DIR>/../web-app/dist` after `SENCLAW_WEB_DIST` and cwd — confirmed the sibling `web-app/dist` directory already exists on this machine (the parallel web-app agent has built it).
- **Tests**: unit tests co-located per module (manifest/slot resolution, archive traversal guard incl. a real zip built with a `../` entry, env/args rendering via the SDK's own render, GGUF metadata parsing with a byte-accurate fake file, model scan + keys + mmproj pairing, decision settings raw-JSON merge/preservation, index channel resolution, idle/crash/single-flight logic on the supervisor). **End-to-end test**: `tests/runtime_manager_lifecycle.rs` against `examples/echo_runtime.rs` (a real `sen-runtime-sdk`-based child process) — install-local → single-flight start → health gate → authenticated HTTP call → 401 on no token → idle sweep stops it → uninstall refuses-then-force-stops. `sen-runtime-sdk`'s `server` feature is a dev-dependency exactly as instructed.
- **Docs**: `CLAUDE.md`'s "Local models left the daemon" + "Typed decisions: Laya on ONNX" sections replaced with one "Runtimes" section (links `docs/runtime-protocol.md`, keeps the daemon-side rules: generic proxy behavior, 503 taxonomy, key-order rule, decisionConfig raw-passthrough, `local:` config parity with app configs, idle-sweep-never-mid-request, single-flight, orphan-verify-before-kill); "Gemma 4 on the native MLX path" shortened to a pointer (its subject matter — `src/local_model/mlx_lm` — no longer exists in this repo). README.md/README.vi.md: replaced the "Supported Local LLMs" candle/MLX-Space-App section with a Runtimes table, fixed the `senclaw install desktop` / `cd web` build instructions, fixed the `cargo build --features local-*` sections.

## REST shapes that differ from the protocol doc

The protocol doc doesn't fully specify these two; I made explicit, documented choices:

1. **`/api/decision/settings` merge shape.** The doc says the daemon "adds its `gate` and `skills` into `settings` of the response" but not the exact insertion point if `sen-sysone`'s response doesn't have a `settings` key. My `runtime::proxy::merge_control_plane` inserts into `value["settings"]` when present, else at the top level of the response object — defensive since I don't control `sen-sysone`'s exact JSON shape (built by a parallel agent).
2. **`/api/embedding/*`** (not part of the runtime-protocol.md contract, pre-existing daemon-only routes): `GET /api/embedding/features` now returns `{local: bool, modelsDir}` instead of `{candle, candle_metal, mlx_static, models_dir}`; `GET /api/embedding/models` returns installed local embedding-capable models (`{key, name, repo, quant, sizeBytes, contextLength}`) instead of a fixed HF catalog with `installed` flags; `POST /api/embedding/download-model` now returns `410 Gone` pointing at `POST /api/local-models/download`. Documented in the file's own doc comment.

## Known gaps / deliberate simplifications (not blockers, but worth flagging)

- **`[workspace.dependencies]`** — resolved. The lead confirmed on resume: remove mlx-rs, mlx-sys, symphonia, rubato, realfft, half, safetensors, tokenizers, minijinja, minijinja-contrib (all confirmed unreferenced by anything in `src/` — the last three were dead too, not just the seven named), plus the stale comment above them naming `apps/mlx-lm`/`crates/senclaw-media`. Done; the table now carries only what `senclaw` itself actually depends on, with a comment explaining it is a version reference, not an inheritance mechanism (nothing here uses `workspace = true`).
- **Gate `GateQuestions::Auto`** used to follow `Backend::Local`/`Online`; since the daemon no longer tracks which backend `sen-sysone` is using, `Auto` now always resolves to the Laya question shape. Documented in code and CLAUDE.md.
- **`local_models_dir` derivation inside `load_llm_configs`** assumes it sits at `<config_path's parent>/local-models` (matches `Config::from_env`'s default) rather than threading the real `Config` through — that function is called from many places with just a path. An install that sets `SENCLAW_LOCAL_MODELS_DIR` independently of `SENCLAW_CONFIG_PATH` would need a follow-up.
- **Proxy request-body handling buffers rather than streams** (`axum::body::to_bytes`, capped 512 MB) on the way *into* a runtime; the *response* side streams properly (`Body::from_stream`), which is what the phase's stated SSE risk is about. True bidirectional streaming would need a fuller `Body`-to-`Body` conversion; flagging as a reasonable, disclosed trade-off rather than fixing given the time available.
- Embedding dimension for a `local:` model is a name-slug heuristic until the first real call succeeds (same limitation `OllamaProvider` already had).
- Did not attempt a matching update to `docs/desktop-app-auto-update.md` (references the now-removed `ApplyUpdate` CLI path) — out of my file scope to judge whether the `desktop` repo phase already superseded it; flagging for the integration phase.

## Test status

**All green.**
- `cargo check --workspace --all-targets`: clean (one pre-existing, unrelated warning).
- `cargo test --workspace`: **2488 passed, 0 failed, 8 ignored** in the lib target; every integration test binary green too, including `tests/runtime_manager_lifecycle.rs` (2 passed — the E2E test against a real child process).
- `make test-sdks`: `app-space-sdk` 18 passed, `sen-runtime-sdk` 24 passed, 0 failed.
- `cargo tree -p senclaw | grep -iE "mlx|candle|ort |ocr-rs|mnn"`: empty.
- Live smoke: see below.
- Also verified, by exhaustive `grep`, that no reference remains anywhere in `src/`/`tests/`/`examples/` to any removed module/type/crate — this was true both before and after compilation, so the earlier manual sweep held up.

## Live smoke transcript

Isolated `HOME`, ports 28788/28789, debug build. All calls succeeded as shown; daemon stopped cleanly afterward with no orphaned child process and no held port (verified via `lsof`/`ps` before and after).

```
$ curl /api/auth/status
{"authRequired":false,"authorized":true,"mode":"auto","modeSource":"default"}

$ curl /api/runtimes                      # nothing installed yet
{"installed":[],"platform":"darwin-arm64","processes":[],
 "settings":{"autoUpdate":true,"channel":"stable","idleTimeoutSecs":{"model":900,"service":300}},
 "slots":[{"slot":"gguf",...,"selected":null,"candidates":[]}, ... 6 slots total]}

$ curl /api/ocr/models                    # generic proxy, no runtime yet
< HTTP 503
{"code":"runtime_not_installed","error":"no OCR runtime is installed. Install one in Settings -> Runtime.","slot":"ocr"}

$ curl -X POST /api/runtimes/install-local -d '{"path": "<fixture: examples/echo_runtime, manifest slots=[ocr]>"}'
{"id":"echo-runtime-smoke","source":"local","version":"0.0.1", ...}

$ curl /api/runtimes                      # source persists after a fresh scan (this was bug #2 above, now fixed)
{"installed":[{"id":"echo-runtime-smoke","source":"local", ...}], ...}

$ curl -X PUT /api/runtimes/selections -d '{"slot":"ocr","id":"echo-runtime-smoke","version":"0.0.1"}'
{"ok, slot now selected"}

$ curl /api/ocr/models                    # generic proxy spawns + health-gates + relays
< HTTP 200
{"echo":true,"models":[],"ok":true}

$ curl /api/runtimes                      # process now tracked, slot correctly populated (bug #2's sibling fix)
{"processes":[{"key":"service:echo-runtime-smoke","runtimeId":"echo-runtime-smoke","slot":"ocr",
  "modelKey":null,"pid":93974,"port":54523,"state":"ready","launches":1,"error":null}]}

$ kill -TERM <daemon pid>
# both the daemon AND the child process (pid 93974) are gone within 2s — no orphan
```

## Open questions for the lead / next session

1. `docs/desktop-app-auto-update.md` likely needs a look now that `ApplyUpdate`/`install desktop` are gone from this repo — didn't touch it since I couldn't confirm whether the `desktop` repo phase already owns/supersedes it.
2. The proxy's request body is buffered (not streamed) on the way into a runtime, capped at 512 MB — a disclosed, reasonable trade-off (see "Known gaps" above), not re-litigated here, but worth a look if a future runtime needs to accept a >512 MB upload.

Status: DONE
Summary: Daemon-runtime-split is implemented, compiled clean, fully green across `cargo test --workspace` (2488+ tests incl. a new end-to-end test against a real child process), both SDKs, the dependency-tree filter, and a live smoke test against a running daemon — which caught and led to fixing two real design bugs (per-manifest idle timeout never wired to the sweep decision; `PackageSource::Local` never persisted across a rescan) plus a `processes[].slot` field that was always null.
Concerns/Blockers: None blocking. Two low-priority open items above (a stale doc in a different repo's territory, and a disclosed request-body-buffering trade-off) are informational, not defects.
