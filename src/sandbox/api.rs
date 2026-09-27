//! REST surface. The web UI and the MCP server both go through `runner`, so
//! anything enforced for one is enforced for the other.

use std::collections::BTreeMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::sandbox::state::AppState;
use crate::sandbox::{caps, code, config, files, monitor, mounts, policy, pty, runner, settings};

pub struct ApiErr(pub StatusCode, pub String);

impl IntoResponse for ApiErr {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<String> for ApiErr {
    fn from(e: String) -> Self {
        ApiErr(StatusCode::BAD_REQUEST, e)
    }
}

impl From<anyhow::Error> for ApiErr {
    fn from(e: anyhow::Error) -> Self {
        ApiErr(StatusCode::BAD_REQUEST, e.to_string())
    }
}

type ApiResult = Result<Json<Value>, ApiErr>;

pub fn api_router(state: AppState) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/health", get(status))
        .route("/caps", get(get_caps))
        .route("/languages", get(languages))
        .route("/sandboxes", get(list_sandboxes).post(create_sandbox))
        .route(
            "/sandboxes/:id",
            get(get_sandbox).patch(patch_sandbox).delete(delete_sandbox),
        )
        .route("/sandboxes/:id/start", post(start_sandbox))
        .route("/sandboxes/:id/stop", post(stop_sandbox))
        .route("/sandboxes/:id/exec", post(exec_sandbox))
        .route("/sandboxes/:id/run", post(run_code))
        .route("/sandboxes/:id/install", post(install))
        .route("/sandboxes/:id/files", get(list_files))
        .route(
            "/sandboxes/:id/file",
            get(read_file).put(write_file).delete(delete_file),
        )
        .route("/sandboxes/:id/mkdir", post(mkdir))
        .route("/sandboxes/:id/stats", get(stats))
        .route("/sandboxes/:id/kill", post(kill))
        .route("/sandboxes/:id/mounts", get(list_mounts).post(add_mount))
        .route("/sandboxes/:id/mounts/remove", post(remove_mount))
        .route("/sandboxes/:id/fs-mode", post(set_fs_mode))
        .route("/sandboxes/:id/trace", post(set_trace))
        .route("/sandboxes/:id/ports", post(set_ports))
        .route("/sandboxes/:id/events", get(list_events).delete(clear_events))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/exec-policy", get(get_exec_policy).put(put_exec_policy))
        .route("/fs-modes", get(fs_modes))
        .route("/sandboxes/:id/terminal", get(pty::terminal_ws))
        .route("/runs", get(list_runs))
        .route("/runs/:id", get(get_run))
        .route("/run-once", post(run_once))
        .with_state(state)
}

// ── status / capabilities ───────────────────────────────────────────────────

async fn status(State(s): State<AppState>) -> ApiResult {
    let c = caps::probe(false).await;
    let count = s.db.list_sandboxes().map(|v| v.len()).unwrap_or(0);
    Ok(Json(json!({
        "ok": true,
        "app": "sandbox",
        "sandboxes": count,
        "caps": c,
        "defaultImage": config::default_image(),
        "execPolicy": policy::load(&s.db),
    })))
}

#[derive(Deserialize)]
struct RefreshQ {
    #[serde(default)]
    refresh: bool,
}

async fn get_caps(Query(q): Query<RefreshQ>) -> ApiResult {
    Ok(Json(serde_json::to_value(caps::probe(q.refresh).await).unwrap()))
}

async fn languages() -> ApiResult {
    Ok(Json(json!({ "languages": code::languages() })))
}

// ── sandboxes ───────────────────────────────────────────────────────────────

async fn list_sandboxes(State(s): State<AppState>) -> ApiResult {
    Ok(Json(json!({ "sandboxes": s.db.list_sandboxes()? })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateBody {
    name: Option<String>,
    backend: Option<String>,
    image: Option<String>,
    #[serde(default)]
    network: bool,
    cpus: Option<f64>,
    memory_mb: Option<i64>,
    timeout_ms: Option<i64>,
    #[serde(default)]
    env: Value,
    #[serde(default)]
    mounts: Vec<MountBody>,
    fs_mode: Option<String>,
    #[serde(default)]
    listen_ports: Vec<u16>,
    #[serde(default)]
    connect_ports: Vec<u16>,
    #[serde(default)]
    loopback_ports: Vec<u16>,
}

async fn create_sandbox(State(s): State<AppState>, Json(b): Json<CreateBody>) -> ApiResult {
    let sb = runner::create(
        &s.db,
        runner::CreateReq {
            name: b.name,
            backend: b.backend,
            image: b.image,
            network: b.network,
            cpus: b.cpus,
            memory_mb: b.memory_mb,
            timeout_ms: b.timeout_ms,
            env: if b.env.is_object() { b.env } else { json!({}) },
            mounts: b
                .mounts
                .iter()
                .map(|m| mounts::validate(&m.source, &m.target, m.read_only))
                .collect::<Result<Vec<_>, _>>()?,
            fs_mode: b.fs_mode.as_deref().and_then(crate::sandbox::fsmode::FsMode::parse),
            ports: crate::sandbox::ports::validate(&b.listen_ports, &b.connect_ports, &b.loopback_ports)?,
        },
    )
    .await?;
    Ok(Json(serde_json::to_value(sb).unwrap()))
}

async fn get_sandbox(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let sb = s.db.sandbox(&id)?;
    // Reconcile against reality. A container can be stopped from outside this
    // app (`docker stop`, a Docker Desktop restart, a reboot), and a row that
    // still says "running" sends the user looking for a container that is gone.
    // Only on the single-sandbox read: doing it in the list would mean one
    // docker call per row.
    if sb.backend == "docker" && sb.status == "running" && !crate::sandbox::backend::docker::is_running(&sb).await
    {
        s.db.set_status(&id, "stopped", None, None)?;
        s.db.clear_container(&id)?;
        return Ok(Json(serde_json::to_value(s.db.sandbox(&id)?).unwrap()));
    }
    Ok(Json(serde_json::to_value(sb).unwrap()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PatchBody {
    name: Option<String>,
    network: Option<bool>,
    cpus: Option<f64>,
    memory_mb: Option<i64>,
    timeout_ms: Option<i64>,
    env: Option<Value>,
}

async fn patch_sandbox(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<PatchBody>,
) -> ApiResult {
    let before = s.db.sandbox(&id)?;
    let sb = s.db.update_limits(
        &id,
        b.name.as_deref(),
        b.network,
        b.cpus,
        b.memory_mb,
        b.timeout_ms,
        b.env.as_ref(),
    )?;
    // Container flags are fixed at `docker run`. Changing them in the DB while
    // a container is up would show limits that are not the ones in force, so
    // the container is recreated instead of quietly diverging.
    let needs_restart = sb.backend == "docker"
        && before.status == "running"
        && (before.network != sb.network
            || before.cpus != sb.cpus
            || before.memory_mb != sb.memory_mb);
    if needs_restart {
        let _ = runner::stop(&s.db, &sb).await;
        runner::ensure_started(&s.db, &sb).await?;
    }
    Ok(Json(json!({
        "sandbox": s.db.sandbox(&id)?,
        "restarted": needs_restart,
    })))
}

#[derive(Deserialize)]
struct DeleteQ {
    #[serde(default)]
    purge: bool,
}

async fn delete_sandbox(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DeleteQ>,
) -> ApiResult {
    let sb = s.db.sandbox(&id)?;
    runner::delete(&s.db, &sb, q.purge).await?;
    Ok(Json(json!({ "ok": true, "purged": q.purge })))
}

async fn start_sandbox(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let sb = s.db.sandbox(&id)?;
    let sb = runner::ensure_started(&s.db, &sb).await?;
    Ok(Json(serde_json::to_value(sb).unwrap()))
}

async fn stop_sandbox(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let sb = s.db.sandbox(&id)?;
    runner::stop(&s.db, &sb).await?;
    Ok(Json(json!({ "ok": true })))
}

// ── running ─────────────────────────────────────────────────────────────────

fn env_map(v: &Value) -> BTreeMap<String, String> {
    v.as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExecBody {
    command: String,
    timeout_ms: Option<i64>,
    #[serde(default)]
    env: Value,
}

async fn exec_sandbox(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<ExecBody>,
) -> ApiResult {
    let sb = s.db.sandbox(&id)?;
    let run = runner::exec(
        &s.db,
        &sb,
        &b.command,
        b.timeout_ms,
        env_map(&b.env),
        "exec",
        None,
        &b.command,
        runner::shell_argv(&sb),
)
    .await?;
    Ok(Json(serde_json::to_value(run).unwrap()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunBody {
    language: String,
    code: String,
    timeout_ms: Option<i64>,
    #[serde(default)]
    env: Value,
}

async fn run_code(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<RunBody>,
) -> ApiResult {
    let sb = s.db.sandbox(&id)?;
    let run = runner::run_code(&s.db, &sb, &b.language, &b.code, b.timeout_ms, env_map(&b.env)).await?;
    Ok(Json(serde_json::to_value(run).unwrap()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunOnceBody {
    language: String,
    code: String,
    backend: Option<String>,
    #[serde(default)]
    network: bool,
    timeout_ms: Option<i64>,
}

async fn run_once(State(s): State<AppState>, Json(b): Json<RunOnceBody>) -> ApiResult {
    let (run, sb) =
        runner::run_once(&s.db, &b.language, &b.code, b.backend, b.network, b.timeout_ms).await?;
    Ok(Json(json!({ "run": run, "backend": sb.backend })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallBody {
    manager: String,
    packages: Vec<String>,
    timeout_ms: Option<i64>,
}

async fn install(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<InstallBody>,
) -> ApiResult {
    let sb = s.db.sandbox(&id)?;
    let run = runner::install(&s.db, &sb, &b.manager, &b.packages, b.timeout_ms).await?;
    Ok(Json(serde_json::to_value(run).unwrap()))
}

// ── runs ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunsQ {
    sandbox_id: Option<String>,
    limit: Option<i64>,
}

async fn list_runs(State(s): State<AppState>, Query(q): Query<RunsQ>) -> ApiResult {
    let runs = s.db.list_runs(q.sandbox_id.as_deref(), q.limit.unwrap_or(50))?;
    Ok(Json(json!({ "runs": runs })))
}

async fn get_run(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(serde_json::to_value(s.db.run(&id)?).unwrap()))
}

// ── settings & filesystem isolation ─────────────────────────────────────────

async fn get_settings(State(s): State<AppState>) -> ApiResult {
    Ok(Json(serde_json::to_value(settings::load(&s.db)).unwrap()))
}

async fn put_settings(
    State(s): State<AppState>,
    Json(body): Json<settings::Settings>,
) -> ApiResult {
    Ok(Json(serde_json::to_value(settings::save(&s.db, &body)?).unwrap()))
}

async fn get_exec_policy(State(s): State<AppState>) -> ApiResult {
    Ok(Json(serde_json::to_value(policy::load(&s.db)).unwrap()))
}

async fn put_exec_policy(
    State(s): State<AppState>,
    Json(body): Json<policy::ExecPolicy>,
) -> ApiResult {
    Ok(Json(serde_json::to_value(policy::save(&s.db, &body)?).unwrap()))
}

/// The three modes plus their labels, so the UI never hardcodes the copy.
async fn fs_modes() -> ApiResult {
    use crate::sandbox::fsmode::FsMode;
    let list: Vec<Value> = [FsMode::Strict, FsMode::Allowlist, FsMode::Open]
        .iter()
        .map(|m| json!({ "value": m.as_str(), "label": m.label(), "jailsReads": m.jails_reads() }))
        .collect();
    Ok(Json(json!({ "modes": list })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FsModeBody {
    fs_mode: String,
}

async fn set_fs_mode(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<FsModeBody>,
) -> ApiResult {
    let mode = crate::sandbox::fsmode::FsMode::parse(&b.fs_mode)
        .ok_or_else(|| ApiErr(StatusCode::BAD_REQUEST, format!("invalid mode `{}`", b.fs_mode)))?;
    let sb = s.db.set_fs_mode(&id, mode)?;
    Ok(Json(serde_json::to_value(sb).unwrap()))
}

// ── ports ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PortsBody {
    #[serde(default)]
    listen: Vec<u16>,
    #[serde(default)]
    connect: Vec<u16>,
    /// Local services on this machine the sandbox may dial. Empty (the
    /// default) means none — see `ports::seatbelt_rules`.
    #[serde(default)]
    loopback: Vec<u16>,
}

async fn set_ports(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<PortsBody>,
) -> ApiResult {
    let before = s.db.sandbox(&id)?;
    let policy = crate::sandbox::ports::validate(&b.listen, &b.connect, &b.loopback)?;
    let sb = s.db.set_ports(&id, &policy)?;
    // Published ports are fixed at `docker run`, so a live container has to be
    // recreated for a new one to exist.
    let recreated = if sb.backend == "docker" && before.status == "running" {
        let _ = runner::stop(&s.db, &sb).await;
        runner::ensure_started(&s.db, &sb).await.is_ok()
    } else {
        false
    };
    let isolation = caps::direct_caps(false).await.kind.as_str().to_string();
    Ok(Json(json!({
        "sandbox": s.db.sandbox(&id)?,
        "containerRecreated": recreated,
        "note": crate::sandbox::ports::note_for(&sb.backend, &isolation, &sb.ports),
    })))
}

// ── activity tracing ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TraceBody {
    enabled: bool,
}

async fn set_trace(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<TraceBody>,
) -> ApiResult {
    Ok(Json(serde_json::to_value(s.db.set_trace(&id, b.enabled)?).unwrap()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventsQ {
    run_id: Option<String>,
    /// `file`, `proc`, `net` — matched as a prefix of the event kind.
    kind: Option<String>,
    limit: Option<i64>,
}

async fn list_events(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<EventsQ>,
) -> ApiResult {
    let events = s.db.list_events(
        &id,
        q.run_id.as_deref(),
        q.kind.as_deref().filter(|k| !k.is_empty()),
        q.limit.unwrap_or(500),
    )?;
    Ok(Json(json!({ "events": events })))
}

async fn clear_events(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    s.db.clear_events(&id)?;
    Ok(Json(json!({ "ok": true })))
}

// ── monitor ─────────────────────────────────────────────────────────────────

async fn stats(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let sb = s.db.sandbox(&id)?;
    Ok(Json(serde_json::to_value(monitor::stats(&sb).await).unwrap()))
}

#[derive(Deserialize)]
struct KillBody {
    /// Omitted = stop everything this sandbox is running.
    pid: Option<u32>,
}

async fn kill(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<KillBody>,
) -> ApiResult {
    let sb = s.db.sandbox(&id)?;
    match b.pid {
        Some(pid) => {
            monitor::kill_pid(&sb, pid).await?;
            Ok(Json(json!({ "ok": true, "killed": pid })))
        }
        None => {
            let n = monitor::kill_all(&sb).await?;
            Ok(Json(json!({ "ok": true, "groups": n })))
        }
    }
}

// ── mounts ──────────────────────────────────────────────────────────────────

async fn list_mounts(State(s): State<AppState>, Path(id): Path<String>) -> ApiResult {
    Ok(Json(json!({ "mounts": s.db.sandbox(&id)?.mounts })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MountBody {
    source: String,
    #[serde(default)]
    target: String,
    #[serde(default)]
    read_only: bool,
}

async fn add_mount(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<MountBody>,
) -> ApiResult {
    let sb = s.db.sandbox(&id)?;
    let m = mounts::validate(&b.source, &b.target, b.read_only)?;
    let next = mounts::add(&sb.mounts, m)?;
    let sb = s.db.set_mounts(&id, &next)?;
    // A running container has its mounts fixed at `docker run`; it has to be
    // recreated for the new one to exist. The direct backend re-reads mounts on
    // every run, so there is nothing to do there.
    let recreated = if sb.backend == "docker" && sb.status == "running" {
        let _ = runner::stop(&s.db, &sb).await;
        runner::ensure_started(&s.db, &sb).await.is_ok()
    } else {
        false
    };
    Ok(Json(json!({ "sandbox": s.db.sandbox(&id)?, "containerRecreated": recreated })))
}

#[derive(Deserialize)]
struct UnmountBody {
    target: String,
}

async fn remove_mount(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<UnmountBody>,
) -> ApiResult {
    let sb = s.db.sandbox(&id)?;
    let next = mounts::remove(&sb.mounts, &b.target);
    let sb = s.db.set_mounts(&id, &next)?;
    // On macOS the mount is a symlink in the workdir; leaving it behind would
    // be a dangling link the file browser then reports as broken. Removing the
    // link never touches what it pointed at.
    let link = std::path::Path::new(&sb.workdir).join(&b.target);
    if std::fs::symlink_metadata(&link).map(|m| m.is_symlink()).unwrap_or(false) {
        let _ = std::fs::remove_file(&link);
    }
    let recreated = if sb.backend == "docker" && sb.status == "running" {
        let _ = runner::stop(&s.db, &sb).await;
        runner::ensure_started(&s.db, &sb).await.is_ok()
    } else {
        false
    };
    Ok(Json(json!({ "sandbox": s.db.sandbox(&id)?, "containerRecreated": recreated })))
}

// ── files ───────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PathQ {
    #[serde(default)]
    path: String,
}

fn scope_of(s: &AppState, id: &str) -> Result<files::Scope, ApiErr> {
    Ok(files::Scope::of(&s.db.sandbox(id)?))
}

async fn list_files(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PathQ>,
) -> ApiResult {
    let root = scope_of(&s, &id)?;
    Ok(Json(json!({ "path": q.path, "entries": files::list(&root, &q.path)? })))
}

async fn read_file(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PathQ>,
) -> ApiResult {
    let root = scope_of(&s, &id)?;
    Ok(Json(json!({ "path": q.path, "content": files::read(&root, &q.path)? })))
}

#[derive(Deserialize)]
struct WriteBody {
    path: String,
    content: String,
}

async fn write_file(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<WriteBody>,
) -> ApiResult {
    let root = scope_of(&s, &id)?;
    let n = files::write(&root, &b.path, &b.content)?;
    Ok(Json(json!({ "ok": true, "path": b.path, "bytes": n })))
}

async fn delete_file(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<PathQ>,
) -> ApiResult {
    let root = scope_of(&s, &id)?;
    files::delete(&root, &q.path)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct MkdirBody {
    path: String,
}

async fn mkdir(
    State(s): State<AppState>,
    Path(id): Path<String>,
    Json(b): Json<MkdirBody>,
) -> ApiResult {
    let root = scope_of(&s, &id)?;
    files::mkdir(&root, &b.path)?;
    Ok(Json(json!({ "ok": true })))
}
