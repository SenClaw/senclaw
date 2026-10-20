//! Outbound A2A calls to a paired peer, over plain HTTP on the LAN. Every
//! connection goes through the LAN-only client (`lan::client`), so a peer
//! whose stored address was edited to point at the internet is refused here.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use super::lan::{self, Endpoint};
use super::server::PairRequest;
use super::store::Peer;
use super::LinkState;

/// Where `peer` listens, checked against the LAN rule. A name is checked when
/// it is resolved, an IP literal here.
fn endpoint_of(peer: &Peer) -> Result<Endpoint> {
    let addr = peer
        .addr
        .as_deref()
        .ok_or_else(|| anyhow!("peer `{}` gave no address; it can only send to us", peer.name))?;
    let ep = lan::parse_endpoint(addr, lan::DEFAULT_PORT).map_err(|e| anyhow!("peer `{}` address `{addr}`: {e}", peer.name))?;
    ep.check().map_err(|e| anyhow!("peer `{}`: {e}", peer.name))?;
    Ok(ep)
}

/// The `result` of a JSON-RPC reply, or the peer's error as `Err`.
fn result_of(peer: &Peer, v: Value) -> Result<Value> {
    if let Some(e) = v.get("error") {
        bail!("peer `{}`: {} ({})", peer.name, e["message"].as_str().unwrap_or("error"), e["code"]);
    }
    Ok(v["result"].clone())
}

/// One JSON-RPC call; returns `result` or the peer's error as `Err`.
pub async fn call(st: &LinkState, peer: &Peer, method: &str, params: Value) -> Result<Value> {
    let token = peer.outbound_token.as_deref().ok_or_else(|| anyhow!("no token for peer `{}`", peer.name))?;
    let ep = endpoint_of(peer)?;
    let body = json!({ "jsonrpc": "2.0", "id": uuid::Uuid::new_v4().to_string(), "method": method, "params": params });
    let resp = st
        .http
        .post(ep.url("/a2a"))
        .bearer_auth(token)
        .header("A2A-Version", super::a2a::PROTOCOL_VERSION)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("reach peer `{}` at {ep}: {}", peer.name, lan::error_chain(&e)))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("peer `{}` answered HTTP {status}", peer.name);
    }
    result_of(peer, resp.json().await.context("peer reply is not JSON")?)
}

/// Redeem the invite `req.code` at `ep`. Returns the peer's
/// `{peerId, name, token, daemonId}`.
pub async fn pair(http: &reqwest::Client, ep: &Endpoint, req: &PairRequest) -> Result<Value> {
    ep.check().map_err(|e| anyhow!(e))?;
    let resp = http
        .post(ep.url("/a2a/pair"))
        .json(req)
        .send()
        .await
        .map_err(|e| anyhow!("reach {ep}: {}", lan::error_chain(&e)))?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("pairing refused (HTTP {status}): {}", v["error"].as_str().unwrap_or("no reason given"));
    }
    if v["token"].as_str().is_none_or(str::is_empty) {
        bail!("pairing reply carried no token");
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn state() -> (tempfile::TempDir, LinkState) {
        let t = tempfile::tempdir().unwrap();
        let st = LinkState::new_at(
            t.path().join("config.json"),
            Arc::new(crate::kanban::db::Db::open(&t.path().join("k.db")).unwrap()),
            super::super::store::PeerStore::new(t.path().join("p.json")),
        );
        (t, st)
    }

    fn peer(addr: Option<&str>) -> Peer {
        Peer {
            id: "x".into(),
            name: "B".into(),
            addr: addr.map(str::to_string),
            outbound_token: Some("t".into()),
            inbound_token_hash: String::new(),
            auto_run: false,
            assignee: None,
            daemon_id: None,
            created_at: String::new(),
        }
    }

    #[tokio::test]
    async fn a_peer_without_an_address_cannot_be_called() {
        let (_t, st) = state();
        let e = call(&st, &peer(None), "GetTask", json!({})).await.unwrap_err();
        assert!(e.to_string().contains("gave no address"), "{e}");
    }

    #[tokio::test]
    async fn a_peer_stored_outside_the_lan_is_not_dialled() {
        let (_t, st) = state();
        for addr in ["8.8.8.8:18791", "http://8.8.8.8:18788", "[2001:4860:4860::8888]:1"] {
            let e = call(&st, &peer(Some(addr)), "GetTask", json!({})).await.unwrap_err();
            assert!(e.to_string().contains("not a LAN address"), "{addr}: {e}");
        }
        let e = call(&st, &peer(Some("ftp://nas")), "GetTask", json!({})).await.unwrap_err();
        assert!(e.to_string().contains("plain http"), "{e}");
        let ep = parse(&pair_target("8.8.4.4"));
        assert!(pair(&st.http, &ep, &PairRequest { code: "X".into(), name: "A".into(), port: None, daemon_id: None, token: None })
            .await
            .unwrap_err()
            .to_string()
            .contains("not a LAN address"));
    }

    fn pair_target(host: &str) -> String {
        format!("{host}:18791")
    }

    fn parse(s: &str) -> Endpoint {
        lan::parse_endpoint(s, lan::DEFAULT_PORT).unwrap()
    }

    #[test]
    fn an_address_from_before_the_rename_is_still_read() {
        let ep = endpoint_of(&peer(Some("http://192.168.1.9:18788"))).unwrap();
        assert_eq!(ep.to_string(), "192.168.1.9:18788");
    }
}
