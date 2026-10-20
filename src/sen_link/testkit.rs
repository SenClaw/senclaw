//! Fixtures the SLink tests share: a whole daemon on loopback — its own
//! config, peers, kanban board and a LAN listener on a free port — so a test
//! can pair two of them and watch a task cross.

use std::net::SocketAddr;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::store::PeerStore;
use super::{listener, LinkState};

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
