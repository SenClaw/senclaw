use crate::channels::Channel;

use super::channel::WeChatChannel;
use super::helpers::{
    extract_text, jid_to_user_id, markdown_to_plain, split_text, user_id_to_jid, ITEM_TYPE_IMAGE,
    ITEM_TYPE_TEXT, ITEM_TYPE_VOICE,
};
use super::types::{WeixinMessageItem, WeixinTextItem};

#[test]
fn test_jid_roundtrip() {
    let jid = user_id_to_jid("user123@im.wechat");
    assert_eq!(jid, "wx:user:user123@im.wechat");
    assert_eq!(jid_to_user_id(&jid), Some("user123@im.wechat"));
    assert!(jid_to_user_id("wx:invalid").is_none());
}

#[test]
fn test_owns_jid() {
    let ch = WeChatChannel::new("test".into(), None::<String>);
    assert!(ch.owns_jid("wx:user:abc123"));
    assert!(!ch.owns_jid("tg:123:user:456"));
}

#[test]
fn test_markdown_to_plain_simple() {
    let md = "**bold** and *italic* and ~~strike~~";
    let plain = markdown_to_plain(md);
    assert!(!plain.contains("**"));
    assert!(!plain.contains('*'));
}

#[test]
fn test_markdown_to_plain_link() {
    let md = "[click here](https://example.com) some text";
    let plain = markdown_to_plain(md);
    assert!(plain.contains("click here"));
    assert!(!plain.contains("https://example.com"));
}

#[test]
fn test_markdown_to_plain_heading() {
    let md = "# Heading\n\nContent";
    let plain = markdown_to_plain(md);
    assert!(plain.contains("Heading"));
    assert!(!plain.contains('#'));
}

#[test]
fn test_extract_text() {
    let items = vec![WeixinMessageItem {
        item_type: Some(ITEM_TYPE_TEXT),
        text_item: Some(WeixinTextItem {
            text: Some("hello".into()),
        }),
        ..Default::default()
    }];
    let text = extract_text(Some(&items));
    assert_eq!(text, "hello");
}

#[test]
fn test_extract_text_image() {
    let items = vec![WeixinMessageItem {
        item_type: Some(ITEM_TYPE_IMAGE),
        ..Default::default()
    }];
    let text = extract_text(Some(&items));
    assert_eq!(text, "[Image]");
}

#[test]
fn test_extract_text_voice() {
    let items = vec![WeixinMessageItem {
        item_type: Some(ITEM_TYPE_VOICE),
        voice_item: Some(WeixinTextItem {
            text: Some("hello".into()),
        }),
        ..Default::default()
    }];
    let text = extract_text(Some(&items));
    assert!(text.contains("[Voice]"));
}

#[test]
fn test_extract_text_empty() {
    let text = extract_text(None);
    assert!(text.is_empty());
}

#[test]
fn test_split_text_short() {
    let parts = split_text("short message", 50);
    assert_eq!(parts.len(), 1);
}

#[test]
fn test_split_text_long() {
    let long = "a".repeat(150);
    let parts = split_text(&long, 50);
    assert!(parts.len() > 1);
}

#[test]
fn test_split_text_empty() {
    let parts = split_text("", 50);
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0], "");
}
