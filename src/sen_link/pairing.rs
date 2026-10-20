//! The pairing handshake. The invite code is a *password*, and a password
//! authenticated key exchange (SPAKE2) turns it into a key without the code
//! — or anything an attacker could test a guess against — ever crossing the
//! network.
//!
//! ```text
//!   A (types the code)                         B (made the invite)
//!   start_a(code)  ──── begin {msg_a} ────────►  start_b(code); K = finish(msg_a)
//!                  ◄─── {sid, msg_b} ──────────
//!   K = finish(msg_b)
//!   seal(offer, K_a→b) ─ finish {sid, sealed} ►  open with K_a→b — fails on a wrong code
//!                  ◄─── sealed(accept, K_b→a) ──  invite used up; peer recorded
//! ```
//!
//! From the shared secret both sides derive, with HKDF over the transcript, a
//! key for each direction of the handshake and the **pair key** that seals
//! every later request (`seal.rs`). Opening the offer is the proof that A knew
//! the code, and opening the accept proves the same of B — nobody has to
//! send a MAC of anything.
//!
//! Someone on the network who does not know the code gets one guess per
//! handshake and learns only whether it was wrong; an invite dies after
//! [`INVITE_MAX_FAILURES`](super::store::INVITE_MAX_FAILURES) of them. What
//! was sniffed from a handshake cannot be tested against guesses offline — the
//! reason this is a PAKE and not a hash of the code mixed into a key.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};

use super::seal::{Key, Sealed};
use super::store::normalize_code;

/// A SPAKE2 message: one byte of side marker and a 32-byte curve point.
pub const MSG_LEN: usize = 33;
const ID_A: &[u8] = b"slink-initiator";
const ID_B: &[u8] = b"slink-responder";
/// Handshakes that have begun and not finished, at once.
const MAX_SESSIONS: usize = 16;
/// How long a begun handshake waits for its second message.
const SESSION_TTL: Duration = Duration::from_secs(60);

/// What the keys of one handshake are for.
#[derive(Debug, Clone)]
pub struct Keys {
    /// Seals the offer (A → B).
    pub a_to_b: Key,
    /// Seals the accept (B → A).
    pub b_to_a: Key,
    /// Seals every request and answer after pairing.
    pub pair: Key,
}

fn derive(shared: &[u8], msg_a: &[u8], msg_b: &[u8]) -> Keys {
    let salt = Sha256::new().chain_update(msg_a).chain_update(msg_b).finalize();
    let hk = Hkdf::<Sha256>::new(Some(&salt), shared);
    let expand = |label: &str| {
        let mut k = [0u8; 32];
        hk.expand(label.as_bytes(), &mut k).expect("32 bytes is a valid HKDF-SHA256 output length");
        k
    };
    Keys { a_to_b: expand("slink 1 pairing a-to-b"), b_to_a: expand("slink 1 pairing b-to-a"), pair: expand("slink 1 pair key") }
}

/// The side that types the code.
pub struct Initiator {
    state: Spake2<Ed25519Group>,
    msg_a: Vec<u8>,
}

impl Initiator {
    /// Begin with `code` (typed, so any case or dashes). Returns the message for B.
    pub fn start(code: &str) -> (Self, Vec<u8>) {
        let (state, msg_a) = Spake2::<Ed25519Group>::start_a(&Password::new(normalize_code(code).as_bytes()), &Identity::new(ID_A), &Identity::new(ID_B));
        (Self { state, msg_a: msg_a.clone() }, msg_a)
    }

    /// B's answer in. The keys are the same as B's only if the codes were.
    pub fn finish(self, msg_b: &[u8]) -> Result<Keys> {
        if msg_b.len() != MSG_LEN {
            return Err(anyhow!("the handshake message is the wrong size"));
        }
        let shared = self.state.finish(msg_b).map_err(|e| anyhow!("the handshake message is not valid: {e:?}"))?;
        Ok(derive(&shared, &self.msg_a, msg_b))
    }
}

/// The side that made the invite: A's message in, B's message and the keys out.
pub fn respond(code: &str, msg_a: &[u8]) -> Result<(Vec<u8>, Keys)> {
    if msg_a.len() != MSG_LEN {
        return Err(anyhow!("the handshake message is the wrong size"));
    }
    let (state, msg_b) = Spake2::<Ed25519Group>::start_b(&Password::new(normalize_code(code).as_bytes()), &Identity::new(ID_A), &Identity::new(ID_B));
    let shared = state.finish(msg_a).map_err(|e| anyhow!("the handshake message is not valid: {e:?}"))?;
    let keys = derive(&shared, msg_a, &msg_b);
    Ok((msg_b, keys))
}

// ---- the wire -------------------------------------------------------------

/// `POST /a2a/pair/begin`
#[derive(Debug, Serialize, Deserialize)]
pub struct Begin {
    /// Base64 of A's handshake message.
    pub msg: String,
}

/// The answer to [`Begin`].
#[derive(Debug, Serialize, Deserialize)]
pub struct BeginReply {
    pub sid: String,
    /// Base64 of B's handshake message.
    pub msg: String,
}

/// `POST /a2a/pair/finish`
#[derive(Debug, Serialize, Deserialize)]
pub struct Finish {
    pub sid: String,
    /// An [`Offer`], sealed with [`Keys::a_to_b`].
    pub sealed: Sealed,
}

/// What A tells B once both know the code.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Offer {
    pub name: String,
    /// Port of A's own SLink listener; with the address this arrived from it tells B where to send A tasks.
    pub port: Option<u16>,
    pub daemon_id: Option<String>,
    /// The token A presents when it calls B.
    pub token: String,
}

/// What B tells A in return, sealed with [`Keys::b_to_a`].
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Accept {
    pub peer_id: String,
    pub name: String,
    pub daemon_id: String,
    /// The token B presents when it calls A.
    pub token: String,
}

/// The additional data of the two sealed handshake messages.
pub fn offer_aad(sid: &str) -> Vec<u8> {
    super::seal::aad("pair-a2b", sid, "")
}

pub fn accept_aad(sid: &str) -> Vec<u8> {
    super::seal::aad("pair-b2a", sid, "")
}

// ---- handshakes in flight (B's side) ---------------------------------------

pub struct Pending {
    pub keys: Keys,
    /// The invite this handshake began for; it must still be the live one at the end.
    pub invite_id: String,
    at: Instant,
}

/// Handshakes B has answered and not yet seen finish.
#[derive(Default)]
pub struct Sessions {
    map: Mutex<HashMap<String, Pending>>,
}

impl Sessions {
    /// Remember a begun handshake under a fresh id. `None` when too many are
    /// already in flight — a flood must not grow memory without bound.
    pub fn insert(&self, keys: Keys, invite_id: String) -> Option<String> {
        self.insert_at(keys, invite_id, Instant::now())
    }

    fn insert_at(&self, keys: Keys, invite_id: String, now: Instant) -> Option<String> {
        let mut m = self.map.lock().unwrap();
        m.retain(|_, p| now.saturating_duration_since(p.at) <= SESSION_TTL);
        if m.len() >= MAX_SESSIONS {
            return None;
        }
        let sid = uuid::Uuid::new_v4().simple().to_string();
        m.insert(sid.clone(), Pending { keys, invite_id, at: now });
        Some(sid)
    }

    /// The handshake `sid` began, once: a second try with the same id finds nothing.
    pub fn take(&self, sid: &str) -> Option<Pending> {
        self.take_at(sid, Instant::now())
    }

    fn take_at(&self, sid: &str, now: Instant) -> Option<Pending> {
        let p = self.map.lock().unwrap().remove(sid)?;
        (now.saturating_duration_since(p.at) <= SESSION_TTL).then_some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sen_link::seal;

    fn handshake(code_a: &str, code_b: &str) -> (Keys, Keys, Vec<u8>, Vec<u8>) {
        let (init, msg_a) = Initiator::start(code_a);
        let (msg_b, keys_b) = respond(code_b, &msg_a).unwrap();
        let keys_a = init.finish(&msg_b).unwrap();
        (keys_a, keys_b, msg_a, msg_b)
    }

    #[test]
    fn both_sides_get_the_same_keys_from_the_same_code_however_it_was_typed() {
        let (a, b, msg_a, msg_b) = handshake("k7mq-2xrp", "K7MQ2XRP");
        assert_eq!((a.a_to_b, a.b_to_a, a.pair), (b.a_to_b, b.b_to_a, b.pair));
        assert_eq!((msg_a.len(), msg_b.len()), (MSG_LEN, MSG_LEN));
        // One secret, three keys, none of them another's.
        assert!(a.a_to_b != a.b_to_a && a.a_to_b != a.pair && a.b_to_a != a.pair);
    }

    #[test]
    fn a_wrong_code_gives_different_keys_so_nothing_opens() {
        let (a, b, ..) = handshake("AAAAAAAA", "K7MQ2XRP");
        assert!(a.a_to_b != b.a_to_b && a.pair != b.pair);
        let offer = seal::seal(&a.a_to_b, &offer_aad("sid"), b"{}");
        assert!(seal::open(&b.a_to_b, &offer_aad("sid"), &offer).is_err(), "B cannot open what a wrong-code A sealed");
    }

    #[test]
    fn two_handshakes_with_the_same_code_do_not_share_keys() {
        let (a1, ..) = handshake("K7MQ2XRP", "K7MQ2XRP");
        let (a2, ..) = handshake("K7MQ2XRP", "K7MQ2XRP");
        assert_ne!(a1.pair, a2.pair, "each pairing is a fresh key");
    }

    #[test]
    fn a_handshake_message_of_the_wrong_size_or_shape_is_an_error_not_a_panic() {
        for bad in [vec![], vec![0u8; 5], vec![0u8; 32], vec![0u8; 34], vec![0xffu8; MSG_LEN], vec![0u8; MSG_LEN]] {
            assert!(respond("K7MQ2XRP", &bad).is_err(), "{} bytes", bad.len());
            let (init, _) = Initiator::start("K7MQ2XRP");
            assert!(init.finish(&bad).is_err(), "{} bytes", bad.len());
        }
    }

    #[test]
    fn the_sealed_handshake_messages_round_trip() {
        let (a, b, ..) = handshake("K7MQ2XRP", "K7MQ2XRP");
        let offer = Offer { name: "Mac A".into(), port: Some(18791), daemon_id: Some("6f1c-0a".into()), token: "slk_a".into() };
        let sealed = seal::seal_json(&a.a_to_b, &offer_aad("s1"), &offer).unwrap();
        let got: Offer = seal::open_json(&b.a_to_b, &offer_aad("s1"), &sealed).unwrap();
        assert_eq!((got.name.as_str(), got.port, got.token.as_str()), ("Mac A", Some(18791), "slk_a"));
        assert!(seal::open_json::<Offer>(&b.a_to_b, &offer_aad("other-session"), &sealed).is_err(), "bound to its session");
        let accept = Accept { peer_id: "p".into(), name: "Mac B".into(), daemon_id: "b".into(), token: "slk_b".into() };
        let back: Accept = seal::open_json(&a.b_to_a, &accept_aad("s1"), &seal::seal_json(&b.b_to_a, &accept_aad("s1"), &accept).unwrap()).unwrap();
        assert_eq!(back.token, "slk_b");
        assert!(seal::open_json::<Accept>(&a.a_to_b, &accept_aad("s1"), &seal::seal_json(&b.b_to_a, &accept_aad("s1"), &accept).unwrap()).is_err(), "the other direction's key");
    }

    fn keys() -> Keys {
        handshake("K7MQ2XRP", "K7MQ2XRP").1
    }

    #[test]
    fn a_begun_handshake_finishes_once_within_its_time_and_the_number_in_flight_is_capped() {
        let s = Sessions::default();
        let t0 = Instant::now();
        let sid = s.insert_at(keys(), "inv".into(), t0).unwrap();
        assert!(s.take_at(&sid, t0 + Duration::from_secs(5)).is_some());
        assert!(s.take_at(&sid, t0 + Duration::from_secs(6)).is_none(), "once");

        let late = s.insert_at(keys(), "inv".into(), t0).unwrap();
        assert!(s.take_at(&late, t0 + SESSION_TTL + Duration::from_secs(1)).is_none(), "too late");

        for _ in 0..MAX_SESSIONS {
            assert!(s.insert_at(keys(), "inv".into(), t0).is_some());
        }
        assert!(s.insert_at(keys(), "inv".into(), t0).is_none(), "full");
        assert!(s.insert_at(keys(), "inv".into(), t0 + SESSION_TTL + Duration::from_secs(1)).is_some(), "room again once the old ones expire");
        assert!(s.take("never-begun").is_none());
    }
}
