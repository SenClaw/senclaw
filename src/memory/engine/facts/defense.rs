//! Keep secrets out of memory.
//!
//! Auto-reflection sends every conversation window to a model and stores what
//! comes back. A user who pastes an API key into a chat would have it copied
//! into the fact store, the graph, and the extraction request. This scrubs the
//! window first.
//!
//! It targets *credentials* and card numbers — strings with a recognisable
//! shape that are never useful to remember. It deliberately leaves email
//! addresses, phone numbers and names alone: for a personal assistant "Alice's
//! email is …" is exactly the kind of fact worth keeping.
//!
//! Patterns are conservative on purpose: a miss leaves a secret in a window
//! the user typed, while a false positive deletes a harmless string from a
//! memory, and neither is checked by a human.

use std::sync::LazyLock;

use regex::{Captures, Regex};

/// What to do with a window that contains a secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Pass text through untouched.
    Off,
    /// Replace each secret with `[REDACTED:<kind>]` and keep the rest.
    #[default]
    Redact,
    /// Keep nothing from a window that contains a secret.
    Block,
}

impl Mode {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "0" | "none" => Mode::Off,
            "block" => Mode::Block,
            // An unrecognised value protects rather than exposes.
            _ => Mode::Redact,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing found (or the mode is `Off`); the text is unchanged.
    Clean(String),
    /// Secrets were replaced; `kinds` names what was found, in order.
    Redacted {
        text: String,
        kinds: Vec<&'static str>,
    },
    /// A secret was found and the mode forbids keeping the window.
    Blocked { kinds: Vec<&'static str> },
}

impl Verdict {
    /// The text that may be stored, if any.
    pub fn text(&self) -> Option<&str> {
        match self {
            Verdict::Clean(t) | Verdict::Redacted { text: t, .. } => Some(t),
            Verdict::Blocked { .. } => None,
        }
    }
}

struct Rule {
    kind: &'static str,
    re: Regex,
    /// Which capture group holds the secret; `0` is the whole match. A group
    /// lets a rule keep its label ("password:") and drop only the value.
    group: usize,
    /// A second look at what the pattern matched. Lets a loose pattern
    /// ("password is …") stay loose without eating ordinary words.
    accept: Option<fn(&str) -> bool>,
}

fn rule(kind: &'static str, pattern: &str, group: usize) -> Rule {
    Rule {
        kind,
        re: Regex::new(pattern).unwrap_or_else(|e| panic!("defense pattern {kind}: {e}")),
        group,
        accept: None,
    }
}

/// A secret read out of running prose has a digit or a symbol in it; a plain
/// word does not ("the password is required").
fn looks_like_a_secret(value: &str) -> bool {
    value
        .chars()
        .any(|c| c.is_ascii_digit() || (!c.is_alphanumeric() && !c.is_whitespace()))
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        // A key block first: its body would otherwise be cut up by later rules.
        rule(
            "private_key",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----(?:[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----)?",
            0,
        ),
        rule("aws_access_key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b", 0),
        rule(
            "github_token",
            r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,})\b",
            0,
        ),
        rule("slack_token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}\b", 0),
        rule("stripe_key", r"\b[sr]k_(?:live|test)_[0-9A-Za-z]{16,}\b", 0),
        rule("google_api_key", r"\bAIza[0-9A-Za-z_\-]{35}\b", 0),
        // sk-…, sk-proj-…, sk-ant-… (OpenAI, Anthropic and the many look-alikes)
        rule("api_key", r"\bsk-[A-Za-z0-9_\-]{20,}", 0),
        rule(
            "jwt",
            r"\beyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
            0,
        ),
        rule(
            "bearer_token",
            r"(?i)\bbearer\s+([A-Za-z0-9._~+/\-]{20,}=*)",
            1,
        ),
        // scheme://user:password@host — keep everything but the password.
        rule(
            "url_credentials",
            r"\b[a-zA-Z][a-zA-Z0-9+.\-]*://[^\s:/@]+:([^\s@/]{3,})@",
            1,
        ),
        // "password: x", "api_key=x" — an assignment is signal enough.
        rule(
            "credential",
            r"(?i)\b(?:password|passwd|pwd|passphrase|secret|api[_\- ]?key|access[_\- ]?token|auth[_\- ]?token|mật khẩu|mat khau)\s*[:=]\s*[\x22'`]?([^\s\x22'`,;]{6,})",
            1,
        ),
        // "mật khẩu là x", "the password is x" — prose, so the value must
        // look like a secret and not a word.
        Rule {
            accept: Some(looks_like_a_secret),
            ..rule(
                "credential",
                r"(?i)\b(?:password|passwd|passphrase|secret|mật khẩu|mat khau)\s+(?:là|is)\s+[\x22'`]?([^\s\x22'`,;]{6,})",
                1,
            )
        },
    ]
});

/// 13–19 digits, optionally grouped by spaces or dashes: a candidate card
/// number, accepted only when its Luhn checksum holds so order numbers and
/// phone numbers are left alone.
static CARD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b\d(?:[ \-]?\d){12,18}\b").unwrap());

fn luhn_valid(digits: &str) -> bool {
    let nums: Vec<u32> = digits.chars().filter_map(|c| c.to_digit(10)).collect();
    if !(13..=19).contains(&nums.len()) {
        return false;
    }
    let sum: u32 = nums
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            if i % 2 == 1 {
                let x = d * 2;
                if x > 9 {
                    x - 9
                } else {
                    x
                }
            } else {
                d
            }
        })
        .sum();
    sum % 10 == 0
}

fn marker(kind: &str) -> String {
    format!("[REDACTED:{kind}]")
}

/// Scrub `text` according to `mode`.
pub fn scrub(text: &str, mode: Mode) -> Verdict {
    if mode == Mode::Off {
        return Verdict::Clean(text.to_string());
    }
    let mut out = text.to_string();
    let mut kinds: Vec<&'static str> = Vec::new();

    for r in RULES.iter() {
        let mut hit = false;
        out =
            r.re.replace_all(&out, |caps: &Captures<'_>| {
                let whole = caps.get(0).expect("group 0");
                let secret = caps.get(r.group).unwrap_or(whole);
                if r.accept.is_some_and(|ok| !ok(secret.as_str())) {
                    return whole.as_str().to_string();
                }
                hit = true;
                if r.group == 0 {
                    return marker(r.kind);
                }
                // Keep the label, replace only the secret.
                let (from, to) = (secret.start() - whole.start(), secret.end() - whole.start());
                let whole = whole.as_str();
                format!("{}{}{}", &whole[..from], marker(r.kind), &whole[to..])
            })
            .into_owned();
        if hit && !kinds.contains(&r.kind) {
            kinds.push(r.kind);
        }
    }

    let mut found_card = false;
    out = CARD
        .replace_all(&out, |caps: &Captures<'_>| {
            let m = caps.get(0).unwrap().as_str();
            if luhn_valid(m) {
                found_card = true;
                marker("card_number")
            } else {
                m.to_string()
            }
        })
        .into_owned();
    if found_card {
        kinds.push("card_number");
    }

    if kinds.is_empty() {
        return Verdict::Clean(out);
    }
    match mode {
        Mode::Block => Verdict::Blocked { kinds },
        _ => Verdict::Redacted { text: out, kinds },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redacted(text: &str) -> (String, Vec<&'static str>) {
        match scrub(text, Mode::Redact) {
            Verdict::Redacted { text, kinds } => (text, kinds),
            other => panic!("expected a redaction in {text:?}, got {other:?}"),
        }
    }

    fn clean(text: &str) {
        assert_eq!(
            scrub(text, Mode::Redact),
            Verdict::Clean(text.to_string()),
            "{text:?} must pass through"
        );
    }

    #[test]
    fn recognisable_tokens_are_replaced_whole() {
        let cases = [
            ("key AKIAIOSFODNN7EXAMPLE here", "aws_access_key"),
            (
                concat!("token gh", "p_abcdefghijklmnopqrstuvwxyz0123456789 end"),
                "github_token",
            ),
            (
                concat!("use github", "_pat_11ABCDEFG0abcdefghijkl_mnop now"),
                "github_token",
            ),
            (concat!("xo", "xb-1234567890-abcdefghij"), "slack_token"),
            (concat!("sk_", "live_abcdefghijklmnop1234"), "stripe_key"),
            (
                concat!("AIza", "SyA1234567890abcdefghijklmnopqrstuv"),
                "google_api_key",
            ),
            ("sk-proj-abcdefghijklmnopqrstuvwxyz", "api_key"),
            ("sk-ant-api03-abcdefghijklmnopqrstu", "api_key"),
            (
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r",
                "jwt",
            ),
        ];
        for (input, kind) in cases {
            let (out, kinds) = redacted(input);
            assert_eq!(kinds, vec![kind], "{input}");
            assert!(out.contains(&marker(kind)), "{out}");
        }
        let (out, _) = redacted("my key is sk-proj-abcdefghijklmnopqrstuvwxyz ok");
        assert_eq!(out, "my key is [REDACTED:api_key] ok");
    }

    #[test]
    fn a_labelled_credential_keeps_its_label_and_loses_the_value() {
        let (out, kinds) = redacted("db password: hunter2hunter2 and more");
        assert_eq!(out, "db password: [REDACTED:credential] and more");
        assert_eq!(kinds, vec!["credential"]);

        let (out, _) = redacted("API_KEY=abcd1234efgh5678");
        assert_eq!(out, "API_KEY=[REDACTED:credential]");

        let (out, _) = redacted("mật khẩu là Sen@2026! nhé");
        assert!(out.contains("mật khẩu là [REDACTED:credential]"), "{out}");
        assert!(out.ends_with(" nhé"), "{out}");
    }

    #[test]
    fn prose_about_a_password_is_not_a_password() {
        clean("the password is required for login");
        clean("Your secret is safe with us");
        clean("mật khẩu là bắt buộc");
        let (out, _) = redacted("the password is Winter2026! please keep it");
        assert_eq!(out, "the password is [REDACTED:credential] please keep it");
    }

    #[test]
    fn url_credentials_and_bearer_tokens_lose_only_the_secret() {
        let (out, _) = redacted("connect postgres://admin:s3cretpass@db.internal:5432/app");
        assert_eq!(
            out,
            "connect postgres://admin:[REDACTED:url_credentials]@db.internal:5432/app"
        );

        let (out, _) = redacted("Authorization: Bearer abcdefghijklmnopqrstuvwx.yz");
        assert_eq!(out, "Authorization: Bearer [REDACTED:bearer_token]");
    }

    #[test]
    fn a_private_key_block_is_one_redaction() {
        let pem = concat!(
            "-----BEGIN RSA ",
            "PRIVATE KEY-----\nMIIEowIBAAKCAQEA\nabcdef\n-----END RSA ",
            "PRIVATE KEY-----"
        );
        let (out, kinds) = redacted(&format!("here:\n{pem}\nthanks"));
        assert_eq!(kinds, vec!["private_key"]);
        assert_eq!(out, "here:\n[REDACTED:private_key]\nthanks");
    }

    #[test]
    fn only_numbers_that_pass_the_card_checksum_are_cards() {
        let (out, kinds) = redacted("card 4111 1111 1111 1111 exp 12/29");
        assert_eq!(kinds, vec!["card_number"]);
        assert!(out.contains("[REDACTED:card_number]"));
        // 16 digits that fail Luhn: an order number, left alone.
        clean("order 1234 5678 9012 3456 shipped");
        // A phone number is too short and has no checksum anyway.
        clean("call 0912345678 tomorrow");
    }

    #[test]
    fn ordinary_text_names_and_contact_details_are_untouched() {
        clean("Alice works at Acme; her email is alice@acme.example and her number is +84 912 345 678.");
        clean("The password policy requires twelve characters.");
        clean("Hôm qua tôi đổi mật khẩu của máy chủ staging.");
        clean("Use the sk shorthand for sketch files.");
        clean("The daemon listens on port 18788 and 18789.");
    }

    #[test]
    fn block_mode_keeps_nothing_and_off_mode_keeps_everything() {
        let text = concat!("token gh", "p_abcdefghijklmnopqrstuvwxyz0123456789");
        assert_eq!(
            scrub(text, Mode::Block),
            Verdict::Blocked {
                kinds: vec!["github_token"]
            }
        );
        assert_eq!(scrub(text, Mode::Block).text(), None);
        assert_eq!(scrub(text, Mode::Off), Verdict::Clean(text.to_string()));
        // Block still lets clean text through.
        assert_eq!(
            scrub("nothing here", Mode::Block).text(),
            Some("nothing here")
        );
    }

    #[test]
    fn several_secrets_in_one_window_are_all_named() {
        let (out, kinds) = redacted(
            "AKIAIOSFODNN7EXAMPLE and password=correct-horse-battery plus 4111111111111111",
        );
        assert_eq!(kinds, vec!["aws_access_key", "credential", "card_number"]);
        assert!(!out.contains("AKIA") && !out.contains("correct-horse") && !out.contains("4111"));
    }

    #[test]
    fn the_mode_parses_and_an_unknown_value_protects() {
        assert_eq!(Mode::parse("off"), Mode::Off);
        assert_eq!(Mode::parse(" BLOCK "), Mode::Block);
        assert_eq!(Mode::parse("redact"), Mode::Redact);
        assert_eq!(Mode::parse("sure"), Mode::Redact);
    }

    #[test]
    fn luhn_accepts_known_numbers_and_rejects_an_off_by_one() {
        assert!(luhn_valid("4111111111111111"));
        assert!(luhn_valid("5555555555554444"));
        assert!(!luhn_valid("4111111111111112"));
        assert!(!luhn_valid("411111111111"));
    }
}
