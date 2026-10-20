//! Sealing the conversation between two paired SenClaws.
//!
//! Pairing (`pairing.rs`) leaves both sides with the same 32-byte key that
//! nobody else has. Every `/a2a` request and answer after that travels as one
//! AES-256-GCM envelope under it, so a machine that can read the network sees
//! ciphertext: not the task, not the answer, not the token.
//!
//! * The *additional data* names the direction and both daemons
//!   (`slink1|req|<from>|<to>`), so an envelope cannot be turned around into an
//!   answer, nor carried to a third daemon that holds the same key.
//! * A request carries an id and the sender's clock; one outside a two-minute
//!   window, or an id already served inside it, is refused — a copy captured
//!   on the wire cannot run a task twice. An answer carries the id of the
//!   request it answers, so an old answer cannot be passed off as a new one.
//! * Nonces are random 96-bit values; with one key per pair that is safe for
//!   far more messages than two daemons will ever exchange.

use std::collections::HashMap;
use std::sync::Mutex;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use rand::RngCore;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const KEY_LEN: usize = 32;
/// A request older or newer than this (sender's clock against ours) is refused,
/// and ids seen within it are remembered.
pub const REPLAY_WINDOW_MS: i64 = 120_000;
/// Largest answer sent back. The listener's body limit is 1 MiB for requests;
/// answers can hold a list of tasks.
pub const MAX_REPLY_BYTES: usize = 5 * 1024 * 1024;
const VERSION: u8 = 1;

/// The envelope that goes on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Sealed {
    pub v: u8,
    /// Base64 of the 12-byte nonce.
    pub n: String,
    /// Base64 of the ciphertext with its 16-byte tag.
    pub c: String,
}

/// What a request carries inside its envelope.
#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    /// The sender's clock, ms since the epoch.
    pub ts: i64,
    /// The token the sender was given at pairing.
    pub token: String,
    /// The JSON-RPC request.
    pub rpc: Value,
}

/// What an answer carries inside its envelope.
#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
    /// The id of the request it answers.
    pub id: String,
    /// The JSON-RPC answer.
    pub rpc: Value,
}

pub type Key = [u8; KEY_LEN];

pub fn encode_key(key: &Key) -> String {
    base64::engine::general_purpose::STANDARD.encode(key)
}

pub fn decode_key(b64: &str) -> Result<Key> {
    let raw = base64::engine::general_purpose::STANDARD.decode(b64.trim()).context("the pair key is not base64")?;
    raw.try_into().map_err(|_| anyhow!("the pair key is not {KEY_LEN} bytes"))
}

/// The additional data of an envelope: what it is, who sent it, who it is for.
pub fn aad(kind: &str, from: &str, to: &str) -> Vec<u8> {
    format!("slink{VERSION}|{kind}|{from}|{to}").into_bytes()
}

pub fn seal(key: &Key, aad: &[u8], plaintext: &[u8]) -> Sealed {
    let cipher = Aes256Gcm::new_from_slice(key).expect("a 32-byte key");
    let mut nonce = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad })
        .expect("AES-GCM encrypts any message this size");
    let b64 = base64::engine::general_purpose::STANDARD;
    Sealed { v: VERSION, n: b64.encode(nonce), c: b64.encode(ct) }
}

/// Open an envelope. Every failure — a wrong key, other additional data, a
/// changed byte, a malformed field — is the same answer: it is not for us.
pub fn open(key: &Key, aad: &[u8], sealed: &Sealed) -> Result<Vec<u8>> {
    let b64 = base64::engine::general_purpose::STANDARD;
    if sealed.v != VERSION {
        bail!("unknown envelope version");
    }
    let nonce = b64.decode(&sealed.n).map_err(|_| anyhow!("bad nonce"))?;
    // `Nonce::from_slice` panics on any other length, and this comes off the network.
    if nonce.len() != 12 {
        bail!("bad nonce");
    }
    let ct = b64.decode(&sealed.c).map_err(|_| anyhow!("bad ciphertext"))?;
    let cipher = Aes256Gcm::new_from_slice(key).expect("a 32-byte key");
    cipher.decrypt(Nonce::from_slice(&nonce), Payload { msg: &ct, aad }).map_err(|_| anyhow!("does not open"))
}

pub fn seal_json<T: Serialize>(key: &Key, aad: &[u8], value: &T) -> Result<Sealed> {
    Ok(seal(key, aad, &serde_json::to_vec(value)?))
}

pub fn open_json<T: DeserializeOwned>(key: &Key, aad: &[u8], sealed: &Sealed) -> Result<T> {
    Ok(serde_json::from_slice(&open(key, aad, sealed)?)?)
}

/// Request ids served within [`REPLAY_WINDOW_MS`], per peer.
#[derive(Default)]
pub struct ReplayGuard {
    seen: Mutex<HashMap<String, i64>>,
}

impl ReplayGuard {
    /// Is this request fresh — inside the window and not served before? Records it when it is.
    pub fn accept(&self, peer: &str, id: &str, ts: i64, now: i64) -> bool {
        if (now - ts).abs() > REPLAY_WINDOW_MS {
            return false;
        }
        let mut seen = self.seen.lock().unwrap();
        seen.retain(|_, t| (now - *t).abs() <= REPLAY_WINDOW_MS);
        seen.insert(format!("{peer}:{id}"), ts).is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: u8) -> Key {
        [b; KEY_LEN]
    }

    #[test]
    fn what_is_sealed_opens_with_the_same_key_and_data_and_is_not_readable_on_the_wire() {
        let a = aad("req", "alice", "bob");
        let s = seal(&key(7), &a, b"Write the release notes; token slk_secret");
        assert_eq!(open(&key(7), &a, &s).unwrap(), b"Write the release notes; token slk_secret");
        let wire = serde_json::to_string(&s).unwrap();
        assert!(!wire.contains("release") && !wire.contains("slk_secret"), "{wire}");
        // The same message sealed twice is two different envelopes.
        assert_ne!(seal(&key(7), &a, b"x").c, seal(&key(7), &a, b"x").c);
    }

    #[test]
    fn a_wrong_key_other_data_or_a_changed_byte_do_not_open() {
        let a = aad("req", "alice", "bob");
        let s = seal(&key(7), &a, b"hello");
        assert!(open(&key(8), &a, &s).is_err(), "another key");
        assert!(open(&key(7), &aad("resp", "alice", "bob"), &s).is_err(), "turned into an answer");
        assert!(open(&key(7), &aad("req", "bob", "alice"), &s).is_err(), "turned around");
        assert!(open(&key(7), &aad("req", "alice", "carol"), &s).is_err(), "carried to a third daemon");
        let mut flipped = s.clone();
        let mut raw = base64::engine::general_purpose::STANDARD.decode(&flipped.c).unwrap();
        raw[0] ^= 1;
        flipped.c = base64::engine::general_purpose::STANDARD.encode(raw);
        assert!(open(&key(7), &a, &flipped).is_err(), "a changed byte");
    }

    #[test]
    fn a_malformed_envelope_is_refused_and_never_panics() {
        let a = aad("req", "a", "b");
        let ok = seal(&key(1), &a, b"x");
        for bad in [
            Sealed { v: 2, ..ok.clone() },
            Sealed { n: "!!".into(), ..ok.clone() },
            Sealed { n: base64::engine::general_purpose::STANDARD.encode([0u8; 5]), ..ok.clone() },
            Sealed { n: base64::engine::general_purpose::STANDARD.encode([0u8; 40]), ..ok.clone() },
            Sealed { c: "??".into(), ..ok.clone() },
            Sealed { c: String::new(), ..ok.clone() },
        ] {
            assert!(open(&key(1), &a, &bad).is_err());
        }
    }

    #[test]
    fn keys_round_trip_and_wrong_lengths_are_refused() {
        assert_eq!(decode_key(&encode_key(&key(9))).unwrap(), key(9));
        for bad in ["", "AAAA", "not base64!", &base64::engine::general_purpose::STANDARD.encode([0u8; 31]), &base64::engine::general_purpose::STANDARD.encode([0u8; 33])] {
            assert!(decode_key(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn typed_messages_round_trip_through_an_envelope() {
        let a = aad("req", "a", "b");
        let req = Request { id: "r1".into(), ts: 5, token: "slk_t".into(), rpc: serde_json::json!({ "method": "ListTasks" }) };
        let back: Request = open_json(&key(3), &a, &seal_json(&key(3), &a, &req).unwrap()).unwrap();
        assert_eq!((back.id.as_str(), back.ts, back.token.as_str()), ("r1", 5, "slk_t"));
        assert_eq!(back.rpc["method"], "ListTasks");
    }

    #[test]
    fn a_replayed_or_stale_request_is_refused_and_forgotten_after_the_window() {
        let g = ReplayGuard::default();
        let now = 1_000_000_000_000;
        assert!(g.accept("p", "a", now, now));
        assert!(!g.accept("p", "a", now, now + 1), "the same id twice");
        assert!(g.accept("q", "a", now, now), "ids are per peer");
        assert!(!g.accept("p", "b", now - REPLAY_WINDOW_MS - 1, now), "too old");
        assert!(!g.accept("p", "c", now + REPLAY_WINDOW_MS + 1, now), "from the future");
        assert!(g.accept("p", "a", now + 3 * REPLAY_WINDOW_MS, now + 3 * REPLAY_WINDOW_MS), "forgotten after the window");
    }
}
