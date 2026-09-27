//! Code executor REPL API.
//!
//! Endpoint:
//!   POST /api/code/run — { code, timeout_ms?, memory_mb?, language? }
//!                        → an outcome with the common shape
//!                          { ok, result, result_type, logs, error, timed_out, duration_ms }
//!
//! Languages:
//!   - `javascript`/`js` (default) and `typescript`/`ts` run in the **sandboxed**
//!     QuickJS engine (see [`crate::mcp::js_server`]) — no host access.
//!   - `bash`/`sh` runs in the **brush** sandbox (pure-Rust shell) — see
//!     [`super::bash_sandbox`]: no env inheritance, empty PATH (external
//!     programs by name blocked), temp cwd, timeout + output cap. When the
//!     `exec` switch in Plugins → Sandbox is on, bash runs in the **OS
//!     sandbox** instead (real shell, write-jailed, network off by default).
//!   - `python`/`py` and `node`/`nodejs` run REAL interpreters inside the OS
//!     sandbox (`crate::sandbox`) — Seatbelt / bubblewrap / docker. They exist
//!     only through the sandbox: the switches in Plugins → Sandbox refuse the
//!     language, they never fall back to raw execution.

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde::Deserialize;

use super::core::{AppError, UiState};

/// Sandbox defaults match the `senclaw-js` MCP server. The sandbox clamps to
/// its own hard ceilings (60 s / 1 GiB) regardless of what's requested here.
const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_MEMORY_MB: u64 = 128;

#[derive(Deserialize)]
pub(crate) struct CodeRunBody {
    code: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    memory_mb: Option<u64>,
    /// Language: `javascript`/`js` (default), `typescript`/`ts`, or `bash`/`sh`.
    #[serde(default)]
    language: Option<String>,
}

enum Lang {
    Js,
    Ts,
    Bash,
    Python,
    Node,
}

fn parse_lang(language: Option<&str>) -> Result<Lang, String> {
    match language.map(|l| l.trim().to_lowercase()) {
        None => Ok(Lang::Js),
        Some(l) if l.is_empty() || l == "javascript" || l == "js" => Ok(Lang::Js),
        Some(l) if l == "typescript" || l == "ts" => Ok(Lang::Ts),
        Some(l) if l == "bash" || l == "sh" || l == "shell" => Ok(Lang::Bash),
        Some(l) if l == "python" || l == "py" || l == "python3" => Ok(Lang::Python),
        Some(l) if l == "node" || l == "nodejs" => Ok(Lang::Node),
        Some(l) => Err(format!(
            "language `{l}` is not supported yet — JavaScript, TypeScript, Bash, Python (sandbox), and Node.js (sandbox) are live"
        )),
    }
}

/// Shape a sandbox [`crate::sandbox::db::Run`] into the common REPL outcome so
/// every language returns the same fields to the UI.
fn sandbox_outcome(run: crate::sandbox::db::Run) -> serde_json::Value {
    let ok = run.exit_code == Some(0) && !run.timed_out;
    serde_json::json!({
        "ok": ok,
        "result": run.stdout,
        "result_type": format!("exit {}", run.exit_code.unwrap_or(-1)),
        "exit_code": run.exit_code,
        "logs": run.stderr.lines().collect::<Vec<_>>(),
        "error": if run.timed_out {
            serde_json::Value::String(format!("execution timed out after {}ms (killed)", run.duration_ms))
        } else if ok {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(format!("exited with code {}", run.exit_code.unwrap_or(-1)))
        },
        "timed_out": run.timed_out,
        "duration_ms": run.duration_ms,
        "isolation": run.isolation,
    })
}

/// Run a snippet through the OS sandbox (throwaway, one-shot).
async fn run_os_sandboxed(
    language: &str,
    code: String,
    network: bool,
    timeout_ms: u64,
) -> serde_json::Value {
    match crate::sandbox::policy::run_once_sandboxed(language, &code, network, Some(timeout_ms as i64))
        .await
    {
        Ok(run) => sandbox_outcome(run),
        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
    }
}

/// Run `code` in the appropriate sandbox and return the common outcome JSON.
/// Shared by the REPL endpoint and the artifact-run endpoint.
pub(super) async fn run_code(
    language: Option<&str>,
    code: String,
    timeout_ms: u64,
    memory_mb: u64,
) -> Result<serde_json::Value, String> {
    let lang = parse_lang(language)?;
    let policy = crate::sandbox::policy::current();
    Ok(match lang {
        // With `exec` enforcement on, bash gets the real OS sandbox (real
        // shell and utilities, write-jailed); otherwise the pure-Rust brush
        // sandbox, exactly as before the sandbox integration.
        Lang::Bash if policy.exec_shell => {
            run_os_sandboxed("bash", code, policy.code_network, timeout_ms).await
        }
        Lang::Bash => super::bash_sandbox::run(code, timeout_ms).await,
        Lang::Python => {
            if !policy.run_python {
                return Err(
                    "Python execution is switched off (Plugins → Sandbox → Run Python)".into(),
                );
            }
            run_os_sandboxed("python", code, policy.code_network, timeout_ms).await
        }
        Lang::Node => {
            if !policy.run_node {
                return Err(
                    "Node.js execution is switched off (Plugins → Sandbox → Run Node.js)".into(),
                );
            }
            run_os_sandboxed("javascript", code, policy.code_network, timeout_ms).await
        }
        Lang::Js | Lang::Ts => {
            let json = match lang {
                Lang::Ts => {
                    crate::mcp::js_server::eval_ts_to_json(code, timeout_ms, memory_mb).await
                }
                _ => crate::mcp::js_server::eval_to_json(code, timeout_ms, memory_mb).await,
            };
            // eval_*_to_json returns a JSON-encoded outcome; forward it verbatim.
            serde_json::from_str(&json).unwrap_or_else(|_| {
                serde_json::json!({ "ok": false, "error": "internal: malformed sandbox output" })
            })
        }
    })
}

pub(crate) async fn code_run(
    State(_state): State<Arc<UiState>>,
    Json(body): Json<CodeRunBody>,
) -> Result<impl IntoResponse, AppError> {
    if body.code.trim().is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "code is empty".into()));
    }
    let timeout = body.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    let memory = body.memory_mb.unwrap_or(DEFAULT_MEMORY_MB);
    let value = run_code(body.language.as_deref(), body.code, timeout, memory)
        .await
        .map_err(|e| AppError(StatusCode::BAD_REQUEST, e))?;
    Ok(Json(value))
}
