//! Credential for the daemon's own loopback callers.
//!
//! Several parts of SenClaw reach the daemon's HTTP API over loopback rather
//! than calling a function: the MCP subprocesses (`space`, `patterns`, `ocr`),
//! the kanban board's LLM lookup, and the local-model Space Apps, whose
//! OpenAI-compatible endpoint *is* a daemon route
//! (`/api/space/apps/<id>/proxy/v1`).
//!
//! Under [`AuthMode::Auto`](crate::gateway::ui_server::auth::AuthMode) those
//! callers pass because they are loopback. Under `always` — the setting that
//! makes a cloud deployment behind a same-host reverse proxy safe — nothing is
//! exempt, so they must present the token like anyone else. The daemon
//! publishes it into its own environment at startup (so children inherit it)
//! and these helpers attach it.
//!
//! Attaching it to a loopback URL that is *not* the daemon is harmless: a
//! stray header on a request to a local process that already runs with the
//! same privileges.

/// Set once by `run_daemon`, after the token is resolved.
///
/// A process-global rather than `std::env::set_var`: by the time the token is
/// known the daemon already has threads running, and `setenv` races every
/// concurrent `getenv` in the process (which is why Rust 2024 made it
/// `unsafe`). Child processes get the token through their MCP config's own env
/// map instead — see [`crate::mcp::helper`].
static DAEMON_TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The daemon's own UI port, for the same class of caller: code that only
/// holds a `config.json` path (e.g. `load_llm_configs`, which many call sites
/// invoke with just a path) but needs to build a loopback URL back into this
/// same process — a `local:<key>` model's `baseURL`
/// (`/api/runtimes/models/<key>/v1`).
static DAEMON_UI_PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

pub fn set_daemon_ui_port(port: u16) {
    let _ = DAEMON_UI_PORT.set(port);
}

/// The daemon's UI port, else `SENCLAW_UI_PORT`, else the compiled default —
/// the same fallback chain `Config::from_env` itself uses, so a caller with no
/// `Config` at hand still resolves the port a running daemon actually bound.
pub fn daemon_ui_port() -> u16 {
    if let Some(p) = DAEMON_UI_PORT.get() {
        return *p;
    }
    std::env::var("SENCLAW_UI_PORT").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(18788)
}

/// Publish the daemon's API token to its in-process callers. Idempotent; a
/// second call with a different token is ignored (there is one daemon).
pub fn set_daemon_token(token: &str) {
    let t = token.trim();
    if !t.is_empty() {
        let _ = DAEMON_TOKEN.set(t.to_string());
    }
}

/// The daemon's API token: the value `run_daemon` published in this process,
/// else `SENCLAW_API_TOKEN` from the environment — which is how a *child*
/// process (an MCP server) receives it.
pub fn daemon_token() -> Option<String> {
    if let Some(t) = DAEMON_TOKEN.get() {
        return Some(t.clone());
    }
    std::env::var("SENCLAW_API_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// True when `url` points at an `/api/…` route on this machine.
///
/// Host-and-path rather than host-and-port: the daemon's port is
/// configurable, and a caller that guessed it wrong would silently lose its
/// credential exactly when the gate is closed.
pub fn is_own_api_url(url: &str) -> bool {
    let rest = match url.strip_prefix("http://") {
        Some(r) => r,
        None => match url.strip_prefix("https://") {
            Some(r) => r,
            None => return false,
        },
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let host = if let Some(end) = authority.rfind(']') {
        &authority[..=end]
    } else {
        authority.split(':').next().unwrap_or("")
    };
    crate::gateway::ui_server::auth::is_loopback_host(host) && path.starts_with("/api/")
}

/// The header a loopback caller should send with a request to `url`, or
/// `None` when the URL is not ours or no token has been published.
pub fn header_for(url: &str) -> Option<(&'static str, String)> {
    if !is_own_api_url(url) {
        return None;
    }
    daemon_token().map(|t| ("X-SenClaw-Token", t))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_our_own_api_routes() {
        assert!(is_own_api_url("http://127.0.0.1:18788/api/llm-config"));
        assert!(is_own_api_url(
            "http://localhost:18788/api/space/apps/mlx-lm/proxy/v1/chat/completions"
        ));
        assert!(is_own_api_url("http://[::1]:18788/api/config"));
        // A different port is still us — the port is configurable.
        assert!(is_own_api_url("http://127.0.0.1:9999/api/config"));
    }

    #[test]
    fn rejects_everything_else() {
        // A Space App's own port serves no /api/ route of ours.
        assert!(!is_own_api_url("http://127.0.0.1:4800/v1/chat/completions"));
        // Never leak the daemon token off-box.
        assert!(!is_own_api_url("https://api.openai.com/api/v1/chat"));
        assert!(!is_own_api_url("https://api.anthropic.com/v1/messages"));
        assert!(!is_own_api_url("http://192.168.1.5:18788/api/config"));
        assert!(!is_own_api_url("localhost:18788/api/config"));
        assert!(!is_own_api_url(""));
    }
}
