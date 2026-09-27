//! Message format conversion for agent input. Mirrors `src-old/agent/SessionBridge.ts`.
//!
//! Fetches recent group messages from SQLite and formats them as XML.

use crate::db::Db;
use crate::types::StoredMessage;

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Format [`StoredMessage`] list into an XML string for agent input.
/// Returns empty string when the list is empty.
///
/// A message with attachments gets an `attachments="…"` note listing them.
/// The files themselves travel separately (as image blocks, OCR text, or
/// extracted document text) — this only tells the model *which* message in the
/// batch they came with, which is otherwise unrecoverable once several
/// messages are folded into one turn.
pub fn format_messages_for_agent(messages: &[StoredMessage]) -> String {
    if messages.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = messages
        .iter()
        .map(|m| {
            let sender = if m.is_bot_reply {
                "assistant".to_string()
            } else {
                escape_xml(&m.sender_name)
            };
            let attachments = m.parsed_attachments();
            let note = if attachments.is_empty() {
                String::new()
            } else {
                let names: Vec<String> = attachments
                    .iter()
                    .map(|a| {
                        let label = a.name.as_deref().unwrap_or(&a.mime_type);
                        escape_xml(label)
                    })
                    .collect();
                format!(" attachments=\"{}\"", names.join(", "))
            };
            format!(
                "<message sender=\"{}\" time=\"{}\"{}>{}</message>",
                sender,
                m.timestamp,
                note,
                escape_xml(&m.content)
            )
        })
        .collect();
    format!("<messages>\n{}\n</messages>", lines.join("\n"))
}

/// Everything a channel-driven turn needs: the formatted prompt, the timestamp
/// cursor to advance to, and the attachments carried by the messages in it.
pub struct GroupPrompt {
    pub prompt: String,
    pub last_timestamp: Option<String>,
    pub attachments: Vec<crate::types::MessageAttachment>,
}

/// Load messages after the last agent timestamp for a group, and format for agent input.
pub fn build_prompt_for_group(db: &Db, chat_jid: &str) -> (String, Option<String>) {
    let built = build_group_prompt(db, chat_jid);
    (built.prompt, built.last_timestamp)
}

/// As [`build_prompt_for_group`], but also collects the attachments of the
/// messages that went into the prompt.
///
/// Attachments from the bot's own replies are skipped — re-feeding an image the
/// agent itself sent would have it analyse its own output.
pub fn build_group_prompt(db: &Db, chat_jid: &str) -> GroupPrompt {
    let since = db.get_last_agent_timestamp(chat_jid).ok().flatten();
    let messages = db
        .get_group_messages(chat_jid, since.as_deref())
        .unwrap_or_default();
    let last_timestamp = messages.last().map(|m| m.timestamp.clone());
    let attachments = messages
        .iter()
        .filter(|m| !m.is_bot_reply)
        .flat_map(|m| m.parsed_attachments())
        .collect();
    GroupPrompt {
        prompt: format_messages_for_agent(&messages),
        last_timestamp,
        attachments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::MessageAttachment;

    fn msg(id: &str, content: &str, attachments: Option<&str>) -> StoredMessage {
        StoredMessage {
            message_id: id.into(),
            chat_jid: "tg:1:user:2".into(),
            sender_jid: "tg:1:user:2".into(),
            sender_name: "Bến".into(),
            content: content.into(),
            timestamp: "2026-08-14T10:00:00Z".into(),
            is_from_me: false,
            is_bot_reply: false,
            reply_to_id: None,
            media_type: None,
            attachments: attachments.map(str::to_string),
        }
    }

    fn one_attachment(name: &str) -> String {
        serde_json::to_string(&vec![MessageAttachment {
            data_url: "data:image/jpeg;base64,QUJD".into(),
            mime_type: "image/jpeg".into(),
            name: Some(name.into()),
        }])
        .unwrap()
    }

    #[test]
    fn attachments_are_noted_on_the_message_that_carried_them() {
        // Several channel messages fold into one turn, so without this the
        // model cannot tell which message the image belonged to.
        let xml = format_messages_for_agent(&[
            msg("1", "xem giúp tôi", Some(&one_attachment("hoa-don.jpg"))),
            msg("2", "cảm ơn", None),
        ]);
        assert!(xml.contains(r#"attachments="hoa-don.jpg""#));
        assert_eq!(xml.matches("attachments=").count(), 1);
    }

    #[test]
    fn attachment_labels_are_xml_escaped() {
        let raw = serde_json::to_string(&vec![MessageAttachment {
            data_url: "data:image/png;base64,QUJD".into(),
            mime_type: "image/png".into(),
            name: Some(r#"a"&<b>.png"#.into()),
        }])
        .unwrap();
        let xml = format_messages_for_agent(&[msg("1", "hi", Some(&raw))]);
        assert!(!xml.contains(r#"a"&<b>"#), "raw markup leaked: {xml}");
        assert!(xml.contains("&quot;&amp;&lt;b&gt;"));
    }

    #[test]
    fn unnamed_attachments_fall_back_to_the_mime() {
        let raw = serde_json::to_string(&vec![MessageAttachment {
            data_url: "data:image/png;base64,QUJD".into(),
            mime_type: "image/png".into(),
            name: None,
        }])
        .unwrap();
        assert!(format_messages_for_agent(&[msg("1", "hi", Some(&raw))])
            .contains(r#"attachments="image/png""#));
    }

    #[test]
    fn malformed_attachment_json_does_not_break_history() {
        // Rows predate the column, or a future client wrote a shape we can't
        // read. Either way history has to keep loading.
        let xml = format_messages_for_agent(&[
            msg("1", "a", Some("{not json")),
            msg("2", "b", Some("")),
            msg("3", "c", None),
        ]);
        assert!(!xml.contains("attachments="));
        assert!(xml.contains(">a<") && xml.contains(">b<") && xml.contains(">c<"));
    }

    #[test]
    fn bot_replies_do_not_contribute_attachments() {
        // Re-feeding an image the agent itself sent would have it analyse its
        // own output.
        let mut reply = msg("2", "đây nhé", Some(&one_attachment("out.png")));
        reply.is_bot_reply = true;
        let user = msg("1", "vẽ giúp tôi", Some(&one_attachment("in.png")));

        let collected: Vec<MessageAttachment> = [user, reply]
            .iter()
            .filter(|m| !m.is_bot_reply)
            .flat_map(|m| m.parsed_attachments())
            .collect();
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].name.as_deref(), Some("in.png"));
    }
}
