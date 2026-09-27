// ===== Tests =====

use super::*;

#[test]
fn group_info_conversion() {
    let g = crate::types::GroupBinding {
        jid: "tg:group:1".into(),
        folder: "team-a".into(),
        name: "Team A".into(),
        channel: "telegram".into(),
        group_type: "group".into(),
        requires_trigger: true,
        allowed_tools: Some(vec!["Read".into(), "Write".into()]),
        allowed_paths: None,
        allowed_work_dirs: Some(vec!["/tmp".into()]),
        bot_token: Some("tok123".into()),
        max_messages: Some(50),
        llm_config_id: Some("llm-1".into()),
        last_active: None,
        added_at: "2026-01-01T00:00:00Z".into(),
    };
    let info = wire::to_group_info(&g);
    assert_eq!(info.jid, "tg:group:1");
    assert_eq!(info.folder, "team-a");
    assert_eq!(
        info.allowed_tools.as_deref(),
        Some(&["Read".into(), "Write".into()][..])
    );
    assert_eq!(info.max_messages, Some(50));
}

#[test]
fn gateway_new_defaults() {
    let gw = gateway::WebSocketGateway::new(18789, Some("secret".into()));
    assert_eq!(gw.port, 18789);
    assert_eq!(gw.token.as_deref(), Some("secret"));
}

#[test]
fn gateway_no_token() {
    let gw = gateway::WebSocketGateway::new(18789, None);
    assert_eq!(gw.token, None);
}
