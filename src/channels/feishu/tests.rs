use crate::channels::Channel;

use super::channel::FeishuChannel;
use super::helpers::{
    check_bot_mention, jid_to_chat_id, jid_to_receive_id_type, make_jid, parse_media_refs,
    parse_text_content,
    remove_bot_mention_placeholders, split_message,
};
use super::types::{DedupState, FeishuDomain};
use super::FEISHU_MAX_LEN;

#[test]
fn test_jid_roundtrip() {
    assert_eq!(make_jid("p2p", "ou_abc123"), "feishu:user:ou_abc123");
    assert_eq!(make_jid("group", "oc_xyz789"), "feishu:group:oc_xyz789");
    assert_eq!(jid_to_chat_id("feishu:user:ou_abc"), Some("ou_abc"));
    assert_eq!(jid_to_chat_id("feishu:group:oc_xyz"), Some("oc_xyz"));
    assert_eq!(jid_to_chat_id("tg:user:123"), None);
}

#[test]
fn test_receive_id_type() {
    assert_eq!(jid_to_receive_id_type("feishu:user:ou_abc"), "open_id");
    assert_eq!(jid_to_receive_id_type("feishu:group:oc_xyz"), "chat_id");
}

#[test]
fn test_owns_jid() {
    let ch = FeishuChannel::new("app".into(), "secret".into(), None);
    assert!(ch.owns_jid("feishu:user:ou_abc"));
    assert!(ch.owns_jid("feishu:group:oc_xyz"));
    assert!(!ch.owns_jid("tg:123:user:456"));
    assert!(!ch.owns_jid("wx:user:xyz"));
}

#[test]
fn test_domain_base_url() {
    assert_eq!(FeishuDomain::Feishu.base_url(), "https://open.feishu.cn");
    assert_eq!(FeishuDomain::Lark.base_url(), "https://open.larksuite.com");
    assert_eq!(
        FeishuDomain::Custom("https://open.example.com".into()).base_url(),
        "https://open.example.com"
    );
}

#[test]
fn test_parse_text_content_text_type() {
    let content = r#"{"text":"Hello world"}"#;
    let result = parse_text_content(content, "text");
    assert_eq!(result, "Hello world");
}

#[test]
fn test_parse_text_content_post_type() {
    let content = r#"{
        "zh_cn": {
            "title": "Rich Title",
            "content": [
                [{"tag": "text", "text": "Hello"}, {"tag": "text", "text": " World"}],
                [{"tag": "a", "text": "Link", "href": "https://example.com"}],
                [{"tag": "img", "image_key": "xxx"}],
                [{"tag": "at", "user_id": "ou_xxx"}]
            ]
        }
    }"#;
    let result = parse_text_content(content, "post");
    assert!(result.contains("Rich Title"));
    assert!(result.contains("Hello World"));
    assert!(result.contains("Link"));
    assert!(result.contains("[Image]"));
    assert!(!result.contains("@"));
}

#[test]
fn test_parse_invalid_json() {
    let result = parse_text_content("plain text", "text");
    assert_eq!(result, "plain text");
}

#[test]
fn test_split_short() {
    let parts = split_message("hello");
    assert_eq!(parts, vec!["hello"]);
}

#[test]
fn test_split_long() {
    let long = "x".repeat(5000);
    let parts = split_message(&long);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].len(), FEISHU_MAX_LEN);
    assert_eq!(parts[1].len(), 5000 - FEISHU_MAX_LEN);
}

#[test]
fn test_split_at_newline() {
    let mut text = "x".repeat(2500);
    text.push('\n');
    text.push_str(&"y".repeat(2000));
    let parts = split_message(&text);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].len(), 2500);
    assert_eq!(parts[1].len(), 2000);
}

#[test]
fn test_dedup() {
    let mut state = DedupState::new();
    assert!(state.try_record("msg1", "app1"));
    assert!(!state.try_record("msg1", "app1"));
    assert!(state.try_record("msg1", "app2")); // different app
    assert!(state.try_record("msg2", "app1"));
}

#[test]
fn test_check_bot_mention() {
    let mentions = vec![
        serde_json::json!({"key": "@bot", "id": {"open_id": "bot123"}, "name": "Bot"}),
        serde_json::json!({"key": "@user", "id": {"open_id": "user456"}, "name": "User"}),
    ];
    assert!(check_bot_mention(Some(&mentions), "bot123"));
    assert!(!check_bot_mention(Some(&mentions), "other789"));
    assert!(!check_bot_mention(None, "bot123"));
    assert!(!check_bot_mention(Some(&mentions), ""));
}

#[test]
fn test_remove_bot_mention() {
    let mentions =
        vec![serde_json::json!({"key": "@bot", "id": {"open_id": "bot123"}, "name": "Bot"})];
    let text = "@bot hello world";
    let result = remove_bot_mention_placeholders(text, Some(&mentions), "bot123");
    assert_eq!(result, "hello world");
    assert!(!result.contains("@bot"));
}

#[test]
fn test_remove_bot_mention_no_match() {
    let mentions =
        vec![serde_json::json!({"key": "@user", "id": {"open_id": "user456"}, "name": "User"})];
    let text = "@bot hello world";
    let result = remove_bot_mention_placeholders(text, Some(&mentions), "bot123");
    assert_eq!(result, "@bot hello world");
}

#[test]
fn test_feishu_domain_constructor() {
    let ch1 = FeishuChannel::new("a".into(), "s".into(), None);
    assert_eq!(ch1.id(), "feishu");

    let ch2 = FeishuChannel::new("a".into(), "s".into(), Some("lark".into()));
    assert_eq!(ch2.id(), "feishu");
}

#[test]
fn parses_image_and_file_media_refs() {
    let refs = parse_media_refs(r#"{"image_key":"img_v2_abc"}"#, "image");
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].key, "img_v2_abc");
    assert_eq!(refs[0].kind, "image");

    let refs = parse_media_refs(
        r#"{"file_key":"file_v2_x","file_name":"report.pdf"}"#,
        "file",
    );
    assert_eq!(refs[0].kind, "file");
    assert_eq!(refs[0].name.as_deref(), Some("report.pdf"));

    // Audio and video file their blob under file_key too.
    assert_eq!(parse_media_refs(r#"{"file_key":"f1"}"#, "audio")[0].kind, "file");
    assert_eq!(parse_media_refs(r#"{"file_key":"f2"}"#, "media")[0].key, "f2");
}

#[test]
fn parses_images_embedded_in_a_post() {
    let content = r#"{"zh_cn":{"title":"t","content":[[
        {"tag":"text","text":"xem"},
        {"tag":"img","image_key":"img_a"}],[
        {"tag":"img","image_key":"img_b"}]]}}"#;
    let refs = parse_media_refs(content, "post");
    assert_eq!(
        refs.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
        vec!["img_a", "img_b"]
    );
}

#[test]
fn text_only_messages_name_no_media() {
    assert!(parse_media_refs(r#"{"text":"hello"}"#, "text").is_empty());
    assert!(parse_media_refs("not json", "image").is_empty());
    assert!(parse_media_refs(r#"{"image_key":""}"#, "image").is_empty());
}

#[test]
fn media_messages_get_a_label_not_their_raw_json() {
    // Without this the agent is handed `{"image_key":…}` as the user's words.
    assert_eq!(parse_text_content(r#"{"image_key":"img_a"}"#, "image"), "[Image]");
    assert_eq!(
        parse_text_content(r#"{"file_key":"f","file_name":"a.pdf"}"#, "file"),
        "[File] a.pdf"
    );
    assert_eq!(parse_text_content(r#"{"file_key":"f"}"#, "audio"), "[Audio]");
}
