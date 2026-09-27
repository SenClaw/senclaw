//! PKCE (RFC 7636) helpers for the native-app authorization-code flow.
//!
//! Every provider we talk to is a *public* client — the client id is baked
//! into a shipped binary, so there is no secret worth protecting. PKCE is what
//! actually binds the authorization code to this process: the code is useless
//! to anyone who intercepts the loopback redirect without the verifier.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// A verifier/challenge pair for one authorization attempt. Never reuse one.
#[derive(Debug, Clone)]
pub struct Pkce {
    /// Sent only on the token-exchange request.
    pub verifier: String,
    /// Sent on the authorize redirect (safe to appear in a URL / browser history).
    pub challenge: String,
}

impl Pkce {
    /// RFC 7636 §4.1 allows 43–128 chars; 32 random bytes base64url-encodes to
    /// 43, the minimum that still carries a full 256 bits of entropy.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        let challenge = challenge_for(&verifier);
        Self {
            verifier,
            challenge,
        }
    }
}

/// S256 transform: base64url(sha256(ascii(verifier))), no padding.
pub fn challenge_for(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// Opaque CSRF token echoed back on the redirect. Compared with a constant-time
/// check in the callback handler so a mismatch can't be probed byte by byte.
pub fn random_state() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Constant-time equality for the `state` echo.
pub fn state_matches(expected: &str, got: &str) -> bool {
    if expected.len() != got.len() {
        return false;
    }
    let diff = expected
        .as_bytes()
        .iter()
        .zip(got.as_bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_is_within_rfc_length_bounds() {
        let p = Pkce::generate();
        assert!(p.verifier.len() >= 43 && p.verifier.len() <= 128);
    }

    #[test]
    fn verifier_is_url_safe_and_unpadded() {
        let p = Pkce::generate();
        assert!(!p.verifier.contains('='));
        assert!(!p.verifier.contains('+'));
        assert!(!p.verifier.contains('/'));
        assert!(!p.challenge.contains('='));
    }

    #[test]
    fn challenge_matches_rfc7636_appendix_b_vector() {
        // The worked example from RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            challenge_for(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn each_generate_is_unique() {
        let a = Pkce::generate();
        let b = Pkce::generate();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.challenge, b.challenge);
    }

    #[test]
    fn state_compare_rejects_mismatch_and_length_diff() {
        let s = random_state();
        assert!(state_matches(&s, &s));
        assert!(!state_matches(&s, "short"));
        let mut tampered = s.clone();
        tampered.pop();
        tampered.push(if s.ends_with('A') { 'B' } else { 'A' });
        assert!(!state_matches(&s, &tampered));
    }
}
