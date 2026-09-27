//! Parse and serialize `USER.md`.
//!
//! The file has two halves, because the data does:
//!
//! * **Front matter** — profile fields the user types into a form
//!   (`name`, `email`, `location`, …). Stable, validated, round-trips to the
//!   Settings UI.
//! * **Directives** — behaviour preferences the *agent* records over time,
//!   each carrying an observation date and an `active` / `superseded` status.
//!
//! The directive shape is borrowed from OpenClaw, and so is the rule that
//! matters most: when a preference changes you mark the old entry
//! `superseded` and write the new one, rather than appending a second
//! contradictory `active` line. Two contradictory actives and the model picks
//! whichever it reads first — usually the stale one.
//!
//! Every field carries a [`Tier`]. Anything not explicitly marked `public` is
//! `private`, so a field someone adds later is protected by default rather
//! than by remembering to protect it.

use serde::{Deserialize, Serialize};

/// Who may see a field or directive.
///
/// This is not an access-control list — the file is readable by every agent
/// and every UI. It decides what goes into a *prompt* for a given chat
/// context, which is where the leak would happen: a group chat's model
/// repeating the owner's home address to everyone in the room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Safe anywhere, including group chats: name, how to address them,
    /// language, timezone. These are what make the agent useful at all.
    Public,
    /// Private conversations only: email, phone, address.
    #[default]
    Private,
}

impl Tier {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "public" => Tier::Public,
            // Anything unrecognised is private. A typo must not downgrade
            // protection.
            _ => Tier::Private,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Tier::Public => "public",
            Tier::Private => "private",
        }
    }
}

/// Fields that default to `public` when the file does not say otherwise.
///
/// Chosen by what a stranger in a group chat learning it would cost: knowing
/// the owner is called "anh A" and writes Vietnamese costs nothing and makes
/// every reply better. Knowing their email is what gets harvested.
const DEFAULT_PUBLIC_FIELDS: &[&str] = &[
    "name",
    "preferred_name",
    "pronouns",
    "language",
    "timezone",
    "occupation",
];

/// One profile field: `key: value` plus its tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub tier: Tier,
}

/// Lifecycle status of a directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DirectiveStatus {
    #[default]
    Active,
    Superseded,
}

impl DirectiveStatus {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "superseded" => DirectiveStatus::Superseded,
            _ => DirectiveStatus::Active,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            DirectiveStatus::Active => "active",
            DirectiveStatus::Superseded => "superseded",
        }
    }
}

/// One behaviour rule, e.g. `Always reply in Vietnamese.`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Directive {
    pub text: String,
    /// `YYYY-MM-DD` the preference was observed. Free-form on read — a hand-
    /// edited file with a malformed date still parses; we never compute on it.
    pub observed: String,
    #[serde(default)]
    pub status: DirectiveStatus,
    /// Directives default to `public`: they shape behaviour ("be concise"),
    /// they are not identifying data.
    #[serde(default = "public_tier")]
    pub tier: Tier,
}

fn public_tier() -> Tier {
    Tier::Public
}

/// A parsed `USER.md`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserProfile {
    pub fields: Vec<Field>,
    pub directives: Vec<Directive>,
    /// Prose outside the recognised sections, kept verbatim so a hand-written
    /// note survives a UI round-trip instead of being silently deleted.
    #[serde(default)]
    pub notes: String,
}

impl UserProfile {
    pub fn is_empty(&self) -> bool {
        self.fields.iter().all(|f| f.value.trim().is_empty())
            && self.directives.is_empty()
            && self.notes.trim().is_empty()
    }

    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|f| f.key == key)
            .map(|f| f.value.as_str())
            .filter(|v| !v.trim().is_empty())
    }

    /// Set (or insert) a field, keeping any tier already chosen for it.
    pub fn set_field(&mut self, key: &str, value: &str) {
        if let Some(f) = self.fields.iter_mut().find(|f| f.key == key) {
            f.value = value.to_string();
            return;
        }
        self.fields.push(Field {
            key: key.to_string(),
            value: value.to_string(),
            tier: default_tier_for(key),
        });
    }

    /// Record a new directive, superseding any active one that conflicts.
    ///
    /// `supersede_matching` is compared case-insensitively against existing
    /// active directives; every match is flipped to `superseded` before the
    /// new entry is appended. Passing `None` supersedes nothing — used when
    /// the preference is genuinely new rather than a change of mind.
    ///
    /// This is the whole point of the status field. Appending without
    /// superseding leaves two contradictory actives, and the model then
    /// follows whichever it happens to read first.
    pub fn add_directive(
        &mut self,
        text: &str,
        observed: &str,
        tier: Tier,
        supersede_matching: Option<&str>,
    ) {
        if let Some(needle) = supersede_matching {
            let needle = needle.trim().to_lowercase();
            if !needle.is_empty() {
                for d in self.directives.iter_mut() {
                    if d.status == DirectiveStatus::Active
                        && d.text.to_lowercase().contains(&needle)
                    {
                        d.status = DirectiveStatus::Superseded;
                    }
                }
            }
        }
        self.directives.push(Directive {
            text: text.trim().to_string(),
            observed: observed.to_string(),
            status: DirectiveStatus::Active,
            tier,
        });
    }

    pub fn active_directives(&self) -> impl Iterator<Item = &Directive> {
        self.directives
            .iter()
            .filter(|d| d.status == DirectiveStatus::Active)
    }
}

fn default_tier_for(key: &str) -> Tier {
    if DEFAULT_PUBLIC_FIELDS.contains(&key) {
        Tier::Public
    } else {
        Tier::Private
    }
}

/// Parse a `USER.md` document.
///
/// Never fails: a malformed file degrades to whatever could be understood.
/// This is a file humans edit in vim, and refusing to load it because one
/// line is wrong would take the agent's knowledge of its owner away over a
/// typo.
pub fn parse(text: &str) -> UserProfile {
    let mut profile = UserProfile::default();
    let (front, body) = split_front_matter(text);

    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_string();
        if key.is_empty() {
            continue;
        }
        // Trailing `# tier: public` comment overrides the default tier.
        let (value, tier) = match rest.split_once('#') {
            Some((v, comment)) => {
                let tier = comment
                    .split_once("tier:")
                    .map(|(_, t)| Tier::parse(t))
                    .unwrap_or_else(|| default_tier_for(&key));
                (v, tier)
            }
            None => (rest, default_tier_for(&key)),
        };
        profile.fields.push(Field {
            key,
            value: unquote(value.trim()).to_string(),
            tier,
        });
    }

    parse_body(body, &mut profile);
    profile
}

/// Split `---`-delimited front matter from the rest.
///
/// A file with no front matter is entirely body — that is what a
/// hand-written `USER.md` copied from OpenClaw looks like, and it should
/// still parse.
fn split_front_matter(text: &str) -> (&str, &str) {
    let trimmed = text.trim_start_matches(['\u{feff}', '\n', '\r']);
    if !trimmed.starts_with("---") {
        return ("", text);
    }
    let after = &trimmed[3..];
    let after = after.strip_prefix('\n').unwrap_or(after);
    match after.find("\n---") {
        Some(end) => {
            let front = &after[..end];
            let rest = &after[end + 4..];
            (front, rest.strip_prefix('\n').unwrap_or(rest))
        }
        None => ("", text),
    }
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Metadata comment preceding a directive:
/// `<!-- observed: 2026-08-15 | status: active | tier: public -->`
fn parse_meta_comment(line: &str) -> Option<(String, DirectiveStatus, Tier)> {
    let inner = line
        .trim()
        .strip_prefix("<!--")?
        .strip_suffix("-->")?
        .trim()
        .to_string();
    let mut observed = String::new();
    let mut status = DirectiveStatus::Active;
    let mut tier = Tier::Public;
    for part in inner.split('|') {
        let Some((k, v)) = part.split_once(':') else {
            continue;
        };
        match k.trim().to_ascii_lowercase().as_str() {
            "observed" => observed = v.trim().to_string(),
            "status" => status = DirectiveStatus::parse(v),
            "tier" => tier = Tier::parse(v),
            _ => {}
        }
    }
    Some((observed, status, tier))
}

fn parse_body(body: &str, profile: &mut UserProfile) {
    let mut pending: Option<(String, DirectiveStatus, Tier)> = None;
    let mut notes: Vec<&str> = Vec::new();
    let mut in_directives = false;
    // Multi-line `<!-- … -->` blocks. Without this, a commented-out example
    // directive (which is exactly how the template documents the format) gets
    // parsed as a real one and injected into every prompt.
    let mut in_comment = false;

    for line in body.lines() {
        let t = line.trim();

        if in_comment {
            if t.contains("-->") {
                in_comment = false;
            }
            continue;
        }
        // Opened but not closed on this line → everything until `-->` is a
        // comment. Single-line `<!-- … -->` falls through to the metadata
        // parser below.
        if t.starts_with("<!--") && !t.contains("-->") {
            in_comment = true;
            continue;
        }

        if let Some(h) = t.strip_prefix("## ") {
            in_directives = h.trim().eq_ignore_ascii_case("directives");
            if !in_directives {
                notes.push(line);
            }
            continue;
        }
        // The H1 title is chrome, not content.
        if t.starts_with("# ") {
            continue;
        }

        if let Some(meta) = parse_meta_comment(t) {
            pending = Some(meta);
            continue;
        }

        if let Some(item) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            // A bullet under any other heading is prose; only the Directives
            // section produces directives.
            if !in_directives {
                notes.push(line);
                continue;
            }
            let (observed, status, tier) = pending.take().unwrap_or_default_meta();
            profile.directives.push(Directive {
                text: item.to_string(),
                observed,
                status,
                tier,
            });
            continue;
        }

        if !in_directives && !t.is_empty() {
            notes.push(line);
        }
    }

    profile.notes = notes.join("\n").trim().to_string();
}

/// Extension so `pending.take()` reads cleanly when no metadata line preceded
/// the bullet (a hand-written file that just lists preferences).
trait MetaDefault {
    fn unwrap_or_default_meta(self) -> (String, DirectiveStatus, Tier);
}

impl MetaDefault for Option<(String, DirectiveStatus, Tier)> {
    fn unwrap_or_default_meta(self) -> (String, DirectiveStatus, Tier) {
        self.unwrap_or_else(|| (String::new(), DirectiveStatus::Active, Tier::Public))
    }
}

/// Serialize back to `USER.md`.
///
/// Round-trips: `parse(&serialize(&p)) == p` for any profile this module
/// produced. The Settings UI depends on that — a save must not quietly drop
/// the notes section or a directive's date.
pub fn serialize(profile: &UserProfile) -> String {
    let mut out = String::from("---\n");
    for f in &profile.fields {
        // Only annotate when the tier differs from what parsing would infer,
        // so the common file stays clean.
        if f.tier == default_tier_for(&f.key) {
            out.push_str(&format!("{}: {}\n", f.key, f.value));
        } else {
            out.push_str(&format!(
                "{}: {}  # tier: {}\n",
                f.key,
                f.value,
                f.tier.as_str()
            ));
        }
    }
    // The header is chrome that `parse` skips (`# ` line) and the blurb is a
    // comment, so neither comes back as `notes` on the next read. Emitting
    // them as plain prose would fold them into the user's own notes and then
    // duplicate them on every save.
    out.push_str("---\n\n# USER.md — Hồ sơ người dùng\n\n");
    out.push_str("<!-- Thông tin về người dùng SenClaw. Agent đọc file này để biết chủ là ai. -->\n\n");

    if !profile.notes.trim().is_empty() {
        out.push_str(profile.notes.trim());
        out.push_str("\n\n");
    }

    out.push_str("## Directives\n\n");
    for d in &profile.directives {
        out.push_str(&format!(
            "<!-- observed: {} | status: {} | tier: {} -->\n\n- {}\n\n",
            if d.observed.is_empty() {
                "unknown"
            } else {
                &d.observed
            },
            d.status.as_str(),
            d.tier.as_str(),
            d.text
        ));
    }
    out
}

/// Template written when no `USER.md` exists yet.
pub fn template() -> String {
    "---\n\
     name:\n\
     preferred_name:\n\
     pronouns:\n\
     language:\n\
     timezone:\n\
     occupation:\n\
     email:\n\
     location:\n\
     ---\n\
     \n\
     # USER.md — Hồ sơ người dùng\n\
     \n\
     <!--\n\
     Thông tin về người dùng SenClaw. Agent đọc file này để biết chủ là ai.\n\
     \n\
     Trường ở phần đầu: `name`, `preferred_name`, `pronouns`, `language`,\n\
     `timezone`, `occupation` mặc định PUBLIC — mọi ngữ cảnh đều thấy, kể cả\n\
     nhóm chat. Mọi trường khác (email, location, …) mặc định PRIVATE, chỉ\n\
     hiện trong hội thoại riêng tư. Ghi đè bằng `# tier: public` cuối dòng.\n\
     \n\
     Mỗi directive gồm một dòng metadata và một câu mệnh lệnh\n\
     (Always / Never / Prefer). Khi đổi ý: đánh dấu mục cũ `superseded` rồi\n\
     viết mục mới — ĐỪNG thêm một mục `active` mâu thuẫn, model sẽ bốc trúng\n\
     cái cũ. Ví dụ, bỏ comment để dùng:\n\
     \n\
     observed: 2026-01-01 | status: active | tier: public\n\
     - Prefer trả lời ngắn gọn, đi thẳng vào kết quả.\n\
     -->\n\
     \n\
     ## Directives\n"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"---
name: Nguyễn Văn A
preferred_name: anh A
email: a@example.com
location: Hà Nội, Việt Nam
timezone: Asia/Ho_Chi_Minh
---

# USER.md — Hồ sơ người dùng

## Directives

<!-- observed: 2026-08-15 | status: active | tier: public -->

- Always trả lời bằng tiếng Việt.

<!-- observed: 2026-08-10 | status: superseded | tier: public -->

- Prefer báo cáo chi tiết từng bước.
"#;

    #[test]
    fn parses_fields_and_directives() {
        let p = parse(SAMPLE);
        assert_eq!(p.field("name"), Some("Nguyễn Văn A"));
        assert_eq!(p.field("email"), Some("a@example.com"));
        assert_eq!(p.directives.len(), 2);
        assert_eq!(p.active_directives().count(), 1);
    }

    #[test]
    fn identity_fields_public_contact_fields_private() {
        // The split that keeps a group chat from learning the owner's email.
        let p = parse(SAMPLE);
        let tier = |k: &str| p.fields.iter().find(|f| f.key == k).unwrap().tier;
        assert_eq!(tier("name"), Tier::Public);
        assert_eq!(tier("preferred_name"), Tier::Public);
        assert_eq!(tier("timezone"), Tier::Public);
        assert_eq!(tier("email"), Tier::Private);
        assert_eq!(tier("location"), Tier::Private);
    }

    #[test]
    fn unknown_field_defaults_to_private() {
        // A field nobody anticipated must be protected by default, not by
        // someone remembering to add it to the public list.
        let p = parse("---\nhome_address: 12 Phố Huế\n---\n");
        assert_eq!(p.fields[0].tier, Tier::Private);
    }

    #[test]
    fn inline_tier_comment_overrides_default() {
        let p = parse("---\nemail: a@b.c  # tier: public\nname: A  # tier: private\n---\n");
        let tier = |k: &str| p.fields.iter().find(|f| f.key == k).unwrap().tier;
        assert_eq!(tier("email"), Tier::Public);
        assert_eq!(tier("name"), Tier::Private);
        assert_eq!(p.field("email"), Some("a@b.c"));
    }

    #[test]
    fn misspelled_tier_falls_back_to_private() {
        let p = parse("---\nemail: a@b.c  # tier: publik\n---\n");
        assert_eq!(p.fields[0].tier, Tier::Private);
    }

    #[test]
    fn no_front_matter_still_parses_directives() {
        // An OpenClaw-style hand-written file.
        let p = parse("# USER.md\n\n## Directives\n\n- Always be concise.\n");
        assert_eq!(p.directives.len(), 1);
        assert_eq!(p.directives[0].status, DirectiveStatus::Active);
    }

    #[test]
    fn add_directive_supersedes_the_conflicting_active_one() {
        // The failure this whole mechanism exists to prevent: two
        // contradictory actives, model follows the stale one.
        let mut p = parse(SAMPLE);
        p.add_directive(
            "Prefer báo cáo ngắn gọn.",
            "2026-08-20",
            Tier::Public,
            Some("báo cáo"),
        );
        let actives: Vec<_> = p.active_directives().map(|d| d.text.as_str()).collect();
        assert!(actives.contains(&"Prefer báo cáo ngắn gọn."));
        assert!(
            !actives.iter().any(|t| t.contains("chi tiết từng bước")),
            "stale directive stayed active: {actives:?}"
        );
    }

    #[test]
    fn add_directive_without_match_supersedes_nothing() {
        let mut p = parse(SAMPLE);
        let before = p.active_directives().count();
        p.add_directive("Never dùng emoji.", "2026-08-20", Tier::Public, None);
        assert_eq!(p.active_directives().count(), before + 1);
    }

    #[test]
    fn round_trips() {
        let p = parse(SAMPLE);
        let p2 = parse(&serialize(&p));
        assert_eq!(p.fields, p2.fields);
        assert_eq!(p.directives, p2.directives);
    }

    #[test]
    fn round_trips_non_default_tier() {
        // The `# tier:` annotation must survive a save, or a user who marked
        // their email public would silently get it re-hidden on next write.
        let p = parse("---\nemail: a@b.c  # tier: public\n---\n");
        let p2 = parse(&serialize(&p));
        assert_eq!(p2.fields[0].tier, Tier::Public);
    }

    #[test]
    fn template_documentation_is_not_captured_as_notes() {
        // Caught on a live daemon: the template's explanatory prose parsed as
        // the user's `notes`, and `serialize` then wrote its own header plus
        // those notes — so the intro line appeared twice, and grew a copy on
        // every save. The docs live in an HTML comment for this reason.
        let p = parse(&template());
        assert_eq!(p.notes, "", "template prose leaked into notes: {:?}", p.notes);
    }

    #[test]
    fn saving_twice_is_idempotent() {
        // The duplication bug above only showed up on the *second* write.
        let once = serialize(&parse(&template()));
        let twice = serialize(&parse(&once));
        assert_eq!(once, twice, "serialize is not a fixed point");
    }

    #[test]
    fn notes_survive_round_trip() {
        let p = parse("---\nname: A\n---\n\n## Context\n\nThích cà phê.\n");
        assert!(p.notes.contains("Thích cà phê"));
        assert!(parse(&serialize(&p)).notes.contains("Thích cà phê"));
    }

    #[test]
    fn template_parses_and_is_empty() {
        // Empty means "nothing to inject" — a fresh install must not push a
        // block of blank fields into every prompt.
        let p = parse(&template());
        assert!(!p.fields.is_empty(), "template should declare field keys");
        assert!(p.field("name").is_none(), "template fields must be blank");
    }

    #[test]
    fn commented_out_directive_is_not_parsed() {
        // The template documents the format with a commented-out example. If
        // the parser saw through the comment, every fresh install would inject
        // a placeholder preference into all its prompts.
        let p = parse(&template());
        assert_eq!(
            p.directives.len(),
            0,
            "template leaked a directive: {:?}",
            p.directives
        );
    }

    #[test]
    fn single_line_meta_comment_still_works() {
        // The multi-line skip must not swallow the metadata form.
        let p = parse(
            "## Directives\n\n<!-- observed: 2026-08-15 | status: active | tier: public -->\n\n- Always be brief.\n",
        );
        assert_eq!(p.directives.len(), 1);
        assert_eq!(p.directives[0].observed, "2026-08-15");
    }

    #[test]
    fn garbage_does_not_panic() {
        for junk in [
            "",
            "---",
            "---\n",
            "<!-- -->",
            "- \n",
            "---\n: novalue\n---\n",
        ] {
            let _ = parse(junk);
        }
    }
}
