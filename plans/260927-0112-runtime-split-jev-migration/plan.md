# SenClaw migration: runtime split + JEV control plane (P0–P1)

Status: **done — awaiting user review** · 2026-09-27 · lead: main session

Incident 2026-09-27 01:40–09:00: host disk ran out (0 B free) while five agents built in parallel; the user approved
clearing `SemaClaw/target`, `senclaw-app-private/target` and the npm cache (+38 GB). Builds now share per-group target
dirs (`.cargo-target-mlx`, `.cargo-target-cpu`) with `CARGO_INCREMENTAL=0`.

## Decisions (user, 2026-09-27)

- JEV v2.2 depth this round: **P0–P1 + runtime split** (control_plane layer; other harness modules stay where they are).
- Runtimes live in **sibling repos** `/Users/benji/Projects/SenClaw/sen-*`, sharing `senclaw/crates/sen-runtime-sdk` by path.
- **Candle leaves the daemon**: GGUF chat + embeddings through upstream llama.cpp, MLX through sen-mlx. Daemon has zero inference code.
- **Parallel agents per repo** after the runtime contract is fixed.
- Daemon + runtimes stay **Rust**; desktop stays **Flutter**; web stays React/Vite.

## Outcome

Old monorepo `/Users/benji/Projects/SemaClaw` (read-only reference, commit `f0f31bd`) becomes:

| Repo | From | Role |
|---|---|---|
| `senclaw/` | `src/`, `tests/`, `assets/`, `skills/`, `app-space-sdk/`, `senclaw-sdk/`, `docs/`, `scripts/`, `evals/`, `examples/` | daemon — harness + control plane + runtime manager; no inference |
| `desktop/` | `desktop_app/`, `update_desktop/` | Flutter desktop console, bundles the daemon |
| `web-app/` | `web/` | React web UI served by the daemon |
| `sen-mlx/` | `apps/mlx-lm`, `apps/local-model-core` | MLX LLM runtime (model mode) |
| `sen-sysone/` | `src/decision/laya`, `online.rs`, `ui_server/decision.rs` | Laya / Jev decision runtime |
| `sen-ocr/` | `src/local_model/ocr`, `ui_server/ocr.rs` | OCR runtime |
| `sen-whisper/` | `crates/senclaw-media`, `ui_server/whisper.rs` | Whisper ASR runtime |
| `sen-tts/` | `src/tts`, `ui_server/tts.rs` | VieNeu + macOS TTS runtime |
| `senclaw-connect/` | `channel_app/` | Flutter mobile remote client (relay pairing) |
| upstream llama.cpp | ggml-org/llama.cpp releases | GGUF runtime, installed by the daemon |

Contract: [`docs/runtime-protocol.md`](../../docs/runtime-protocol.md) + [`crates/sen-runtime-sdk`](../../crates/sen-runtime-sdk).
Architecture source: `/Users/benji/Projects/SenClaw/JEV Architecture v2.html` (v2.2).

## Non-goals

- Not migrated: `hub-backend/`, `senclaw-extension-chrome/`, `9router/`, `apps/candle`, `apps/drawio`, old `plans/`, `bench-results/`.
- Follow-up 2026-09-27: `channel_app/` is now `senclaw-connect/` (package `senclaw_connect`). Pairing mints `senclaw://connect`, matching desktop and web, and still scans the old `semaclaw://connect` codes.
- The old repo's uncommitted WIP (`missing_tool` in `zen_core/conversation.rs`, `run_tools.rs`, `tools/task.rs`) — migrated at `f0f31bd` instead; port it once finished.
- JEV P2–P5 (tiers switched on through G2, Memory/KB/Event bus P3, Learner P4, DAG/delegate P5).
- Publishing releases, pushing to GitHub, committing (the user reviews first).

## Acceptance criteria

1. `senclaw`: `cargo test --workspace` green; `cargo tree` shows no mlx/candle/ort/ocr-rs/MNN; baseline tests that existed still pass (minus tests of code that moved out, which move with it).
2. Each `sen-*` runtime: builds, `cargo test` green, `make package` produces an archive with a valid manifest, serves its §4 contract; installed through `POST /api/runtimes/install-local` and driven by the daemon end to end.
3. GGUF: daemon installs an upstream llama.cpp build, downloads a GGUF, loads it JIT through a `local:` provider, streams a chat; a GGUF embedding model answers `/v1/embeddings`.
4. Space App registration works in the new daemon: `register` (manifest URL), `register-local`, `install-zip`, MCP + LLM autoRegister, start/stop/restart, uninstall — existing tests plus a live check.
5. `desktop`: `flutter analyze` no errors, `flutter test` green; Runtime screen (LM Studio-style) + Local models screen; OCR/TTS/Whisper/Decision screens show the runtime-missing state.
6. `web-app`: `npm run build` green; same screens.
7. Control plane P0–P1 per phase 06, unit-tested; default behavior unchanged (new specs shadow-only, JEV_OFF honored).
8. Docs: protocol doc; README + CLAUDE.md per repo carry the relevant old "Rules for Claude".

## Phases

| # | Phase | Owner | Depends on | Status |
|---|---|---|---|---|
| 00 | [Scaffold, baseline, contract, SDK](phase-00-scaffold.md) | lead | — | done |
| 01 | [Daemon runtime split](phase-01-daemon-runtime-split.md) | agent | 00 | done (+ 19 contract fixes after conformance review) |
| 02 | [sen-mlx + sen-whisper](phase-02-mlx-runtimes.md) | agent | 00 | done (145/26 tests, live checks on real Qwen + Whisper, packaged) |
| 03 | [sen-sysone + sen-ocr + sen-tts](phase-03-cpu-runtimes.md) | agent | 00 | done (57/14/37 tests, live checks on real models, packaged) |
| 04 | [desktop](phase-04-desktop.md) | agent | 00 | done (analyze clean, 303 + 7 tests green, update_desktop 12/12) |
| 05 | [web-app](phase-05-web-app.md) | agent | 00 | done (tsc + build green) |
| 06 | [Control plane P0–P1](phase-06-control-plane.md) | agent | 01 | done (agent_status + L1 offload wired behind switches) |
| 07 | [Integration, E2E, review](phase-07-integration.md) | lead | 01–06 | done (E2E + security fixes verified live; [report](../reports/lead-260927-1017-e2e-integration.md)) |

## Rules for every phase

- **Old repo is read-only.** Another session may be working in `/Users/benji/Projects/SemaClaw`; never write there.
- **Never touch the user's live daemon or data.** A SenClaw daemon runs on 18788/18789 with real data in `~/.senclaw`
  (models, DB, tokens). Tests and manual runs use `HOME=<scratch dir>` (every path derives from it) and ports
  `SENCLAW_UI_PORT=28788 SENCLAW_WS_PORT=28789`, runtimes on ephemeral ports. Reading an already-downloaded model
  from the real `~/.senclaw/local-models` (read-only) for a live check is allowed; writing is not.
- Stay inside your repo. `crates/sen-runtime-sdk` and `app-space-sdk` are frozen this round — need a change? list it in your report.
- Do not commit. No plan ids / phase numbers / finding codes in code, comments, test names.
- Match surrounding style: the old code explains *why* in comments; keep that density, keep existing tests.
- Heavy native builds: `CARGO_BUILD_JOBS=4`. Stop every process you start; never kill one you did not start.
- Report to `senclaw/plans/reports/<agent>-260927-<hhmm>-<slug>.md`, ending with
  `Status: DONE | DONE_WITH_CONCERNS | BLOCKED | NEEDS_CONTEXT`, a 1–2 sentence summary, concerns, unresolved questions.
