//! Pairing policy: what an unknown chat gets, and what approving it does.
//!
//! Before this existed, a Telegram message from a chat with no binding
//! *completed* the channel's pending binding on sight — whoever messaged the
//! bot first owned it, with no time limit and no human in the loop. The bot
//! now answers with a code and waits: only [`approve`] creates a binding, and
//! only a person calls it.
//!
//! The two halves live apart on purpose. The router side ([`request`]) runs on
//! untrusted input and may only ever *record* a request; the approval side runs
//! behind the daemon's authenticated API.

use std::sync::Arc;

use anyhow::{bail, Result};

use crate::db::Db;
use crate::types::{Channel, ChannelPairing, IncomingMessage};

/// What the bot says back to a chat it does not know yet.
///
/// It names the code and where to approve it, and says plainly that nothing is
/// running — otherwise silence is indistinguishable from a broken bot, which is
/// what made the old auto-claim behaviour feel necessary in the first place.
///
/// The routes it offers are the two that cannot go stale: the Settings page and
/// a command typed into any already-connected chat, both served by the daemon
/// that just sent this message. The `senclaw pairing` CLI is deliberately *not*
/// named here — it resolves through `PATH`, and a months-old binary installed
/// there answers "unrecognized subcommand 'pairing'", which reads as the
/// pairing feature being broken rather than the binary being old.
pub fn render_challenge(p: &ChannelPairing) -> String {
    format!(
        "🔒 Chat này chưa được kết nối.\n\n\
         Mã xác thực: {}\n\n\
         Gửi mã này cho chủ nhân SenClaw. Duyệt bằng một trong hai cách:\n\
         • Settings → Channels → Pairing\n\
         • Gõ `pair approve {}` trong bất kỳ chat nào đã kết nối\n\n\
         Mã hết hạn sau 1 giờ. Tôi chưa xử lý tin nhắn nào cho tới khi được duyệt.",
        p.code, p.code
    )
}

/// Which channel row a Telegram message arrived through.
///
/// A channel whose stored `botToken` is empty means "use the global default bot
/// from .env" — which is what the Settings form tells the user. It does **not**
/// mean "any bot": matching an empty stored token against every incoming token
/// let a message from an unrelated bot claim this channel's pending binding.
/// So an empty stored token matches only the configured default.
pub fn resolve_telegram_channel(
    db: &Db,
    incoming_token: &str,
    default_token: &str,
) -> Option<Channel> {
    if incoming_token.is_empty() {
        return None;
    }
    let channels = db.find_channels_by_platform("telegram").ok()?;
    channels.into_iter().find(|ch| {
        let creds: serde_json::Value =
            serde_json::from_str(&ch.credentials_json).unwrap_or_default();
        let stored = creds["botToken"].as_str().unwrap_or("");
        if stored.is_empty() {
            !default_token.is_empty() && incoming_token == default_token
        } else {
            stored == incoming_token
        }
    })
}

/// A recorded request, and whether this call is what created it.
pub struct Requested {
    pub pairing: ChannelPairing,
    /// True when a fresh code was just minted. The router announces on this and
    /// nothing else — see [`request`].
    pub is_new: bool,
}

/// Record (or re-serve) a pairing request for an unbound chat.
///
/// Re-serves the *same* live code on a repeat message rather than minting a new
/// one: a person who messages the bot three times while walking to their laptop
/// must not end up with three codes, two of which the UI shows as separate
/// strangers knocking.
///
/// `is_new` is the flood guard *and* the expiry fix, and it has to be both. A
/// "have we challenged this chat before" flag would go quiet forever after the
/// first code: an hour later `find_live_pairing` stops matching, a **new** code
/// is minted — and the user, still holding the dead one, would be told nothing
/// and have no way to learn the replacement. Keying the announcement to the row
/// rather than to the chat re-announces exactly when there is something new to
/// say.
pub fn request(db: &Db, channel: &Channel, msg: &IncomingMessage) -> Result<Requested> {
    if let Some(live) = db.find_live_pairing(channel.id, &msg.chat_jid)? {
        return Ok(Requested {
            pairing: live,
            is_new: false,
        });
    }
    let chat_type = match msg.chat_type {
        crate::types::ChatType::Private => "user",
        _ => "group",
    };
    let pairing = db.create_pairing(
        channel.id,
        &msg.chat_jid,
        chat_type,
        &msg.sender_jid,
        &msg.sender_name,
        None,
        msg.bot_token.as_deref(),
    )?;
    Ok(Requested {
        pairing,
        is_new: true,
    })
}

/// What approving a request did, for the caller to report back.
#[derive(Debug)]
pub struct Approved {
    pub pairing: ChannelPairing,
    /// Agent folder the chat is now bound to.
    pub agent_folder: String,
    /// True when an existing pending binding was filled in, false when a new
    /// binding was created alongside the channel's existing ones.
    pub filled_pending: bool,
}

/// Turn a pending request into a binding. The only path that grants access.
///
/// Refusals are worded for a person, because all three reach a UI button:
/// an expired code, a code somebody already acted on, and a channel with no
/// agent behind it are different problems with different fixes.
pub fn approve(db: &Db, id: i64) -> Result<Approved> {
    let Some(pairing) = db.get_pairing(id)? else {
        bail!("pairing request {id} không tồn tại");
    };
    if pairing.status != "pending" {
        bail!(
            "pairing request này đã được xử lý rồi (trạng thái: {})",
            pairing.status
        );
    }
    if crate::db::pairings::is_expired(&pairing.expires_at) {
        // Retire it so the list stops offering an Approve button that cannot work.
        let _ = db.resolve_pairing(id, "expired");
        bail!("mã đã hết hạn — bảo người dùng nhắn lại cho bot để lấy mã mới");
    }

    // Prefer filling the channel's pending binding: that is the row the user
    // created in the UI expecting exactly this chat to land in it.
    let pending = db.get_pending_bindings_for_channel(pairing.channel_id)?;
    let (agent_id, filled_pending) = if let Some(b) = pending.first() {
        db.complete_pending_binding(b.id, &pairing.chat_jid)?;
        (b.agent_id, true)
    } else {
        // No pending row — a second person being let into a channel that is
        // already paired. Bind them to the same agent as the existing chats.
        let existing = db.list_bindings_for_channel(pairing.channel_id)?;
        let Some(first) = existing.first() else {
            bail!("channel này chưa gắn agent nào — tạo agent cho nó trước khi duyệt");
        };
        let now = crate::util::local_time::local_iso_string_now();
        db.insert_binding(
            Some(&pairing.chat_jid),
            first.binding.agent_id,
            pairing.channel_id,
            first.binding.bot_token_override.as_deref(),
            first.binding.max_messages,
            &now,
        )?;
        (first.binding.agent_id, false)
    };

    // Narrowed to `status='pending'`, so a second approver gets 0 rows and the
    // error above rather than a silent double-grant.
    if db.resolve_pairing(id, "approved")? == 0 {
        bail!("pairing request này vừa được xử lý bởi một phiên khác");
    }
    let _ = db.expire_other_pairings(pairing.channel_id, &pairing.chat_jid, id);

    let agent_folder = db
        .get_agent(agent_id)
        .ok()
        .flatten()
        .map(|a| a.folder)
        .unwrap_or_default();

    Ok(Approved {
        pairing,
        agent_folder,
        filled_pending,
    })
}

/// Turn a request away. Does not block the chat from asking again — a rejection
/// is "not now", and a real block is the absence of a binding, which is already
/// the default.
pub fn reject(db: &Db, id: i64) -> Result<ChannelPairing> {
    let Some(pairing) = db.get_pairing(id)? else {
        bail!("pairing request {id} không tồn tại");
    };
    if db.resolve_pairing(id, "rejected")? == 0 {
        bail!(
            "pairing request này đã được xử lý rồi (trạng thái: {})",
            pairing.status
        );
    }
    Ok(pairing)
}

/// Approve by the code a person typed, rather than by row id — the shape the
/// CLI and the Settings text box both want.
pub fn approve_by_code(db: &Db, code: &str) -> Result<Approved> {
    let Some(p) = db.find_pending_pairing_by_code(code)? else {
        bail!("không tìm thấy pairing request nào đang chờ với mã này");
    };
    approve(db, p.id)
}

/// Shared handle type for the surfaces that need the DB and nothing else.
pub type PairingDb = Arc<Db>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatType, IncomingMessage};

    fn db_with_channel(bot_token: &str) -> (Db, i64) {
        let db = Db::open_in_memory(&crate::config::Config::from_env()).unwrap();
        let creds = if bot_token.is_empty() {
            "{}".to_string()
        } else {
            format!("{{\"botToken\":\"{bot_token}\"}}")
        };
        let id = db
            .insert_channel("telegram", "TG", &creds, "2026-09-08T00:00:00Z")
            .unwrap();
        (db, id)
    }

    fn agent_and_pending_binding(db: &Db, channel_id: i64) -> i64 {
        let agent_id = db
            .insert_agent("main", "Main", false, None, None, "", None, "2026-09-08T00:00:00Z")
            .unwrap();
        db.insert_binding(None, agent_id, channel_id, None, None, "2026-09-08T00:00:00Z")
            .unwrap();
        agent_id
    }

    fn msg(chat_jid: &str, token: &str) -> IncomingMessage {
        IncomingMessage {
            id: "1".into(),
            chat_jid: chat_jid.into(),
            sender_name: "Alice".into(),
            sender_jid: "tg:99:user:812".into(),
            content: "hello".into(),
            timestamp: "2026-09-08T00:00:00Z".into(),
            is_from_me: false,
            chat_type: ChatType::Private,
            mentions_bot_username: None,
            bot_token: Some(token.into()),
            native_msg_id: None,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn empty_stored_token_matches_only_the_configured_default() {
        let (db, _) = db_with_channel("");
        // The hole this closes: an empty stored token used to match *any*
        // incoming bot, so an unrelated bot could claim this channel.
        assert!(resolve_telegram_channel(&db, "some-other-bot", "the-default").is_none());
        assert!(resolve_telegram_channel(&db, "the-default", "the-default").is_some());
        // No default configured — an empty stored token then matches nothing.
        assert!(resolve_telegram_channel(&db, "anything", "").is_none());
    }

    #[test]
    fn explicit_stored_token_matches_only_itself() {
        let (db, _) = db_with_channel("bot-A");
        assert!(resolve_telegram_channel(&db, "bot-A", "").is_some());
        assert!(resolve_telegram_channel(&db, "bot-B", "bot-B").is_none());
    }

    #[test]
    fn a_message_records_a_request_and_grants_nothing() {
        let (db, ch_id) = db_with_channel("bot-A");
        agent_and_pending_binding(&db, ch_id);
        let channel = db.get_channel(ch_id).unwrap().unwrap();

        let p = request(&db, &channel, &msg("tg:99:user:812", "bot-A")).unwrap().pairing;
        assert_eq!(p.status, "pending");
        assert_eq!(p.sender_name, "Alice");
        // The binding is still pending — the message did not claim it.
        assert!(db.get_binding_by_jid("tg:99:user:812").unwrap().is_none());
        assert_eq!(db.get_pending_bindings_for_channel(ch_id).unwrap().len(), 1);
    }

    #[test]
    fn repeat_messages_reuse_one_code_and_are_announced_once() {
        let (db, ch_id) = db_with_channel("bot-A");
        let channel = db.get_channel(ch_id).unwrap().unwrap();
        let a = request(&db, &channel, &msg("tg:99:user:812", "bot-A")).unwrap();
        let b = request(&db, &channel, &msg("tg:99:user:812", "bot-A")).unwrap();
        // Three "hello"s must not read as three strangers knocking.
        assert_eq!(a.pairing.id, b.pairing.id);
        assert_eq!(a.pairing.code, b.pairing.code);
        assert_eq!(db.list_pairings(true).unwrap().len(), 1);
        // Only the first mints, so only the first is spoken aloud.
        assert!(a.is_new);
        assert!(!b.is_new, "re-serving a live code must not re-announce it");
    }

    #[test]
    fn a_message_after_expiry_mints_a_new_code_and_announces_it() {
        let (db, ch_id) = db_with_channel("bot-A");
        let channel = db.get_channel(ch_id).unwrap().unwrap();
        let first = request(&db, &channel, &msg("tg:99:user:812", "bot-A")).unwrap();
        assert!(first.is_new);

        // An hour passes.
        db.with_conn(|c| {
            c.execute(
                "UPDATE channel_pairings SET expires_at='2000-01-01T00:00:00Z' WHERE id=?1",
                rusqlite::params![first.pairing.id],
            )?;
            Ok(())
        })
        .unwrap();

        let second = request(&db, &channel, &msg("tg:99:user:812", "bot-A")).unwrap();
        assert_ne!(second.pairing.code, first.pairing.code);
        // The bug this pins: keyed on the chat instead of the row, the bot went
        // silent here and the user was left holding a code that no longer works
        // with no way to learn its replacement.
        assert!(
            second.is_new,
            "a freshly minted code must be announced, even to a chat already challenged once"
        );
    }

    #[test]
    fn approval_fills_the_pending_binding() {
        let (db, ch_id) = db_with_channel("bot-A");
        agent_and_pending_binding(&db, ch_id);
        let channel = db.get_channel(ch_id).unwrap().unwrap();
        let p = request(&db, &channel, &msg("tg:99:user:812", "bot-A")).unwrap().pairing;

        let out = approve(&db, p.id).unwrap();
        assert!(out.filled_pending);
        assert_eq!(out.agent_folder, "main");
        assert!(db.get_binding_by_jid("tg:99:user:812").unwrap().is_some());
        assert_eq!(db.get_pending_bindings_for_channel(ch_id).unwrap().len(), 0);
    }

    #[test]
    fn a_second_chat_gets_its_own_binding_on_the_same_agent() {
        let (db, ch_id) = db_with_channel("bot-A");
        let agent_id = agent_and_pending_binding(&db, ch_id);
        let channel = db.get_channel(ch_id).unwrap().unwrap();

        let first = request(&db, &channel, &msg("tg:99:user:1", "bot-A")).unwrap().pairing;
        approve(&db, first.id).unwrap();

        // No pending row left — the second person must still be approvable.
        let second = request(&db, &channel, &msg("tg:99:user:2", "bot-A")).unwrap().pairing;
        let out = approve(&db, second.id).unwrap();
        assert!(!out.filled_pending);
        let b = db.get_binding_by_jid("tg:99:user:2").unwrap().unwrap();
        assert_eq!(b.agent_id, agent_id);
    }

    #[test]
    fn approving_twice_is_refused_rather_than_double_granted() {
        let (db, ch_id) = db_with_channel("bot-A");
        agent_and_pending_binding(&db, ch_id);
        let channel = db.get_channel(ch_id).unwrap().unwrap();
        let p = request(&db, &channel, &msg("tg:99:user:812", "bot-A")).unwrap().pairing;

        approve(&db, p.id).unwrap();
        let err = approve(&db, p.id).unwrap_err().to_string();
        assert!(err.contains("đã được xử lý"), "{err}");
    }

    #[test]
    fn an_expired_code_is_refused_and_retired() {
        let (db, ch_id) = db_with_channel("bot-A");
        agent_and_pending_binding(&db, ch_id);
        let channel = db.get_channel(ch_id).unwrap().unwrap();
        let p = request(&db, &channel, &msg("tg:99:user:812", "bot-A")).unwrap().pairing;
        db.with_conn(|c| {
            c.execute(
                "UPDATE channel_pairings SET expires_at='2000-01-01T00:00:00Z' WHERE id=?1",
                rusqlite::params![p.id],
            )?;
            Ok(())
        })
        .unwrap();

        let err = approve(&db, p.id).unwrap_err().to_string();
        assert!(err.contains("hết hạn"), "{err}");
        // Retired, so the list stops offering an Approve button that cannot work.
        assert_eq!(db.get_pairing(p.id).unwrap().unwrap().status, "expired");
        assert!(db.get_binding_by_jid("tg:99:user:812").unwrap().is_none());
    }

    #[test]
    fn a_channel_with_no_agent_refuses_with_a_fixable_message() {
        let (db, ch_id) = db_with_channel("bot-A");
        let channel = db.get_channel(ch_id).unwrap().unwrap();
        let p = request(&db, &channel, &msg("tg:99:user:812", "bot-A")).unwrap().pairing;
        let err = approve(&db, p.id).unwrap_err().to_string();
        assert!(err.contains("chưa gắn agent"), "{err}");
    }

    #[test]
    fn approve_by_code_ignores_the_formatting_a_human_adds() {
        let (db, ch_id) = db_with_channel("bot-A");
        agent_and_pending_binding(&db, ch_id);
        let channel = db.get_channel(ch_id).unwrap().unwrap();
        let p = request(&db, &channel, &msg("tg:99:user:812", "bot-A")).unwrap().pairing;

        let typed = format!(" {}-{} ", &p.code[..4], &p.code[4..]).to_lowercase();
        let out = approve_by_code(&db, &typed).unwrap();
        assert_eq!(out.pairing.id, p.id);
    }

    #[test]
    fn rejecting_leaves_the_chat_unbound_and_not_reapprovable() {
        let (db, ch_id) = db_with_channel("bot-A");
        agent_and_pending_binding(&db, ch_id);
        let channel = db.get_channel(ch_id).unwrap().unwrap();
        let p = request(&db, &channel, &msg("tg:99:user:812", "bot-A")).unwrap().pairing;

        reject(&db, p.id).unwrap();
        assert!(db.get_binding_by_jid("tg:99:user:812").unwrap().is_none());
        assert!(approve(&db, p.id).is_err());
    }

    #[test]
    fn the_challenge_names_the_code_and_says_nothing_is_running() {
        let (db, ch_id) = db_with_channel("bot-A");
        let channel = db.get_channel(ch_id).unwrap().unwrap();
        let p = request(&db, &channel, &msg("tg:99:user:812", "bot-A")).unwrap().pairing;
        let text = render_challenge(&p);
        assert!(text.contains(&p.code));
        // Silence is indistinguishable from a broken bot; the text has to say
        // that the message was received but deliberately not acted on.
        assert!(text.contains("chưa xử lý"));
        // Both routes are served by this very daemon, so neither can be a
        // version behind the message offering it.
        assert!(text.contains("Settings"), "{text}");
        assert!(text.contains("pair approve"), "{text}");
        // The CLI resolves through PATH and a stale binary there answers
        // "unrecognized subcommand 'pairing'" — which reads as pairing being
        // broken. Never send a user to it from inside the product.
        assert!(!text.contains("senclaw pairing"), "{text}");
    }
}
