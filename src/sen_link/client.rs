//! Outbound A2A calls to a paired peer — over HTTP when it has a URL, else
//! through its hub channel.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use super::store::Peer;
use super::LinkState;

fn endpoint(base: &str) -> String {
    format!("{}/a2a", base.trim_end_matches('/'))
}

/// The `result` of a JSON-RPC reply, or the peer's error as `Err`.
fn result_of(peer: &Peer, v: Value) -> Result<Value> {
    if let Some(e) = v.get("error") {
        bail!("peer `{}`: {} ({})", peer.name, e["message"].as_str().unwrap_or("error"), e["code"]);
    }
    Ok(v["result"].clone())
}

/// One JSON-RPC call; returns `result` or the peer's error as `Err`.
pub async fn call(st: &Arc<LinkState>, peer: &Peer, method: &str, params: Value) -> Result<Value> {
    let token = peer.outbound_token.as_deref().ok_or_else(|| anyhow!("no token for peer `{}`", peer.name))?;
    let body = json!({ "jsonrpc": "2.0", "id": uuid::Uuid::new_v4().to_string(), "method": method, "params": params });
    if let Some(url) = peer.url.as_deref() {
        let resp = st
            .http
            .post(endpoint(url))
            .bearer_auth(token)
            .header("A2A-Version", super::a2a::PROTOCOL_VERSION)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("reach peer `{}`", peer.name))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("peer `{}` answered HTTP {status}", peer.name);
        }
        return result_of(peer, resp.json().await.context("peer reply is not JSON")?);
    }
    if let Some(relay) = &peer.relay {
        let link = super::relay::ensure(st, relay)?;
        let (status, v) = link.request("/a2a", Some(token), body).await?;
        if status != 200 {
            bail!("peer `{}` answered {status} over the hub: {}", peer.name, v["error"].as_str().unwrap_or(""));
        }
        return result_of(peer, v);
    }
    bail!("peer `{}` gave no URL and no hub channel; it can only send to us", peer.name)
}

/// Redeem `code` at `base_url`. Returns the peer's `{peerId, name, token, url}`.
pub async fn pair(http: &reqwest::Client, base_url: &str, code: &str, my_name: &str, my_url: Option<&str>, my_token: &str) -> Result<Value> {
    let resp = http
        .post(format!("{}/a2a/pair", base_url.trim_end_matches('/')))
        .json(&json!({ "code": code, "name": my_name, "url": my_url, "token": my_token }))
        .send()
        .await
        .with_context(|| format!("reach {base_url}"))?;
    let status = resp.status();
    let v: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("pairing refused (HTTP {status}): {}", v["error"].as_str().unwrap_or("no reason given"));
    }
    check_pair_reply(v)
}

/// Redeem an invite through its hub channel (`senlink:` connection string).
pub async fn pair_via_hub(st: &Arc<LinkState>, connect: &super::relay::Connect, my_name: &str, my_url: Option<&str>, my_token: &str) -> Result<Value> {
    let link = super::relay::ensure(st, &connect.relay())?;
    let (status, v) = link
        .request("/a2a/pair", None, json!({ "code": connect.code, "name": my_name, "url": my_url, "token": my_token }))
        .await?;
    if status != 200 {
        bail!("pairing refused ({status}): {}", v["error"].as_str().unwrap_or("no reason given"));
    }
    check_pair_reply(v)
}

fn check_pair_reply(v: Value) -> Result<Value> {
    if v["token"].as_str().is_none_or(str::is_empty) {
        bail!("pairing reply carried no token");
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_tolerates_a_trailing_slash() {
        assert_eq!(endpoint("http://h:1/"), "http://h:1/a2a");
    }

    #[tokio::test]
    async fn a_peer_without_url_or_hub_cannot_be_called() {
        let t = tempfile::tempdir().unwrap();
        let st = Arc::new(LinkState::new_at(
            t.path().join("config.json"),
            Arc::new(crate::kanban::db::Db::open(&t.path().join("k.db")).unwrap()),
            super::super::store::PeerStore::new(t.path().join("p.json")),
        ));
        let p = Peer {
            id: "x".into(),
            name: "B".into(),
            url: None,
            outbound_token: Some("t".into()),
            inbound_token_hash: String::new(),
            auto_run: false,
            assignee: None,
            relay: None,
            created_at: String::new(),
        };
        let e = call(&st, &p, "GetTask", json!({})).await.unwrap_err();
        assert!(e.to_string().contains("no URL and no hub channel"));
    }
}
