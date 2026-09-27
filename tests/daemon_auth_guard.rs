//! Source guards for the daemon's own network exposure.
//!
//! The daemon — unlike a Space App — has an API token, but the token only
//! helps if three things stay true, and all three have been broken before by
//! an innocuous-looking edit:
//!
//! 1. CORS never goes back to `permissive()` (ACAO `*` let any website the
//!    user visited read `/api/llm-config` off the loopback daemon).
//! 2. The serve sites bind from config, not a hardcoded literal (a hardcoded
//!    `0.0.0.0` would expose the daemon with no opt-in).
//! 3. Both serve sites carry the auth middleware AND `ConnectInfo` — without
//!    `into_make_service_with_connect_info` the middleware sees no peer
//!    address and (fail-closed) rejects even loopback clients.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let p: PathBuf = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Every `.rs` under `src/`, skipping the auth module's own doc comments.
fn src_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().map(|x| x == "rs").unwrap_or(false) {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(&repo_root().join("src"), &mut out);
    out
}

#[test]
fn cors_is_never_permissive() {
    let mut offenders = Vec::new();
    for path in src_files() {
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        for (i, line) in body.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") || code.starts_with("///") {
                continue;
            }
            if code.contains("CorsLayer::permissive")
                || code.contains("CorsLayer::very_permissive")
            {
                offenders.push(format!("{}:{}", path.display(), i + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "CorsLayer::permissive sends `Access-Control-Allow-Origin: *`, letting any \
         website the user visits read the loopback daemon's API (including cleartext \
         provider keys from /api/llm-config). Use gateway::ui_server::auth::restrictive_cors(). \
         Offending lines: {offenders:?}"
    );
}

#[test]
fn daemon_serve_sites_bind_from_config() {
    let lib = read("src/lib.rs");
    assert!(
        lib.contains("let ws_addr = format!(\"{ui_bind_host}:{ws_port}\")"),
        "WS gateway must bind cfg.ui_server.bind_host, not a hardcoded host"
    );
    assert!(
        lib.contains("let http_addr = format!(\"{ui_bind_host}:{http_port}\")"),
        "UI server must bind cfg.ui_server.bind_host, not a hardcoded host"
    );
    let core = read("src/gateway/ui_server/core.rs");
    assert!(
        core.contains("state.config.ui_server.bind_host"),
        "start_ui_server must bind from config"
    );
    // No serve site may hardcode INADDR_ANY.
    for rel in ["src/lib.rs", "src/gateway/ui_server/core.rs"] {
        let body = read(rel);
        assert!(
            !body.contains("bind(\"0.0.0.0") && !body.contains("\"0.0.0.0:"),
            "{rel} hardcodes 0.0.0.0 — exposure must be opt-in via SENCLAW_UI_BIND_HOST"
        );
    }
}

#[test]
fn daemon_serve_sites_layer_auth_with_connect_info() {
    let lib = read("src/lib.rs");
    // Both routers get the middleware...
    assert!(
        lib.contains("ui_server::auth::http_auth_mw"),
        "UI router must layer http_auth_mw"
    );
    assert!(
        lib.contains("ui_server::auth::ws_auth_mw"),
        "WS router must layer ws_auth_mw (the in-band `connect` token is not a gate: \
         the dispatcher runs handlers for sockets that never authenticated)"
    );
    // ...and both serve with ConnectInfo, or the middleware fails closed on
    // every request, loopback included.
    let with_connect_info = lib
        .matches("into_make_service_with_connect_info::<std::net::SocketAddr>()")
        .count();
    assert!(
        with_connect_info >= 2,
        "both the UI and WS serve sites must use \
         into_make_service_with_connect_info::<SocketAddr>() so the auth middleware \
         can see the peer address; found {with_connect_info}"
    );
}

#[test]
fn auth_defaults_to_loopback_and_open_paths_are_minimal() {
    let cfg = read("src/config.rs");
    assert!(
        cfg.contains("SENCLAW_UI_BIND_HOST"),
        "daemon bind host must be its own env knob, separate from the Space-App \
         SENCLAW_BIND_HOST (apps have no auth of their own)"
    );
    // Default stays loopback.
    let idx = cfg
        .find("SENCLAW_UI_BIND_HOST")
        .expect("bind host knob present");
    let window = &cfg[idx..(idx + 400).min(cfg.len())];
    assert!(
        window.contains("127.0.0.1"),
        "SENCLAW_UI_BIND_HOST must default to 127.0.0.1"
    );

    // Only the login handshake and the status probe may skip the token.
    let auth = read("src/gateway/ui_server/auth.rs");
    let start = auth.find("OPEN_API_PATHS").expect("OPEN_API_PATHS present");
    let end = start + auth[start..].find(';').expect("declaration ends");
    let decl = &auth[start..end];
    assert!(
        decl.contains("/api/auth/login") && decl.contains("/api/auth/status"),
        "the open-path list must contain the two handshake routes"
    );
    let open_count = decl.matches("\"/api/").count();
    assert_eq!(
        open_count, 2,
        "only /api/auth/login and /api/auth/status may bypass the token; found \
         {open_count} open paths in: {decl}"
    );
}

// ===== Auth mode (auto | always | off) =====

/// `always` exists for one reason: a reverse proxy on the daemon's own host
/// makes every Internet client arrive from 127.0.0.1. An `authorize` that
/// returns early on a loopback peer before consulting the mode would hand that
/// entire deployment a free pass, and it would look like it was working.
#[test]
fn loopback_exemption_is_conditional_on_the_mode() {
    let auth = read("src/gateway/ui_server/auth.rs");
    let start = auth
        .find("pub fn authorize(")
        .expect("authorize present");
    let body = &auth[start..];
    let end = start + body.find("\nfn unauthorized(").unwrap_or(body.len());
    let body = &auth[start..end];

    let exempt = body
        .find("is_loopback()")
        .expect("authorize still has a loopback exemption");
    let guard = body
        .find("mode != AuthMode::Always")
        .expect(
            "the loopback exemption must be guarded by the mode — without it, \
             `always` is indistinguishable from `auto` behind a same-host proxy",
        );
    assert!(
        guard < exempt,
        "the mode check must come *before* the loopback exemption in authorize()"
    );
}

/// A typo in `SENCLAW_AUTH_MODE` must not open the daemon. Same call as
/// `SENCLAW_APP_TOKEN_MODE`: fall back to the safe default, never to `off`.
#[test]
fn unknown_auth_mode_falls_back_to_auto_not_off() {
    let auth = read("src/gateway/ui_server/auth.rs");
    let start = auth
        .find("pub fn from_env_value(")
        .expect("from_env_value present");
    let end = start + auth[start..].find("\n    pub fn as_str").unwrap_or(0);
    let body = &auth[start..end.max(start)];
    assert!(
        body.contains("DEFAULT_AUTH_MODE"),
        "from_env_value must fall back to DEFAULT_AUTH_MODE"
    );
    assert!(
        !body.contains("Self::Off"),
        "an unrecognised SENCLAW_AUTH_MODE must never resolve to Off"
    );
    assert!(
        auth.contains("pub const DEFAULT_AUTH_MODE: AuthMode = AuthMode::Auto;"),
        "the default posture stays `auto` — a daemon on loopback must not start \
         demanding a token after an upgrade"
    );
}

/// The switch is the gate's own control. Putting it in `OPEN_API_PATHS` would
/// let an anonymous remote client turn the gate off.
#[test]
fn auth_mode_route_is_gated() {
    let core = read("src/gateway/ui_server/core.rs");
    assert!(
        core.contains("\"/api/auth/mode\""),
        "the auth-mode route must be mounted"
    );
    let auth = read("src/gateway/ui_server/auth.rs");
    let start = auth.find("OPEN_API_PATHS").expect("OPEN_API_PATHS present");
    let end = start + auth[start..].find(';').expect("declaration ends");
    assert!(
        !auth[start..end].contains("/api/auth/mode"),
        "/api/auth/mode must stay gated — it is the switch that turns the gate off"
    );
}

// ===== Container image =====

/// The daemon's bind host and the Space-App bind host are different knobs on
/// purpose: apps have no authentication of their own. Setting the app one to
/// a wildcard in the image would publish every installed app's REST and MCP
/// surface to whatever can reach the container.
#[test]
fn dockerfile_does_not_expose_space_apps() {
    let df = read("Dockerfile");
    for line in df.lines() {
        let code = line.trim();
        if code.starts_with('#') {
            continue;
        }
        assert!(
            !code.contains("SENCLAW_BIND_HOST=0.0.0.0"),
            "the image must not bind Space Apps beyond loopback: {code}"
        );
    }
    assert!(
        df.contains("SENCLAW_UI_BIND_HOST=0.0.0.0"),
        "the daemon itself must bind the container's interfaces, or `-p` reaches nothing"
    );
}

/// Under `always` every route but the two handshake ones answers 401, so a
/// healthcheck pointed anywhere else marks a perfectly healthy container
/// unhealthy forever.
#[test]
fn dockerfile_healthcheck_uses_an_open_route() {
    let df = read("Dockerfile");
    let start = df.find("HEALTHCHECK").expect("image has a healthcheck");
    let block = &df[start..(start + 400).min(df.len())];
    assert!(
        block.contains("/api/auth/status"),
        "the healthcheck must hit an unauthenticated route; found: {block}"
    );
    assert!(
        df.contains("SENCLAW_AUTH_MODE=always"),
        "the image defaults to `always` — with --network host or a sidecar proxy, \
         `auto` would exempt every caller"
    );
}
