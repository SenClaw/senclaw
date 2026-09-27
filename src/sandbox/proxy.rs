//! The allowlisting egress proxy — how "this app may reach only these sites"
//! is actually enforced.
//!
//! No OS sandbox here can filter outbound traffic by hostname. macOS Seatbelt's
//! profile language accepts only `*` or `localhost` as a remote host (measured:
//! anything else is a parse error), and bubblewrap has no per-host concept at
//! all. So per-site egress is built the other way around:
//!
//! 1. The sandbox gets **no direct egress** — no `connect` ports, no resolver.
//! 2. It gets **one loopback port**: this proxy.
//! 3. `HTTP_PROXY` / `HTTPS_PROXY` point at it.
//!
//! A client that honours the proxy environment reaches the allowed sites. A
//! client that ignores it reaches *nothing*, because its direct connection is
//! refused by the sandbox itself. That is the property worth having: the failure
//! mode of an uncooperative client is a broken request, not a bypass.
//!
//! # What this is not
//!
//! * **Not a TLS interceptor.** `CONNECT` is tunnelled after the host check, so
//!   the app's traffic stays end-to-end encrypted and SenClaw never sees inside
//!   it. The check is on *where*, never on *what*.
//! * **Not a virtual-host oracle.** For plaintext HTTP the decision is made on
//!   the request target, then the bytes are relayed to that origin. Two sites
//!   sharing one IP cannot be told apart by IP, so an app could send a `Host:`
//!   header for a site it did not get — to a server it *did* get. Use HTTPS
//!   allowlisting (the normal case) where SNI and the certificate bind the name.
//! * **Not a route to this machine.** Loopback and link-local addresses are
//!   refused after resolution, so neither an allowlisted name that resolves to
//!   `127.0.0.1` nor a DNS rebind mid-connection can walk back to SenClaw's own
//!   API — the escape this whole subsystem exists to close.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Ports a website is allowed to be on. The allowlist is about *where*, but a
/// tunnel to an arbitrary port on an allowed host would also carry SSH or a
/// database protocol, which is not what "may reach this website" means.
const WEB_PORTS: &[u16] = &[80, 443, 8080, 8443];

/// Header slurp cap and timeout — a client that sends neither a request line nor
/// a blank line is not a client.
const MAX_HEADER: usize = 16 * 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How many distinct refused hosts to remember for the UI.
const RECENT_DENIED: usize = 12;

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyStats {
    pub allowed: u64,
    pub denied: u64,
    /// Distinct hosts that were refused, newest last. This is the field that
    /// turns "the app is broken" into "the app wanted `x.com`, add it or don't".
    pub recent_denied: Vec<String>,
}

/// A running proxy. Dropping it stops the listener — so an app's proxy dies with
/// the app's launch record and cannot outlive the process it was opened for.
pub struct HostProxy {
    pub port: u16,
    allow: Arc<RwLock<Vec<String>>>,
    stats: Arc<Mutex<ProxyStats>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for HostProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl HostProxy {
    /// Bind a proxy on loopback for `label` (an app id, used in logs).
    pub async fn spawn(label: impl Into<String>, hosts: Vec<String>) -> Result<HostProxy> {
        Self::spawn_inner(label.into(), hosts, false).await
    }

    /// `test_mode` relaxes two rules that make the happy path untestable on a
    /// developer machine: the loopback refusal and the web-port list. It is
    /// reachable only from this module's tests — every public entry point passes
    /// `false`.
    async fn spawn_inner(label: String, hosts: Vec<String>, test_mode: bool) -> Result<HostProxy> {
        // Loopback only, and port 0: the proxy is reachable by the sandbox on
        // this machine and by nothing else.
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("bind the allowlist proxy on loopback")?;
        let port = listener.local_addr()?.port();
        let allow = Arc::new(RwLock::new(hosts));
        let stats = Arc::new(Mutex::new(ProxyStats::default()));

        let (a, s, l) = (allow.clone(), stats.clone(), label.clone());
        let task = tokio::spawn(async move {
            loop {
                let (sock, _peer) = match listener.accept().await {
                    Ok(x) => x,
                    // A closed listener ends the loop; anything else is a
                    // transient accept error worth retrying.
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => break,
                    Err(_) => continue,
                };
                let (a, s, l) = (a.clone(), s.clone(), l.clone());
                tokio::spawn(async move {
                    if let Err(e) = serve(sock, &a, &s, &l, test_mode).await {
                        tracing::debug!("[sandbox-proxy:{l}] connection ended: {e}");
                    }
                });
            }
        });

        tracing::info!("[sandbox-proxy:{label}] listening on 127.0.0.1:{port}");
        Ok(HostProxy { port, allow, stats, task })
    }

    pub fn stats(&self) -> ProxyStats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Replace the allowlist of a running proxy — used when the user edits the
    /// hosts while the app is up, so the change lands without a restart.
    pub fn set_hosts(&self, hosts: Vec<String>) {
        if let Ok(mut a) = self.allow.write() {
            *a = hosts;
        }
    }

    pub fn hosts(&self) -> Vec<String> {
        self.allow.read().map(|a| a.clone()).unwrap_or_default()
    }
}

async fn serve(
    mut client: TcpStream,
    allow: &Arc<RwLock<Vec<String>>>,
    stats: &Arc<Mutex<ProxyStats>>,
    label: &str,
    test_mode: bool,
) -> Result<()> {
    let mut buf = Vec::with_capacity(2048);
    let head = tokio::time::timeout(HEADER_TIMEOUT, read_head(&mut client, &mut buf)).await;
    match head {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            let _ = client.write_all(b"HTTP/1.1 408 Request Timeout\r\n\r\n").await;
            return Ok(());
        }
    }

    let req = parse_request(&buf);
    let (host, port, first_bytes, connect) = match req {
        Req::Connect { host, port } => (host, port, Vec::new(), true),
        Req::Http { host, port, rewritten } => (host, port, rewritten, false),
        Req::Bad(why) => {
            let _ = client
                .write_all(format!("HTTP/1.1 400 Bad Request\r\n\r\n{why}\n").as_bytes())
                .await;
            return Ok(());
        }
    };

    if !test_mode && !WEB_PORTS.contains(&port) {
        deny(&mut client, connect, &format!("{host}:{port}"), stats, label,
             &format!("port {port} is not a web port")).await;
        return Ok(());
    }
    let allowed = allow
        .read()
        .map(|a| crate::sandbox::app_policy::host_allowed(&host, &a))
        .unwrap_or(false);
    if !allowed {
        deny(&mut client, connect, &host, stats, label, "not in this app's allowed sites").await;
        return Ok(());
    }

    // Resolve here, and connect to the address we checked — never to a name the
    // upstream stack might resolve again to something else.
    let addr = match resolve_allowed(&host, port, test_mode).await {
        Ok(a) => a,
        Err(why) => {
            deny(&mut client, connect, &host, stats, label, &why).await;
            return Ok(());
        }
    };

    let mut upstream = match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let msg = format!("cannot reach {host}: {e}");
            let _ = client
                .write_all(format!("HTTP/1.1 502 Bad Gateway\r\n\r\n{msg}\n").as_bytes())
                .await;
            return Ok(());
        }
        Err(_) => {
            let _ = client.write_all(b"HTTP/1.1 504 Gateway Timeout\r\n\r\n").await;
            return Ok(());
        }
    };

    if let Ok(mut s) = stats.lock() {
        s.allowed = s.allowed.saturating_add(1);
    }
    if connect {
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
    } else if !first_bytes.is_empty() {
        upstream.write_all(&first_bytes).await?;
    }
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

async fn deny(
    client: &mut TcpStream,
    _connect: bool,
    host: &str,
    stats: &Arc<Mutex<ProxyStats>>,
    label: &str,
    why: &str,
) {
    if let Ok(mut s) = stats.lock() {
        s.denied = s.denied.saturating_add(1);
        if !s.recent_denied.iter().any(|h| h == host) {
            s.recent_denied.push(host.to_string());
            if s.recent_denied.len() > RECENT_DENIED {
                s.recent_denied.remove(0);
            }
        }
    }
    tracing::info!("[sandbox-proxy:{label}] refused {host}: {why}");
    // The body is what a developer reads in the app's log, so it says what to do.
    let body = format!(
        "SenClaw sandbox: `{host}` is blocked for this app ({why}).\n\
         Add it in Plugins → Space Apps → sandbox settings, or switch the app to full network.\n"
    );
    // One write, not two: a client that reads once must not get the status line
    // without the explanation — which is exactly how the first version of this
    // code produced a flaky test and would have produced a confusing app log.
    let mut out = format!(
        "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body.as_bytes());
    let _ = client.write_all(&out).await;
}

/// Read until the end of the header block.
async fn read_head(client: &mut TcpStream, buf: &mut Vec<u8>) -> Result<()> {
    let mut chunk = [0u8; 1024];
    loop {
        let n = client.read(&mut chunk).await?;
        if n == 0 {
            anyhow::bail!("client closed before sending a request");
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(());
        }
        if buf.len() > MAX_HEADER {
            anyhow::bail!("request head over {MAX_HEADER} bytes");
        }
    }
}

/// Resolve `host`, refusing every address that points back at this machine.
///
/// Both directions of the rebinding problem are closed here: the check is on the
/// resolved address (not on the name), and the caller connects to exactly the
/// address that was checked.
async fn resolve_allowed(host: &str, port: u16, test_mode: bool) -> Result<SocketAddr, String> {
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("cannot resolve {host}: {e}"))?;
    let mut saw_any = false;
    for a in addrs {
        saw_any = true;
        if test_mode || !ip_forbidden(a.ip()) {
            return Ok(a);
        }
    }
    Err(if saw_any {
        format!("{host} resolves to an address on this machine, which is never allowed")
    } else {
        format!("cannot resolve {host}")
    })
}

/// Addresses the proxy refuses to be a bridge to. Loopback is the escape that
/// motivated the whole design; link-local carries the cloud metadata service
/// that hands out instance credentials.
fn ip_forbidden(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast()
                || v4.is_multicast()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // fe80::/10 — link-local, and the IPv6 metadata endpoint.
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // ::ffff:127.0.0.1 and friends: judge the mapped address.
                || v6.to_ipv4_mapped().map(|v4| v4.is_loopback() || v4.is_link_local()).unwrap_or(false)
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) enum Req {
    Connect { host: String, port: u16 },
    Http { host: String, port: u16, rewritten: Vec<u8> },
    Bad(&'static str),
}

/// Parse the first request line of a proxied connection.
///
/// Pure, and tested directly: a mistake here is a hole (the wrong host checked)
/// rather than an error anyone would notice.
pub(crate) fn parse_request(buf: &[u8]) -> Req {
    let end = match buf.windows(2).position(|w| w == b"\r\n") {
        Some(i) => i,
        None => return Req::Bad("no request line"),
    };
    let line = match std::str::from_utf8(&buf[..end]) {
        Ok(l) => l,
        Err(_) => return Req::Bad("request line is not text"),
    };
    let mut parts = line.split_whitespace();
    let (method, target) = match (parts.next(), parts.next()) {
        (Some(m), Some(t)) => (m, t),
        _ => return Req::Bad("malformed request line"),
    };

    if method.eq_ignore_ascii_case("CONNECT") {
        // An explicit port is required: a tunnel carries opaque bytes, and
        // guessing where to send them is not a proxy's decision to make.
        if !target.contains(':') {
            return Req::Bad("CONNECT needs host:port");
        }
        return match split_host_port(target, 443) {
            Some((host, port)) => Req::Connect { host, port },
            None => Req::Bad("CONNECT needs host:port"),
        };
    }

    // Everything else must be absolute-form (`GET http://host/path`) — that is
    // what a client sends *to a proxy*. Origin-form means the client thinks it
    // is talking to the origin server, which it is not.
    let rest = match target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("HTTP://"))
    {
        Some(r) => r,
        None => {
            return Req::Bad(
                "this is SenClaw's sandbox proxy, not a web server: send an absolute-form \
                 request (or CONNECT for https)",
            )
        }
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match split_host_port(authority, 80) {
        Some(x) => x,
        None => return Req::Bad("malformed host in the request target"),
    };
    // Rewrite to origin-form. Absolute-form is legal to forward, but plenty of
    // servers (and every router in front of them) handle only origin-form.
    let mut rewritten = Vec::with_capacity(buf.len());
    rewritten.extend_from_slice(method.as_bytes());
    rewritten.push(b' ');
    rewritten.extend_from_slice(path.as_bytes());
    if let Some(version) = parts.next() {
        rewritten.push(b' ');
        rewritten.extend_from_slice(version.as_bytes());
    }
    rewritten.extend_from_slice(&buf[end..]);
    Req::Http { host, port, rewritten }
}

fn split_host_port(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let a = authority.trim();
    if a.is_empty() {
        return None;
    }
    // Strip userinfo, then reject IPv6 literals: an allowlist entry can never be
    // one (see `app_policy::normalise_host`), so accepting the syntax here would
    // only create a way to spell loopback.
    let a = a.rsplit_once('@').map(|(_, h)| h).unwrap_or(a);
    if a.contains('[') || a.contains(']') {
        return None;
    }
    match a.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            Some((h.to_ascii_lowercase(), p.parse().ok()?))
        }
        Some(_) => None,
        None => Some((a.to_ascii_lowercase(), default_port)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn connect_is_parsed_with_its_port() {
        assert_eq!(
            parse_request(b"CONNECT api.example.com:443 HTTP/1.1\r\nHost: x\r\n\r\n"),
            Req::Connect { host: "api.example.com".into(), port: 443 }
        );
        // Uppercase method, odd port, mixed-case host.
        assert_eq!(
            parse_request(b"connect API.example.com:8443 HTTP/1.1\r\n\r\n"),
            Req::Connect { host: "api.example.com".into(), port: 8443 }
        );
        assert!(matches!(parse_request(b"CONNECT api.example.com HTTP/1.1\r\n\r\n"), Req::Bad(_)));
    }

    #[test]
    fn plain_http_is_rewritten_to_origin_form() {
        let raw = b"GET http://example.com/a/b?c=1 HTTP/1.1\r\nHost: example.com\r\nX: 1\r\n\r\n";
        match parse_request(raw) {
            Req::Http { host, port, rewritten } => {
                assert_eq!((host.as_str(), port), ("example.com", 80));
                let s = String::from_utf8(rewritten).unwrap();
                assert!(s.starts_with("GET /a/b?c=1 HTTP/1.1\r\n"), "got: {s}");
                assert!(s.contains("X: 1"), "the rest of the head must survive: {s}");
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn a_body_already_read_is_kept() {
        let raw = b"POST http://example.com/p HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello";
        match parse_request(raw) {
            Req::Http { rewritten, .. } => {
                let s = String::from_utf8(rewritten).unwrap();
                assert!(s.ends_with("\r\n\r\nhello"), "the body must be forwarded too: {s}");
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn origin_form_is_refused_with_an_explanation() {
        // Someone pointing a browser at the proxy port, or an app that ignored
        // the proxy env and connected here by accident.
        assert!(matches!(parse_request(b"GET /health HTTP/1.1\r\n\r\n"), Req::Bad(_)));
        assert!(matches!(parse_request(b"\r\n"), Req::Bad(_)));
        assert!(matches!(parse_request(b"no newline"), Req::Bad(_)));
    }

    #[test]
    fn ipv6_literals_and_userinfo_do_not_smuggle_a_host() {
        // `[::1]` would otherwise be a spelling of loopback that the allowlist
        // never sees as loopback.
        assert!(split_host_port("[::1]:443", 443).is_none());
        assert_eq!(
            split_host_port("user:pw@example.com:443", 80),
            Some(("example.com".into(), 443))
        );
    }

    #[test]
    fn this_machine_is_never_a_permitted_destination() {
        for ip in [
            "127.0.0.1", "127.9.9.9", "0.0.0.0", "169.254.169.254", "224.0.0.1", "255.255.255.255",
        ] {
            assert!(ip_forbidden(ip.parse().unwrap()), "{ip} must be refused");
        }
        for ip in ["::1", "fe80::1", "::ffff:127.0.0.1", "ff02::1"] {
            assert!(ip_forbidden(ip.parse().unwrap()), "{ip} must be refused");
        }
        // Ordinary public and LAN addresses are fine — the allowlist decides
        // those, not this function.
        for ip in ["93.184.216.34", "8.8.8.8", "192.168.1.10", "10.0.0.5"] {
            assert!(!ip_forbidden(ip.parse().unwrap()), "{ip} must be allowed through");
        }
        assert!(!ip_forbidden(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[tokio::test]
    async fn a_blocked_host_gets_403_and_is_reported() {
        let p = HostProxy::spawn("test", vec!["allowed.example".into()]).await.unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", p.port)).await.unwrap();
        c.write_all(b"CONNECT blocked.example:443 HTTP/1.1\r\n\r\n").await.unwrap();
        let mut buf = Vec::new();
        c.read_to_end(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf).to_string();
        assert!(resp.starts_with("HTTP/1.1 403"), "got: {resp}");
        assert!(resp.contains("blocked.example"), "the app's log must name the host: {resp}");

        // …and the UI can see what the app wanted.
        let s = p.stats();
        assert_eq!(s.denied, 1);
        assert_eq!(s.recent_denied, vec!["blocked.example"]);
        assert_eq!(s.allowed, 0);
    }

    #[tokio::test]
    async fn an_allowlisted_name_that_resolves_here_is_still_refused() {
        // DNS rebinding, and the plain case of allowlisting a name that points at
        // 127.0.0.1: the decision is made on the resolved address.
        let p = HostProxy::spawn("test", vec!["localtest.me".into(), "x.invalid".into()])
            .await
            .unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", p.port)).await.unwrap();
        // `localtest.me` resolves to 127.0.0.1 by design; if this machine has no
        // resolver the request is refused for the other reason, which is also a
        // refusal — either way the assertion below holds.
        c.write_all(b"CONNECT localtest.me:443 HTTP/1.1\r\n\r\n").await.unwrap();
        let mut buf = Vec::new();
        c.read_to_end(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf).to_string();
        assert!(resp.starts_with("HTTP/1.1 403"), "got: {resp}");
    }

    #[tokio::test]
    async fn a_non_web_port_is_refused_even_on_an_allowed_host() {
        let p = HostProxy::spawn("test", vec!["allowed.example".into()]).await.unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", p.port)).await.unwrap();
        c.write_all(b"CONNECT allowed.example:22 HTTP/1.1\r\n\r\n").await.unwrap();
        let mut buf = Vec::new();
        c.read_to_end(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf).to_string();
        assert!(resp.starts_with("HTTP/1.1 403"), "got: {resp}");
        assert!(resp.contains("web port"), "got: {resp}");
    }

    #[tokio::test]
    async fn an_allowed_host_is_tunnelled_end_to_end() {
        // The proxy refuses loopback destinations, so the only way to test the
        // happy path locally is to let this one instance permit it.
        let server = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let sport = server.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = server.accept().await.unwrap();
            let mut b = vec![0u8; 64];
            let n = s.read(&mut b).await.unwrap();
            assert_eq!(&b[..n], b"ping");
            s.write_all(b"pong").await.unwrap();
        });

        // `127.0.0.1` rather than `localhost`: the name resolves to ::1 first on
        // this machine, where the test server is not listening.
        let p = HostProxy::spawn_inner("test".into(), vec!["127.0.0.1".into()], true)
            .await
            .unwrap();
        let mut c = TcpStream::connect(("127.0.0.1", p.port)).await.unwrap();
        c.write_all(format!("CONNECT 127.0.0.1:{sport} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut buf = vec![0u8; 128];
        let n = c.read(&mut buf).await.unwrap();
        assert!(
            String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"),
            "got: {}",
            String::from_utf8_lossy(&buf[..n])
        );
        c.write_all(b"ping").await.unwrap();
        let n = c.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"pong", "the tunnel must carry bytes both ways");
        assert_eq!(p.stats().allowed, 1);
    }

    #[tokio::test]
    async fn editing_the_hosts_reaches_a_running_proxy() {
        let p = HostProxy::spawn("test", vec![]).await.unwrap();
        assert!(p.hosts().is_empty());
        p.set_hosts(vec!["late.example".into()]);
        assert_eq!(p.hosts(), vec!["late.example"]);
    }
}
