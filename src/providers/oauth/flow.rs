//! The interactive sign-in: build an authorize URL, catch the loopback
//! redirect, exchange the code.
//!
//! The callback listener is a single-shot raw TCP server rather than an axum
//! app. It has to bind a *specific* loopback port for Codex, live for exactly
//! one request, and disappear — spinning up a framework for that is more
//! machinery than the job needs.
//!
//! Binding is hardcoded to `127.0.0.1`. Unlike the Space App servers there is
//! no `SENCLAW_BIND_HOST` knob here: an OAuth redirect target that answers on
//! a LAN interface would let anyone on the network race the browser for the
//! authorization code.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::OauthManager;
use super::pkce::{self, Pkce};
use super::provider::{self, CallbackPort, OauthProviderDef};
use super::store::OauthAccount;
use super::token;

/// How long the user gets to finish the browser flow before the port is freed.
const FLOW_TIMEOUT: Duration = Duration::from_secs(300);

/// Where an in-flight sign-in has got to. The UI polls this.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FlowState {
    /// Waiting for the user to finish in the browser.
    Pending,
    /// Device flow: the user must type `userCode` at `verificationUri`.
    AwaitingUserCode {
        #[serde(rename = "userCode")]
        user_code: String,
        #[serde(rename = "verificationUri")]
        verification_uri: String,
        /// Same page with the code pre-filled, when the provider offers one.
        #[serde(rename = "verificationUriComplete")]
        verification_uri_complete: Option<String>,
    },
    /// Done — the account is in the store.
    Completed {
        #[serde(rename = "accountId")]
        account_id: String,
        label: String,
    },
    /// Gave up. `error` is safe to show the user.
    Failed { error: String },
}

/// Handed back to the caller so it can open the browser and poll.
#[derive(Debug, Clone, Serialize)]
pub struct StartedFlow {
    #[serde(rename = "flowId")]
    pub flow_id: String,
    /// Page for the user to open. For a device flow this is the verification
    /// page rather than a redirect URL.
    #[serde(rename = "authorizeUrl")]
    pub authorize_url: String,
    pub provider: String,
    /// Which grant is running, so the UI knows whether to show a code.
    pub kind: &'static str,
    /// Loopback port the redirect will land on. Zero for device flows, which
    /// need no listener.
    pub port: u16,
    /// Device flow only: the code the user types.
    #[serde(rename = "userCode", skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
}

/// Begin a sign-in, dispatching on the provider's grant type.
pub async fn start(manager: Arc<OauthManager>, provider_id: &str) -> Result<StartedFlow> {
    let def = provider::get(provider_id)
        .ok_or_else(|| anyhow!("unknown OAuth provider `{provider_id}`"))?;
    if !def.is_available() {
        bail!(
            "{} sign-in is not available in this build: it was compiled without the provider's \
             OAuth client (see src/providers/oauth/provider.rs)",
            def.display_name
        );
    }

    match def.flow {
        provider::FlowKind::AuthCodePkce => start_auth_code(manager, def).await,
        provider::FlowKind::DeviceCode => start_device_code(manager, def).await,
    }
}

/// RFC 8628. Requests a user code up front so the caller can display it
/// immediately, then polls in the background until the user finishes.
async fn start_device_code(
    manager: Arc<OauthManager>,
    def: &'static OauthProviderDef,
) -> Result<StartedFlow> {
    let auth = token::request_device_code(&manager.http, def).await?;

    let flow_id = format!("flow_{}", pkce::random_state());
    manager.set_flow_state(
        &flow_id,
        FlowState::AwaitingUserCode {
            user_code: auth.user_code.clone(),
            verification_uri: auth.verification_uri.clone(),
            verification_uri_complete: auth.verification_uri_complete.clone(),
        },
    );

    let started = StartedFlow {
        flow_id: flow_id.clone(),
        authorize_url: auth
            .verification_uri_complete
            .clone()
            .unwrap_or_else(|| auth.verification_uri.clone()),
        provider: def.id.to_string(),
        kind: "device_code",
        port: 0,
        user_code: Some(auth.user_code.clone()),
    };

    tokio::spawn(async move {
        let outcome = poll_device_grant(&manager, def, &auth).await;
        let next = match outcome {
            Ok(account) => FlowState::Completed {
                account_id: account.id.clone(),
                label: account.label.clone(),
            },
            Err(e) => FlowState::Failed {
                error: e.to_string(),
            },
        };
        manager.set_flow_state(&flow_id, next);
    });

    Ok(started)
}

async fn poll_device_grant(
    manager: &OauthManager,
    def: &'static OauthProviderDef,
    auth: &token::DeviceAuthorization,
) -> Result<OauthAccount> {
    let mut interval = auth.interval;
    // The device code's own lifetime bounds the loop; cap it at the flow
    // timeout so a provider advertising a very long expiry can't pin a task.
    let deadline = auth.expires_in.min(FLOW_TIMEOUT.as_secs() as i64).max(1) as u64;
    let mut waited = 0u64;

    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        waited += interval;
        if waited >= deadline {
            bail!("sign-in timed out — the code expired before it was entered");
        }

        match token::poll_device_token(&manager.http, def, &auth.device_code).await? {
            token::DevicePoll::Pending => continue,
            token::DevicePoll::SlowDown(next) => {
                // Honour the provider's back-off, never speeding back up.
                interval = interval.max(next);
            }
            token::DevicePoll::Granted(tokens) => {
                return manager.adopt_tokens(def, *tokens).await;
            }
        }
    }
}

/// Binds the callback port immediately so a collision is reported now, not
/// after the user has already authorised in the browser.
async fn start_auth_code(
    manager: Arc<OauthManager>,
    def: &'static OauthProviderDef,
) -> Result<StartedFlow> {
    let listener = bind_callback(def).await?;
    let port = listener.port()?;
    let redirect_uri = def.redirect_uri(port);

    let pkce = Pkce::generate();
    let state = pkce::random_state();
    let authorize_url = build_authorize_url(def, &redirect_uri, &state, &pkce.challenge);

    let flow_id = format!("flow_{}", pkce::random_state());
    manager.set_flow_state(&flow_id, FlowState::Pending);

    let task_manager = Arc::clone(&manager);
    let task_flow_id = flow_id.clone();
    let verifier = pkce.verifier.clone();
    tokio::spawn(async move {
        let outcome = run_callback(
            &task_manager,
            def,
            listener,
            &redirect_uri,
            &verifier,
            &state,
        )
        .await;

        let next = match outcome {
            Ok(account) => FlowState::Completed {
                account_id: account.id.clone(),
                label: account.label.clone(),
            },
            Err(e) => FlowState::Failed {
                error: e.to_string(),
            },
        };
        task_manager.set_flow_state(&task_flow_id, next);
    });

    Ok(StartedFlow {
        flow_id,
        authorize_url,
        provider: def.id.to_string(),
        kind: "auth_code_pkce",
        port,
        user_code: None,
    })
}

/// The loopback listener pair behind one redirect URI.
///
/// The redirect is `http://localhost:<port>/…`, and `localhost` resolves to
/// BOTH loopback addresses — on Windows the resolver even prefers `::1`. A
/// v4-only listener mostly still works (the browser falls back when `::1`
/// refuses), but if any other process listens on `::1:<port>` the redirect
/// lands there instead. Holding both stacks closes that hole; v6 stays
/// best-effort because machines with IPv6 disabled must keep working.
#[derive(Debug)]
struct CallbackListener {
    v4: TcpListener,
    v6: Option<TcpListener>,
}

impl CallbackListener {
    fn port(&self) -> Result<u16> {
        Ok(self.v4.local_addr()?.port())
    }

    async fn accept(&self) -> std::io::Result<TcpStream> {
        match &self.v6 {
            Some(v6) => tokio::select! {
                r = self.v4.accept() => r.map(|(s, _)| s),
                r = v6.accept() => r.map(|(s, _)| s),
            },
            None => self.v4.accept().await.map(|(s, _)| s),
        }
    }
}

async fn bind_callback(def: &OauthProviderDef) -> Result<CallbackListener> {
    match def.callback_port {
        CallbackPort::Ephemeral => {
            // The two stacks must share one port number (there is only one
            // redirect URI). An ephemeral v4 port may already be taken on v6
            // by an unrelated process, so try a few draws before settling for
            // v4-only.
            for _ in 0..5 {
                let v4 = TcpListener::bind(("127.0.0.1", 0))
                    .await
                    .context("bind loopback callback port")?;
                let port = v4.local_addr()?.port();
                if let Ok(v6) = TcpListener::bind(("::1", port)).await {
                    return Ok(CallbackListener { v4, v6: Some(v6) });
                }
            }
            let v4 = TcpListener::bind(("127.0.0.1", 0))
                .await
                .context("bind loopback callback port")?;
            Ok(CallbackListener { v4, v6: None })
        }
        CallbackPort::Fixed(port) => {
            let v4 = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
                anyhow!(
                    "port {port} is required by {} but is already in use ({e}). \
                     Close whatever is listening on it and try again.",
                    def.display_name
                )
            })?;
            // No port to renegotiate here — a v6 failure just means v4-only.
            let v6 = TcpListener::bind(("::1", port)).await.ok();
            Ok(CallbackListener { v4, v6 })
        }
    }
}

/// Assemble the authorize URL. Every value is percent-encoded; the challenge
/// and state are already URL-safe base64 but encoding them anyway keeps one
/// code path.
pub fn build_authorize_url(
    def: &OauthProviderDef,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
) -> String {
    let mut params: Vec<(String, String)> = vec![
        ("client_id".into(), def.client_id.to_string()),
        ("response_type".into(), "code".into()),
        ("redirect_uri".into(), redirect_uri.to_string()),
        ("scope".into(), def.scope_string()),
        ("code_challenge".into(), code_challenge.to_string()),
        ("code_challenge_method".into(), "S256".into()),
        ("state".into(), state.to_string()),
    ];
    for (k, v) in def.extra_authorize_params {
        params.push(((*k).into(), (*v).into()));
    }

    let query = params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");

    format!("{}?{}", def.authorize_url, query)
}

/// Serve the loopback listener until the real redirect arrives, then exchange.
async fn run_callback(
    manager: &OauthManager,
    def: &'static OauthProviderDef,
    listener: CallbackListener,
    redirect_uri: &str,
    verifier: &str,
    state: &str,
) -> Result<OauthAccount> {
    let params = tokio::time::timeout(FLOW_TIMEOUT, wait_for_redirect(&listener, def))
        .await
        .map_err(|_| anyhow!("sign-in timed out after {}s", FLOW_TIMEOUT.as_secs()))??;

    if let Some(err) = params.get("error") {
        let detail = params
            .get("error_description")
            .map(|d| format!(": {d}"))
            .unwrap_or_default();
        bail!("{} denied the sign-in ({err}{detail})", def.display_name);
    }

    let returned_state = params.get("state").map(String::as_str).unwrap_or_default();
    // Anthropic can fold the state into the code as `code#state`; accept the
    // match from either position before rejecting.
    let code = params
        .get("code")
        .ok_or_else(|| anyhow!("callback carried no authorization code"))?;
    let embedded_state = code.split_once('#').map(|(_, s)| s).unwrap_or_default();

    if !pkce::state_matches(state, returned_state) && !pkce::state_matches(state, embedded_state) {
        bail!("callback state did not match — sign-in rejected as a possible forgery");
    }

    let tokens = token::exchange_code(&manager.http, def, code, redirect_uri, verifier, state)
        .await
        .context("exchanging the authorization code")?;

    manager.adopt_tokens(def, tokens).await
}

/// Accept connections until one looks like the OAuth redirect. Browsers also
/// probe `/favicon.ico`, and a stray request must not end the flow.
async fn wait_for_redirect(
    listener: &CallbackListener,
    def: &OauthProviderDef,
) -> Result<HashMap<String, String>> {
    loop {
        let mut stream = listener.accept().await.context("accept callback")?;
        let Some(target) = read_request_target(&mut stream).await? else {
            respond(&mut stream, 400, "Bad request").await;
            continue;
        };

        let (path, query) = split_target(&target);
        if path != def.callback_path {
            respond(&mut stream, 404, "Not found").await;
            continue;
        }

        let params = parse_query(query);
        if !params.contains_key("code") && !params.contains_key("error") {
            respond(&mut stream, 400, "Missing authorization code").await;
            continue;
        }

        // Deliberately non-committal: the state check and token exchange have
        // not run yet, so claiming success here would be a lie whenever the
        // redirect turns out to be forged or the exchange fails. The real
        // outcome shows up in SenClaw.
        let message = if params.contains_key("code") {
            format!(
                "Returning to SenClaw to finish connecting {}. You can close this tab.",
                def.display_name
            )
        } else {
            "Sign-in was cancelled. You can close this tab.".to_string()
        };
        respond(&mut stream, 200, &message).await;
        return Ok(params);
    }
}

/// Read just enough of the request to get the target. The whole request line
/// must arrive in the first few KiB — a redirect that doesn't is not one.
async fn read_request_target(stream: &mut TcpStream) -> Result<Option<String>> {
    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await.context("read callback")?;
    if n == 0 {
        return Ok(None);
    }
    let head = String::from_utf8_lossy(&buf[..n]);
    let Some(line) = head.lines().next() else {
        return Ok(None);
    };
    Ok(parse_request_target(line).map(|s| s.to_string()))
}

/// `GET /callback?code=x HTTP/1.1` → `/callback?code=x`
pub fn parse_request_target(request_line: &str) -> Option<&str> {
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?;
    if method != "GET" {
        return None;
    }
    parts.next()
}

/// Split a request target into path and raw query.
pub fn split_target(target: &str) -> (&str, &str) {
    match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    }
}

/// Percent-decode an `a=1&b=2` query string.
pub fn parse_query(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let key = urlencoding::decode(k)
            .unwrap_or_else(|_| k.into())
            .into_owned();
        let val = urlencoding::decode(v)
            .unwrap_or_else(|_| v.into())
            .into_owned();
        out.insert(key, val);
    }
    out
}

async fn respond(stream: &mut TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    // Escaped because `message` embeds a provider display name.
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>SenClaw</title>\
         <body style=\"font:16px system-ui;padding:3rem;text-align:center\">\
         <p>{}</p></body>",
        html_escape(message)
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_normal_request_line() {
        assert_eq!(
            parse_request_target("GET /callback?code=abc&state=xyz HTTP/1.1"),
            Some("/callback?code=abc&state=xyz")
        );
    }

    #[test]
    fn rejects_non_get_methods() {
        assert_eq!(parse_request_target("POST /callback HTTP/1.1"), None);
        assert_eq!(parse_request_target(""), None);
        assert_eq!(parse_request_target("garbage"), None);
    }

    #[test]
    fn splits_path_from_query() {
        assert_eq!(split_target("/callback?a=1"), ("/callback", "a=1"));
        assert_eq!(split_target("/callback"), ("/callback", ""));
        assert_eq!(split_target("/auth/callback?x"), ("/auth/callback", "x"));
    }

    #[test]
    fn decodes_query_parameters() {
        let q = parse_query("code=ab%23cd&state=x%2By&empty=");
        assert_eq!(q.get("code").unwrap(), "ab#cd");
        assert_eq!(q.get("state").unwrap(), "x+y");
        assert_eq!(q.get("empty").unwrap(), "");
    }

    #[test]
    fn tolerates_a_malformed_query() {
        let q = parse_query("");
        assert!(q.is_empty());
        let q = parse_query("novalue&a=1");
        assert_eq!(q.get("novalue").unwrap(), "");
        assert_eq!(q.get("a").unwrap(), "1");
    }

    #[test]
    fn authorize_url_carries_pkce_and_provider_extras() {
        let def = provider::get("claude").unwrap();
        let url = build_authorize_url(def, "http://localhost:9999/callback", "st4te", "ch4llenge");

        assert!(url.starts_with("https://claude.ai/oauth/authorize?"));
        assert!(url.contains("code_challenge=ch4llenge"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=st4te"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
        // Provider-specific extra.
        assert!(url.contains("code=true"));
    }

    #[test]
    fn authorize_url_percent_encodes_scopes_and_redirect() {
        let def = provider::get("antigravity").unwrap();
        let url = build_authorize_url(def, "http://localhost:1/callback", "s", "c");
        // Scope URLs contain `:` and `/` which must not leak raw into the query.
        assert!(url.contains("https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fcloud-platform"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1%2Fcallback"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
    }

    #[test]
    fn codex_authorize_url_carries_its_cli_flow_flags() {
        let def = provider::get("codex").unwrap();
        let url = build_authorize_url(def, "http://localhost:1455/auth/callback", "s", "c");
        assert!(url.contains("id_token_add_organizations=true"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
        assert!(url.contains("scope=openid%20profile%20email%20offline_access"));
    }

    #[test]
    fn html_escaping_neutralises_markup() {
        assert_eq!(html_escape("<b>&\"x\""), "&lt;b&gt;&amp;&quot;x&quot;");
    }

    #[tokio::test]
    async fn ephemeral_bind_picks_a_free_loopback_port() {
        let def = provider::get("claude").unwrap();
        let listener = bind_callback(def).await.unwrap();
        let addr = listener.v4.local_addr().unwrap();
        assert!(
            addr.ip().is_loopback(),
            "must not bind a routable interface"
        );
        assert_ne!(addr.port(), 0);
        assert_eq!(listener.port().unwrap(), addr.port());

        // When the machine has IPv6 at all, both stacks must share the port —
        // there is only one redirect URI.
        if let Some(v6) = &listener.v6 {
            let a6 = v6.local_addr().unwrap();
            assert!(a6.ip().is_loopback());
            assert_eq!(a6.port(), addr.port(), "v4/v6 ports diverged");
        }
    }

    /// `localhost` resolves to ::1 first on some systems (Windows notably);
    /// the code must arrive even when the browser never touches 127.0.0.1.
    #[tokio::test]
    async fn callback_arrives_over_ipv6_loopback() {
        let def = provider::get("claude").unwrap();
        let listener = bind_callback(def).await.unwrap();
        if listener.v6.is_none() {
            return; // machine has no usable ::1 — nothing to exercise
        }
        let port = listener.port().unwrap();

        let server = tokio::spawn(async move { wait_for_redirect(&listener, def).await });

        let mut real = TcpStream::connect(("::1", port)).await.unwrap();
        real.write_all(b"GET /callback?code=v6code&state=s HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut body = Vec::new();
        let _ = real.read_to_end(&mut body).await;

        let params = server.await.unwrap().unwrap();
        assert_eq!(params.get("code").unwrap(), "v6code");
    }

    #[tokio::test]
    async fn a_taken_fixed_port_fails_with_an_actionable_message() {
        let def = provider::get("codex").unwrap();
        let CallbackPort::Fixed(port) = def.callback_port else {
            panic!("codex should pin a port");
        };
        // Occupy it first; if the machine already has 1455 busy the test still
        // exercises the same branch.
        let _squatter = TcpListener::bind(("127.0.0.1", port)).await;
        if _squatter.is_ok() {
            let err = bind_callback(def).await.unwrap_err().to_string();
            assert!(err.contains("already in use"), "{err}");
            assert!(err.contains("1455"), "{err}");
        }
    }

    #[tokio::test]
    async fn callback_server_ignores_stray_requests_and_returns_the_real_one() {
        let def = provider::get("claude").unwrap();
        let listener = CallbackListener {
            v4: TcpListener::bind(("127.0.0.1", 0)).await.unwrap(),
            v6: None,
        };
        let port = listener.port().unwrap();

        let server = tokio::spawn(async move { wait_for_redirect(&listener, def).await });

        // A browser favicon probe on the wrong path must not end the flow.
        let mut probe = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        probe
            .write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        let mut sink = Vec::new();
        let _ = probe.read_to_end(&mut sink).await;
        assert!(String::from_utf8_lossy(&sink).contains("404"));

        // Then the real redirect.
        let mut real = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        real.write_all(
            b"GET /callback?code=thecode&state=thestate HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await
        .unwrap();
        let mut body = Vec::new();
        let _ = real.read_to_end(&mut body).await;
        let page = String::from_utf8_lossy(&body);
        // The page must not claim success — the state check has not run yet.
        assert!(page.contains("Returning to SenClaw"), "{page}");
        assert!(!page.contains("is connected"), "{page}");

        let params = server.await.unwrap().unwrap();
        assert_eq!(params.get("code").unwrap(), "thecode");
        assert_eq!(params.get("state").unwrap(), "thestate");
    }

    #[tokio::test]
    async fn callback_server_surfaces_a_denial() {
        let def = provider::get("claude").unwrap();
        let listener = CallbackListener {
            v4: TcpListener::bind(("127.0.0.1", 0)).await.unwrap(),
            v6: None,
        };
        let port = listener.port().unwrap();
        let server = tokio::spawn(async move { wait_for_redirect(&listener, def).await });

        let mut deny = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        deny.write_all(
            b"GET /callback?error=access_denied&error_description=nope HTTP/1.1\r\nHost: x\r\n\r\n",
        )
        .await
        .unwrap();
        let mut body = Vec::new();
        let _ = deny.read_to_end(&mut body).await;
        assert!(String::from_utf8_lossy(&body).contains("cancelled"));

        let params = server.await.unwrap().unwrap();
        assert_eq!(params.get("error").unwrap(), "access_denied");
    }
}
