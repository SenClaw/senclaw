//! The LAN listener: the A2A surface on a port of its own, and nothing else.
//!
//! Before this, receiving a task meant binding the whole UI server beyond
//! loopback — the daemon's `/api`, its MCP bridge and its WebSocket gateway
//! went along. This socket carries four routes (`GET /.well-known/agent-card.json`,
//! `POST /a2a`, `POST /a2a/pair/begin`, `POST /a2a/pair/finish`) and refuses
//! any connection whose source is not a LAN address before a byte of its
//! request is read.
//!
//! A supervisor opens it while `senLink.enabled`, moves it when the host or
//! port changes, closes it when SLink is turned off, and retries a bind that
//! failed (port in use) every few seconds.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::extract::{ConnectInfo, DefaultBodyLimit, Request};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{lan, server, LinkState};

/// A2A messages are text and a few fields; a megabyte is generous.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// How often the supervisor looks again when nobody woke it (a bind that
/// failed, or `config.json` edited by hand).
const RECHECK: Duration = Duration::from_secs(10);

/// What the listener is doing, for `GET /api/sen-link/status`.
#[derive(Debug, Default, Clone)]
pub struct ListenStatus {
    pub addr: Option<SocketAddr>,
    pub error: Option<String>,
}

/// The three routes, behind the LAN-source check.
pub fn router(st: Arc<LinkState>) -> Router {
    Router::new()
        .route("/.well-known/agent-card.json", get(server::agent_card))
        .route("/a2a", post(server::rpc))
        .route("/a2a/pair/begin", post(server::pair_begin))
        .route("/a2a/pair/finish", post(server::pair_finish))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn(lan_only))
        .with_state(st)
}

/// Refuse a connection from outside the LAN. A router that forwards a port
/// shows its public side as the source, so this also closes a forwarded port.
async fn lan_only(ConnectInfo(from): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    if !lan::is_lan_addr(from.ip()) {
        tracing::warn!(from = %from.ip(), "[slink] refused a connection from outside the LAN");
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(req).await
}

/// Serve the LAN router on an already-bound socket until `stop` is cancelled
/// (requests in flight finish, idle keep-alive connections close at once) or
/// serving fails.
pub async fn serve_until(listener: TcpListener, st: Arc<LinkState>, stop: CancellationToken) -> std::io::Result<()> {
    axum::serve(listener, router(st).into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move { stop.cancelled().await })
        .await
}

/// Serve until the task is dropped.
pub async fn serve_on(listener: TcpListener, st: Arc<LinkState>) -> std::io::Result<()> {
    serve_until(listener, st, CancellationToken::new()).await
}

/// How long a closing listener waits for requests in flight.
const GRACE: Duration = Duration::from_secs(2);

struct Running {
    key: (IpAddr, u16),
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl Running {
    /// Stop accepting, let requests in flight finish and drop idle connections.
    /// Without this a peer's pooled connection would keep answering — 404, once
    /// SLink is off — long after the socket was supposed to be closed.
    async fn shutdown(mut self) {
        self.stop.cancel();
        let _ = tokio::time::timeout(GRACE, &mut self.task).await;
    }
}

impl Drop for Running {
    /// Whatever is still running when this goes away is cut off.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Start the supervisor. Abort the returned handle to stop it and close the socket.
pub fn start(st: &Arc<LinkState>) -> JoinHandle<()> {
    tokio::spawn(supervise(Arc::downgrade(st), Arc::clone(&st.wake)))
}

async fn supervise(weak: Weak<LinkState>, wake: Arc<Notify>) {
    let mut running: Option<Running> = None;
    loop {
        let Some(st) = weak.upgrade() else { break };
        let s = st.settings();
        let want = s
            .enabled
            .then(|| s.listen_host.trim().parse::<IpAddr>().map(|ip| (ip, s.listen_port)))
            .transpose();

        match want {
            Err(_) => {
                if let Some(r) = running.take() {
                    r.shutdown().await;
                }
                st.set_listen(None, Some(format!("listenHost `{}` is not an IP address", s.listen_host)));
            }
            Ok(None) => {
                if let Some(r) = running.take() {
                    r.shutdown().await;
                }
                st.set_listen(None, None);
            }
            Ok(Some(key)) => {
                if running.as_ref().is_some_and(|r| r.key != key || r.task.is_finished()) {
                    if let Some(r) = running.take() {
                        r.shutdown().await;
                    }
                }
                if running.is_none() {
                    match TcpListener::bind(SocketAddr::new(key.0, key.1)).await {
                        Ok(listener) => {
                            let bound = listener.local_addr().ok();
                            let served = Arc::clone(&st);
                            let stop = CancellationToken::new();
                            let stopped = stop.clone();
                            let task = tokio::spawn(async move {
                                if let Err(e) = serve_until(listener, served, stopped).await {
                                    tracing::warn!("[slink] the LAN listener stopped: {e}");
                                }
                            });
                            tracing::info!(addr = ?bound, "[slink] listening on the LAN");
                            st.set_listen(bound, None);
                            running = Some(Running { key, stop, task });
                        }
                        Err(e) => {
                            let hint = if e.kind() == std::io::ErrorKind::AddrInUse { " (port already in use)" } else { "" };
                            tracing::warn!("[slink] cannot listen on {}:{}: {e}", key.0, key.1);
                            st.set_listen(None, Some(format!("cannot listen on {}:{}: {e}{hint}", key.0, key.1)));
                        }
                    }
                }
            }
        }
        drop(st);
        tokio::select! {
            _ = wake.notified() => {}
            _ = tokio::time::sleep(RECHECK) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::Request as HttpRequest;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::*;
    use crate::sen_link::store::PeerStore;

    fn link_state(dir: &std::path::Path, config: Value) -> Arc<LinkState> {
        let cfg = dir.join("config.json");
        std::fs::write(&cfg, json!({ "senLink": config }).to_string()).unwrap();
        Arc::new(LinkState::new_at(
            cfg,
            Arc::new(crate::kanban::db::Db::open(&dir.join("kanban.db")).unwrap()),
            PeerStore::new(dir.join("peers.json")),
        ))
    }

    async fn ask(app: &Router, from: &str, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = HttpRequest::builder().method(method).uri(path).header("content-type", "application/json");
        req = req.extension(ConnectInfo(from.parse::<SocketAddr>().unwrap()));
        let req = req.body(body.map_or_else(Body::empty, |b| Body::from(b.to_string()))).unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn the_lan_socket_serves_the_a2a_routes_and_nothing_else() {
        let t = tempfile::tempdir().unwrap();
        let app = router(link_state(t.path(), json!({ "enabled": true, "name": "Mac" })));
        let lan = "192.168.1.5:50000";

        let (s, card) = ask(&app, lan, "GET", "/.well-known/agent-card.json", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(card["name"], "Mac");
        assert_eq!(ask(&app, lan, "POST", "/a2a", Some(json!({}))).await.0, StatusCode::UNAUTHORIZED);
        assert_eq!(ask(&app, lan, "POST", "/a2a/pair/begin", Some(json!({}))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(ask(&app, lan, "POST", "/a2a/pair/finish", Some(json!({}))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        // The old one-step pairing is gone, and so is anything else under /a2a.
        for path in ["/a2a/pair", "/a2a/other"] {
            assert_eq!(ask(&app, lan, "POST", path, Some(json!({}))).await.0, StatusCode::NOT_FOUND, "{path}");
        }

        // None of the daemon's other surfaces exist on this port.
        for path in ["/api/sen-link/status", "/api/sen-link/settings", "/api/kanban/boards", "/api/auth/status", "/mcp/core", "/"] {
            assert_eq!(ask(&app, lan, "GET", path, None).await.0, StatusCode::NOT_FOUND, "{path}");
        }
        assert_eq!(ask(&app, lan, "GET", "/a2a", None).await.0, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn a_connection_from_outside_the_lan_is_refused_before_anything_is_read() {
        let t = tempfile::tempdir().unwrap();
        let app = router(link_state(t.path(), json!({ "enabled": true })));
        for from in ["8.8.8.8:5000", "203.0.113.9:5000", "[2001:4860:4860::8888]:5000", "[::ffff:8.8.8.8]:5000"] {
            for (m, p) in [("GET", "/.well-known/agent-card.json"), ("POST", "/a2a"), ("POST", "/a2a/pair/begin"), ("POST", "/a2a/pair/finish"), ("GET", "/nothing")] {
                assert_eq!(ask(&app, from, m, p, Some(json!({}))).await.0, StatusCode::FORBIDDEN, "{from} {m} {p}");
            }
        }
        // Tailscale, link-local and loopback sources are LAN.
        for from in ["100.101.102.103:5000", "169.254.1.1:5000", "127.0.0.1:5000", "[fd7a:115c:a1e0::1]:5000"] {
            assert_eq!(ask(&app, from, "GET", "/.well-known/agent-card.json", None).await.0, StatusCode::OK, "{from}");
        }
    }

    #[tokio::test]
    async fn a_daemon_that_is_off_or_hidden_shows_no_card_but_a_hidden_one_still_pairs() {
        let t = tempfile::tempdir().unwrap();
        let lan = "10.0.0.2:5000";

        let off = router(link_state(t.path(), json!({ "enabled": false })));
        assert_eq!(ask(&off, lan, "GET", "/.well-known/agent-card.json", None).await.0, StatusCode::NOT_FOUND);
        assert_eq!(ask(&off, lan, "POST", "/a2a/pair/begin", Some(json!({ "msg": "AA==" }))).await.0, StatusCode::NOT_FOUND);
        assert_eq!(ask(&off, lan, "POST", "/a2a", Some(json!({}))).await.0, StatusCode::NOT_FOUND);

        let t2 = tempfile::tempdir().unwrap();
        let hidden = router(link_state(t2.path(), json!({ "enabled": true, "discoverable": false })));
        assert_eq!(ask(&hidden, lan, "GET", "/.well-known/agent-card.json", None).await.0, StatusCode::NOT_FOUND);
        assert_eq!(
            ask(&hidden, lan, "POST", "/a2a/pair/begin", Some(json!({ "msg": "AA==" }))).await.0,
            StatusCode::FORBIDDEN,
            "the pairing door is open (and has no invite to answer for)"
        );
    }

    #[tokio::test]
    async fn the_card_names_the_address_the_caller_used() {
        let t = tempfile::tempdir().unwrap();
        let app = router(link_state(t.path(), json!({ "enabled": true })));
        let mut req = HttpRequest::builder().uri("/.well-known/agent-card.json").header("host", "mac-mini.local:18791");
        req = req.extension(ConnectInfo("192.168.1.5:1".parse::<SocketAddr>().unwrap()));
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let card: Value = serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(card["supportedInterfaces"][0]["url"], "http://mac-mini.local:18791/a2a");
    }

    /// The supervisor opens the socket when SLink is turned on, moves with the
    /// settings, and closes it when SLink is turned off.
    #[tokio::test]
    async fn the_listener_follows_the_settings() {
        let t = tempfile::tempdir().unwrap();
        let free = || {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let (p1, p2) = (free(), free());
        let st = link_state(t.path(), json!({ "enabled": false, "listenHost": "127.0.0.1", "listenPort": p1 }));
        let supervisor = start(&st);
        let write = |v: Value| std::fs::write(st.config_path(), json!({ "senLink": v }).to_string()).unwrap();
        let reachable = |port: u16| std::net::TcpStream::connect_timeout(&format!("127.0.0.1:{port}").parse().unwrap(), Duration::from_millis(300)).is_ok();
        let settle = || tokio::time::sleep(Duration::from_millis(300));

        settle().await;
        assert!(!reachable(p1), "off: nothing listens");
        assert!(st.listen_status().addr.is_none());

        write(json!({ "enabled": true, "listenHost": "127.0.0.1", "listenPort": p1 }));
        st.wake.notify_one();
        settle().await;
        assert!(reachable(p1), "on: the socket is open");
        assert_eq!(st.listen_status().addr.map(|a| a.port()), Some(p1));

        write(json!({ "enabled": true, "listenHost": "127.0.0.1", "listenPort": p2 }));
        st.wake.notify_one();
        settle().await;
        assert!(reachable(p2) && !reachable(p1), "a new port moves the socket");

        // A client holding a keep-alive connection open, as a peer's pooled client does.
        let pooled = reqwest::Client::builder().no_proxy().build().unwrap();
        let card = format!("http://127.0.0.1:{p2}/.well-known/agent-card.json");
        assert_eq!(pooled.get(&card).send().await.unwrap().status(), 200);

        write(json!({ "enabled": false, "listenHost": "127.0.0.1", "listenPort": p2 }));
        st.wake.notify_one();
        settle().await;
        assert!(!reachable(p2), "off again: closed");
        assert!(st.listen_status().addr.is_none());
        let after = pooled.get(&card).send().await;
        assert!(after.is_err(), "the pooled connection was closed with the listener, not left answering 404: {after:?}");

        // A port someone else holds is reported, not fatal.
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = held.local_addr().unwrap().port();
        write(json!({ "enabled": true, "listenHost": "127.0.0.1", "listenPort": busy }));
        st.wake.notify_one();
        settle().await;
        let err = st.listen_status().error.unwrap();
        assert!(err.contains("cannot listen") && err.contains("in use"), "{err}");

        write(json!({ "enabled": true, "listenHost": "mac-mini.local", "listenPort": p1 }));
        st.wake.notify_one();
        settle().await;
        assert!(st.listen_status().error.unwrap().contains("not an IP address"));

        supervisor.abort();
    }
}
