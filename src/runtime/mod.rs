//! Runtime manager: the daemon links no inference code. Every engine — MLX,
//! llama.cpp, the Laya decision model, OCR, Whisper, TTS — is a *runtime*: a
//! separate program the daemon installs, launches as a child process and
//! talks to over loopback HTTP, the way LM Studio runs its engines.
//!
//! Contract: `docs/runtime-protocol.md` + [`sen_runtime_sdk`] (manifest types,
//! launch environment, the server scaffold a runtime binary mounts).
//!
//! - [`store`] — installed packages on disk: scan, install (directory or
//!   `.tar.gz`/`.zip`, with traversal guards), uninstall.
//! - [`settings`] — `runtimes/settings.json`: slot selections, update channel,
//!   idle timeouts.
//! - [`index`] — `runtimes/index.json`: the catalog of installable runtimes,
//!   fetched/cached/bundled.
//! - [`llamacpp`] — resolving and installing an upstream llama.cpp build.
//! - [`jobs`] — background install jobs with progress and cancel.
//! - [`supervisor`] — spawn, health-gate, idle-sweep, crash accounting, orphan
//!   cleanup, stop-all.
//! - [`manager`] — [`manager::RuntimeManager`], the one seam the rest of the
//!   daemon goes through.
//! - [`clients`] — internal typed calls (decision ask, OCR recognize) for code
//!   running inside the daemon.
//! - [`proxy`] — the legacy-namespace reverse proxy (`/api/ocr/*` etc.) and
//!   the local-model route (`/api/runtimes/models/:key/*`).
//! - [`rest`] — `/api/runtimes/*` (§5.1).
//! - [`updates`] — check-for-updates + auto-update.

pub mod clients;
pub mod index;
pub mod jobs;
pub mod llamacpp;
pub mod manager;
pub mod proxy;
pub mod rest;
pub mod settings;
pub mod store;
pub mod supervisor;
pub mod updates;
pub mod version;

pub use manager::{RuntimeClientError, RuntimeManager, RuntimeManagerConfig};
pub use version::cmp_versions;
