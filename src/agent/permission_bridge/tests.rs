use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::api::PermissionBridgeApi;
use super::bridge::PermissionBridge;
use super::types::{RuleAction, RuleMatcher, RuleMatcherType, ToolAutoAcceptRule};
use super::utils::{capitalize_first, format_content, short_id, truncate_content};

struct StubApi;
impl PermissionBridgeApi for StubApi {}

fn stub_api() -> Arc<dyn PermissionBridgeApi> {
    Arc::new(StubApi)
}

#[derive(Default)]
struct RecordingApi {
    responses: Mutex<Vec<(String, String, String)>>,
    persisted_rules: Mutex<Vec<String>>,
}

impl PermissionBridgeApi for RecordingApi {
    fn respond_to_tool_permission(&self, group_jid: &str, tool_name: &str, selected: &str) {
        self.responses.lock().unwrap().push((
            group_jid.to_string(),
            tool_name.to_string(),
            selected.to_string(),
        ));
    }

    fn persist_tool_rule(&self, rule: &ToolAutoAcceptRule) {
        self.persisted_rules.lock().unwrap().push(rule.id.clone());
    }
}

#[test]
fn test_short_id_is_8_hex_chars() {
    let id = short_id();
    assert_eq!(id.len(), 8);
    assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn test_capitalize_first() {
    assert_eq!(capitalize_first("allow"), "Allow");
    assert_eq!(capitalize_first(""), "");
    assert_eq!(capitalize_first("a"), "A");
    assert_eq!(capitalize_first("ABC"), "ABC");
}

#[test]
fn test_format_content_string() {
    let v = serde_json::json!("hello world");
    assert_eq!(format_content(&v), "hello world");
}

#[test]
fn test_format_content_diff_patch() {
    let v = serde_json::json!({
        "patch": [
            {"lines": ["+added line", "-removed line"]},
            {"lines": [" context line"]}
        ]
    });
    assert_eq!(
        format_content(&v),
        "+added line\n-removed line\n context line"
    );
}

#[test]
fn test_format_content_fallback_json() {
    let v = serde_json::json!({"key": "value", "nested": {"a": 1}});
    let result = format_content(&v);
    assert!(result.contains("\"key\""));
    assert!(result.contains("\"value\""));
}

#[test]
fn test_truncate_content_no_truncation() {
    let s = "short message";
    assert_eq!(truncate_content(s, 200), s);
}

#[test]
fn test_truncate_content_utf8_no_panic_mid_char() {
    // 198 ASCII + "ị" (3 UTF-8 bytes) — raw byte 200 lies inside "ị" without boundary fix
    let s = format!("{}ị", "a".repeat(198));
    assert_eq!(s.len(), 201);
    let result = truncate_content(&s, 200);
    assert!(result.starts_with(&"a".repeat(198)));
    assert!(result.contains("chars omitted"));
}

#[test]
fn test_truncate_content_with_overflow() {
    let s = "x".repeat(250);
    let result = truncate_content(&s, 200);
    assert!(result.starts_with(&"x".repeat(200)));
    assert!(result.contains("50 chars omitted"));
}

#[test]
fn test_resolve_permission_not_found() {
    let bridge = PermissionBridge::new(stub_api(), None);
    assert!(!bridge.resolve_permission("nonexistent", "allow"));
}

#[test]
fn test_resolve_permission_first_responder_wins() {
    let bridge = PermissionBridge::new(stub_api(), None);

    // Set a permission-request callback to prevent auto-deny path
    let captured_id = Arc::new(Mutex::new(String::new()));
    {
        let captured_id = Arc::clone(&captured_id);
        bridge.set_permission_request_callback(move |_chat_jid, request_id, _payload| {
            *captured_id.lock().unwrap() = request_id.to_string();
        });
    }

    let options: HashMap<String, String> = [
        ("allow".into(), "Allow".into()),
        ("refuse".into(), "Refuse".into()),
    ]
    .into();
    bridge.handle_permission_request(
        "Bash",
        "Bash(rm -rf /)",
        "Run command?",
        &serde_json::json!("rm -rf /"),
        &options,
        "group-1",
        "chat-1",
        None,
    );

    let request_id = captured_id.lock().unwrap().clone();
    assert!(!request_id.is_empty(), "request ID should be captured");

    // First resolution should succeed
    assert!(bridge.resolve_permission(&request_id, "allow"));

    // Second resolution on same ID should fail (already consumed)
    assert!(!bridge.resolve_permission(&request_id, "refuse"));
}

#[test]
fn test_default_rules_do_not_auto_accept_skill_or_task() {
    let api = Arc::new(RecordingApi::default());
    let bridge = PermissionBridge::new(api.clone(), None);
    bridge.set_permission_request_callback(|_, _, _| {});
    let options: HashMap<String, String> = [
        ("allow".into(), "Allow".into()),
        ("refuse".into(), "Refuse".into()),
    ]
    .into();

    bridge.handle_permission_request(
        "Skill",
        "Skill(agent-browser)",
        "Load skill?",
        &serde_json::json!({"skill": "agent-browser"}),
        &options,
        "group-1",
        "chat-1",
        None,
    );
    bridge.handle_permission_request(
        "Task",
        "Task",
        "Launch agent?",
        &serde_json::json!({"subagent_type": "general-purpose"}),
        &options,
        "group-1",
        "chat-1",
        None,
    );

    let responses = api.responses.lock().unwrap().clone();
    assert!(responses.is_empty());
}

#[test]
fn test_skill_exact_rule_auto_accepts_only_selected_skill() {
    let api = Arc::new(RecordingApi::default());
    let bridge = PermissionBridge::new(api.clone(), None);
    bridge.set_permission_request_callback(|_, _, _| {});
    bridge.add_rule(ToolAutoAcceptRule {
        id: "skill-auto-access:agent-browser".into(),
        matcher: RuleMatcher {
            matcher_type: RuleMatcherType::SkillExact,
            pattern: None,
            tool_name: None,
            skill_name: Some("agent-browser".into()),
            server: None,
            tool: None,
            category: None,
        },
        action: RuleAction::AutoAccept,
        enabled: true,
        description: None,
    });
    let options: HashMap<String, String> = [
        ("allow".into(), "Allow".into()),
        ("refuse".into(), "Refuse".into()),
    ]
    .into();

    bridge.handle_permission_request(
        "Skill",
        "Skill(agent-browser)",
        "Load skill?",
        &serde_json::json!({"skill": "agent-browser"}),
        &options,
        "group-1",
        "chat-1",
        None,
    );
    bridge.handle_permission_request(
        "Skill",
        "Skill(web-research)",
        "Load skill?",
        &serde_json::json!({"skill": "web-research"}),
        &options,
        "group-1",
        "chat-1",
        None,
    );

    let responses = api.responses.lock().unwrap().clone();
    assert_eq!(
        responses,
        vec![("group-1".into(), "Skill".into(), "allow".into())]
    );
}

#[test]
fn test_mcp_server_rule_auto_accepts_hyphenated_server() {
    // Regression: app-space MCP servers keep hyphens in the tool name
    // (e.g. "mcp__ssh-manager-mcp__ssh_list_hosts"). An "Auto Access ALL"
    // rule stores server "ssh-manager-mcp"; the matcher must accept the
    // hyphenated tool name, not only the underscore-normalized form.
    let api = Arc::new(RecordingApi::default());
    let bridge = PermissionBridge::new(api.clone(), None);
    bridge.set_permission_request_callback(|_, _, _| {});
    bridge.add_rule(ToolAutoAcceptRule {
        id: "mcp:ssh-manager-mcp:*".into(),
        matcher: RuleMatcher {
            matcher_type: RuleMatcherType::McpServer,
            pattern: None,
            tool_name: None,
            skill_name: None,
            server: Some("ssh-manager-mcp".into()),
            tool: None,
            category: None,
        },
        action: RuleAction::AutoAccept,
        enabled: true,
        description: None,
    });
    let options: HashMap<String, String> = [
        ("allow".into(), "Allow".into()),
        ("refuse".into(), "Refuse".into()),
    ]
    .into();

    bridge.handle_permission_request(
        "mcp__ssh-manager-mcp__ssh_list_hosts",
        "mcp__ssh-manager-mcp__ssh_list_hosts",
        "Run tool?",
        &serde_json::json!(null),
        &options,
        "group-1",
        "chat-1",
        None,
    );
    // A tool from a different server must NOT auto-accept.
    bridge.handle_permission_request(
        "mcp__other-server__do_thing",
        "mcp__other-server__do_thing",
        "Run tool?",
        &serde_json::json!(null),
        &options,
        "group-1",
        "chat-1",
        None,
    );

    let responses = api.responses.lock().unwrap().clone();
    assert_eq!(
        responses,
        vec![(
            "group-1".into(),
            "mcp__ssh-manager-mcp__ssh_list_hosts".into(),
            "allow".into()
        )]
    );
}

#[test]
fn test_handle_callback_unknown_prefix() {
    let bridge = PermissionBridge::new(stub_api(), None);
    assert_eq!(bridge.handle_callback("X:123:allow", "chat-1"), None);
}

#[test]
fn test_resolve_ask_question_batch_not_found() {
    let bridge = PermissionBridge::new(stub_api(), None);
    assert!(!bridge.resolve_ask_question_batch("nonexistent", &serde_json::json!({"0": 0}), None));
}

// ===== FormUI bridge tests =====

fn sample_form_request(agent_id: &str) -> crate::zen_core::FormRequestData {
    let fields: Vec<crate::zen_core::FormField> = serde_json::from_value(serde_json::json!([
        {"type": "static_text", "text": "Header", "variant": "heading"},
        {"type": "text", "key": "env", "label": "Environment", "required": true, "default": "staging"},
        {"type": "checkbox", "key": "dry_run", "label": "Dry run", "default": true}
    ]))
    .unwrap();
    crate::zen_core::FormRequestData {
        agent_id: agent_id.to_string(),
        title: "Deploy".to_string(),
        surface: "inline".to_string(),
        submit_label: "Submit".to_string(),
        fields,
    }
}

/// Records `respond_to_form` deliveries plus plain messages (snapshot path).
#[derive(Default)]
struct FormRecordingApi {
    forms: Mutex<Vec<(String, String, HashMap<String, serde_json::Value>, bool)>>,
    sent: Mutex<Vec<String>>,
    web: bool,
}

impl PermissionBridgeApi for FormRecordingApi {
    fn is_web_jid(&self, _chat_jid: &str) -> bool {
        self.web
    }
    fn send_message(
        &self,
        _chat_jid: &str,
        text: &str,
        _bot_token: Option<&str>,
    ) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(text.to_string());
        Ok(())
    }
    fn respond_to_form(
        &self,
        group_jid: &str,
        agent_id: &str,
        values: HashMap<String, serde_json::Value>,
        submitted: bool,
    ) {
        self.forms.lock().unwrap().push((
            group_jid.to_string(),
            agent_id.to_string(),
            values,
            submitted,
        ));
    }
}

#[test]
fn test_resolve_form_not_found() {
    let bridge = PermissionBridge::new(stub_api(), None);
    assert!(!bridge.resolve_form("nonexistent", HashMap::new(), true));
}

#[test]
fn test_form_request_round_trip_via_web() {
    let api = Arc::new(FormRecordingApi {
        web: true,
        ..Default::default()
    });
    let bridge = PermissionBridge::new(api.clone(), None);

    // Capture the WS notification (requestId + payload).
    let captured = Arc::new(Mutex::new(None::<(String, String)>));
    {
        let captured = Arc::clone(&captured);
        bridge.set_form_request_callback(move |chat_jid, request_id, payload| {
            *captured.lock().unwrap() = Some((request_id.to_string(), payload.title.clone()));
            assert_eq!(chat_jid, "web:chat-1");
            assert_eq!(payload.fields.len(), 3);
        });
    }
    let resolved_cb = Arc::new(Mutex::new(None::<String>));
    {
        let resolved_cb = Arc::clone(&resolved_cb);
        bridge.set_form_resolved_callback(move |_chat_jid, request_id, values| {
            assert_eq!(values["env"], serde_json::json!("prod"));
            *resolved_cb.lock().unwrap() = Some(request_id.to_string());
        });
    }

    bridge.handle_form_request(&sample_form_request("main"), "group-1", "web:chat-1", None);
    let (request_id, title) = captured.lock().unwrap().clone().expect("WS notified");
    assert_eq!(title, "Deploy");

    // First responder wins; second submit is a no-op.
    let mut values = HashMap::new();
    values.insert("env".to_string(), serde_json::json!("prod"));
    assert!(bridge.resolve_form(&request_id, values.clone(), true));
    assert!(!bridge.resolve_form(&request_id, values, true));

    let forms = api.forms.lock().unwrap().clone();
    assert_eq!(forms.len(), 1);
    let (group_jid, agent_id, delivered, submitted) = &forms[0];
    assert_eq!(group_jid, "group-1");
    assert_eq!(agent_id, "main");
    assert!(submitted);
    assert_eq!(delivered["env"], serde_json::json!("prod"));
    assert_eq!(
        resolved_cb.lock().unwrap().as_deref(),
        Some(request_id.as_str())
    );
}

#[test]
fn test_form_request_degraded_channel_auto_submits_defaults() {
    // Non-web jid + no WS sink → snapshot text + auto-submit defaults with
    // submitted=false so the agent is never blocked forever.
    let api = Arc::new(FormRecordingApi::default());
    let bridge = PermissionBridge::new(api.clone(), None);

    bridge.handle_form_request(&sample_form_request("main"), "group-1", "tg:123", None);

    let sent = api.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains("Deploy"));
    assert!(sent[0].contains("Environment"));

    let forms = api.forms.lock().unwrap().clone();
    assert_eq!(forms.len(), 1);
    let (_, _, values, submitted) = &forms[0];
    assert!(!submitted);
    assert_eq!(values["env"], serde_json::json!("staging"));
    assert_eq!(values["dry_run"], serde_json::json!(true));
    // static_text contributes no value
    assert_eq!(values.len(), 2);

    // Pending entry must be consumed — resolving later returns false.
    assert!(!bridge.resolve_form("anything", HashMap::new(), true));
}

#[test]
fn allow_persists_the_scoped_permission_key_not_the_tool_name() {
    // Regression: the bridge used to hand `pending.tool_name` ("Skill") to the
    // persistence callback while `PermissionManager` looks the grant up under
    // `Skill(<name>)`. The saved value could never match, so "never ask for
    // <skill> Skill in this project" was silently a one-shot approval and the
    // card came back on the next engine.
    let api = Arc::new(RecordingApi::default());
    let bridge = PermissionBridge::new(api.clone(), None);

    let captured_id = Arc::new(Mutex::new(String::new()));
    {
        let captured_id = Arc::clone(&captured_id);
        bridge.set_permission_request_callback(move |_chat_jid, request_id, _payload| {
            *captured_id.lock().unwrap() = request_id.to_string();
        });
    }
    let persisted: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let persisted = Arc::clone(&persisted);
        bridge.set_tool_allowed_callback(move |group_jid: &str, key: &str| {
            persisted
                .lock()
                .unwrap()
                .push((group_jid.to_string(), key.to_string()));
        });
    }

    let options: HashMap<String, String> = [
        ("allow".into(), "Confirm, never ask for ai-office-run Skill".into()),
        ("refuse".into(), "Reject".into()),
    ]
    .into();
    bridge.handle_permission_request(
        "Skill",
        "Skill(ai-office-run)",
        "Load skill?",
        &serde_json::json!({"skill": "ai-office-run"}),
        &options,
        "group-1",
        "chat-1",
        None,
    );

    let request_id = captured_id.lock().unwrap().clone();
    assert!(bridge.resolve_permission(&request_id, "allow"));

    assert_eq!(
        *persisted.lock().unwrap(),
        vec![("group-1".to_string(), "Skill(ai-office-run)".to_string())]
    );
    // The response back to the engine still routes on the tool name — that is
    // the ResponseRegistry key and must not become the scoped one.
    assert_eq!(
        *api.responses.lock().unwrap(),
        vec![(
            "group-1".to_string(),
            "Skill".to_string(),
            "allow".to_string()
        )]
    );
}

#[test]
fn allow_falls_back_to_tool_name_when_no_key_supplied() {
    // Older/foreign emitters may send an empty key; persisting nothing would
    // lose the approval outright, so the tool name remains the fallback.
    let api = Arc::new(RecordingApi::default());
    let bridge = PermissionBridge::new(api.clone(), None);
    let captured_id = Arc::new(Mutex::new(String::new()));
    {
        let captured_id = Arc::clone(&captured_id);
        bridge.set_permission_request_callback(move |_c, request_id, _p| {
            *captured_id.lock().unwrap() = request_id.to_string();
        });
    }
    let persisted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let persisted = Arc::clone(&persisted);
        bridge.set_tool_allowed_callback(move |_g: &str, key: &str| {
            persisted.lock().unwrap().push(key.to_string());
        });
    }
    let options: HashMap<String, String> =
        [("allow".into(), "Allow".into())].into();
    bridge.handle_permission_request(
        "mcp__ai-office-mcp__office_status",
        "",
        "Run tool?",
        &serde_json::json!(null),
        &options,
        "group-1",
        "chat-1",
        None,
    );
    let request_id = captured_id.lock().unwrap().clone();
    assert!(bridge.resolve_permission(&request_id, "allow"));
    assert_eq!(
        *persisted.lock().unwrap(),
        vec!["mcp__ai-office-mcp__office_status".to_string()]
    );
}

// ===== "allow" installs a globally-scoped rule =====

fn approve(bridge: &PermissionBridge, tool: &str, key: &str, content: serde_json::Value) {
    let captured = Arc::new(Mutex::new(String::new()));
    {
        let captured = Arc::clone(&captured);
        bridge.set_permission_request_callback(move |_c, id, _p| {
            *captured.lock().unwrap() = id.to_string();
        });
    }
    let options: HashMap<String, String> = [("allow".into(), "Allow".into())].into();
    bridge.handle_permission_request(
        tool, key, "?", &content, &options, "group-1", "chat-1", None,
    );
    let id = captured.lock().unwrap().clone();
    assert!(
        bridge.resolve_permission(&id, "allow"),
        "request {key} should resolve"
    );
}

#[test]
fn allowing_a_skill_stops_the_next_chat_asking_again() {
    // The whole point of the option: approving once must silence the prompt
    // everywhere, not just in the chat it was clicked in. The rule is what
    // carries it across chats, so it has to exist and to short-circuit
    // `handle_permission_request` before a second card is ever built.
    let api = Arc::new(RecordingApi::default());
    let bridge = PermissionBridge::new(api.clone(), None);

    approve(
        &bridge,
        "Skill",
        "Skill(ai-office-run)",
        serde_json::json!({"skill": "ai-office-run"}),
    );
    assert_eq!(
        *api.persisted_rules.lock().unwrap(),
        vec!["skill-auto-access:ai-office-run".to_string()],
        "id must match the one the Skills panel uses, so the rule is revocable there"
    );

    // A different chat asks for the same skill: auto-accepted, no card.
    let seen = Arc::new(Mutex::new(0usize));
    {
        let seen = Arc::clone(&seen);
        bridge.set_permission_request_callback(move |_c, _id, _p| {
            *seen.lock().unwrap() += 1;
        });
    }
    let options: HashMap<String, String> = [("allow".into(), "Allow".into())].into();
    bridge.handle_permission_request(
        "Skill",
        "Skill(ai-office-run)",
        "?",
        &serde_json::json!({"skill": "ai-office-run"}),
        &options,
        "group-2",
        "chat-2",
        None,
    );
    assert_eq!(*seen.lock().unwrap(), 0, "second chat must not be prompted");

    // A different skill is untouched — the grant is scoped to what was shown.
    bridge.handle_permission_request(
        "Skill",
        "Skill(other-skill)",
        "?",
        &serde_json::json!({"skill": "other-skill"}),
        &options,
        "group-2",
        "chat-2",
        None,
    );
    assert_eq!(*seen.lock().unwrap(), 1, "an unapproved skill still asks");
}

#[test]
fn approval_rules_stay_as_narrow_as_the_option_label() {
    use RuleMatcherType as M;
    let cases: Vec<(&str, &str, &str, M)> = vec![
        (
            "Skill",
            "Skill(ai-office-run)",
            "skill-auto-access:ai-office-run",
            M::SkillExact,
        ),
        (
            "mcp__ai-office-mcp__office_status",
            "mcp__ai-office-mcp__office_status",
            "mcp:ai-office-mcp:office_status",
            M::McpServer,
        ),
        (
            "Bash",
            "Bash(git status)",
            "bash-auto-access:git status",
            M::BashRegex,
        ),
        (
            "Write",
            "Write",
            "tool-category:file-edit",
            M::ToolCategory,
        ),
        ("Task", "Task", "tool-exact:Task", M::ToolExact),
    ];
    for (tool, key, want_id, want_type) in cases {
        let rule = PermissionBridge::rule_for_approval(tool, key)
            .unwrap_or_else(|| panic!("no rule for {key}"));
        assert_eq!(rule.id, want_id, "id for {key}");
        assert_eq!(
            std::mem::discriminant(&rule.matcher.matcher_type),
            std::mem::discriminant(&want_type),
            "matcher for {key}"
        );
        assert!(rule.enabled);
    }

    // An MCP approval grants the one tool, never the whole server.
    let rule = PermissionBridge::rule_for_approval(
        "mcp__ai-office-mcp__office_status",
        "mcp__ai-office-mcp__office_status",
    )
    .unwrap();
    assert_eq!(rule.matcher.tool.as_deref(), Some("office_status"));

    // An empty key yields nothing rather than a rule matching everything.
    assert!(PermissionBridge::rule_for_approval("Skill", "").is_none());
}

#[test]
fn bash_prefix_approval_does_not_leak_to_a_similarly_named_command() {
    let api = Arc::new(RecordingApi::default());
    let bridge = PermissionBridge::new(api.clone(), None);
    approve(
        &bridge,
        "Bash",
        "Bash(git:*)",
        serde_json::json!({"command": "git status"}),
    );

    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let seen = Arc::clone(&seen);
        bridge.set_permission_request_callback(move |_c, _id, payload| {
            seen.lock().unwrap().push(payload.tool_name.clone());
        });
    }
    let options: HashMap<String, String> = [("allow".into(), "Allow".into())].into();
    let ask = |cmd: &str| {
        bridge.handle_permission_request(
            "Bash",
            &format!("Bash({cmd})"),
            "?",
            &serde_json::json!({"command": cmd}),
            &options,
            "group-2",
            "chat-2",
            None,
        );
    };
    ask("git push --force"); // covered by the prefix
    assert!(seen.lock().unwrap().is_empty(), "`git …` must be silent");

    ask("github-cli auth"); // shares the letters, not the prefix
    ask("rm -rf /");
    assert_eq!(
        seen.lock().unwrap().len(),
        2,
        "prefix must not swallow `github-cli` or unrelated commands"
    );
}

#[test]
fn bash_pattern_rules_match_the_command_not_the_tool_name() {
    // Regression: BashGlob/BashRegex were tested against `tool_name`, which is
    // always the literal "Bash", so every Bash rule written in the Tool Rules
    // panel was inert.
    let api = Arc::new(RecordingApi::default());
    let bridge = PermissionBridge::new(api.clone(), None);
    bridge.add_rule(ToolAutoAcceptRule {
        id: "bash-glob:npm".into(),
        matcher: RuleMatcher {
            matcher_type: RuleMatcherType::BashGlob,
            pattern: Some("npm *".into()),
            tool_name: None,
            skill_name: None,
            server: None,
            tool: None,
            category: None,
        },
        action: RuleAction::AutoAccept,
        enabled: true,
        description: None,
    });
    bridge.set_permission_request_callback(|_, _, _| {});
    let options: HashMap<String, String> = [("allow".into(), "Allow".into())].into();

    bridge.handle_permission_request(
        "Bash",
        "Bash(npm test)",
        "?",
        &serde_json::json!({"command": "npm test"}),
        &options,
        "g",
        "c",
        None,
    );
    // A non-Bash tool must never be matched by a Bash pattern.
    bridge.handle_permission_request(
        "mcp__x__y",
        "mcp__x__y",
        "?",
        &serde_json::json!(null),
        &options,
        "g",
        "c",
        None,
    );

    assert_eq!(
        *api.responses.lock().unwrap(),
        vec![("g".into(), "Bash".into(), "allow".into())],
        "only the Bash command should auto-accept"
    );
}

// ===== Tool gate =====

mod gated {
    use super::*;
    use crate::agent::permission_bridge::{GateCall, PermissionGate};

    #[derive(Default)]
    struct FakeGate {
        allow: bool,
        panics: bool,
        judged: Mutex<usize>,
        answers: Mutex<Vec<(i64, String)>>,
    }

    #[async_trait::async_trait]
    impl PermissionGate for FakeGate {
        fn wants(&self, tool_name: &str, _content: &serde_json::Value) -> bool {
            tool_name == "Bash"
        }
        async fn judge(&self, _tool: &str, _content: &serde_json::Value, _chat: &str) -> GateCall {
            *self.judged.lock().unwrap() += 1;
            if self.panics {
                panic!("the gate broke");
            }
            GateCall { allow: self.allow, log_id: Some(7) }
        }
        fn record_answer(&self, log_id: i64, option_key: &str) {
            self.answers.lock().unwrap().push((log_id, option_key.to_string()));
        }
    }

    struct Harness {
        bridge: Arc<PermissionBridge>,
        api: Arc<RecordingApi>,
        gate: Arc<FakeGate>,
        prompted: Arc<Mutex<Vec<String>>>,
    }

    fn harness(gate: FakeGate) -> Harness {
        let api = Arc::new(RecordingApi::default());
        let bridge = Arc::new(PermissionBridge::new(api.clone(), None));
        let gate = Arc::new(gate);
        bridge.set_tool_gate(gate.clone());
        let prompted = Arc::new(Mutex::new(Vec::new()));
        {
            let prompted = Arc::clone(&prompted);
            bridge.set_permission_request_callback(move |_chat, request_id, _payload| {
                prompted.lock().unwrap().push(request_id.to_string());
            });
        }
        Harness { bridge, api, gate, prompted }
    }

    fn ask(h: &Harness, tool: &str) {
        let options: HashMap<String, String> =
            [("agree".into(), "Confirm".into()), ("refuse".into(), "Reject".into())].into();
        h.bridge.handle_permission_request_gated(
            tool,
            &format!("{tool}(bun test)"),
            "Run command?",
            &serde_json::json!({"command": "bun test"}),
            &options,
            "group-1",
            "chat-1",
            None,
        );
    }

    /// Wait for the background judgment to land somewhere.
    async fn settle(h: &Harness) {
        for _ in 0..200 {
            if !h.prompted.lock().unwrap().is_empty() || !h.api.responses.lock().unwrap().is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test]
    async fn a_confident_gate_approves_once_without_a_prompt() {
        let h = harness(FakeGate { allow: true, ..Default::default() });
        ask(&h, "Bash");
        settle(&h).await;
        assert_eq!(
            h.api.responses.lock().unwrap().as_slice(),
            [("group-1".to_string(), "Bash".to_string(), "agree".to_string())],
            "a one-time approval, never a saved grant"
        );
        assert!(h.prompted.lock().unwrap().is_empty());
        assert!(h.api.persisted_rules.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unsure_gate_leaves_the_prompt_and_hears_the_answer() {
        let h = harness(FakeGate::default());
        ask(&h, "Bash");
        settle(&h).await;
        let id = h.prompted.lock().unwrap().first().cloned().expect("the prompt showed");
        assert!(h.bridge.resolve_permission(&id, "refuse"));
        assert_eq!(h.gate.answers.lock().unwrap().as_slice(), [(7, "refuse".to_string())]);
    }

    #[tokio::test]
    async fn a_gate_that_panics_still_leaves_the_person_a_prompt() {
        let h = harness(FakeGate { panics: true, ..Default::default() });
        ask(&h, "Bash");
        settle(&h).await;
        assert_eq!(h.prompted.lock().unwrap().len(), 1);
        assert!(h.api.responses.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn other_tools_and_accepted_rules_never_reach_the_gate() {
        let h = harness(FakeGate { allow: true, ..Default::default() });
        ask(&h, "Write");
        assert_eq!(h.prompted.lock().unwrap().len(), 1, "prompted at once, no gate");

        let h = harness(FakeGate { allow: true, ..Default::default() });
        h.bridge.add_rule(ToolAutoAcceptRule {
            id: "all".into(),
            matcher: RuleMatcher {
                matcher_type: RuleMatcherType::Always,
                pattern: None,
                tool_name: None,
                skill_name: None,
                server: None,
                tool: None,
                category: None,
            },
            action: RuleAction::AutoAccept,
            enabled: true,
            description: None,
        });
        ask(&h, "Bash");
        assert_eq!(h.api.responses.lock().unwrap()[0].2, "allow", "the rule answered");
        assert_eq!(*h.gate.judged.lock().unwrap(), 0);
    }
}
