# Phase 00 — Scaffold, baseline, contract, SDK (done)

- Copied git-tracked files (working tree) from the old repo:
  - `senclaw/` ← `src tests assets skills app-space-sdk senclaw-sdk docs scripts evals examples` + root files
    (`Cargo.toml Cargo.lock build.rs README* SENCLAW.md WIDGET_CONTRACT.md LICENSE Dockerfile docker-compose.yml .dockerignore .env.example .gitignore CLAUDE.md Makefile`).
  - `desktop/` ← `desktop_app/*`, `desktop/update_desktop/` ← `update_desktop/*`.
  - `web-app/` ← `web/*`.
- The old repo's uncommitted WIP (3 files) failed to compile (`missing_tool` not wired in `engine.rs`); those files were
  taken at `HEAD` (`f0f31bd`) instead.
- `senclaw/Cargo.toml` workspace: `members = ["."]`, `exclude = ["app-space-sdk", "crates/sen-runtime-sdk"]` — both SDKs
  are their own workspace roots (each manifest carries `[workspace]`), because other repos depend on them by path and
  Cargo otherwise made every runtime build parse the daemon's manifest (a half-edited daemon `Cargo.toml` broke all
  `sen-*` builds). Test them with `cargo test --manifest-path <sdk>/Cargo.toml`. Engine apps and `senclaw-media` removed.
- New crate `senclaw/crates/sen-runtime-sdk` (24 tests green): manifest schema + strict validation + placeholder rendering,
  launch environment, platform keys, common API bodies, legacy settings import, axum server scaffold
  (loopback-only bind, bearer auth, `/health` readiness, `/runtime/info`, parent watchdog, graceful shutdown).
- Contract written: `senclaw/docs/runtime-protocol.md`.
- `sen-mlx sen-sysone sen-ocr sen-whisper sen-tts` created with `git init -b main` (empty).
- Baseline: `cargo check --workspace --all-targets` green on the migrated daemon.
- Baseline `cargo test --workspace` on the migrated daemon: **2632 passed, 0 failed, 18 ignored**
  (full log: `/private/tmp/claude-501/-Users-benji-Projects-SenClaw/e2656e13-9e79-469a-8071-53768bad6455/scratchpad/baseline-test.log`).
