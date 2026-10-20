//! Outbound A2A calls to a paired peer, over plain HTTP on the LAN — with
//! everything that matters sealed under the pair key (`seal.rs`). Every
//! connection goes through the LAN-only client (`lan::client`), so a peer
//! whose stored address was edited to point at the internet is refused here.

use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use serde_json::{json, Value};

use super::lan::{self, Endpoint, Host};
use super::pairing::{self, Accept, Initiator, Offer};
use super::seal::{self, Key};
use super::server::FROM_HEADER;
use super::store::Peer;
use super::LinkState;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

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

/// The pair key and the peer's id, which a sealed call needs. A peer paired
/// before the conversation was sealed has neither.
fn secrets_of(peer: &Peer) -> Result<(Key, &str)> {
    match (peer.key.as_deref(), peer.daemon_id.as_deref()) {
        (Some(key), Some(id)) => Ok((seal::decode_key(key)?, id)),
        _ => bail!("peer `{}` was paired by an older version, before calls were sealed; pair it again", peer.name),
    }
}

/// The `result` of a JSON-RPC reply, or the peer's error as `Err`.
fn result_of(peer: &Peer, v: Value) -> Result<Value> {
    if let Some(e) = v.get("error") {
        bail!("peer `{}`: {} ({})", peer.name, e["message"].as_str().unwrap_or("error"), e["code"]);
    }
    Ok(v["result"].clone())
}

/// Why a call to one address failed.
enum Failed {
    /// The connection could not be made: the address may have changed.
    Unreachable(anyhow::Error),
    /// Anything after that: the peer answered, or the request went out.
    Other(anyhow::Error),
}

/// One sealed JSON-RPC call to `ep`.
async fn call_at(st: &LinkState, peer: &Peer, ep: &Endpoint, rpc: &Value) -> Result<Value, Failed> {
    let other = |e: anyhow::Error| Failed::Other(e);
    let token = peer.outbound_token.as_deref().ok_or_else(|| other(anyhow!("no token for peer `{}`", peer.name)))?;
    let (key, peer_id) = secrets_of(peer).map_err(other)?;
    let own = st.store.daemon_id();
    let id = uuid::Uuid::new_v4().to_string();
    let request = seal::Request { id: id.clone(), ts: chrono::Utc::now().timestamp_millis(), token: token.to_string(), rpc: rpc.clone() };
    let sealed = seal::seal_json(&key, &seal::aad("req", own, peer_id), &request).map_err(other)?;
    let resp = st
        .http
        .post(ep.url("/a2a"))
        .header(FROM_HEADER, own)
        .header("A2A-Version", super::a2a::PROTOCOL_VERSION)
        .json(&sealed)
        .send()
        .await
        .map_err(|e| {
            let err = anyhow!("reach peer `{}` at {ep}: {}", peer.name, lan::error_chain(&e));
            // Only a refused or failed connect says the request never left: a
            // timeout after it may have been delivered, and must not be retried elsewhere.
            if e.is_connect() { Failed::Unreachable(err) } else { Failed::Other(err) }
        })?;
    let status = resp.status();
    if !status.is_success() {
        let why = if status == reqwest::StatusCode::UNAUTHORIZED {
            " (it did not accept the request: the pairing may be gone on its side, or the two clocks differ by more than two minutes)"
        } else {
            ""
        };
        return Err(Failed::Other(anyhow!("peer `{}` answered HTTP {status}{why}", peer.name)));
    }
    let sealed: seal::Sealed = resp.json().await.map_err(|e| Failed::Other(anyhow!("peer reply is not an envelope: {e}")))?;
    let reply: seal::Reply = seal::open_json(&key, &seal::aad("resp", peer_id, own), &sealed)
        .map_err(|_| Failed::Other(anyhow!("peer `{}` answered with something that is not sealed under our key", peer.name)))?;
    if reply.id != id {
        return Err(Failed::Other(anyhow!("peer `{}` answered another request", peer.name)));
    }
    result_of(peer, reply.rpc).map_err(other)
}

/// One JSON-RPC call; returns `result` or the peer's error as `Err`.
///
/// When the peer's address is an IP that no longer accepts connections and
/// discovery has seen the same daemon elsewhere (DHCP gave it a new address),
/// the call is repeated there and the stored address follows. A name is left to
/// DNS. Following an announcement is safe because a request is sealed: what
/// answers at the new address either holds the pair key or learns nothing.
pub async fn call(st: &LinkState, peer: &Peer, method: &str, params: Value) -> Result<Value> {
    let primary = endpoint_of(peer)?;
    let rpc = json!({ "jsonrpc": "2.0", "id": uuid::Uuid::new_v4().to_string(), "method": method, "params": params });
    match call_at(st, peer, &primary, &rpc).await {
        Ok(v) => Ok(v),
        Err(Failed::Other(e)) => Err(e),
        Err(Failed::Unreachable(e)) => {
            if let (Host::Ip(_), Some(id)) = (&primary.host, peer.daemon_id.as_deref()) {
                for ep in st.discovery.find(id).map(|d| d.endpoints()).unwrap_or_default() {
                    if ep == primary || ep.check().is_err() {
                        continue;
                    }
                    if let Ok(v) = call_at(st, peer, &ep, &rpc).await {
                        let _ = st.store.update(&peer.id, |p| p.addr = Some(ep.to_string()));
                        tracing::info!(peer = %peer.id, from = %primary, to = %ep, "[slink] the peer moved; its address was updated");
                        return Ok(v);
                    }
                }
            }
            Err(e)
        }
    }
}

/// Why pairing did not happen.
#[derive(Debug)]
pub struct PairError {
    /// Something answered at the address (and said no); false when nothing did.
    pub reached: bool,
    pub error: anyhow::Error,
}

impl std::fmt::Display for PairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.error)
    }
}

/// What a finished pairing leaves with A.
#[derive(Debug)]
pub struct Paired {
    pub peer_id: String,
    pub name: String,
    pub daemon_id: String,
    /// The token B presents when it calls A.
    pub token: String,
    /// The pair key, the same one B holds.
    pub key: Key,
}

/// Pair with the daemon at `ep` using its invite `code`: the two steps of the
/// handshake, with `offer` — who we are, and the token B will call us with —
/// sealed under the key the code made. Neither the code nor the token is ever
/// on the wire in the clear.
pub async fn pair(http: &reqwest::Client, ep: &Endpoint, code: &str, offer: &Offer) -> Result<Paired, PairError> {
    let unreached = |error: anyhow::Error| PairError { reached: false, error };
    let refused = |error: anyhow::Error| PairError { reached: true, error };
    ep.check().map_err(|e| unreached(anyhow!(e)))?;

    // Send one step; the body is JSON on success and `{"error": …}` otherwise.
    async fn step(http: &reqwest::Client, ep: &Endpoint, path: &str, body: &impl serde::Serialize) -> Result<(reqwest::StatusCode, Value), anyhow::Error> {
        let resp = http.post(ep.url(path)).json(body).send().await.map_err(|e| anyhow!("reach {ep}: {}", lan::error_chain(&e)))?;
        let status = resp.status();
        Ok((status, resp.json().await.unwrap_or(Value::Null)))
    }
    let denied = |status: reqwest::StatusCode, v: &Value| anyhow!("pairing refused (HTTP {status}): {}", v["error"].as_str().unwrap_or("no reason given"));

    let (initiator, msg_a) = Initiator::start(code);
    let (status, v) = step(http, ep, "/a2a/pair/begin", &pairing::Begin { msg: B64.encode(msg_a) }).await.map_err(unreached)?;
    if !status.is_success() {
        return Err(refused(denied(status, &v)));
    }
    let begun: pairing::BeginReply = serde_json::from_value(v).map_err(|_| refused(anyhow!("the other side sent a malformed handshake")))?;
    if begun.sid.is_empty() || begun.sid.len() > 64 {
        return Err(refused(anyhow!("the other side sent a malformed handshake")));
    }
    let msg_b = B64.decode(&begun.msg).map_err(|_| refused(anyhow!("the other side sent a malformed handshake")))?;
    let keys = initiator.finish(&msg_b).map_err(refused)?;

    let sealed = seal::seal_json(&keys.a_to_b, &pairing::offer_aad(&begun.sid), offer).map_err(|e| unreached(e))?;
    let (status, v) = step(http, ep, "/a2a/pair/finish", &pairing::Finish { sid: begun.sid.clone(), sealed }).await.map_err(unreached)?;
    if !status.is_success() {
        return Err(refused(denied(status, &v)));
    }
    let reply: seal::Sealed = serde_json::from_value(v).map_err(|_| refused(anyhow!("the other side sent a malformed answer")))?;
    let accept: Accept = seal::open_json(&keys.b_to_a, &pairing::accept_aad(&begun.sid), &reply)
        .map_err(|_| refused(anyhow!("the other side could not prove it knew the invite code; nothing was paired")))?;
    let daemon_id = super::store::clean_daemon_id(&accept.daemon_id).ok_or_else(|| refused(anyhow!("the other side sent an id that is not valid")))?;
    if accept.token.is_empty() {
        return Err(refused(anyhow!("pairing reply carried no token")));
    }
    Ok(Paired { peer_id: accept.peer_id, name: accept.name, daemon_id, token: accept.token, key: keys.pair })
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
            key: None,
            knowledge: Default::default(),
            created_at: String::new(),
        }
    }

    #[tokio::test]
    async fn a_peer_paired_before_calls_were_sealed_is_asked_to_pair_again() {
        let (_t, st) = state();
        let e = call(&st, &peer(Some("192.168.1.9:18791")), "GetTask", json!({})).await.unwrap_err();
        assert!(e.to_string().contains("pair it again"), "{e}");
        let mut half = peer(Some("192.168.1.9:18791"));
        half.daemon_id = Some("6f1c-0a".into());
        assert!(call(&st, &half, "GetTask", json!({})).await.unwrap_err().to_string().contains("pair it again"), "an id without a key is no better");
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
        let ep = parse("8.8.4.4:18791");
        let offer = Offer { name: "A".into(), port: None, daemon_id: None, token: "slk_t".into() };
        let e = pair(&st.http, &ep, "K7MQ2XRP", &offer).await.unwrap_err();
        assert!(!e.reached && e.to_string().contains("not a LAN address"), "{e}");
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
