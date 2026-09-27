//! The contract between the SenClaw daemon and its runtimes.
//!
//! A *runtime* is an inference engine the daemon never links: MLX, llama.cpp,
//! the Laya decision model, OCR, Whisper, TTS. Each one is a separate program,
//! installed as a package under `~/.senclaw/runtimes/<id>/<version>/`, launched
//! by the daemon as a child process and reached over loopback HTTP — the way
//! LM Studio runs its engines. This crate is the part both sides must agree on:
//!
//! - [`manifest`] — `senclaw-runtime.json`: what a package is, which *slots* it
//!   can be selected for, and how to launch it (with `{placeholder}` rendering
//!   that refuses a misspelt placeholder instead of passing it through).
//! - [`env`] — the environment the daemon hands a runtime at launch, and
//!   [`env::LaunchEnv`] to read it back on the runtime side.
//! - [`platform`] — the `darwin-arm64`-style platform keys packages declare.
//! - [`api`] — the JSON bodies every runtime answers with (`/health`,
//!   `/runtime/info`, errors).
//! - [`legacy`] — reading a runtime's settings out of the daemon's
//!   `config.json` on first start, so a machine upgraded from the in-daemon
//!   engines keeps its choices.
//! - `server` (feature `server`, on by default) — the axum scaffold a runtime
//!   binary mounts its routes into: loopback-only bind, bearer-token auth,
//!   `/health` + `/runtime/info`, a watchdog that exits when the daemon dies,
//!   and graceful shutdown.
//!
//! The full protocol, including the daemon's side, is documented in the
//! senclaw repository at `docs/runtime-protocol.md`.

pub mod api;
pub mod env;
pub mod legacy;
pub mod manifest;
pub mod platform;

#[cfg(feature = "server")]
pub mod server;

pub use manifest::{Capability, Entry, ModelFormat, RunMode, RuntimeManifest, RuntimeType, Slot};
