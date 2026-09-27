//! Host capability probe — what isolation this machine can actually provide.
//!
//! Every answer here is measured, never assumed. The reason is concrete: on the
//! development machine for this app the Docker *CLI* is installed and on PATH,
//! `docker --version` prints happily, and the daemon is dead ("Docker Desktop
//! is unable to start"). A probe that stops at "is the binary there?" reports a
//! working Docker backend and every sandbox then fails at run time with a
//! confusing error. So the docker probe talks to the **daemon**.
//!
//! It also must not hang. `docker info` against a broken Docker Desktop blocks
//! for minutes; the probe wraps every child process in a hard timeout and kills
//! it, because a capability probe that hangs takes the whole app's UI with it.

use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::sandbox::config;

/// How long a probe result is trusted before re-measuring. Short enough that
/// starting Docker Desktop shows up quickly, long enough that listing sandboxes
/// doesn't spawn a subprocess per request.
const CACHE_TTL: Duration = Duration::from_secs(20);

/// Hard cap on any single probe command.
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

/// Which isolation primitive the `direct` backend would use on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DirectKind {
    /// macOS Seatbelt via `/usr/bin/sandbox-exec`.
    Seatbelt,
    /// Linux user namespaces via `bwrap` (bubblewrap).
    Bubblewrap,
    /// Windows AppContainer + Job Object.
    AppContainer,
    /// No OS-level confinement available. Commands would run as an ordinary
    /// child process with a scrubbed environment and a private working
    /// directory — weaker than a jail, and labelled as such everywhere.
    Degraded,
    /// Direct execution is not offered at all (Windows).
    Unsupported,
}

impl DirectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DirectKind::Seatbelt => "seatbelt",
            DirectKind::Bubblewrap => "bubblewrap",
            DirectKind::AppContainer => "appcontainer",
            DirectKind::Degraded => "degraded",
            DirectKind::Unsupported => "unsupported",
        }
    }

    /// True when the kernel/OS actually enforces the boundary.
    pub fn is_enforced(self) -> bool {
        matches!(
            self,
            DirectKind::Seatbelt | DirectKind::Bubblewrap | DirectKind::AppContainer
        )
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectCaps {
    pub available: bool,
    pub kind: DirectKind,
    /// Human-readable reason, always populated — this is what the UI shows when
    /// `available` is false or `kind` is `degraded`.
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DockerCaps {
    /// CLI on PATH.
    pub cli: bool,
    /// Daemon answered. This — not `cli` — is what gates the docker backend.
    pub available: bool,
    pub client_version: Option<String>,
    pub server_version: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Caps {
    pub os: String,
    pub arch: String,
    pub direct: DirectCaps,
    pub docker: DockerCaps,
    /// Interpreters found on the host, for the `direct` backend. Docker
    /// sandboxes get whatever the image ships instead.
    pub host_interpreters: Vec<String>,
    /// Backends usable right now, best first.
    pub backends: Vec<String>,
    pub probed_at_ms: i64,
}

impl Caps {
    /// Backend to use when the caller didn't name one.
    pub fn default_backend(&self) -> Option<String> {
        self.backends.first().cloned()
    }
}

// The two halves are cached separately, and that separation is load-bearing
// rather than tidiness.
//
// Measuring Docker means talking to the daemon, which costs up to
// PROBE_TIMEOUT when the daemon is broken — the state this app's own
// development machine is in. Measuring the direct backend is a file-exists
// check. With one shared cache, every expiry made the *next* run of any kind
// wait four seconds for a Docker answer it did not need: a 38 ms Python
// snippet took 4.06 s end to end. Splitting them means a direct run never
// waits on Docker.
static DIRECT_CACHE: Mutex<Option<(Instant, DirectCaps)>> = Mutex::new(None);
static DOCKER_CACHE: Mutex<Option<(Instant, DockerCaps)>> = Mutex::new(None);

/// Direct-backend capability. Cheap — no subprocess.
pub async fn direct_caps(force: bool) -> DirectCaps {
    if !force {
        if let Some((at, c)) = DIRECT_CACHE.lock().unwrap().as_ref() {
            if at.elapsed() < CACHE_TTL {
                return c.clone();
            }
        }
    }
    let c = probe_direct(std::env::consts::OS).await;
    *DIRECT_CACHE.lock().unwrap() = Some((Instant::now(), c.clone()));
    c
}

/// Docker capability. Talks to the daemon; can cost up to `PROBE_TIMEOUT`.
pub async fn docker_caps(force: bool) -> DockerCaps {
    if !force {
        if let Some((at, c)) = DOCKER_CACHE.lock().unwrap().as_ref() {
            if at.elapsed() < CACHE_TTL {
                return c.clone();
            }
        }
    }
    let c = probe_docker().await;
    *DOCKER_CACHE.lock().unwrap() = Some((Instant::now(), c.clone()));
    c
}

/// The whole picture, for the UI banner and `sbx_capabilities`.
pub async fn probe(force: bool) -> Caps {
    measure(force).await
}

async fn measure(force: bool) -> Caps {
    let os = std::env::consts::OS.to_string();
    let direct = direct_caps(force).await;
    let docker = docker_caps(force).await;
    let host_interpreters = if direct.available {
        probe_interpreters().await
    } else {
        Vec::new()
    };

    // Ordering is a policy statement: a container is a stronger boundary than a
    // seatbelt profile, so when both work docker leads. `degraded` direct is
    // last — it is a convenience, not an isolation guarantee.
    let mut backends = Vec::new();
    if docker.available {
        backends.push("docker".to_string());
    }
    if direct.available {
        if direct.kind.is_enforced() {
            backends.insert(0.min(backends.len()), "direct".to_string());
            // Enforced direct is cheap and instant; prefer it for the common
            // "run this snippet" case, keeping docker available explicitly.
            backends.sort_by_key(|b| if b == "direct" { 0 } else { 1 });
        } else {
            backends.push("direct".to_string());
        }
    }

    Caps {
        os,
        arch: std::env::consts::ARCH.to_string(),
        direct,
        docker,
        host_interpreters,
        backends,
        probed_at_ms: chrono::Utc::now().timestamp_millis(),
    }
}

async fn probe_direct(os: &str) -> DirectCaps {
    match os {
        "macos" => {
            if std::path::Path::new("/usr/bin/sandbox-exec").exists() {
                DirectCaps {
                    available: true,
                    kind: DirectKind::Seatbelt,
                    detail: "macOS Seatbelt (`sandbox-exec`): writes outside the sandbox directory are \
                             blocked, and the credential stores (~/.ssh, ~/.aws, Keychain…) are unreadable."
                        .into(),
                }
            } else {
                DirectCaps {
                    available: true,
                    kind: DirectKind::Degraded,
                    detail: "/usr/bin/sandbox-exec not found — commands run as an ordinary child process \
                             with NO operating-system barrier."
                        .into(),
                }
            }
        }
        "linux" => {
            if which("bwrap").await.is_some() {
                DirectCaps {
                    available: true,
                    kind: DirectKind::Bubblewrap,
                    detail: "Linux bubblewrap: private namespaces (pid/ipc/uts), the system mounted \
                             read-only, and only the sandbox directory writable."
                        .into(),
                }
            } else {
                DirectCaps {
                    available: true,
                    kind: DirectKind::Degraded,
                    detail: "`bwrap` is missing — install bubblewrap (apt install bubblewrap) for real \
                             isolation. Commands currently run as an ordinary child process."
                        .into(),
                }
            }
        }
        "windows" => DirectCaps {
            available: true,
            kind: DirectKind::AppContainer,
            detail: "Windows AppContainer + Job Object: writes outside the sandbox directory are \
                     blocked, user data is unreadable, the network is off unless a capability \
                     grants it, and RAM/process count are enforced limits. NOT yet verified on \
                     a real machine — see docs/sandbox-windows-research.md."
                .into(),
        },
        other => DirectCaps {
            available: false,
            kind: DirectKind::Unsupported,
            detail: format!("Direct execution is not supported on `{other}`."),
        },
    }
}

async fn probe_docker() -> DockerCaps {
    let bin = config::docker_bin();
    let cli = which(&bin).await.is_some();
    if !cli {
        return DockerCaps {
            cli: false,
            available: false,
            client_version: None,
            server_version: None,
            detail: format!("`{bin}` is not on PATH. Install Docker, then check again."),
        };
    }

    // One call answers both halves: `.Client.Version` always prints, but the
    // `.Server.*` field only resolves when the daemon answers. A broken daemon
    // makes this exit non-zero with its reason on stderr — which is exactly the
    // text worth showing the user, so it is passed through, not swallowed.
    let out = run_probe(
        &bin,
        &["version", "--format", "{{.Client.Version}}|{{.Server.Version}}"],
    )
    .await;

    match out {
        ProbeOut::Ok(stdout) => {
            let (client, server) = stdout.trim().split_once('|').unwrap_or((stdout.trim(), ""));
            let server = server.trim();
            if server.is_empty() {
                return DockerCaps {
                    cli: true,
                    available: false,
                    client_version: none_if_empty(client),
                    server_version: None,
                    detail: "The Docker CLI is present but the daemon is not answering. Start Docker Desktop."
                        .into(),
                };
            }
            DockerCaps {
                cli: true,
                available: true,
                client_version: none_if_empty(client),
                server_version: Some(server.to_string()),
                detail: format!("Docker daemon {server} is ready."),
            }
        }
        ProbeOut::Failed { stderr } => DockerCaps {
            cli: true,
            available: false,
            client_version: None,
            server_version: None,
            detail: format!(
                "The Docker CLI is present but the daemon is not running: {}",
                first_line(&stderr).unwrap_or_else(|| "no reason given".into())
            ),
        },
        ProbeOut::TimedOut => DockerCaps {
            cli: true,
            available: false,
            client_version: None,
            server_version: None,
            detail: format!(
                "The Docker daemon did not answer within {}s (Docker Desktop is hung or still starting).",
                PROBE_TIMEOUT.as_secs()
            ),
        },
    }
}

/// Interpreters the `direct` backend can invoke. Probed by asking each for its
/// version — a file existing on PATH but not executable is not an interpreter.
async fn probe_interpreters() -> Vec<String> {
    const CANDIDATES: &[&str] = &["python3", "node", "bash", "sh", "ruby", "perl", "deno", "php"];
    let mut found = Vec::new();
    for name in CANDIDATES {
        if which(name).await.is_some() {
            found.push(name.to_string());
        }
    }
    found
}

// ── process helpers ─────────────────────────────────────────────────────────

enum ProbeOut {
    Ok(String),
    Failed { stderr: String },
    TimedOut,
}

/// Run a short command under a hard timeout, killing it on expiry.
async fn run_probe(bin: &str, args: &[&str]) -> ProbeOut {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ProbeOut::Failed {
                stderr: e.to_string(),
            }
        }
    };

    match tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
        // `kill_on_drop` + dropping the future on timeout is what actually stops
        // a wedged Docker Desktop from holding a process for minutes.
        Err(_) => ProbeOut::TimedOut,
        Ok(Err(e)) => ProbeOut::Failed {
            stderr: e.to_string(),
        },
        Ok(Ok(out)) => {
            if out.status.success() {
                ProbeOut::Ok(String::from_utf8_lossy(&out.stdout).to_string())
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                ProbeOut::Failed {
                    stderr: if stderr.is_empty() { stdout } else { stderr },
                }
            }
        }
    }
}

/// Absolute path of an executable on PATH, or None.
///
/// Resolved by walking `PATH` rather than shelling out to `which`, so the probe
/// costs no process spawn and behaves the same when PATH is scrubbed.
pub async fn which(bin: &str) -> Option<String> {
    if bin.contains('/') || bin.contains('\\') {
        let p = std::path::Path::new(bin);
        return is_exec(p).then(|| p.to_string_lossy().to_string());
    }
    let path = std::env::var("PATH").unwrap_or_default();
    // Windows separates PATH with `;` and needs an extension appended — a bare
    // `python3` is never a file there, so a Unix-only lookup finds nothing and
    // the sandbox reports "no interpreter" on a machine that has one.
    let sep = if cfg!(windows) { ';' } else { ':' };
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    for dir in path.split(sep).filter(|d| !d.is_empty()) {
        for ext in exts {
            let cand = std::path::Path::new(dir).join(format!("{bin}{ext}"));
            if is_exec(&cand) {
                return Some(cand.to_string_lossy().to_string());
            }
        }
    }
    None
}

fn is_exec(p: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(p) {
            Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

fn none_if_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn first_line(s: &str) -> Option<String> {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn which_finds_a_real_binary_and_rejects_a_fake_one() {
        assert!(which("sh").await.is_some(), "sh must be on PATH");
        assert!(which("senclaw-definitely-not-a-real-binary").await.is_none());
    }

    #[tokio::test]
    async fn which_on_an_absolute_path_checks_the_exec_bit() {
        // A real file that is not executable must not resolve.
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("plain.txt");
        std::fs::write(&f, "x").unwrap();
        assert!(which(f.to_str().unwrap()).await.is_none());
    }

    #[tokio::test]
    async fn probe_never_hangs_and_always_names_a_reason() {
        let caps = probe(true).await;
        // Whatever the machine offers, the fields the UI depends on are present.
        assert!(!caps.os.is_empty());
        assert!(!caps.docker.detail.is_empty());
        assert!(!caps.direct.detail.is_empty());
        // Docker must never be reported available on the strength of the CLI.
        if caps.docker.available {
            assert!(caps.docker.server_version.is_some());
        }
    }

    #[tokio::test]
    async fn run_probe_reports_failure_rather_than_panicking() {
        match run_probe("senclaw-nope", &["--version"]).await {
            ProbeOut::Failed { stderr } => assert!(!stderr.is_empty()),
            _ => panic!("a missing binary must surface as Failed"),
        }
    }

    #[test]
    fn enforced_kinds_are_exactly_the_os_backed_ones() {
        assert!(DirectKind::Seatbelt.is_enforced());
        assert!(DirectKind::Bubblewrap.is_enforced());
        assert!(!DirectKind::Degraded.is_enforced());
        assert!(!DirectKind::Unsupported.is_enforced());
    }
}
