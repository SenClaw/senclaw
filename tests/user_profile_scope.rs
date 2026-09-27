//! Cross-repo enforcement of the Soul Core tier rule.
//!
//! `USER.md` is readable by every agent, every channel and every UI — that is
//! the point of it being global. What must never happen is the *private* half
//! (email, phone, home address, the SSH hosts in `TOOLS.md`) reaching a room
//! with strangers in it, because a group-chat model will repeat it when asked.
//!
//! The rule lives in one function, [`user_profile::render`], and every caller
//! is supposed to go through it. "Supposed to" is what this file replaces:
//! these tests enumerate the JID shapes every channel adapter actually mints
//! and assert the outcome directly, so a new channel — or a refactor that
//! moves the check — fails `cargo test` instead of leaking quietly.
//!
//! Sibling of `tests/space_app_bind_loopback.rs`, which enforces the
//! loopback-bind rule the same way and for the same reason: the failure is
//! invisible in review and expensive in production.

use senclaw::user_profile::{self, ProfileScope};

/// Every JID shape the channel adapters mint for a **group** conversation.
///
/// Sources: `chat_id_to_jid` in `src/channels/telegram.rs`, the `p2p`/else
/// split in `src/channels/feishu/helpers.rs`, plus the cowork/virtual
/// instance ids used by the dispatch layer.
const GROUP_JIDS: &[&str] = &[
    "tg:123456:group:-1001234567890",
    "feishu:group:oc_abcdef0123456789",
    "qq:group:987654321",
    "wx:group:chatroom-1",
];

/// JID shapes that identify a conversation with the owner alone.
const PRIVATE_JIDS: &[&str] = &[
    "web:main",
    "web:reminders:main",
    "app:channel-1:user:sender-9",
    "tg:123456:user:99887766",
    "feishu:user:ou_abcdef0123456789",
    "wx:user:wxid_abc123",
];

/// Shapes that carry no usable signal. These must fail closed.
const UNKNOWN_JIDS: &[&str] = &[
    "",
    "gibberish",
    "cowork:team-alpha",
    "virtual:worker-3",
    "schedule_21843f68-1449-4a4d-b273-b3dcfa37277e",
    "unknown:channel:42",
];

const PROFILE: &str = r#"---
name: Nguyễn Văn A
preferred_name: anh A
timezone: Asia/Ho_Chi_Minh
language: vi
email: PRIVATE-EMAIL@example.com
location: PRIVATE-ADDRESS 12 Phố Huế
phone: PRIVATE-PHONE-0900000000
---

## Directives

<!-- observed: 2026-08-15 | status: active | tier: public -->

- Always trả lời bằng tiếng Việt.

<!-- observed: 2026-08-15 | status: active | tier: private -->

- Always gửi hoá đơn tới PRIVATE-DIRECTIVE địa chỉ nhà.

<!-- observed: 2026-08-01 | status: superseded | tier: public -->

- Prefer SUPERSEDED-RULE báo cáo thật dài.
"#;

/// Every marker that must never appear in a group-chat render.
const PRIVATE_MARKERS: &[&str] = &[
    "PRIVATE-EMAIL",
    "PRIVATE-ADDRESS",
    "PRIVATE-PHONE",
    "PRIVATE-DIRECTIVE",
];

fn render_for(jid: &str) -> Option<String> {
    let profile = user_profile::parse::parse(PROFILE);
    user_profile::render(&profile, ProfileScope::for_instance(jid))
}

#[test]
fn group_chats_never_receive_private_fields() {
    for jid in GROUP_JIDS {
        let out = render_for(jid).unwrap_or_default();
        for marker in PRIVATE_MARKERS {
            assert!(
                !out.contains(marker),
                "JID {jid} leaked {marker} into a group chat:\n{out}"
            );
        }
    }
}

#[test]
fn unknown_jids_fail_closed() {
    // A channel added later mints a shape nobody here anticipated. Getting
    // less context is survivable; publishing a home address is not.
    for jid in UNKNOWN_JIDS {
        assert_eq!(
            ProfileScope::for_instance(jid),
            ProfileScope::PublicOnly,
            "unrecognised JID {jid:?} must not resolve to Full"
        );
        let out = render_for(jid).unwrap_or_default();
        for marker in PRIVATE_MARKERS {
            assert!(!out.contains(marker), "JID {jid:?} leaked {marker}");
        }
    }
}

#[test]
fn group_chats_still_receive_the_public_half() {
    // The tier split is only worth its complexity if the public half survives
    // — otherwise we may as well omit the profile in groups entirely.
    for jid in GROUP_JIDS {
        let out = render_for(jid).expect("public fields should still render");
        assert!(out.contains("Nguyễn Văn A"), "{jid}: name missing");
        assert!(out.contains("Asia/Ho_Chi_Minh"), "{jid}: timezone missing");
        assert!(out.contains("tiếng Việt"), "{jid}: public directive missing");
    }
}

#[test]
fn private_chats_receive_everything() {
    for jid in PRIVATE_JIDS {
        let out = render_for(jid).expect("profile should render");
        for marker in PRIVATE_MARKERS {
            assert!(out.contains(marker), "JID {jid} withheld {marker} from the owner");
        }
    }
}

#[test]
fn superseded_directives_reach_nobody() {
    // Two contradictory active rules is the failure the status field exists
    // to prevent; rendering a superseded one recreates it.
    for jid in GROUP_JIDS.iter().chain(PRIVATE_JIDS).chain(UNKNOWN_JIDS) {
        let out = render_for(jid).unwrap_or_default();
        assert!(
            !out.contains("SUPERSEDED-RULE"),
            "JID {jid} rendered a superseded directive:\n{out}"
        );
    }
}

#[test]
fn group_renders_never_hint_that_something_was_withheld() {
    // "(email hidden)" tells the model there is data to go and ask for. In a
    // group chat the person it would ask is a stranger.
    for jid in GROUP_JIDS {
        let out = render_for(jid).unwrap_or_default().to_lowercase();
        for hint in ["ẩn", "hidden", "redacted", "withheld", "private"] {
            assert!(!out.contains(hint), "JID {jid} hinted at withheld data: {out}");
        }
    }
}

#[test]
fn tools_notes_are_private_only() {
    // TOOLS.md holds SSH hosts and internal IPs — same gate as the private
    // half of the profile, enforced separately because it is a separate file.
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = senclaw::config::Config::from_env();
    cfg.paths.tools_notes_path = dir.path().join("TOOLS.md");
    std::fs::write(
        &cfg.paths.tools_notes_path,
        "home-server → 192.168.1.100 SECRET-HOST",
    )
    .unwrap();

    for jid in PRIVATE_JIDS {
        let block = user_profile::tools_notes_block(&cfg, jid);
        assert!(block.is_some(), "{jid}: owner should see local notes");
    }
    for jid in GROUP_JIDS.iter().chain(UNKNOWN_JIDS) {
        assert!(
            user_profile::tools_notes_block(&cfg, jid).is_none(),
            "JID {jid} leaked TOOLS.md"
        );
    }
}

#[test]
fn every_group_jid_sample_is_actually_recognised_as_a_group() {
    // Guards the guard: if a sample stopped matching the `:group:` convention
    // it would resolve to PublicOnly for the wrong reason (fail-closed
    // fallback), and the leak tests above would pass without testing anything.
    for jid in GROUP_JIDS {
        assert!(
            jid.contains(":group:"),
            "sample {jid} no longer matches the group convention — update the \
             scope resolver, not just this list"
        );
    }
}
