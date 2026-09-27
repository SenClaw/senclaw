//! JavaScript executor MCP server (`senclaw-js`).
//!
//! Exposes a **sandboxed** JavaScript runtime to agents. The engine is QuickJS
//! (via `rquickjs`) running fully in-process with *no* host bindings wired in:
//!
//!   - No filesystem, network, process, or environment access from JS.
//!   - Only the standard ECMAScript intrinsics (Object, Array, JSON, Math,
//!     Date, RegExp, Map/Set, BigInt, Promise, …) plus a capturing `console`.
//!   - Every evaluation is bounded by a **wall-clock timeout** (interrupt
//!     handler) and a **memory limit**; a runaway loop or allocation is killed,
//!     not allowed to hang or OOM the daemon.
//!
//! Tools:
//!   - `js_eval(code, timeout_ms?, memory_mb?)`      — run a snippet, return its
//!     value, captured `console` output, and any thrown error.
//!   - `js_eval_file(path, timeout_ms?, memory_mb?)` — read a `.js`/`.mjs` file
//!     from disk and run it in the same sandbox.
//!   - `js_capabilities()`                           — describe the sandbox
//!     policy (limits + what is / isn't available).
//!
//! The QuickJS `Context`/`Runtime` are `!Send`, so each evaluation builds and
//! tears down its own engine inside a `spawn_blocking` task — nothing JS-related
//! ever crosses an `.await`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use rmcp::ServiceExt;
use rquickjs::{CatchResultExt, Context, Function, Runtime, Value};
use serde::Serialize;

// ── Defaults / caps ──────────────────────────────────────────────────────────

const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_MEMORY_MB: u64 = 128;
/// Hard upper bounds so a caller can't ask the sandbox to run forever or eat
/// all RAM regardless of what they pass.
const MAX_TIMEOUT_MS: u64 = 60_000;
const MAX_MEMORY_MB: u64 = 1_024;
/// Truncate any single string (result repr or a console line) to keep tool
/// payloads bounded.
const MAX_STR_LEN: usize = 100_000;
/// Refuse source larger than this — well past any reasonable snippet.
const MAX_SOURCE_BYTES: usize = 4 * 1024 * 1024;

/// JS prelude installed before user code. Defines a `console` that funnels every
/// argument list through a single native sink (`__senclaw_print`), plus a
/// formatter used by the host to render the final value.
const PRELUDE: &str = r#"
(() => {
  const fmt1 = (x) => {
    if (typeof x === 'string') return x;
    if (typeof x === 'bigint') return x.toString() + 'n';
    if (typeof x === 'function') return x.toString();
    if (x === undefined) return 'undefined';
    try { return JSON.stringify(x); } catch (e) {
      try { return String(x); } catch (e2) { return '[unserializable]'; }
    }
  };
  const emit = (level, args) =>
    __senclaw_print('[' + level + '] ' + Array.prototype.map.call(args, fmt1).join(' '));
  globalThis.console = {
    log:   function () { emit('log', arguments); },
    info:  function () { emit('info', arguments); },
    warn:  function () { emit('warn', arguments); },
    error: function () { emit('error', arguments); },
    debug: function () { emit('debug', arguments); },
  };
  globalThis.__senclaw_format = (v) => {
    if (v === undefined) return 'undefined';
    if (typeof v === 'bigint') return v.toString() + 'n';
    if (typeof v === 'function') return v.toString();
    try { return JSON.stringify(v, null, 2); } catch (e) {
      try { return String(v); } catch (e2) { return '[unserializable]'; }
    }
  };
})();
"#;

// ── Outcome shape returned to the model (serialized to JSON) ─────────────────

#[derive(Debug, Serialize)]
struct EvalOutcome {
    /// True when the script ran to completion without throwing.
    ok: bool,
    /// Rendered final value (JSON-ish). `null` when the script threw.
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<String>,
    /// JS `typeof` of the final value (e.g. "object", "number", "undefined").
    #[serde(skip_serializing_if = "Option::is_none")]
    result_type: Option<String>,
    /// Captured `console.*` lines, in order.
    logs: Vec<String>,
    /// Error message + stack when the script threw or was killed.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// True when the script was aborted because it exceeded the timeout.
    timed_out: bool,
    duration_ms: u128,
}

impl EvalOutcome {
    fn host_error(msg: String, duration_ms: u128) -> Self {
        Self {
            ok: false,
            result: None,
            result_type: None,
            logs: Vec::new(),
            error: Some(msg),
            timed_out: false,
            duration_ms,
        }
    }

    fn to_json(&self) -> String {
        serde_json::to_string_pretty(self)
            .unwrap_or_else(|e| format!("{{\"ok\":false,\"error\":\"serialize: {e}\"}}"))
    }
}

fn clamp_str(mut s: String) -> String {
    if s.len() > MAX_STR_LEN {
        s.truncate(MAX_STR_LEN);
        s.push_str("\n…[truncated]");
    }
    s
}

// ── Core sandbox ─────────────────────────────────────────────────────────────

/// Build a fresh QuickJS engine, run `code`, and tear it down. Blocking; call
/// from `spawn_blocking`. Never panics on JS errors — they become `EvalOutcome`.
fn run_sandbox(code: &str, timeout_ms: u64, memory_mb: u64) -> EvalOutcome {
    let start = Instant::now();

    if code.len() > MAX_SOURCE_BYTES {
        return EvalOutcome::host_error(
            format!(
                "source too large ({} bytes, max {MAX_SOURCE_BYTES})",
                code.len()
            ),
            start.elapsed().as_millis(),
        );
    }

    let timeout_ms = timeout_ms.clamp(1, MAX_TIMEOUT_MS);
    let memory_mb = memory_mb.clamp(1, MAX_MEMORY_MB);

    let rt = match Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            return EvalOutcome::host_error(
                format!("runtime init: {e}"),
                start.elapsed().as_millis(),
            )
        }
    };
    rt.set_memory_limit((memory_mb as usize) * 1024 * 1024);
    rt.set_max_stack_size(2 * 1024 * 1024);

    // Wall-clock kill switch. QuickJS calls the interrupt handler on backward
    // jumps and calls, so infinite loops are caught.
    let timed_out = Arc::new(AtomicBool::new(false));
    {
        let flag = timed_out.clone();
        let deadline = start + Duration::from_millis(timeout_ms);
        rt.set_interrupt_handler(Some(Box::new(move || {
            if Instant::now() >= deadline {
                flag.store(true, Ordering::Relaxed);
                true
            } else {
                false
            }
        })));
    }

    let ctx = match Context::full(&rt) {
        Ok(c) => c,
        Err(e) => {
            return EvalOutcome::host_error(
                format!("context init: {e}"),
                start.elapsed().as_millis(),
            )
        }
    };

    let logs: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let (result, result_type, error) = ctx.with(|ctx| {
        // Native console sink.
        let sink = logs.clone();
        let print = Function::new(ctx.clone(), move |line: String| {
            sink.borrow_mut().push(clamp_str(line));
        });
        let globals = ctx.globals();
        if let Ok(f) = print {
            let _ = globals.set("__senclaw_print", f);
        }
        if let Err(e) = ctx.eval::<Value, _>(PRELUDE).catch(&ctx) {
            return (None, None, Some(format!("prelude failed: {e}")));
        }

        match ctx.eval::<Value, _>(code).catch(&ctx) {
            Ok(v) => {
                let type_name = format!("{:?}", v.type_of()).to_lowercase();
                let repr = globals
                    .get::<_, Function>("__senclaw_format")
                    .and_then(|f| f.call::<_, String>((v,)))
                    .unwrap_or_else(|_| "[format error]".to_string());
                (Some(clamp_str(repr)), Some(type_name), None)
            }
            Err(caught) => (None, None, Some(caught.to_string())),
        }
    });

    let timed_out = timed_out.load(Ordering::Relaxed);
    let error = error.map(|e| {
        if timed_out {
            format!("execution timed out after {timeout_ms}ms (killed)")
        } else {
            e
        }
    });

    let captured_logs = logs.borrow().clone();
    EvalOutcome {
        ok: error.is_none(),
        result,
        result_type,
        logs: captured_logs,
        error,
        timed_out,
        duration_ms: start.elapsed().as_millis(),
    }
}

/// Run JavaScript in the sandbox and return the [`EvalOutcome`] as a JSON
/// string. Used by both the `js_eval` MCP tool and the UI server's
/// `/api/code/run` REPL endpoint. Runs the (`!Send`) engine inside
/// `spawn_blocking` so the async runtime is never blocked.
pub async fn eval_to_json(code: String, timeout_ms: u64, memory_mb: u64) -> String {
    tokio::task::spawn_blocking(move || run_sandbox(&code, timeout_ms, memory_mb))
        .await
        .map(|o| o.to_json())
        .unwrap_or_else(|e| EvalOutcome::host_error(format!("sandbox task join: {e}"), 0).to_json())
}

/// Transpile TypeScript to JavaScript (types stripped) and run it in the same
/// sandbox as [`eval_to_json`]. A transpile failure is reported as a sandbox
/// error outcome (so the caller gets the same shape either way).
pub async fn eval_ts_to_json(code: String, timeout_ms: u64, memory_mb: u64) -> String {
    match crate::mcp::ts_transpile::transpile_ts(&code) {
        Ok(js) => eval_to_json(js, timeout_ms, memory_mb).await,
        Err(e) => EvalOutcome::host_error(e, 0).to_json(),
    }
}

// ── Parameter schemas ────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct JsEvalParams {
    /// JavaScript source to evaluate. The value of the final expression/statement
    /// is returned; use `console.log(...)` to surface intermediate output.
    code: String,
    /// Wall-clock limit in milliseconds (default 5000, max 60000). Exceeding it
    /// aborts the script.
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Memory limit in MiB (default 128, max 1024).
    #[serde(default)]
    memory_mb: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct BashRunParams {
    /// Bash script to run in the brush sandbox.
    code: String,
    /// Wall-clock limit in milliseconds (default 5000, max 60000). Enforced by
    /// killing the sandbox child process.
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct JsEvalFileParams {
    /// Absolute or workspace-relative path to a `.js` / `.mjs` file to run.
    path: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    memory_mb: Option<u64>,
}

// ── MCP server ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct McpJsServer {
    default_timeout_ms: u64,
    default_memory_mb: u64,
}

impl McpJsServer {
    /// Always available — the sandbox needs no configuration, only limits, and
    /// both have defaults. Returns `Option` purely so every child server in the
    /// aggregated `senclaw-core` process is constructed the same way.
    pub fn from_env() -> Result<Option<Self>> {
        let default_timeout_ms = std::env::var("SENCLAW_JS_TIMEOUT_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);
        let default_memory_mb = std::env::var("SENCLAW_JS_MEMORY_MB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MEMORY_MB)
            .clamp(1, MAX_MEMORY_MB);
        Ok(Some(Self {
            default_timeout_ms,
            default_memory_mb,
        }))
    }
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpJsServer {
    #[rmcp::tool(
        description = "Run JavaScript in an isolated sandbox (QuickJS) — no filesystem, network, or process access. Returns the final value, captured console output, and any error. Bounded by a wall-clock timeout and memory limit. Use for calculations, data transforms, JSON munging, regex tests, and verifying small JS logic."
    )]
    async fn js_eval(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            JsEvalParams,
        >,
    ) -> String {
        let timeout = p.timeout_ms.unwrap_or(self.default_timeout_ms);
        let memory = p.memory_mb.unwrap_or(self.default_memory_mb);
        eval_to_json(p.code, timeout, memory).await
    }

    #[rmcp::tool(
        description = "Run TypeScript in the sandbox: the source is transpiled to JavaScript (types stripped — no type-checking) and executed with the same isolation and limits as js_eval. Use for TS snippets, interfaces, generics, enums."
    )]
    async fn js_eval_ts(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            JsEvalParams,
        >,
    ) -> String {
        let timeout = p.timeout_ms.unwrap_or(self.default_timeout_ms);
        let memory = p.memory_mb.unwrap_or(self.default_memory_mb);
        eval_ts_to_json(p.code, timeout, memory).await
    }

    #[rmcp::tool(
        description = "Run a Bash script in the brush sandbox (pure-Rust shell): no environment, empty PATH (external programs like ls/curl/rm by name are blocked), a temp working directory, and a kill-enforced timeout. Returns stdout (`result`), stderr lines (`logs`), exit_code, and timing. Use for shell logic, arithmetic, text processing with builtins. NOT an OS jail — absolute-path binaries can still be reached."
    )]
    async fn bash_run(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            BashRunParams,
        >,
    ) -> String {
        let timeout = p.timeout_ms.unwrap_or(self.default_timeout_ms);
        let v = crate::gateway::ui_server::bash_sandbox::run(p.code, timeout).await;
        serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
    }

    #[rmcp::tool(
        description = "Read a JavaScript file from disk and run it in the sandbox (same isolation as js_eval). Convenient for executing a script you've written to a file."
    )]
    async fn js_eval_file(
        &self,
        rmcp::handler::server::wrapper::Parameters(p): rmcp::handler::server::wrapper::Parameters<
            JsEvalFileParams,
        >,
    ) -> String {
        let timeout = p.timeout_ms.unwrap_or(self.default_timeout_ms);
        let memory = p.memory_mb.unwrap_or(self.default_memory_mb);
        let path = p.path;
        tokio::task::spawn_blocking(move || match std::fs::read_to_string(&path) {
            Ok(code) => run_sandbox(&code, timeout, memory),
            Err(e) => EvalOutcome::host_error(format!("read {path}: {e}"), 0),
        })
        .await
        .map(|o| o.to_json())
        .unwrap_or_else(|e| EvalOutcome::host_error(format!("sandbox task join: {e}"), 0).to_json())
    }

    #[rmcp::tool(
        description = "Describe the JS sandbox policy: enforced limits and which globals are available vs. blocked. Call this if unsure what the sandbox can do before running code."
    )]
    fn js_capabilities(&self) -> String {
        let v = serde_json::json!({
            "engine": "QuickJS (rquickjs)",
            "isolation": "in-process, no host bindings",
            "limits": {
                "default_timeout_ms": self.default_timeout_ms,
                "max_timeout_ms": MAX_TIMEOUT_MS,
                "default_memory_mb": self.default_memory_mb,
                "max_memory_mb": MAX_MEMORY_MB,
                "max_source_bytes": MAX_SOURCE_BYTES,
                "max_output_chars_per_string": MAX_STR_LEN,
            },
            "available": [
                "ECMAScript intrinsics: Object, Array, String, Number, Boolean, BigInt, Math, JSON, Date, RegExp, Map, Set, WeakMap, WeakSet, Symbol, Proxy, Reflect, Promise, typed arrays, ArrayBuffer",
                "console.log / info / warn / error / debug (captured and returned)"
            ],
            "blocked": [
                "filesystem (no fs / require / import)",
                "network (no fetch / XMLHttpRequest / WebSocket)",
                "process / environment (no process, no globalThis env access)",
                "timers (no setTimeout / setInterval — no event loop is driven)"
            ],
            "notes": [
                "Each evaluation gets a fresh runtime; no state persists between calls.",
                "Promises resolve synchronously only; there is no async event loop, so awaited I/O is unavailable.",
                "Infinite loops and over-allocation are killed by the timeout / memory guard."
            ]
        });
        serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string())
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

    let server = McpJsServer::from_env()?.expect("js server has no required env");
    let service = server.serve(rmcp::transport::io::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_arithmetic() {
        let o = run_sandbox("1 + 2 * 3", 2000, 64);
        assert!(o.ok, "error: {:?}", o.error);
        assert_eq!(o.result.as_deref(), Some("7"));
        assert_eq!(o.result_type.as_deref(), Some("int"));
    }

    #[test]
    fn captures_console() {
        let o = run_sandbox("console.log('hi', 42); 'done'", 2000, 64);
        assert!(o.ok, "error: {:?}", o.error);
        assert_eq!(o.result.as_deref(), Some("\"done\""));
        assert_eq!(o.logs, vec!["[log] hi 42".to_string()]);
    }

    #[test]
    fn serializes_objects() {
        let o = run_sandbox("({a: 1, b: [2, 3]})", 2000, 64);
        assert!(o.ok, "error: {:?}", o.error);
        let r = o.result.unwrap();
        assert!(r.contains("\"a\": 1"), "got {r}");
        assert!(r.contains("\"b\""), "got {r}");
    }

    #[test]
    fn reports_thrown_errors() {
        let o = run_sandbox("throw new Error('boom')", 2000, 64);
        assert!(!o.ok);
        assert!(
            o.error.as_deref().unwrap().contains("boom"),
            "got {:?}",
            o.error
        );
    }

    #[test]
    fn syntax_errors_are_caught_not_panics() {
        let o = run_sandbox("function (", 2000, 64);
        assert!(!o.ok);
        assert!(o.error.is_some());
    }

    #[test]
    fn infinite_loop_is_killed_by_timeout() {
        let o = run_sandbox("while (true) {}", 300, 64);
        assert!(!o.ok);
        assert!(o.timed_out, "expected timeout, got {:?}", o.error);
    }

    #[tokio::test]
    async fn eval_to_json_returns_outcome_shape() {
        let json = eval_to_json("40 + 2".to_string(), 2000, 64).await;
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["result"], "42");
        assert!(v["duration_ms"].is_number());
    }

    #[test]
    fn no_network_or_fs_globals() {
        let o = run_sandbox(
            "typeof fetch + ',' + typeof require + ',' + typeof process",
            2000,
            64,
        );
        assert!(o.ok, "error: {:?}", o.error);
        assert_eq!(
            o.result.as_deref(),
            Some("\"undefined,undefined,undefined\"")
        );
    }
}
