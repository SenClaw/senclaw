#!/usr/bin/env bash
# Type-check the Windows sandbox backend (src/sandbox/backend/direct_windows.rs and
# its tests) from a Mac or Linux box, and run the platform-neutral half of it.
#
# The whole crate cannot be cross-checked cheaply (a full `cargo check --target
# x86_64-pc-windows-gnu --lib` needs ~2 GB of build output), so this builds a tiny
# scratch crate that `#[path]`-includes the REAL files with stubs for the few
# crate-internal types they touch. It proves the Win32 calls are used with the right
# signatures; it does not prove the jail holds — .github/workflows/sandbox-windows.yml
# does that on a Windows runner.
#
#   scripts/typecheck-windows-sandbox.sh            # check + run winproc tests
#   CARGO_TARGET_DIR=/path scripts/typecheck-windows-sandbox.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/senclaw-winchk.XXXXXX")"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${TMPDIR:-/tmp}/senclaw-winchk-target}"
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$WORK/src"
cat > "$WORK/Cargo.toml" <<'EOF'
[package]
name = "winchk"
version = "0.0.0"
edition = "2021"

[dependencies]
tokio = { version = "1", features = ["rt", "rt-multi-thread", "macros"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tempfile = "3"

# Keep in step with [target.'cfg(windows)'.dependencies.windows] in ../Cargo.toml.
[target.'cfg(windows)'.dependencies.windows]
version = "0.58"
features = [
  "Win32_Foundation", "Win32_Security", "Win32_Security_Authorization",
  "Win32_Security_Isolation", "Win32_Storage_FileSystem",
  "Win32_System_Diagnostics_ToolHelp", "Win32_System_JobObjects",
  "Win32_System_Memory", "Win32_System_Pipes", "Win32_System_Threading",
]
EOF

cat > "$WORK/src/lib.rs" <<EOF
#![allow(dead_code)]
pub mod bg_tasks {
    #[derive(Clone, Debug, Default)]
    pub struct Held;
}
pub mod sandbox {
    pub mod db {
        use crate::sandbox::{fsmode::FsMode, mounts::Mount, ports::PortPolicy};
        #[derive(Clone)]
        pub struct Sandbox {
            pub id: String, pub name: String, pub backend: String, pub image: Option<String>,
            pub workdir: String, pub network: bool, pub cpus: f64, pub memory_mb: i64,
            pub pids_limit: i64, pub timeout_ms: i64, pub env: serde_json::Value,
            pub mounts: Vec<Mount>, pub fs_mode: FsMode, pub trace_enabled: bool,
            pub ports: PortPolicy, pub status: String, pub container_id: Option<String>,
            pub last_error: Option<String>, pub created_at: i64, pub updated_at: i64,
            pub last_used_at: Option<i64>,
        }
        pub fn new_id() -> String {
            format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
        }
    }
    pub mod fsmode {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        pub enum FsMode { Strict, Allowlist, Open }
    }
    pub mod mounts {
        #[derive(Clone)]
        pub struct Mount { pub source: String, pub target: String, pub read_only: bool }
    }
    pub mod ports {
        #[derive(Clone, Default)]
        pub struct PortPolicy;
    }
    pub mod monitor {
        pub fn register(_: &str, _: u32) {}
        pub fn unregister(_: &str, _: u32) {}
    }
    pub mod config {
        pub fn workspaces_dir() -> std::path::PathBuf { std::path::PathBuf::from("w") }
    }
    pub mod backend {
        use std::collections::BTreeMap;
        use crate::sandbox::db::Sandbox;
        pub const MAX_OUTPUT: usize = 100_000;
        #[derive(Debug, Clone)]
        pub struct ExecSpec {
            pub script: String, pub timeout_ms: u64, pub extra_env: BTreeMap<String, String>,
            pub argv: Option<Vec<String>>, pub keep_leftovers: bool,
        }
        #[derive(Debug, Clone, Default)]
        pub struct Outcome {
            pub exit_code: Option<i32>, pub stdout: String, pub stderr: String, pub truncated: bool,
            pub timed_out: bool, pub duration_ms: i64, pub isolation: String,
            pub group: Option<u32>, pub held: Option<crate::bg_tasks::Held>,
        }
        pub fn clamp(s: String) -> (String, bool) { (s, false) }
        pub fn build_env(_sb: &Sandbox, extra: &BTreeMap<String, String>, home: &str) -> Vec<(String, String)> {
            let mut v = vec![
                ("PATH".to_string(), std::env::var("PATH").unwrap_or_default()),
                ("HOME".to_string(), home.to_string()),
                ("TMPDIR".to_string(), format!("{home}/.tmp")),
            ];
            for (k, val) in extra { v.push((k.clone(), val.clone())); }
            v
        }
        #[path = "$ROOT/src/sandbox/backend/winproc.rs"]
        pub mod winproc;
        #[cfg(windows)]
        #[path = "$ROOT/src/sandbox/backend/direct_windows.rs"]
        pub mod direct_windows;
    }
}
EOF

cd "$WORK"
echo "== type-check direct_windows.rs for x86_64-pc-windows-msvc (lib + tests)"
cargo check --tests --target x86_64-pc-windows-msvc "$@"
echo "== run winproc unit tests on this machine"
cargo test --lib backend::winproc
