//! Paired peers and pending invites, in `~/.senclaw/sen-link/peers.json`
//! (0600). Outbound tokens (what we present to a peer) are kept as given;
//! inbound tokens (what a peer presents to us) only as SHA-256.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Invite lifetime.
pub const INVITE_TTL_SECS: i64 = 10 * 60;
/// Unambiguous alphabet for spoken/typed invite codes.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// A dedicated hub channel shared by exactly two daemons (the hub admits at
/// most two connections per channel). Frames on it are AES-GCM encrypted with
/// `key`, which only the two peers hold — the hub relays ciphertext.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RelayInfo {
    pub hub_url: String,
    pub channel_id: String,
    pub access_token: String,
    /// Base64 of the 32-byte channel key.
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Peer {
    pub id: String,
    pub name: String,
    /// The peer's base URL (`http://host:18788`); `None` when it gave none.
    pub url: Option<String>,
    /// Token we send to the peer.
    pub outbound_token: Option<String>,
    /// SHA-256 of the token the peer sends us.
    pub inbound_token_hash: String,
    /// Incoming tasks go straight to Ready (auto-dispatched) instead of Triage.
    #[serde(default)]
    pub auto_run: bool,
    /// Assignee for this peer's incoming tasks.
    #[serde(default)]
    pub assignee: Option<String>,
    /// Reached through a hub channel instead of (or when there is no) `url`.
    #[serde(default)]
    pub relay: Option<RelayInfo>,
    pub created_at: String,
}

/// A peer as the REST API shows it: no secrets.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerView {
    pub id: String,
    pub name: String,
    pub url: Option<String>,
    /// Reached through the hub relay.
    pub via_relay: bool,
    pub can_send: bool,
    pub auto_run: bool,
    pub assignee: Option<String>,
    pub created_at: String,
}

impl From<&Peer> for PeerView {
    fn from(p: &Peer) -> Self {
        Self {
            id: p.id.clone(),
            name: p.name.clone(),
            url: p.url.clone(),
            via_relay: p.relay.is_some(),
            can_send: (p.url.is_some() || p.relay.is_some()) && p.outbound_token.is_some(),
            auto_run: p.auto_run,
            assignee: p.assignee.clone(),
            created_at: p.created_at.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Invite {
    code_hash: String,
    pub expires_at: i64,
    /// The hub channel minted for this invite; the pairing peer keeps it.
    #[serde(default)]
    pub relay: Option<RelayInfo>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Data {
    #[serde(default)]
    peers: Vec<Peer>,
    #[serde(default)]
    invites: Vec<Invite>,
}

pub struct PeerStore {
    path: PathBuf,
    lock: Mutex<()>,
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

pub fn random_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    format!("slk_{}", hex::encode(b))
}

fn random_code() -> String {
    use rand::Rng;
    let mut r = rand::thread_rng();
    (0..8).map(|_| CODE_ALPHABET[r.gen_range(0..CODE_ALPHABET.len())] as char).collect()
}

fn normalize_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_uppercase()
}

impl PeerStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path, lock: Mutex::new(()) }
    }

    pub fn default_path() -> PathBuf {
        crate::util::paths::senclaw_home().join("sen-link").join("peers.json")
    }

    fn read(&self) -> Data {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn write(&self, d: &Data) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        std::fs::write(&self.path, serde_json::to_vec_pretty(d)?).with_context(|| format!("write {}", self.path.display()))?;
        crate::util::file_perms::restrict_best_effort(&self.path);
        Ok(())
    }

    fn edit<T>(&self, f: impl FnOnce(&mut Data) -> T) -> Result<T> {
        let _g = self.lock.lock().unwrap();
        let mut d = self.read();
        let out = f(&mut d);
        self.write(&d)?;
        Ok(out)
    }

    pub fn peers(&self) -> Vec<Peer> {
        let _g = self.lock.lock().unwrap();
        self.read().peers
    }

    pub fn get(&self, id: &str) -> Option<Peer> {
        self.peers().into_iter().find(|p| p.id == id)
    }

    /// The peer presenting `token`.
    pub fn by_inbound_token(&self, token: &str) -> Option<Peer> {
        let h = sha256_hex(token);
        self.peers()
            .into_iter()
            .find(|p| crate::gateway::ui_server::auth::ct_eq(&p.inbound_token_hash, &h))
    }

    /// A one-time pairing code, valid for [`INVITE_TTL_SECS`], optionally
    /// bound to a hub channel minted for it.
    pub fn create_invite(&self, now: i64, relay: Option<RelayInfo>) -> Result<(String, i64)> {
        let code = random_code();
        let expires_at = now + INVITE_TTL_SECS;
        let h = sha256_hex(&code);
        self.edit(|d| {
            d.invites.retain(|i| i.expires_at > now);
            d.invites.push(Invite { code_hash: h, expires_at, relay });
        })?;
        Ok((code, expires_at))
    }

    /// Consume an invite, returning it when `code` was live. A miss writes
    /// nothing, so unauthenticated guesses cannot churn the file. With
    /// `channel`, only the invite minted for that hub channel matches; without
    /// it (HTTP pairing) only invites that have no hub channel do — the code
    /// alone must not claim a channel meant for whoever holds its string.
    pub fn redeem_invite(&self, code: &str, now: i64, channel: Option<&str>) -> Result<Option<Invite>> {
        let h = sha256_hex(&normalize_code(code));
        let _g = self.lock.lock().unwrap();
        let mut d = self.read();
        let matches = |i: &Invite| {
            i.expires_at > now
                && i.code_hash == h
                && match channel {
                    Some(c) => i.relay.as_ref().is_some_and(|r| r.channel_id == c),
                    None => i.relay.is_none(),
                }
        };
        let Some(found) = d.invites.iter().find(|i| matches(i)).cloned() else {
            return Ok(None);
        };
        d.invites.retain(|i| i.expires_at > now && i.code_hash != h);
        self.write(&d)?;
        Ok(Some(found))
    }

    /// Hub channels still worth listening on: live invites and relay peers.
    pub fn relay_channels(&self, now: i64) -> Vec<RelayInfo> {
        let _g = self.lock.lock().unwrap();
        let d = self.read();
        d.invites
            .iter()
            .filter(|i| i.expires_at > now)
            .filter_map(|i| i.relay.clone())
            .chain(d.peers.iter().filter_map(|p| p.relay.clone()))
            .collect()
    }

    pub fn upsert(&self, peer: Peer) -> Result<()> {
        self.edit(|d| {
            d.peers.retain(|p| p.id != peer.id);
            d.peers.push(peer);
        })
    }

    pub fn update(&self, id: &str, f: impl FnOnce(&mut Peer)) -> Result<Option<Peer>> {
        self.edit(|d| {
            let p = d.peers.iter_mut().find(|p| p.id == id)?;
            f(p);
            Some(p.clone())
        })
    }

    pub fn remove(&self, id: &str) -> Result<bool> {
        self.edit(|d| {
            let before = d.peers.len();
            d.peers.retain(|p| p.id != id);
            d.peers.len() != before
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, PeerStore) {
        let t = tempfile::tempdir().unwrap();
        let s = PeerStore::new(t.path().join("sen-link/peers.json"));
        (t, s)
    }

    #[test]
    fn an_invite_is_single_use_and_expires() {
        let (_t, s) = store();
        let (code, exp) = s.create_invite(1000, None).unwrap();
        assert_eq!(code.len(), 8);
        assert_eq!(exp, 1000 + INVITE_TTL_SECS);
        let typed = format!("{}-{}", &code[..4], code[4..].to_lowercase());
        assert!(s.redeem_invite(&typed, 1001, None).unwrap().is_some());
        assert!(s.redeem_invite(&code, 1002, None).unwrap().is_none());
        let (late, _) = s.create_invite(2000, None).unwrap();
        assert!(s.redeem_invite(&late, 2000 + INVITE_TTL_SECS + 1, None).unwrap().is_none());
    }

    #[test]
    fn a_relay_invite_only_redeems_on_its_own_channel() {
        let (_t, s) = store();
        let relay = RelayInfo { hub_url: "http://hub".into(), channel_id: "ch-1".into(), access_token: "t".into(), key: "k".into() };
        let (code, _) = s.create_invite(10, Some(relay.clone())).unwrap();
        assert_eq!(s.relay_channels(11), vec![relay.clone()]);
        assert!(s.redeem_invite(&code, 11, Some("ch-2")).unwrap().is_none());
        assert!(s.redeem_invite(&code, 11, None).unwrap().is_none(), "HTTP must not claim a hub invite");
        let inv = s.redeem_invite(&code, 11, Some("ch-1")).unwrap().unwrap();
        assert_eq!(inv.relay, Some(relay));
        assert!(s.relay_channels(12).is_empty());
    }

    #[test]
    fn inbound_tokens_are_matched_by_hash_and_never_stored() {
        let (t, s) = store();
        let tok = random_token();
        s.upsert(Peer {
            id: "p1".into(),
            name: "B".into(),
            url: None,
            outbound_token: None,
            inbound_token_hash: sha256_hex(&tok),
            auto_run: false,
            assignee: None,
            relay: None,
            created_at: "now".into(),
        })
        .unwrap();
        assert_eq!(s.by_inbound_token(&tok).unwrap().id, "p1");
        assert!(s.by_inbound_token("slk_other").is_none());
        let raw = std::fs::read_to_string(t.path().join("sen-link/peers.json")).unwrap();
        assert!(!raw.contains(&tok));
        assert!(!PeerView::from(&s.get("p1").unwrap()).can_send);
        assert!(s.remove("p1").unwrap());
        assert!(s.peers().is_empty());
    }
}
