//! Fixtures the SLink tests share: a whole daemon on loopback — its own
//! config, peers, kanban board and a LAN listener on a free port — so a test
//! can pair two of them and watch a task cross.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::Request;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tower::ServiceExt;

use super::store::{Peer, PeerStore};
use super::{api_router, listener, LinkState};

pub struct Node {
    pub st: Arc<LinkState>,
    pub port: u16,
    server: JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// A SLink that is on, named `name`, listening on `127.0.0.1` at a free port.
/// `extra` overrides keys of its `senLink` settings.
pub async fn node(name: &str, extra: Value) -> Node {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let mut cfg = json!({ "enabled": true, "name": name, "listenHost": "127.0.0.1", "listenPort": addr.port() });
    for (k, v) in extra.as_object().into_iter().flatten() {
        cfg[k] = v.clone();
    }
    let path = dir.path().join("config.json");
    std::fs::write(&path, json!({ "senLink": cfg }).to_string()).unwrap();
    let st = Arc::new(LinkState::new_at(
        path,
        Arc::new(crate::kanban::db::Db::open(&dir.path().join("kanban.db")).unwrap()),
        PeerStore::new(dir.path().join("sen-link/peers.json")),
    ));
    st.set_listen(Some(addr), None);
    let served = Arc::clone(&st);
    let server = tokio::spawn(async move {
        let _ = listener::serve_on(listener, served).await;
    });
    Node { st, port: addr.port(), server, _dir: dir }
}

/// A port nothing listens on right now.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Pair `a` with `b` the way a person does: `b` makes an invite, `a` pairs with
/// `b` at `addr` (default: `b`'s own address). Returns each side's record of the other.
pub async fn paired_at(a: &Node, b: &Node, addr: Option<String>) -> (Peer, Peer) {
    let (code, _) = b.st.store.create_invite(crate::kanban::api::now()).unwrap();
    let body = json!({ "addr": addr.unwrap_or_else(|| format!("127.0.0.1:{}", b.port)), "code": code });
    let req = Request::builder().method("POST").uri("/api/sen-link/peers/pair").header("content-type", "application/json").body(Body::from(body.to_string())).unwrap();
    let resp = api_router(Arc::clone(&a.st)).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200, "pairing failed");
    let at_a = a.st.store.by_daemon_id(b.st.store.daemon_id()).expect("a knows b");
    let at_b = b.st.store.by_daemon_id(a.st.store.daemon_id()).expect("b knows a");
    (at_a, at_b)
}

pub async fn paired(a: &Node, b: &Node) -> (Peer, Peer) {
    paired_at(a, b, None).await
}

/// A TCP relay in front of `target` that keeps every byte that crosses it, in
/// both directions — what a machine sniffing the network would see.
pub struct Tap {
    pub port: u16,
    pub seen: Arc<Mutex<Vec<u8>>>,
    task: JoinHandle<()>,
}

impl Drop for Tap {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Tap {
    pub fn bytes(&self) -> Vec<u8> {
        self.seen.lock().unwrap().clone()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes()).into_owned()
    }
}

pub async fn tap(target: u16) -> Tap {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut client, _)) = listener.accept().await else { return };
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let Ok(mut server) = TcpStream::connect(("127.0.0.1", target)).await else { return };
                let (mut cr, mut cw) = client.split();
                let (mut sr, mut sw) = server.split();
                let up = async {
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = cr.read(&mut buf).await {
                        if n == 0 || sw.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                        log.lock().unwrap().extend_from_slice(&buf[..n]);
                    }
                };
                let down = async {
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = sr.read(&mut buf).await {
                        if n == 0 || cw.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                        log.lock().unwrap().extend_from_slice(&buf[..n]);
                    }
                };
                tokio::select! { _ = up => {}, _ = down => {} }
            });
        }
    });
    Tap { port, seen, task }
}
