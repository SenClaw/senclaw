//! OS-sandbox MCP server (`senclaw-sandbox`).
//!
//! The `sbx_*` toolset from the Sandbox Space App, served by the daemon binary
//! over stdio and backed by the built-in engine (`crate::sandbox`). Tools call
//! the engine directly — the same `runner` the REST API and the Web UI use, so
//! a limit enforced for one caller is enforced for all of them. State lives in
//! `~/.senclaw/sandbox/sandbox.sqlite` (WAL), safely shared with the daemon
//! process.
//!
//! Canonical names: `mcp__senclaw-sandbox__sbx_<verb>`. The `sbx_` prefix is a
//! registry exception like `cog_` — do not "normalize" it to `sandbox_`.

use std::collections::BTreeMap;

use anyhow::Result;
use rmcp::ServiceExt;
use serde_json::{json, Value};

use crate::sandbox::db::{Db, Run};
use crate::sandbox::{caps, code, files, fsmode, monitor, mounts, policy, ports, runner, settings};

fn ok_json(v: Value) -> String {
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".into())
}

fn err_json(e: impl std::fmt::Display) -> String {
    ok_json(json!({ "error": e.to_string() }))
}

/// A run, shaped for an agent: the fields that decide what to do next come
/// first, and the isolation actually applied is always included so the agent
/// can tell the user what protected them.
fn run_summary(run: &Run) -> Value {
    json!({
        "runId": run.id,
        "ok": run.exit_code == Some(0) && !run.timed_out,
        "exitCode": run.exit_code,
        "timedOut": run.timed_out,
        "truncated": run.truncated,
        "durationMs": run.duration_ms,
        "isolation": run.isolation,
        "network": run.network,
        "stdout": run.stdout,
        "stderr": run.stderr,
    })
}

fn env_map(env: Option<BTreeMap<String, String>>) -> BTreeMap<String, String> {
    env.unwrap_or_default()
}

// ── Parameter shapes (camelCase, matching the Space-App tool schemas) ────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct CapabilitiesParams {
    /// Re-measure instead of using the cached result (default false).
    #[serde(default)]
    refresh: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct RunOnceParams {
    /// One of the supported languages (see sbx_capabilities).
    language: String,
    /// The source code to run.
    code: String,
    /// `direct` or `docker`. Leave empty to pick automatically.
    #[serde(default)]
    backend: Option<String>,
    /// Allow network access (default false).
    #[serde(default)]
    network: Option<bool>,
    /// Run deadline in ms; default 30000, maximum 600000.
    #[serde(default)]
    timeout_ms: Option<i64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct CreateParams {
    #[serde(default)]
    name: Option<String>,
    /// `direct` or `docker`.
    #[serde(default)]
    backend: Option<String>,
    /// Docker image, docker backend only (default python:3.12-slim).
    #[serde(default)]
    image: Option<String>,
    /// Default false. Must be on to install packages.
    #[serde(default)]
    network: Option<bool>,
    #[serde(default)]
    cpus: Option<f64>,
    #[serde(default)]
    memory_mb: Option<i64>,
    /// Default deadline for each run in this sandbox.
    #[serde(default)]
    timeout_ms: Option<i64>,
    /// Ports the sandbox may serve on, reachable at 127.0.0.1:<port>.
    #[serde(default)]
    listen_ports: Option<Vec<u16>>,
    /// The only remote ports it may dial out to, e.g. [443]. Per PORT, not per
    /// host — it cannot express "only this website".
    #[serde(default)]
    connect_ports: Option<Vec<u16>>,
    /// Services on THIS machine the sandbox may call. Empty (default) = none.
    #[serde(default)]
    loopback_ports: Option<Vec<u16>>,
    /// Disk READ isolation: strict (default) | allowlist | open.
    #[serde(default)]
    fs_mode: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ExecParams {
    sandbox_id: String,
    /// Shell command; may span several lines. It reaches the shell on stdin,
    /// so quotes inside it survive untouched.
    command: String,
    #[serde(default)]
    timeout_ms: Option<i64>,
    /// Extra environment variables for this run.
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct RunInParams {
    sandbox_id: String,
    language: String,
    code: String,
    #[serde(default)]
    timeout_ms: Option<i64>,
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct InstallParams {
    sandbox_id: String,
    /// `pip`, `npm` or `apt`.
    manager: String,
    packages: Vec<String>,
    /// Default 300000, because installs are slow.
    #[serde(default)]
    timeout_ms: Option<i64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct UpdateParams {
    sandbox_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    network: Option<bool>,
    #[serde(default)]
    cpus: Option<f64>,
    #[serde(default)]
    memory_mb: Option<i64>,
    #[serde(default)]
    timeout_ms: Option<i64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct DeleteParams {
    sandbox_id: String,
    /// Also delete the files. Not recoverable.
    #[serde(default)]
    purge: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct FilesParams {
    sandbox_id: String,
    /// Relative to the sandbox root; empty means the root.
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct FileReadParams {
    sandbox_id: String,
    path: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct FileWriteParams {
    sandbox_id: String,
    path: String,
    content: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct StatsParams {
    sandbox_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct KillParams {
    sandbox_id: String,
    /// Omit to stop everything. Take a pid from sbx_stats.
    #[serde(default)]
    pid: Option<u32>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct MountParams {
    sandbox_id: String,
    /// Absolute path of the folder on the real machine.
    source: String,
    /// Folder name inside the sandbox. Empty means the source folder's own name.
    #[serde(default)]
    target: Option<String>,
    /// Read-only (default false).
    #[serde(default)]
    read_only: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct UnmountParams {
    sandbox_id: String,
    /// The folder name inside the sandbox, as given when mounting.
    target: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct FsModeParams {
    sandbox_id: String,
    /// `strict`, `allowlist` or `open`.
    fs_mode: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct SettingsParams {
    /// `strict`, `allowlist` or `open`.
    #[serde(default)]
    default_fs_mode: Option<String>,
    /// Absolute paths. REPLACES the whole list rather than adding to it.
    #[serde(default)]
    allowlist: Option<Vec<String>>,
    #[serde(default)]
    default_network: Option<bool>,
    #[serde(default)]
    default_memory_mb: Option<i64>,
    #[serde(default)]
    default_cpus: Option<f64>,
    #[serde(default)]
    default_timeout_ms: Option<i64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct PortsParams {
    sandbox_id: String,
    /// Ports the sandbox may bind (1024 and above). REPLACES the current list.
    #[serde(default)]
    listen: Option<Vec<u16>>,
    /// Remote ports it may connect out to, e.g. [443]. REPLACES the current list.
    #[serde(default)]
    connect: Option<Vec<u16>>,
    /// Ports of services on THIS machine it may dial (e.g. an egress proxy you
    /// run for it). Empty = no local service at all. REPLACES the current list.
    #[serde(default)]
    loopback: Option<Vec<u16>>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct TraceParams {
    sandbox_id: String,
    enabled: bool,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct EventsParams {
    sandbox_id: String,
    /// Take it from `runId` in a run result. Empty means all runs.
    #[serde(default)]
    run_id: Option<String>,
    /// `file`, `proc` or `net`. Empty means every kind.
    #[serde(default)]
    kind: Option<String>,
    /// Default 200.
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
struct RunsParams {
    /// Leave empty for all sandboxes.
    #[serde(default)]
    sandbox_id: Option<String>,
    /// Default 20.
    #[serde(default)]
    limit: Option<i64>,
}

// ── Server ───────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct McpSandboxServer {
    db: Db,
}

impl McpSandboxServer {
    /// Build from the shared sandbox DB, or `None` when the data dir cannot be
    /// opened. Unlike the other children this has no env gate — the sandbox is
    /// either usable on this machine or it isn't.
    pub fn from_env() -> Result<Option<Self>> {
        Ok(crate::sandbox::shared_db().map(|db| Self { db }))
    }

    fn sandbox(&self, id: &str) -> std::result::Result<crate::sandbox::db::Sandbox, String> {
        self.db.sandbox(id.trim()).map_err(|e| e.to_string())
    }
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpSandboxServer {
    #[rmcp::tool(
        description = "Check what kind of sandbox this machine can actually run: docker (needs a live daemon) or direct execution confined by the operating system (macOS Seatbelt / Linux bubblewrap / Windows AppContainer). Call it before creating a sandbox if unsure, and again right after the user has started Docker."
    )]
    async fn sbx_capabilities(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            CapabilitiesParams,
        >,
    ) -> String {
        let c = caps::probe(p.refresh.unwrap_or(false)).await;
        ok_json(json!({
            "os": c.os,
            "backends": c.backends,
            "recommended": c.default_backend(),
            "direct": c.direct,
            "docker": c.docker,
            "hostInterpreters": c.host_interpreters,
            "languages": code::languages(),
            "execPolicy": policy::load(&self.db),
        }))
    }

    #[rmcp::tool(
        description = "Run a snippet in a throwaway sandbox and delete it afterwards. This is the tool for almost every 'run this Python for me' request. The network is OFF by default. Languages: python, javascript, typescript, bash, sh, ruby, perl, php."
    )]
    async fn sbx_run(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            RunOnceParams,
        >,
    ) -> String {
        if p.language.trim().is_empty() || p.code.trim().is_empty() {
            return err_json("`language` and `code` are required");
        }
        match runner::run_once(
            &self.db,
            &p.language,
            &p.code,
            p.backend.filter(|b| !b.trim().is_empty()),
            p.network.unwrap_or(false),
            p.timeout_ms,
        )
        .await
        {
            Ok((run, sb)) => ok_json(json!({ "backend": sb.backend, "run": run_summary(&run) })),
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(
        description = "Create a long-lived sandbox for several commands in a row (files and installed packages persist between runs). Use it for multi-step work; for a single snippet use sbx_run instead."
    )]
    async fn sbx_create(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            CreateParams,
        >,
    ) -> String {
        let ports = match ports::validate(
            &p.listen_ports.unwrap_or_default(),
            &p.connect_ports.unwrap_or_default(),
            &p.loopback_ports.unwrap_or_default(),
        ) {
            Ok(v) => v,
            Err(e) => return err_json(e),
        };
        match runner::create(
            &self.db,
            runner::CreateReq {
                name: p.name.filter(|s| !s.trim().is_empty()),
                backend: p.backend.filter(|s| !s.trim().is_empty()),
                image: p.image.filter(|s| !s.trim().is_empty()),
                network: p.network.unwrap_or(false),
                cpus: p.cpus,
                memory_mb: p.memory_mb,
                timeout_ms: p.timeout_ms,
                env: json!({}),
                mounts: Vec::new(),
                fs_mode: p.fs_mode.as_deref().and_then(fsmode::FsMode::parse),
                ports,
            },
        )
        .await
        {
            Ok(sb) => ok_json(json!({
                "sandboxId": sb.id,
                "name": sb.name,
                "backend": sb.backend,
                "image": sb.image,
                "network": sb.network,
                "note": "The container/process only starts on the first run.",
            })),
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(
        description = "List existing sandboxes with their status, backend and resource limits."
    )]
    fn sbx_list(&self) -> String {
        match self.db.list_sandboxes() {
            Ok(v) => ok_json(json!({ "sandboxes": v })),
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(
        description = "Run a shell command in an existing sandbox. The command reaches the shell on stdin, so quotes inside it survive untouched."
    )]
    async fn sbx_exec(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            ExecParams,
        >,
    ) -> String {
        if p.command.trim().is_empty() {
            return err_json("`command` is required");
        }
        let sb = match self.sandbox(&p.sandbox_id) {
            Ok(sb) => sb,
            Err(e) => return err_json(e),
        };
        match runner::exec(
            &self.db,
            &sb,
            &p.command,
            p.timeout_ms,
            env_map(p.env),
            "exec",
            None,
            &p.command,
            runner::shell_argv(&sb),
        )
        .await
        {
            Ok(run) => ok_json(run_summary(&run)),
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(
        description = "Run a snippet inside an existing sandbox, keeping its state. Languages: python, javascript, typescript, bash, sh, ruby, perl, php."
    )]
    async fn sbx_run_in(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            RunInParams,
        >,
    ) -> String {
        let sb = match self.sandbox(&p.sandbox_id) {
            Ok(sb) => sb,
            Err(e) => return err_json(e),
        };
        match runner::run_code(
            &self.db,
            &sb,
            &p.language,
            &p.code,
            p.timeout_ms,
            env_map(p.env),
        )
        .await
        {
            Ok(run) => ok_json(run_summary(&run)),
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(
        description = "Install packages into a sandbox with pip, npm or apt. The sandbox must have the network on — use sbx_update to turn it on first."
    )]
    async fn sbx_install(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            InstallParams,
        >,
    ) -> String {
        let sb = match self.sandbox(&p.sandbox_id) {
            Ok(sb) => sb,
            Err(e) => return err_json(e),
        };
        match runner::install(&self.db, &sb, &p.manager, &p.packages, p.timeout_ms).await {
            Ok(run) => ok_json(run_summary(&run)),
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(
        description = "Change a sandbox: network on/off, CPU/RAM limits, run deadline. On the docker backend, changing the network or resources recreates the container (files are kept)."
    )]
    async fn sbx_update(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            UpdateParams,
        >,
    ) -> String {
        let before = match self.sandbox(&p.sandbox_id) {
            Ok(sb) => sb,
            Err(e) => return err_json(e),
        };
        let sb = match self.db.update_limits(
            &before.id,
            p.name.as_deref(),
            p.network,
            p.cpus,
            p.memory_mb,
            p.timeout_ms,
            None,
        ) {
            Ok(sb) => sb,
            Err(e) => return err_json(e),
        };
        // Docker limits are baked into `docker run`, so a live container is
        // recreated rather than left running under limits nobody sees.
        let mut restarted = false;
        if sb.backend == "docker"
            && before.status == "running"
            && (before.network != sb.network
                || before.cpus != sb.cpus
                || before.memory_mb != sb.memory_mb)
        {
            let _ = runner::stop(&self.db, &sb).await;
            restarted = runner::ensure_started(&self.db, &sb).await.is_ok();
        }
        ok_json(json!({ "sandbox": sb, "containerRecreated": restarted }))
    }

    #[rmcp::tool(
        description = "Delete a sandbox. Files are KEPT by default; pass purge=true to delete them as well."
    )]
    async fn sbx_delete(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            DeleteParams,
        >,
    ) -> String {
        let sb = match self.sandbox(&p.sandbox_id) {
            Ok(sb) => sb,
            Err(e) => return err_json(e),
        };
        let purge = p.purge.unwrap_or(false);
        match runner::delete(&self.db, &sb, purge).await {
            Ok(()) => ok_json(json!({
                "ok": true,
                "deleted": sb.name,
                "files": if purge { "purged" } else { "still on disk" },
            })),
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(description = "List files in the sandbox by relative path.")]
    fn sbx_files(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            FilesParams,
        >,
    ) -> String {
        let sb = match self.sandbox(&p.sandbox_id) {
            Ok(sb) => sb,
            Err(e) => return err_json(e),
        };
        match files::list(&files::Scope::of(&sb), p.path.as_deref().unwrap_or("")) {
            Ok(entries) => ok_json(json!({ "entries": entries })),
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(description = "Read a text file from the sandbox.")]
    fn sbx_file_read(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            FileReadParams,
        >,
    ) -> String {
        let sb = match self.sandbox(&p.sandbox_id) {
            Ok(sb) => sb,
            Err(e) => return err_json(e),
        };
        match files::read(&files::Scope::of(&sb), &p.path) {
            Ok(c) => c,
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(
        description = "Write a text file into the sandbox (parent folders are created). Use it to hand data to the code."
    )]
    fn sbx_file_write(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            FileWriteParams,
        >,
    ) -> String {
        let sb = match self.sandbox(&p.sandbox_id) {
            Ok(sb) => sb,
            Err(e) => return err_json(e),
        };
        match files::write(&files::Scope::of(&sb), &p.path, &p.content) {
            Ok(n) => ok_json(json!({ "ok": true, "path": p.path, "bytes": n })),
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(
        description = "How much CPU and RAM the sandbox is using, with the processes running inside it (pid, %CPU, RAM, elapsed, command). Use it when the user asks whether something is still running, why the machine feels slow, or before deciding what to stop."
    )]
    async fn sbx_stats(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            StatsParams,
        >,
    ) -> String {
        let sb = match self.sandbox(&p.sandbox_id) {
            Ok(sb) => sb,
            Err(e) => return err_json(e),
        };
        ok_json(serde_json::to_value(monitor::stats(&sb).await).unwrap_or_default())
    }

    #[rmcp::tool(
        description = "Stop processes in a sandbox. Omit `pid` to stop EVERYTHING it is running. Only processes belonging to that sandbox can be stopped — nothing else on the machine."
    )]
    async fn sbx_kill(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            KillParams,
        >,
    ) -> String {
        let sb = match self.sandbox(&p.sandbox_id) {
            Ok(sb) => sb,
            Err(e) => return err_json(e),
        };
        match p.pid {
            Some(pid) => match monitor::kill_pid(&sb, pid).await {
                Ok(()) => ok_json(json!({ "ok": true, "killed": pid })),
                Err(e) => err_json(e),
            },
            None => match monitor::kill_all(&sb).await {
                Ok(n) => ok_json(json!({ "ok": true, "groups": n })),
                Err(e) => err_json(e),
            },
        }
    }

    #[rmcp::tool(
        description = "Mount a real folder from this machine into a sandbox so the code can read and write actual data. It is READ-WRITE by default — pass readOnly=true when reading is enough, and prefer readOnly whenever the code is not yet trusted. The home directory, system directories and credential folders cannot be mounted."
    )]
    async fn sbx_mount(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            MountParams,
        >,
    ) -> String {
        let sb = match self.sandbox(&p.sandbox_id) {
            Ok(sb) => sb,
            Err(e) => return err_json(e),
        };
        let m = match mounts::validate(
            &p.source,
            p.target.as_deref().unwrap_or(""),
            p.read_only.unwrap_or(false),
        ) {
            Ok(m) => m,
            Err(e) => return err_json(e),
        };
        let next = match mounts::add(&sb.mounts, m.clone()) {
            Ok(v) => v,
            Err(e) => return err_json(e),
        };
        match self.db.set_mounts(&sb.id, &next) {
            Ok(sb) => {
                // A live container has its mounts fixed at `docker run`.
                let mut note = String::new();
                if sb.backend == "docker" && sb.status == "running" {
                    let _ = runner::stop(&self.db, &sb).await;
                    note = match runner::ensure_started(&self.db, &sb).await {
                        Ok(_) => {
                            " The container was recreated so the new folder is visible.".into()
                        }
                        Err(e) => format!(" NOTE: recreating the container failed: {e}"),
                    };
                }
                ok_json(json!({
                    "ok": true,
                    "mounted": m.source,
                    "at": m.target,
                    "readOnly": m.read_only,
                    "note": note,
                }))
            }
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(
        description = "Unmount a folder from a sandbox. This only removes the link; it does NOT delete anything in the real folder."
    )]
    async fn sbx_unmount(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            UnmountParams,
        >,
    ) -> String {
        let sb = match self.sandbox(&p.sandbox_id) {
            Ok(sb) => sb,
            Err(e) => return err_json(e),
        };
        let next = mounts::remove(&sb.mounts, &p.target);
        if next.len() == sb.mounts.len() {
            return err_json(format!(
                "the sandbox has no folder mounted at `{}`",
                p.target
            ));
        }
        match self.db.set_mounts(&sb.id, &next) {
            Ok(sb) => {
                // Remove the symlink so the file browser stops showing a broken
                // entry. This never touches the real folder.
                let link = std::path::Path::new(&sb.workdir).join(&p.target);
                if std::fs::symlink_metadata(&link)
                    .map(|m| m.is_symlink())
                    .unwrap_or(false)
                {
                    let _ = std::fs::remove_file(&link);
                }
                if sb.backend == "docker" && sb.status == "running" {
                    let _ = runner::stop(&self.db, &sb).await;
                    let _ = runner::ensure_started(&self.db, &sb).await;
                }
                ok_json(json!({ "ok": true, "unmounted": p.target, "realFolder": "untouched" }))
            }
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(
        description = "Change a sandbox's disk READ isolation: `strict` (only the sandbox and its mounts), `allowlist` (plus the folders declared in settings), `open` (the whole disk). Takes effect on the next run. Not applicable to the docker backend — a container is already fully isolated."
    )]
    fn sbx_fs_mode(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            FsModeParams,
        >,
    ) -> String {
        let Some(mode) = fsmode::FsMode::parse(&p.fs_mode) else {
            return err_json(format!(
                "invalid mode `{}` (strict, allowlist, open)",
                p.fs_mode
            ));
        };
        let sb = match self.sandbox(&p.sandbox_id) {
            Ok(sb) => sb,
            Err(e) => return err_json(e),
        };
        if sb.backend == "docker" {
            return ok_json(json!({
                "ok": false,
                "note": "This sandbox uses docker — a container already isolates the whole disk, so there is no read mode to change.",
            }));
        }
        match self.db.set_fs_mode(&sb.id, mode) {
            Ok(sb) => ok_json(
                json!({ "ok": true, "sandbox": sb.name, "fsMode": mode.as_str(), "label": mode.label() }),
            ),
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(
        description = "Read or change the sandbox defaults: the read-isolation new sandboxes start with, the folders readable in `allowlist` mode, and the default network/CPU/RAM/deadline. Call it with no arguments just to read them."
    )]
    fn sbx_settings(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            SettingsParams,
        >,
    ) -> String {
        let cur = settings::load(&self.db);
        let touched = p.default_fs_mode.is_some()
            || p.allowlist.is_some()
            || p.default_network.is_some()
            || p.default_memory_mb.is_some()
            || p.default_cpus.is_some()
            || p.default_timeout_ms.is_some();
        if !touched {
            return ok_json(serde_json::to_value(&cur).unwrap_or_default());
        }
        let next = settings::Settings {
            default_fs_mode: p
                .default_fs_mode
                .as_deref()
                .and_then(fsmode::FsMode::parse)
                .unwrap_or(cur.default_fs_mode),
            allowlist: p.allowlist.unwrap_or_else(|| cur.allowlist.clone()),
            default_network: p.default_network.unwrap_or(cur.default_network),
            default_memory_mb: p.default_memory_mb.unwrap_or(cur.default_memory_mb),
            default_cpus: p.default_cpus.unwrap_or(cur.default_cpus),
            default_timeout_ms: p.default_timeout_ms.unwrap_or(cur.default_timeout_ms),
        };
        match settings::save(&self.db, &next) {
            Ok(saved) => ok_json(serde_json::to_value(saved).unwrap_or_default()),
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(
        description = "Open specific ports for a sandbox while the rest of the network stays closed. `listen` = ports the sandbox may serve on, reachable from this machine at 127.0.0.1:<port> — this is how you run an app inside a sandbox. `connect` = the only remote ports it may dial out to, so `connect:[443]` means HTTPS and nothing else — note it is per PORT, not per host: it cannot mean 'only this website'. `loopback` = services on THIS machine the sandbox may call; empty (the default) means none, which is what stops sandboxed code from reaching SenClaw's own unauthenticated API. Sending empty lists closes everything again. On macOS all three are enforced exactly; on docker and Linux opening a port grants the sandbox a network, and the reply says so."
    )]
    async fn sbx_ports(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            PortsParams,
        >,
    ) -> String {
        let before = match self.sandbox(&p.sandbox_id) {
            Ok(sb) => sb,
            Err(e) => return err_json(e),
        };
        let policy = match ports::validate(
            &p.listen.unwrap_or_default(),
            &p.connect.unwrap_or_default(),
            &p.loopback.unwrap_or_default(),
        ) {
            Ok(p) => p,
            Err(e) => return err_json(e),
        };
        match self.db.set_ports(&before.id, &policy) {
            Ok(sb) => {
                if sb.backend == "docker" && before.status == "running" {
                    let _ = runner::stop(&self.db, &sb).await;
                    let _ = runner::ensure_started(&self.db, &sb).await;
                }
                let isolation = caps::direct_caps(false).await.kind.as_str().to_string();
                ok_json(json!({
                    "listen": sb.ports.listen,
                    "connect": sb.ports.connect,
                    "loopback": sb.ports.loopback,
                    "reachableAt": sb.ports.listen.iter().map(|p| format!("127.0.0.1:{p}")).collect::<Vec<_>>(),
                    "note": ports::note_for(&sb.backend, &isolation, &sb.ports),
                }))
            }
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(
        description = "Turn activity tracing on or off, for testing: it records file reads and writes, process launches, and which addresses were contacted. OFF by default. Turn it on, run the code again, then read the result with sbx_events. NOTE: this is an observation tool for testing, NOT security evidence — the hook runs inside the sandbox, so code that deliberately hides can evade it. What actually stops hostile code is the sandbox itself (read/write/network isolation), not this."
    )]
    fn sbx_trace(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            TraceParams,
        >,
    ) -> String {
        match self.db.set_trace(p.sandbox_id.trim(), p.enabled) {
            Ok(sb) => ok_json(json!({
                "ok": true,
                "sandbox": sb.name,
                "tracing": if p.enabled { "ON" } else { "OFF" },
                "note": if p.enabled {
                    "Run the code again, then read sbx_events. This is an observation tool for testing, not security evidence."
                } else { "" },
            })),
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(
        description = "Read the traced events: which files were read or written, which processes were launched, which addresses were contacted (including hostnames looked up). Filter with `kind` = file | proc | net, or with `runId` for a single run."
    )]
    fn sbx_events(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            EventsParams,
        >,
    ) -> String {
        let sb = match self.sandbox(&p.sandbox_id) {
            Ok(sb) => sb,
            Err(e) => return err_json(e),
        };
        match self.db.list_events(
            &sb.id,
            p.run_id.as_deref().filter(|s| !s.is_empty()),
            p.kind.as_deref().filter(|s| !s.is_empty()),
            p.limit.unwrap_or(200),
        ) {
            Ok(events) if events.is_empty() => ok_json(json!({
                "traceEnabled": sb.trace_enabled,
                "events": [],
                "note": if sb.trace_enabled {
                    "No events yet. Tracing is on for this sandbox — run some code and look again.".to_string()
                } else {
                    format!("Tracing is OFF for sandbox `{}`, so nothing was recorded. Turn it on with sbx_trace and run the code again.", sb.name)
                },
            })),
            Ok(events) => ok_json(json!({ "traceEnabled": sb.trace_enabled, "events": events })),
            Err(e) => err_json(e),
        }
    }

    #[rmcp::tool(
        description = "Run history: command, exit code, duration, and the isolation actually applied."
    )]
    fn sbx_runs(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            RunsParams,
        >,
    ) -> String {
        match self.db.list_runs(
            p.sandbox_id.as_deref().filter(|s| !s.is_empty()),
            p.limit.unwrap_or(20),
        ) {
            Ok(runs) => ok_json(json!({ "runs": runs })),
            Err(e) => err_json(e),
        }
    }
}

pub async fn run_stdio_server() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let server = McpSandboxServer::from_env()?
        .ok_or_else(|| anyhow::anyhow!("sandbox engine unavailable (cannot open data dir/db)"))?;
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
