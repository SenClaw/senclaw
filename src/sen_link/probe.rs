//! "Check this address": what the add-a-peer form shows before the invite
//! code is typed — where a typed IP or name lands, whether a SLink answers
//! there, and why not when none does.
//!
//! Failures are answers, not errors: the caller always gets a JSON object with
//! a machine-readable `code` (`ok`, `bad_address`, `unresolved`, `not_lan`,
//! `unreachable`, `not_slink`) and a sentence for a person.

use std::net::IpAddr;
use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{json, Value};

use super::a2a;
use super::lan::{self, Endpoint, Host};

const TIMEOUT: Duration = Duration::from_secs(5);
/// An Agent Card is a few KB; anything bigger is not one.
pub(super) const MAX_CARD_BYTES: usize = 64 * 1024;

struct Answer<'a> {
    addr: &'a str,
    resolved: &'a [String],
}

impl Answer<'_> {
    fn with(&self, code: &str, reachable: bool, hidden: bool, card: Option<Value>, error: Option<String>) -> Value {
        json!({
            "ok": code == "ok",
            "code": code,
            "addr": self.addr,
            "resolved": self.resolved,
            "reachable": reachable,
            "hidden": hidden,
            "card": card,
            "error": error,
        })
    }

    fn failed(&self, code: &str, error: String) -> Value {
        self.with(code, false, false, None, Some(error))
    }

    fn not_slink(&self, what: String) -> Value {
        self.with("not_slink", true, false, None, Some(format!("{} {what}, but it is not a SLink", self.addr)))
    }
}

/// Read a response body, giving up past `max` bytes.
pub(super) async fn read_limited(mut resp: reqwest::Response, max: usize) -> Option<Vec<u8>> {
    if resp.content_length().is_some_and(|n| n as usize > max) {
        return None;
    }
    let mut out = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        if out.len() + chunk.len() > max {
            return None;
        }
        out.extend_from_slice(&chunk);
    }
    Some(out)
}

fn why_unreachable(ep: &Endpoint, e: &reqwest::Error) -> String {
    if e.is_timeout() {
        format!("no answer from {ep} within {} s — is SLink on there, is the port right, is a firewall in the way?", TIMEOUT.as_secs())
    } else if e.is_connect() {
        format!("{ep} refused or could not be reached ({}) — SLink off, wrong port, or blocked", lan::error_chain(e))
    } else {
        format!("cannot talk to {ep}: {}", lan::error_chain(e))
    }
}

/// Who answers at `ep`: its Agent Card, read and checked, or `None` when
/// nothing answers, the answer is not a SenClaw card, or the card is hidden.
pub(super) async fn identity_at(http: &reqwest::Client, ep: &Endpoint, timeout: Duration) -> Option<a2a::CardIdentity> {
    let resp = http.get(ep.url("/.well-known/agent-card.json")).timeout(timeout).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let card: Value = serde_json::from_slice(&read_limited(resp, MAX_CARD_BYTES).await?).ok()?;
    a2a::card_identity(&card)
}

pub async fn probe(http: &reqwest::Client, input: &str, default_port: u16) -> Value {
    let ep = match lan::parse_endpoint(input, default_port) {
        Ok(ep) => ep,
        Err(e) => return Answer { addr: input.trim(), resolved: &[] }.failed("bad_address", e),
    };
    let addr = ep.to_string();

    let ips: Vec<IpAddr> = match &ep.host {
        Host::Ip(ip) => vec![*ip],
        Host::Name(name) => match tokio::net::lookup_host((name.as_str(), ep.port)).await {
            Ok(found) => found.map(|a| a.ip()).collect(),
            Err(e) => return Answer { addr: &addr, resolved: &[] }.failed("unresolved", format!("{name} cannot be resolved: {e}")),
        },
    };
    let mut resolved: Vec<String> = ips.iter().map(IpAddr::to_string).collect();
    resolved.sort();
    resolved.dedup();
    let a = Answer { addr: &addr, resolved: &resolved };
    if ips.is_empty() {
        return a.failed("unresolved", format!("{addr} resolves to no address"));
    }
    if !ips.iter().copied().any(lan::is_lan_addr) {
        return a.failed(
            "not_lan",
            format!("{addr} points outside the LAN ({}); SLink only talks to private-network addresses", resolved.join(", ")),
        );
    }

    let resp = match http.get(ep.url("/.well-known/agent-card.json")).timeout(TIMEOUT).send().await {
        Ok(r) => r,
        Err(e) => return a.failed("unreachable", why_unreachable(&ep, &e)),
    };
    let status = resp.status();
    if status.is_success() {
        let card: Value = match read_limited(resp, MAX_CARD_BYTES).await {
            Some(b) => serde_json::from_slice(&b).unwrap_or(Value::Null),
            None => Value::Null,
        };
        return match a2a::card_identity(&card) {
            Some(id) => a.with(
                "ok",
                true,
                false,
                Some(json!({ "name": id.name, "version": id.version, "daemonId": id.daemon_id, "pairable": id.pairable })),
                None,
            ),
            None => a.not_slink("answers".into()),
        };
    }
    if status == StatusCode::NOT_FOUND {
        // A hidden SLink has no card but still has a pairing door; an empty
        // body is a malformed pairing request, which only that door answers 422.
        let door = http.post(ep.url("/a2a/pair/begin")).json(&json!({})).timeout(TIMEOUT).send().await;
        if door.is_ok_and(|r| r.status() == StatusCode::UNPROCESSABLE_ENTITY) {
            return a.with("ok", true, true, None, None);
        }
    }
    a.not_slink(format!("answers HTTP {status}"))
}
