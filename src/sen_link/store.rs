//! Paired peers and pending invites, in `~/.senclaw/sen-link/peers.json`
//! (0600), and this daemon's identity beside it. Outbound tokens (what we
//! present to a peer) are kept as given; inbound tokens (what a peer presents
//! to us) only as SHA-256.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Invite lifetime.
pub const INVITE_TTL_SECS: i64 = 10 * 60;
/// Unambiguous alphabet for spoken/typed invite codes.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Peer {
    pub id: String,
    pub name: String,
    /// Where the peer listens, `host:port`, host an IP or a DNS name — kept the
    /// way it was typed, so a name keeps following DNS. `None` when the peer
    /// could not tell us (it can then send to us but we cannot send to it).
    /// Files written before this was called `addr` name it `url`.
    #[serde(default, alias = "url")]
    pub addr: Option<String>,
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
    /// The peer's own id (its `identity.json`). A peer paired before ids
    /// existed has none and should be paired again.
    #[serde(default)]
    pub daemon_id: Option<String>,
    pub created_at: String,
}

/// A peer as the REST API shows it: no secrets.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerView {
    pub id: String,
    pub name: String,
    pub addr: Option<String>,
    pub can_send: bool,
    pub auto_run: bool,
    pub assignee: Option<String>,
    /// Paired by an earlier build: its address is the old UI port and it has
    /// no id, so it works only while the other side still serves `/a2a` there.
    pub legacy: bool,
    /// Seen on the network lately (mDNS or a scan). Only a peer with an id can be.
    pub online: bool,
    pub created_at: String,
}

impl PeerView {
    pub fn with_online(mut self, online: bool) -> Self {
        self.online = online;
        self
    }
}

impl From<&Peer> for PeerView {
    fn from(p: &Peer) -> Self {
        Self {
            id: p.id.clone(),
            name: p.name.clone(),
            addr: p.addr.clone(),
            can_send: p.addr.is_some() && p.outbound_token.is_some(),
            auto_run: p.auto_run,
            assignee: p.assignee.clone(),
            legacy: p.daemon_id.is_none(),
            online: false,
            created_at: p.created_at.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Invite {
    code_hash: String,
    pub expires_at: i64,
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
    daemon_id: OnceLock<String>,
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

/// An id a peer sent: short, printable, nothing that needs escaping anywhere it is shown.
pub fn clean_daemon_id(raw: &str) -> Option<String> {
    let id = raw.trim();
    (!id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then(|| id.to_string())
}

impl PeerStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path, lock: Mutex::new(()), daemon_id: OnceLock::new() }
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

    fn write_text(&self, text: Vec<u8>) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        std::fs::write(&self.path, text).with_context(|| format!("write {}", self.path.display()))?;
        crate::util::file_perms::restrict_best_effort(&self.path);
        Ok(())
    }

    fn write(&self, d: &Data) -> Result<()> {
        self.write_text(serde_json::to_vec_pretty(d)?)
    }

    fn edit<T>(&self, f: impl FnOnce(&mut Data) -> T) -> Result<T> {
        let _g = self.lock.lock().unwrap();
        let mut d = self.read();
        let out = f(&mut d);
        self.write(&d)?;
        Ok(out)
    }

    /// This daemon's id, made on first use and kept in `identity.json` beside
    /// the peers. Peers store it to recognise this daemon again.
    pub fn daemon_id(&self) -> &str {
        self.daemon_id.get_or_init(|| {
            let path = self.path.with_file_name("identity.json");
            let stored = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| v["daemonId"].as_str().and_then(clean_daemon_id));
            if let Some(id) = stored {
                return id;
            }
            let id = uuid::Uuid::new_v4().to_string();
            let saved = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&path, serde_json::json!({ "daemonId": id }).to_string()));
            match saved {
                Ok(()) => crate::util::file_perms::restrict_best_effort(&path),
                Err(e) => tracing::warn!("[slink] cannot save {}: {e}", path.display()),
            }
            id
        })
    }

    /// Clean up what the hub relay of earlier builds left in `peers.json`:
    /// peers reachable only through a hub are dropped (their names returned),
    /// the rest lose the hub channel, hub invites go. The file is copied to
    /// `peers.json.bak-<unix time>` first, and a file with nothing to clean is
    /// left untouched.
    pub fn migrate_hub_leftovers(&self) -> Result<Vec<String>> {
        let _g = self.lock.lock().unwrap();
        let Ok(text) = std::fs::read_to_string(&self.path) else { return Ok(Vec::new()) };
        let Ok(mut root) = serde_json::from_str::<Value>(&text) else { return Ok(Vec::new()) };
        let has_relay = |v: &Value| v.get("relay").is_some_and(|r| !r.is_null());
        let dirty = ["peers", "invites"]
            .iter()
            .any(|k| root[*k].as_array().is_some_and(|a| a.iter().any(has_relay)));
        if !dirty {
            return Ok(Vec::new());
        }
        let backup = self.path.with_extension(format!("json.bak-{}", chrono::Utc::now().timestamp()));
        std::fs::copy(&self.path, &backup).with_context(|| format!("back up to {}", backup.display()))?;
        crate::util::file_perms::restrict_best_effort(&backup);

        let mut dropped = Vec::new();
        if let Some(peers) = root.get_mut("peers").and_then(Value::as_array_mut) {
            peers.retain_mut(|p| {
                if !has_relay(p) {
                    return true;
                }
                let reachable = ["url", "addr"]
                    .iter()
                    .any(|k| p.get(*k).and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty()));
                if reachable {
                    if let Some(o) = p.as_object_mut() {
                        o.remove("relay");
                    }
                } else {
                    dropped.push(p["name"].as_str().unwrap_or("peer").to_string());
                }
                reachable
            });
        }
        if let Some(invites) = root.get_mut("invites").and_then(Value::as_array_mut) {
            invites.retain(|i| !has_relay(i));
        }
        self.write_text(serde_json::to_vec_pretty(&root)?)?;
        Ok(dropped)
    }

    pub fn peers(&self) -> Vec<Peer> {
        let _g = self.lock.lock().unwrap();
        self.read().peers
    }

    pub fn get(&self, id: &str) -> Option<Peer> {
        self.peers().into_iter().find(|p| p.id == id)
    }

    pub fn by_daemon_id(&self, daemon_id: &str) -> Option<Peer> {
        self.peers().into_iter().find(|p| p.daemon_id.as_deref() == Some(daemon_id))
    }

    /// The peer presenting `token`.
    pub fn by_inbound_token(&self, token: &str) -> Option<Peer> {
        let h = sha256_hex(token);
        self.peers()
            .into_iter()
            .find(|p| crate::gateway::ui_server::auth::ct_eq(&p.inbound_token_hash, &h))
    }

    /// A one-time pairing code, valid for [`INVITE_TTL_SECS`].
    pub fn create_invite(&self, now: i64) -> Result<(String, i64)> {
        let code = random_code();
        let expires_at = now + INVITE_TTL_SECS;
        let h = sha256_hex(&code);
        self.edit(|d| {
            d.invites.retain(|i| i.expires_at > now);
            d.invites.push(Invite { code_hash: h, expires_at });
        })?;
        Ok((code, expires_at))
    }

    /// Is there an invite someone could still redeem?
    pub fn has_live_invite(&self, now: i64) -> bool {
        let _g = self.lock.lock().unwrap();
        self.read().invites.iter().any(|i| i.expires_at > now)
    }

    /// Consume an invite; `true` when `code` was live. A miss writes nothing,
    /// so unauthenticated guesses cannot churn the file.
    pub fn redeem_invite(&self, code: &str, now: i64) -> Result<bool> {
        let h = sha256_hex(&normalize_code(code));
        let _g = self.lock.lock().unwrap();
        let mut d = self.read();
        if !d.invites.iter().any(|i| i.expires_at > now && i.code_hash == h) {
            return Ok(false);
        }
        d.invites.retain(|i| i.expires_at > now && i.code_hash != h);
        self.write(&d)?;
        Ok(true)
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

    fn peer(id: &str) -> Peer {
        Peer {
            id: id.into(),
            name: format!("peer-{id}"),
            addr: None,
            outbound_token: None,
            inbound_token_hash: String::new(),
            auto_run: false,
            assignee: None,
            daemon_id: None,
            created_at: "now".into(),
        }
    }

    #[test]
    fn an_invite_is_single_use_and_expires() {
        let (_t, s) = store();
        let (code, exp) = s.create_invite(1000).unwrap();
        assert_eq!(code.len(), 8);
        assert_eq!(exp, 1000 + INVITE_TTL_SECS);
        assert!(s.has_live_invite(1001));
        let typed = format!("{}-{}", &code[..4], code[4..].to_lowercase());
        assert!(s.redeem_invite(&typed, 1001).unwrap());
        assert!(!s.redeem_invite(&code, 1002).unwrap());
        assert!(!s.has_live_invite(1002));
        let (late, _) = s.create_invite(2000).unwrap();
        assert!(!s.redeem_invite(&late, 2000 + INVITE_TTL_SECS + 1).unwrap());
        assert!(!s.has_live_invite(2000 + INVITE_TTL_SECS + 1));
    }

    #[test]
    fn inbound_tokens_are_matched_by_hash_and_never_stored() {
        let (t, s) = store();
        let tok = random_token();
        let mut p = peer("p1");
        p.inbound_token_hash = sha256_hex(&tok);
        s.upsert(p).unwrap();
        assert_eq!(s.by_inbound_token(&tok).unwrap().id, "p1");
        assert!(s.by_inbound_token("slk_other").is_none());
        let raw = std::fs::read_to_string(t.path().join("sen-link/peers.json")).unwrap();
        assert!(!raw.contains(&tok));
        assert!(!PeerView::from(&s.get("p1").unwrap()).can_send);
        assert!(s.remove("p1").unwrap());
        assert!(s.peers().is_empty());
    }

    #[test]
    fn the_daemon_id_is_made_once_and_survives_a_restart() {
        let (t, s) = store();
        let id = s.daemon_id().to_string();
        assert!(clean_daemon_id(&id).is_some());
        assert_eq!(s.daemon_id(), id);
        let again = PeerStore::new(t.path().join("sen-link/peers.json"));
        assert_eq!(again.daemon_id(), id);
        assert!(t.path().join("sen-link/identity.json").exists());
    }

    #[test]
    fn an_id_a_peer_sends_is_checked() {
        assert_eq!(clean_daemon_id(" 6f1c-AA ").as_deref(), Some("6f1c-AA"));
        for bad in ["", "   ", "a b", "<script>", &"x".repeat(65)] {
            assert!(clean_daemon_id(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn a_file_from_before_the_rename_still_reads() {
        let (t, s) = store();
        std::fs::create_dir_all(t.path().join("sen-link")).unwrap();
        std::fs::write(
            t.path().join("sen-link/peers.json"),
            r#"{"peers": [{"id": "a", "name": "Old", "url": "http://192.168.1.9:18788", "outboundToken": "t", "inboundTokenHash": "h", "createdAt": "x"}], "invites": []}"#,
        )
        .unwrap();
        let p = s.get("a").unwrap();
        assert_eq!(p.addr.as_deref(), Some("http://192.168.1.9:18788"));
        let view = PeerView::from(&p);
        assert!(view.legacy && view.can_send);
    }

    #[test]
    fn hub_leftovers_are_cleaned_with_a_backup_and_only_when_present() {
        let (t, s) = store();
        let path = t.path().join("sen-link/peers.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let relay = r#"{"hubUrl": "http://h", "channelId": "c", "accessToken": "t", "key": "k"}"#;
        std::fs::write(
            &path,
            format!(
                r#"{{"peers": [
                    {{"id": "hub-only", "name": "Hub only", "url": null, "outboundToken": "t", "inboundTokenHash": "h", "relay": {relay}, "createdAt": "x"}},
                    {{"id": "both", "name": "Both", "url": "http://10.0.0.2:18788", "outboundToken": "t", "inboundTokenHash": "h", "relay": {relay}, "createdAt": "x"}},
                    {{"id": "plain", "name": "Plain", "url": "http://10.0.0.3:18788", "outboundToken": "t", "inboundTokenHash": "h", "createdAt": "x"}}
                ], "invites": [
                    {{"codeHash": "a", "expiresAt": 9999999999, "relay": {relay}}},
                    {{"codeHash": "b", "expiresAt": 9999999999}}
                ]}}"#
            ),
        )
        .unwrap();

        let dropped = s.migrate_hub_leftovers().unwrap();
        assert_eq!(dropped, vec!["Hub only".to_string()]);
        let ids: Vec<String> = s.peers().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["both", "plain"]);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("relay") && !raw.contains("channelId"), "{raw}");
        let backups: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("peers.json.bak-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert!(std::fs::read_to_string(backups[0].path()).unwrap().contains("channelId"), "the backup is the original");

        // A clean file is left alone: no second backup, no rewrite.
        assert!(s.migrate_hub_leftovers().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 2, "peers.json + one backup");
        // A missing file is not an error.
        let (_t2, empty) = store();
        assert!(empty.migrate_hub_leftovers().unwrap().is_empty());
    }
}
