//! Render a profile into the prompt block, honouring the tier rule.
//!
//! **This module is the only place the tier rule is enforced.** Every consumer
//! — first-turn injection, the MCP tool, the REST layer, Space-App bridge —
//! goes through [`render`]. Letting each caller filter for itself guarantees
//! that one of them eventually forgets, and the failure mode is the owner's
//! email address appearing in a group chat.

use super::parse::{Tier, UserProfile};
use crate::util::text::truncate_on_char_boundary;

/// How much of the profile a given chat context may see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileScope {
    /// Inject nothing.
    None,
    /// Identity and preferences only — safe in a room with strangers.
    PublicOnly,
    /// Everything, including contact details.
    Full,
}

impl ProfileScope {
    fn allows(self, tier: Tier) -> bool {
        match self {
            ProfileScope::None => false,
            ProfileScope::PublicOnly => tier == Tier::Public,
            ProfileScope::Full => true,
        }
    }

    /// Decide the scope for a chat instance from its JID.
    ///
    /// Every channel adapter already encodes the chat kind in the JID it
    /// mints — `tg:<bot>:user:<id>` vs `tg:<bot>:group:<id>`
    /// (`channels/telegram.rs`), `feishu:user:` vs `feishu:group:`
    /// (`channels/feishu/helpers.rs`), `wx:user:` (p2p only),
    /// `app:<cid>:user:<sender>` (`channels/app.rs`). So the JID alone is
    /// enough and no `ChatType` has to be threaded down through the engine.
    ///
    /// Unrecognised shapes get `PublicOnly`, never `Full`. Being wrong in the
    /// cautious direction costs the agent some context; being wrong the other
    /// way publishes personal data to a room of strangers.
    pub fn for_instance(instance_id: &str) -> Self {
        // The owner's own surfaces: the web UI and the paired mobile app.
        if instance_id.starts_with("web:") || instance_id.starts_with("app:") {
            return ProfileScope::Full;
        }
        // Check group before user: a JID carrying both segments is a group.
        if instance_id.contains(":group:") {
            return ProfileScope::PublicOnly;
        }
        if instance_id.contains(":user:") {
            return ProfileScope::Full;
        }
        ProfileScope::PublicOnly
    }
}

/// Character budget for the rendered block.
///
/// Matches the separate, deliberately small budget OpenClaw gives `USER.md`
/// (~4k) rather than its 20k general per-file cap: this block rides along on
/// every session, and a profile that needs more than a few thousand
/// characters has stopped being a profile.
pub const MAX_PROFILE_CHARS: usize = 4_000;

/// Render the `<user_profile>` block, or `None` when there is nothing the
/// scope is allowed to see.
pub fn render(profile: &UserProfile, scope: ProfileScope) -> Option<String> {
    if scope == ProfileScope::None {
        return None;
    }

    let mut ident: Vec<String> = Vec::new();
    for f in &profile.fields {
        if f.value.trim().is_empty() || !scope.allows(f.tier) {
            continue;
        }
        ident.push(format!("{}: {}", label_for(&f.key), f.value.trim()));
    }

    let directives: Vec<&str> = profile
        .active_directives()
        .filter(|d| scope.allows(d.tier))
        .map(|d| d.text.as_str())
        .collect();

    if ident.is_empty() && directives.is_empty() {
        return None;
    }

    let mut body = String::new();
    if !ident.is_empty() {
        body.push_str("Người dùng — ");
        body.push_str(&ident.join(" · "));
        body.push('\n');
    }
    if !directives.is_empty() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str("Quy tắc người dùng đã đặt:\n");
        for d in &directives {
            body.push_str("- ");
            body.push_str(d);
            body.push('\n');
        }
    }

    // Deliberately no "(some fields hidden)" marker. Telling the model that
    // withheld data exists just makes it ask the group chat for it.
    let body = truncate_on_char_boundary(body.trim(), MAX_PROFILE_CHARS);

    Some(format!("<user_profile>\n{body}\n</user_profile>"))
}

/// Human labels so the model does not have to interpret snake_case keys.
fn label_for(key: &str) -> &str {
    match key {
        "name" => "tên",
        "preferred_name" => "xưng hô",
        "pronouns" => "đại từ",
        "language" => "ngôn ngữ",
        "timezone" => "múi giờ",
        "occupation" => "nghề nghiệp",
        "email" => "email",
        "location" => "địa điểm",
        "phone" => "điện thoại",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::super::parse::parse;
    use super::*;

    const SAMPLE: &str = r#"---
name: Nguyễn Văn A
preferred_name: anh A
email: a@example.com
location: Hà Nội
timezone: Asia/Ho_Chi_Minh
---

## Directives

<!-- observed: 2026-08-15 | status: active | tier: public -->

- Always trả lời bằng tiếng Việt.

<!-- observed: 2026-08-01 | status: superseded | tier: public -->

- Prefer báo cáo dài.

<!-- observed: 2026-08-15 | status: active | tier: private -->

- Always gửi hoá đơn tới email cá nhân.
"#;

    #[test]
    fn full_scope_includes_contact_details() {
        let out = render(&parse(SAMPLE), ProfileScope::Full).unwrap();
        assert!(out.contains("a@example.com"));
        assert!(out.contains("Hà Nội"));
        assert!(out.contains("Nguyễn Văn A"));
    }

    #[test]
    fn public_scope_withholds_contact_details() {
        let out = render(&parse(SAMPLE), ProfileScope::PublicOnly).unwrap();
        assert!(out.contains("Nguyễn Văn A"), "name is public");
        assert!(out.contains("Asia/Ho_Chi_Minh"), "timezone is public");
        assert!(!out.contains("a@example.com"), "email leaked: {out}");
        assert!(!out.contains("Hà Nội"), "location leaked: {out}");
    }

    #[test]
    fn public_scope_withholds_private_directives() {
        let out = render(&parse(SAMPLE), ProfileScope::PublicOnly).unwrap();
        assert!(out.contains("tiếng Việt"));
        assert!(!out.contains("hoá đơn"), "private directive leaked: {out}");
    }

    #[test]
    fn superseded_directives_are_never_rendered() {
        // Injecting both sides of a changed preference recreates exactly the
        // bug the status field exists to prevent.
        for scope in [ProfileScope::Full, ProfileScope::PublicOnly] {
            let out = render(&parse(SAMPLE), scope).unwrap();
            assert!(
                !out.contains("báo cáo dài"),
                "superseded leaked at {scope:?}"
            );
        }
    }

    #[test]
    fn none_scope_renders_nothing() {
        assert!(render(&parse(SAMPLE), ProfileScope::None).is_none());
    }

    #[test]
    fn empty_profile_renders_nothing() {
        // A fresh install must not push an empty block into every prompt.
        assert!(render(&parse(&super::super::parse::template()), ProfileScope::Full).is_none());
        assert!(render(&parse(""), ProfileScope::Full).is_none());
    }

    #[test]
    fn does_not_announce_that_fields_were_hidden() {
        let out = render(&parse(SAMPLE), ProfileScope::PublicOnly).unwrap();
        for marker in ["ẩn", "hidden", "redact", "withheld"] {
            assert!(!out.to_lowercase().contains(marker), "leak hint: {out}");
        }
    }

    #[test]
    fn budget_is_capped_on_a_char_boundary() {
        let mut p = parse("---\nname: A\n---\n");
        for i in 0..500 {
            p.add_directive(
                &format!("Always làm việc thứ {i} với chữ có dấu tiếng Việt"),
                "2026-08-15",
                Tier::Public,
                None,
            );
        }
        let out = render(&p, ProfileScope::Full).unwrap();
        // Wrapper tags sit outside the budget; the body is what is capped.
        assert!(out.len() < MAX_PROFILE_CHARS + 64, "len {}", out.len());
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    // ===== scope resolution =====

    #[test]
    fn owner_surfaces_get_full_scope() {
        for jid in ["web:main", "web:reminders:main", "app:c1:user:u9"] {
            assert_eq!(ProfileScope::for_instance(jid), ProfileScope::Full, "{jid}");
        }
    }

    #[test]
    fn direct_messages_get_full_scope() {
        for jid in [
            "tg:123456:user:99",
            "feishu:user:ou_abc",
            "wx:user:wxid_abc",
        ] {
            assert_eq!(ProfileScope::for_instance(jid), ProfileScope::Full, "{jid}");
        }
    }

    #[test]
    fn group_chats_never_get_full_scope() {
        for jid in [
            "tg:123456:group:-100999",
            "feishu:group:oc_abc",
            "qq:group:12345",
        ] {
            assert_eq!(
                ProfileScope::for_instance(jid),
                ProfileScope::PublicOnly,
                "{jid}"
            );
        }
    }

    #[test]
    fn unknown_jid_shape_fails_closed() {
        for jid in ["", "gibberish", "cowork:team-1", "virtual:worker-3"] {
            assert_eq!(
                ProfileScope::for_instance(jid),
                ProfileScope::PublicOnly,
                "{jid} should fail closed"
            );
        }
    }
}
