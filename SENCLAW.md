# SENCLAW.md

Repository notes for the SenClaw agent when this checkout is a chat's working
directory. The maintainer guidance lives in [CLAUDE.md](CLAUDE.md) — read that
first; everything below defers to it.

- Rust daemon in `src/`, web UI in `web/`, desktop app in `desktop_app/`,
  mobile app in `channel_app/`.
- Verify with `cargo test` (daemon), `cd web && npx tsc --noEmit` (web),
  `cd desktop_app && dart analyze` (desktop). Never run `cargo fmt` across the
  repo.
- Plans and reports go under `plans/`, documentation under `docs/`.
