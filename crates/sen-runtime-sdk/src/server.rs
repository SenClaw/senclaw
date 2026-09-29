//! The server scaffold every runtime binary mounts its routes into.
//!
//! [`serve`] owns what must be identical across runtimes and is easy to get
//! subtly wrong once per copy:
//!
//! - **Loopback only.** A runtime authenticates with a per-launch token, but a
//!   wildcard bind would still hand the port to the LAN. A non-loopback host is
//!   refused unless `SENCLAW_RUNTIME_ALLOW_REMOTE=1`, which the daemon never sets.
//! - **Bearer auth on everything but `/health`**, constant-time, when the daemon
//!   supplied `SENCLAW_RUNTIME_TOKEN`. Loopback is a boundary around the
//!   machine, not around a process: without it any local program (another
//!   runtime, a Space App) could drive the engine.
//! - **`/health` and `/runtime/info`**, with a [`Readiness`] handle so a
//!   model-mode runtime answers 503 while its weights load.
//! - **A parent watchdog.** When the daemon dies — crash, `kill -9`, a desktop
//!   app force-quit — the runtime exits within ~2 s instead of living on as an
//!   orphan that holds gigabytes of weights and a port.
//! - **Graceful shutdown** on SIGINT/SIGTERM (Ctrl-Break on Windows) and on
//!   `POST /runtime/shutdown`.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::sync::Notify;

use crate::api::{codes, ErrorBody, Health, RuntimeInfo};
use crate::env::LaunchEnv;
use crate::manifest::{Capability, RunMode};

/// Whether the runtime can take requests yet.
#[derive(Clone, Debug)]
pub struct Readiness(Arc<AtomicU8>);

const LOADING: u8 = 0;
const READY: u8 = 1;
const FAILED: u8 = 2;

impl Readiness {
    /// Starts ready — the right default for a service runtime, which answers
    /// `/health` before touching any weights.
    pub fn ready() -> Readiness {
        Readiness(Arc::new(AtomicU8::new(READY)))
    }

    /// Starts loading — for a model-mode runtime that loads its one model at
    /// startup and must not look healthy before it can generate.
    pub fn loading() -> Readiness {
        Readiness(Arc::new(AtomicU8::new(LOADING)))
    }

    pub fn set_ready(&self) {
        self.0.store(READY, Ordering::SeqCst);
    }

    /// The load failed for good: `/health` answers 500 so the daemon stops
    /// waiting at once instead of timing out.
    pub fn set_failed(&self) {
        self.0.store(FAILED, Ordering::SeqCst);
    }

    pub fn is_ready(&self) -> bool {
        self.0.load(Ordering::SeqCst) == READY
    }

    fn state(&self) -> u8 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Everything [`serve`] needs besides the runtime's own routes.
pub struct ServeOptions {
    pub env: LaunchEnv,
    pub mode: RunMode,
    pub capabilities: Vec<Capability>,
    pub readiness: Readiness,
    /// Extra `/runtime/info` detail, computed per request.
    pub info_detail: Option<Arc<dyn Fn() -> serde_json::Value + Send + Sync>>,
    /// Command-line overrides (`--host`, `--port`); they win over the env.
    pub args: ServeArgs,
}

/// The flags every runtime's `serve` subcommand accepts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServeArgs {
    pub host: Option<String>,
    pub port: Option<u16>,
    /// Model mode: the model to serve (else `SENCLAW_MODEL_PATH`).
    pub model: Option<std::path::PathBuf>,
    /// Anything this parser does not know, in order, for the runtime to read.
    pub rest: Vec<String>,
}

impl ServeArgs {
    /// Parse `--host H --port P --model M` (also `--flag=value`) out of an
    /// argument list, leaving everything else in `rest`.
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<ServeArgs, String> {
        let mut out = ServeArgs::default();
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            let (flag, inline) = match arg.split_once('=') {
                Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
                _ => (arg.clone(), None),
            };
            let mut value = |name: &str| -> Result<String, String> {
                match inline.clone() {
                    Some(v) => Ok(v),
                    None => it.next().ok_or_else(|| format!("{name} needs a value")),
                }
            };
            match flag.as_str() {
                "--host" => out.host = Some(value("--host")?),
                "--port" => {
                    let v = value("--port")?;
                    out.port = Some(v.parse().map_err(|_| format!("--port `{v}` is not a port"))?);
                }
                "--model" => out.model = Some(value("--model")?.into()),
                _ => out.rest.push(arg),
            }
        }
        Ok(out)
    }
}

struct Shared {
    env: LaunchEnv,
    mode: RunMode,
    capabilities: Vec<Capability>,
    readiness: Readiness,
    info_detail: Option<Arc<dyn Fn() -> serde_json::Value + Send + Sync>>,
    shutdown: Arc<Notify>,
}

/// Install a `tracing` subscriber writing to stderr (the daemon captures it
/// into `~/.senclaw/logs/runtimes/<id>.log`). `RUST_LOG` overrides the level.
pub fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // Colour only on a terminal: the daemon's log file is read as text.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_target(false)
        .try_init();
}

/// Bind, mount the common routes around `routes`, and serve until shutdown.
pub async fn serve(routes: Router, opts: ServeOptions) -> anyhow::Result<()> {
    let host = opts.args.host.clone().unwrap_or_else(|| opts.env.host.clone());
    let port = opts
        .args
        .port
        .or(opts.env.port)
        .ok_or_else(|| anyhow::anyhow!("no port: pass --port or set SENCLAW_RUNTIME_PORT"))?;
    let ip: IpAddr = host.parse().map_err(|_| anyhow::anyhow!("--host `{host}` is not an IP address"))?;
    if !ip.is_loopback() && !opts.env.allow_remote {
        anyhow::bail!(
            "refusing to bind {ip}: runtimes listen on loopback only \
             (set SENCLAW_RUNTIME_ALLOW_REMOTE=1 to override for development)"
        );
    }

    let shutdown = Arc::new(Notify::new());
    let shared = Arc::new(Shared {
        env: opts.env.clone(),
        mode: opts.mode,
        capabilities: opts.capabilities.clone(),
        readiness: opts.readiness.clone(),
        info_detail: opts.info_detail.clone(),
        shutdown: shutdown.clone(),
    });

    let app = router(routes, shared.clone());
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(ip, port)).await?;
    tracing::info!(
        "{} {} listening on {} (auth: {}, parent: {:?})",
        opts.env.id,
        opts.env.version,
        listener.local_addr()?,
        if opts.env.token.is_some() { "bearer" } else { "off" },
        opts.env.parent_pid
    );

    if let Some(pid) = opts.env.parent_pid {
        spawn_parent_watchdog(pid, shutdown.clone());
    }

    let stop = shutdown.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = stop.notified() => {}
                _ = os_signal() => {}
            }
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}

/// The full router: common routes + the runtime's routes, behind auth.
/// Exposed for tests; [`serve`] is the normal entry point.
fn router(routes: Router, shared: Arc<Shared>) -> Router {
    let protected = Router::new()
        .route("/runtime/info", get(info))
        .route("/runtime/shutdown", post(shutdown_route))
        .with_state(shared.clone())
        .merge(routes)
        .layer(middleware::from_fn_with_state(shared.clone(), require_token));
    Router::new().route("/health", get(health)).with_state(shared).merge(protected)
}

async fn health(State(s): State<Arc<Shared>>) -> Response {
    let body = |status: &str| Health {
        status: status.to_string(),
        id: s.env.id.clone(),
        version: s.env.version.clone(),
    };
    match s.readiness.state() {
        READY => (StatusCode::OK, Json(body("ok"))).into_response(),
        FAILED => (StatusCode::INTERNAL_SERVER_ERROR, Json(body("failed"))).into_response(),
        _ => (StatusCode::SERVICE_UNAVAILABLE, Json(body("loading"))).into_response(),
    }
}

async fn info(State(s): State<Arc<Shared>>) -> Json<RuntimeInfo> {
    Json(RuntimeInfo {
        id: s.env.id.clone(),
        version: s.env.version.clone(),
        mode: s.mode,
        capabilities: s.capabilities.clone(),
        pid: std::process::id(),
        detail: s.info_detail.as_ref().map(|f| f()).unwrap_or(serde_json::Value::Null),
    })
}

async fn shutdown_route(State(s): State<Arc<Shared>>) -> StatusCode {
    s.shutdown.notify_waiters();
    StatusCode::ACCEPTED
}

async fn require_token(State(s): State<Arc<Shared>>, req: Request, next: Next) -> Response {
    let Some(expected) = s.env.token.as_deref() else {
        return next.run(req).await;
    };
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(ErrorBody::with_code("missing or wrong runtime token", codes::UNAUTHORIZED)),
        )
            .into_response()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn os_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn spawn_parent_watchdog(pid: u32, shutdown: Arc<Notify>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if !process_alive(pid) {
                tracing::warn!("parent process {pid} is gone; exiting");
                shutdown.notify_waiters();
                // A request that never finishes must not keep an orphan alive.
                tokio::time::sleep(Duration::from_secs(5)).await;
                std::process::exit(0);
            }
        }
    });
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // Signal 0 checks existence without delivering anything. EPERM means the
    // process exists under another user — still alive.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return false;
        }
        let alive = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
        CloseHandle(handle);
        alive
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn shared(token: Option<&str>, readiness: Readiness) -> Arc<Shared> {
        let mut env = LaunchEnv::from_lookup("sen-test", "0.0.1", |_| None);
        env.token = token.map(str::to_string);
        Arc::new(Shared {
            env,
            mode: RunMode::Service,
            capabilities: vec![Capability::Ocr],
            readiness,
            info_detail: Some(Arc::new(|| serde_json::json!({"loaded": []}))),
            shutdown: Arc::new(Notify::new()),
        })
    }

    fn app(token: Option<&str>, readiness: Readiness) -> Router {
        let routes = Router::new().route("/api/ocr/models", get(|| async { "models" }));
        router(routes, shared(token, readiness))
    }

    async fn status(app: Router, path: &str, bearer: Option<&str>) -> StatusCode {
        let mut req = HttpRequest::builder().uri(path);
        if let Some(t) = bearer {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap().status()
    }

    #[tokio::test]
    async fn health_is_open_everything_else_needs_the_token() {
        let a = app(Some("s3cret"), Readiness::ready());
        assert_eq!(status(a.clone(), "/health", None).await, StatusCode::OK);
        assert_eq!(status(a.clone(), "/api/ocr/models", None).await, StatusCode::UNAUTHORIZED);
        assert_eq!(status(a.clone(), "/api/ocr/models", Some("wrong")).await, StatusCode::UNAUTHORIZED);
        assert_eq!(status(a.clone(), "/api/ocr/models", Some("s3cret")).await, StatusCode::OK);
        assert_eq!(status(a, "/runtime/info", Some("s3cret")).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn no_token_means_no_auth_for_hand_started_runs() {
        let a = app(None, Readiness::ready());
        assert_eq!(status(a, "/api/ocr/models", None).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn health_reports_loading_then_ready() {
        let readiness = Readiness::loading();
        let a = app(None, readiness.clone());
        assert_eq!(status(a.clone(), "/health", None).await, StatusCode::SERVICE_UNAVAILABLE);
        readiness.set_ready();
        assert_eq!(status(a.clone(), "/health", None).await, StatusCode::OK);
        readiness.set_failed();
        assert_eq!(status(a, "/health", None).await, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn serve_args_parse_both_flag_styles() {
        let args = ServeArgs::parse(
            ["--host", "127.0.0.1", "--port=4999", "--model", "/m", "--threads", "4"]
                .into_iter()
                .map(String::from),
        )
        .unwrap();
        assert_eq!(args.host.as_deref(), Some("127.0.0.1"));
        assert_eq!(args.port, Some(4999));
        assert_eq!(args.model, Some("/m".into()));
        assert_eq!(args.rest, vec!["--threads", "4"]);
        assert!(ServeArgs::parse(["--port".to_string(), "x".to_string()]).is_err());
    }

    #[test]
    fn constant_time_eq_matches_only_equal() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn this_process_is_alive() {
        assert!(process_alive(std::process::id()));
    }
}
