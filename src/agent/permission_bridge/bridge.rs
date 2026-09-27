//! PermissionBridge — relay sema-core permission requests to inline keyboards / Web UI.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::types::InlineButton;

use super::api::{PermissionBridgeApi, PREFIX_ASK, PREFIX_PERM};
use super::gate::{GateCall, PermissionGate};
use super::types::{
    AskQuestionData, AskQuestionPayload, FormPayload, PendingAskQuestion, PendingForm,
    PendingPermission, PermissionOption, PermissionPayload, RuleAction, RuleMatcher,
    RuleMatcherType, ToolAutoAcceptRule, ToolCategory,
};
use super::utils::{capitalize_first, format_content, short_id, truncate_content};

const DEPRECATED_DEFAULT_RULE_IDS: &[&str] = &["tool-category-skill", "tool-category-agent"];

pub struct PermissionBridge {
    pub(crate) pending_permissions: Mutex<HashMap<String, PendingPermission>>,
    pub(crate) pending_ask_questions: Mutex<HashMap<String, PendingAskQuestion>>,
    pub(crate) pending_forms: Mutex<HashMap<String, PendingForm>>,
    max_content_length: usize,
    api: Arc<dyn PermissionBridgeApi>,

    // Tool auto-accept rules
    tool_rules: Mutex<Vec<ToolAutoAcceptRule>>,
    accept_all: Mutex<bool>,

    // Callback setters (set once during daemon wiring)
    pub(crate) on_activity: Mutex<Option<Box<dyn Fn(&str) + Send + Sync>>>,
    pub(crate) on_permission_request:
        Mutex<Option<Box<dyn Fn(&str, &str, PermissionPayload) + Send + Sync>>>,
    pub(crate) on_ask_question_request:
        Mutex<Option<Box<dyn Fn(&str, &str, AskQuestionPayload) + Send + Sync>>>,
    pub(crate) on_form_request: Mutex<Option<Box<dyn Fn(&str, &str, FormPayload) + Send + Sync>>>,
    pub(crate) on_permission_resolved:
        Mutex<Option<Box<dyn Fn(&str, &str, &str, &str) + Send + Sync>>>,
    pub(crate) on_ask_question_resolved:
        Mutex<Option<Box<dyn Fn(&str, &str, HashMap<String, String>) + Send + Sync>>>,
    pub(crate) on_form_resolved:
        Mutex<Option<Box<dyn Fn(&str, &str, HashMap<String, serde_json::Value>) + Send + Sync>>>,
    /// Fired when user selects "allow" (never ask again) for a tool.
    /// Signature: `(group_jid, tool_name)` — used to persist the approval to DB.
    pub(crate) on_tool_allowed: Mutex<Option<Box<dyn Fn(&str, &str) + Send + Sync>>>,
    /// May approve a request before it prompts (see [`PermissionGate`]).
    tool_gate: Mutex<Option<Arc<dyn PermissionGate>>>,
}

impl PermissionBridge {
    pub fn new(api: Arc<dyn PermissionBridgeApi>, max_content_length: Option<usize>) -> Self {
        Self {
            pending_permissions: Mutex::new(HashMap::new()),
            pending_ask_questions: Mutex::new(HashMap::new()),
            pending_forms: Mutex::new(HashMap::new()),
            max_content_length: max_content_length.unwrap_or(200),
            api,
            tool_rules: Mutex::new(default_tool_rules()),
            accept_all: Mutex::new(false),
            on_activity: Mutex::new(None),
            on_permission_request: Mutex::new(None),
            on_ask_question_request: Mutex::new(None),
            on_form_request: Mutex::new(None),
            on_permission_resolved: Mutex::new(None),
            on_ask_question_resolved: Mutex::new(None),
            on_form_resolved: Mutex::new(None),
            on_tool_allowed: Mutex::new(None),
            tool_gate: Mutex::new(None),
        }
    }

    // ===== Rule management =====

    pub fn add_rule(&self, rule: ToolAutoAcceptRule) {
        if DEPRECATED_DEFAULT_RULE_IDS.contains(&rule.id.as_str()) {
            tracing::info!(
                "[PermissionBridge] ignoring deprecated default rule id={}",
                rule.id
            );
            return;
        }
        let mut rules = self.tool_rules.lock().unwrap();
        rules.retain(|r| r.id != rule.id); // replace if same id
        tracing::info!(
            "[PermissionBridge] add_rule id={} server={:?} tool={:?} action={:?} enabled={}",
            rule.id,
            rule.matcher.server,
            rule.matcher.tool,
            rule.action,
            rule.enabled
        );
        rules.push(rule);
    }

    pub fn remove_rule(&self, rule_id: &str) {
        self.tool_rules.lock().unwrap().retain(|r| r.id != rule_id);
    }

    pub fn update_rule(&self, rule: ToolAutoAcceptRule) {
        if DEPRECATED_DEFAULT_RULE_IDS.contains(&rule.id.as_str()) {
            self.remove_rule(&rule.id);
            return;
        }
        let mut rules = self.tool_rules.lock().unwrap();
        if let Some(existing) = rules.iter_mut().find(|r| r.id == rule.id) {
            *existing = rule;
        }
    }

    /// Install the gate consulted by [`Self::handle_permission_request_gated`].
    pub fn set_tool_gate(&self, gate: Arc<dyn PermissionGate>) {
        *self.tool_gate.lock().unwrap() = Some(gate);
    }

    pub fn set_accept_all(&self, enabled: bool) {
        *self.accept_all.lock().unwrap() = enabled;
    }

    pub fn get_rules(&self) -> Vec<ToolAutoAcceptRule> {
        self.tool_rules.lock().unwrap().clone()
    }

    /// Check if `tool_name` (sema-core format, e.g. `mcp__browser__search`) is auto-accepted.
    fn should_auto_accept(&self, tool_name: &str, content: &serde_json::Value) -> bool {
        if *self.accept_all.lock().unwrap() {
            return true;
        }
        let rules = self.tool_rules.lock().unwrap();
        rules.iter().any(|r| {
            r.enabled
                && Self::rule_matches(r, tool_name, content)
                && matches!(
                    r.action,
                    RuleAction::AutoAccept | RuleAction::AutoAcceptAndAllow
                )
        })
    }

    /// The shell command a Bash permission request is about, or `None` for any
    /// other tool. Normal requests carry `{"command": …}` from Bash's
    /// `gen_tool_permission`; a bare string is accepted too so a hand-built
    /// request body still resolves.
    fn bash_command<'a>(tool_name: &str, content: &'a serde_json::Value) -> Option<&'a str> {
        if tool_name != "Bash" {
            return None;
        }
        content
            .get("command")
            .and_then(|v| v.as_str())
            .or_else(|| content.as_str())
    }

    /// The globally-scoped rule an "allow / never ask again" answer installs.
    ///
    /// The per-chat grant alone made the option a lie: it read "in this
    /// project" but only ever covered the one chat it was clicked in, so a new
    /// conversation asked all over again. The rule is derived from the
    /// permission key so it grants exactly what the user was shown — this
    /// skill, this command, this MCP tool — never a category broader than the
    /// button's own label. Ids match the ones the Tool Rules / Skills panels
    /// use, so a rule created here is visible and revocable there.
    pub(crate) fn rule_for_approval(
        tool_name: &str,
        permission_key: &str,
    ) -> Option<ToolAutoAcceptRule> {
        let build = |id: String, matcher: RuleMatcher, description: String| {
            Some(ToolAutoAcceptRule {
                id,
                matcher,
                action: RuleAction::AutoAccept,
                enabled: true,
                description: Some(description),
            })
        };
        let empty = RuleMatcher {
            matcher_type: RuleMatcherType::Always,
            pattern: None,
            tool_name: None,
            skill_name: None,
            server: None,
            tool: None,
            category: None,
        };

        // Skill(<name>)
        if let Some(skill) = permission_key
            .strip_prefix("Skill(")
            .and_then(|r| r.strip_suffix(')'))
            .filter(|s| !s.is_empty())
        {
            return build(
                format!("skill-auto-access:{skill}"),
                RuleMatcher {
                    matcher_type: RuleMatcherType::SkillExact,
                    skill_name: Some(skill.to_string()),
                    ..empty
                },
                format!("Auto accept Skill {skill}"),
            );
        }

        // Bash(<cmd>) or Bash(<prefix>:*) — anchored regex mirrors the
        // per-chat prefix semantics exactly (the prefix alone, or the prefix
        // followed by whitespace), so `git` never covers `github-cli`.
        if let Some(inner) = permission_key
            .strip_prefix("Bash(")
            .and_then(|r| r.strip_suffix(')'))
            .filter(|s| !s.is_empty())
        {
            let (pattern, description) = match inner.strip_suffix(":*") {
                Some(prefix) if !prefix.is_empty() => (
                    format!("^{}(\\s|$)", regex::escape(prefix)),
                    format!("Auto accept `{prefix}` commands"),
                ),
                _ => (
                    format!("^{}$", regex::escape(inner)),
                    format!("Auto accept `{inner}`"),
                ),
            };
            return build(
                format!("bash-auto-access:{inner}"),
                RuleMatcher {
                    matcher_type: RuleMatcherType::BashRegex,
                    pattern: Some(pattern),
                    ..empty
                },
                description,
            );
        }

        // mcp__<server>__<tool> — scoped to the one tool, never the whole
        // server, because that is all the prompt asked about.
        if let Some((server, tool)) = permission_key
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once("__"))
            .filter(|(s, t)| !s.is_empty() && !t.is_empty())
        {
            return build(
                format!("mcp:{server}:{tool}"),
                RuleMatcher {
                    matcher_type: RuleMatcherType::McpServer,
                    server: Some(server.to_string()),
                    tool: Some(tool.to_string()),
                    ..empty
                },
                format!("Auto accept {permission_key}"),
            );
        }

        // Edit / Write / NotebookEdit — the option reads "never ask for file
        // editing", so the rule covers the category rather than the one tool
        // that happened to trigger the prompt.
        if matches!(permission_key, "Edit" | "Write" | "NotebookEdit") {
            return build(
                "tool-category:file-edit".to_string(),
                RuleMatcher {
                    matcher_type: RuleMatcherType::ToolCategory,
                    category: Some(ToolCategory::FileEdit),
                    ..empty
                },
                "Auto accept file editing".to_string(),
            );
        }

        // Any other tool, keyed by its plain name.
        if permission_key.is_empty() || permission_key != tool_name {
            return None;
        }
        build(
            format!("tool-exact:{tool_name}"),
            RuleMatcher {
                matcher_type: RuleMatcherType::ToolExact,
                tool_name: Some(tool_name.to_string()),
                ..empty
            },
            format!("Auto accept {tool_name}"),
        )
    }

    /// Install + persist the global rule for an approved request. In-memory
    /// first so the very next request in this session short-circuits before a
    /// prompt is built.
    fn install_approval_rule(&self, pending: &PendingPermission) {
        let Some(rule) = Self::rule_for_approval(&pending.tool_name, &pending.permission_key)
        else {
            tracing::info!(
                "[PermissionBridge] no global rule derived for tool={} key={}",
                pending.tool_name,
                pending.permission_key
            );
            return;
        };
        tracing::info!(
            "[PermissionBridge] installing global auto-accept rule id={} from approval of {}",
            rule.id,
            pending.permission_key
        );
        self.api.persist_tool_rule(&rule);
        self.add_rule(rule);
    }

    fn rule_matches(
        rule: &ToolAutoAcceptRule,
        tool_name: &str,
        content: &serde_json::Value,
    ) -> bool {
        match rule.matcher.matcher_type {
            RuleMatcherType::Always => true,
            RuleMatcherType::ToolExact => rule.matcher.tool_name.as_deref() == Some(tool_name),
            RuleMatcherType::SkillExact => {
                if tool_name != "Skill" {
                    return false;
                }
                let Some(expected_skill) = rule.matcher.skill_name.as_deref() else {
                    return false;
                };
                content.get("skill").and_then(|v| v.as_str()) == Some(expected_skill)
            }
            RuleMatcherType::McpServer => {
                let Some(server) = rule.matcher.server.as_deref() else {
                    return false;
                };
                // sema-core tool name format: mcp__{server}__{tool}. The server
                // segment may KEEP hyphens (e.g. app-space servers like
                // "ssh-manager-mcp") or appear normalized to underscores
                // depending on the registration path. Build candidate prefixes
                // covering both forms — original (hyphenated) AND normalized —
                // and also the "senclaw-" stripped short form, then match the
                // tool name against whichever applies.
                let stripped = server.strip_prefix("senclaw-").unwrap_or(server);
                let candidates = [
                    format!("mcp__{server}__"), // original (may have hyphens)
                    format!("mcp__{}__", server.replace('-', "_")), // normalized
                    format!("mcp__{stripped}__"), // senclaw- stripped, hyphens
                    format!("mcp__{}__", stripped.replace('-', "_")), // stripped + normalized
                ];
                let Some(prefix) = candidates
                    .iter()
                    .find(|p| tool_name.starts_with(p.as_str()))
                else {
                    return false;
                };
                // If tool is None/empty, match all tools of this server
                match rule.matcher.tool.as_deref() {
                    None | Some("") => true,
                    Some(expected_tool) => {
                        let actual_tool = &tool_name[prefix.len()..];
                        actual_tool == expected_tool
                    }
                }
            }
            RuleMatcherType::McpGlob => {
                let Some(pattern) = rule.matcher.pattern.as_deref() else {
                    return false;
                };
                glob_match(pattern, tool_name)
            }
            // Bash patterns describe the COMMAND. Testing them against
            // `tool_name` — always the literal "Bash" — meant no Bash rule
            // written in the Tool Rules panel could ever fire; `git *` was
            // matched against "Bash" and silently did nothing.
            RuleMatcherType::BashGlob => {
                let Some(pattern) = rule.matcher.pattern.as_deref() else {
                    return false;
                };
                let Some(command) = Self::bash_command(tool_name, content) else {
                    return false;
                };
                glob_match(pattern, command)
            }
            RuleMatcherType::BashRegex => {
                let Some(pattern) = rule.matcher.pattern.as_deref() else {
                    return false;
                };
                let Some(command) = Self::bash_command(tool_name, content) else {
                    return false;
                };
                regex::Regex::new(pattern)
                    .map(|re| re.is_match(command))
                    .unwrap_or(false)
            }
            RuleMatcherType::ToolCategory => match rule.matcher.category {
                Some(ToolCategory::All) => true,
                Some(ToolCategory::Bash) => tool_name == "Bash",
                Some(ToolCategory::FileEdit) => {
                    matches!(tool_name, "Edit" | "Write" | "NotebookEdit")
                }
                Some(ToolCategory::Skill) => tool_name == "Skill",
                Some(ToolCategory::Agent) => tool_name == "Task",
                Some(ToolCategory::Mcp) => tool_name.starts_with("mcp__"),
                None => false,
            },
        }
    }

    // ===== Callback setters =====

    /// Inject activity callback (used by AgentPool to reset timeout timer).
    pub fn set_activity_callback<F: Fn(&str) + Send + Sync + 'static>(&self, cb: F) {
        *self.on_activity.lock().unwrap() = Some(Box::new(cb));
    }

    /// Inject permission-request notifier (WS notification to Web UI).
    pub fn set_permission_request_callback<
        F: Fn(&str, &str, PermissionPayload) + Send + Sync + 'static,
    >(
        &self,
        cb: F,
    ) {
        *self.on_permission_request.lock().unwrap() = Some(Box::new(cb));
    }

    /// Inject ask-question-request notifier (WS notification to Web UI).
    pub fn set_ask_question_request_callback<
        F: Fn(&str, &str, AskQuestionPayload) + Send + Sync + 'static,
    >(
        &self,
        cb: F,
    ) {
        *self.on_ask_question_request.lock().unwrap() = Some(Box::new(cb));
    }

    /// Inject form-request notifier (WS notification to Web UI).
    pub fn set_form_request_callback<F: Fn(&str, &str, FormPayload) + Send + Sync + 'static>(
        &self,
        cb: F,
    ) {
        *self.on_form_request.lock().unwrap() = Some(Box::new(cb));
    }

    /// Inject form-resolution notifier (broadcast to other endpoints).
    pub fn set_form_resolved_callback<
        F: Fn(&str, &str, HashMap<String, serde_json::Value>) + Send + Sync + 'static,
    >(
        &self,
        cb: F,
    ) {
        *self.on_form_resolved.lock().unwrap() = Some(Box::new(cb));
    }

    /// Inject permission-resolution notifier (broadcast to other endpoints).
    pub fn set_permission_resolved_callback<
        F: Fn(&str, &str, &str, &str) + Send + Sync + 'static,
    >(
        &self,
        cb: F,
    ) {
        *self.on_permission_resolved.lock().unwrap() = Some(Box::new(cb));
    }

    /// Inject callback fired when "allow" (never ask again) is selected.
    /// Receives `(group_jid, tool_name)` so callers can persist the approval.
    pub fn set_tool_allowed_callback<F: Fn(&str, &str) + Send + Sync + 'static>(&self, cb: F) {
        *self.on_tool_allowed.lock().unwrap() = Some(Box::new(cb));
    }

    /// Inject ask-question-resolution notifier (broadcast to other endpoints).
    pub fn set_ask_question_resolved_callback<
        F: Fn(&str, &str, HashMap<String, String>) + Send + Sync + 'static,
    >(
        &self,
        cb: F,
    ) {
        *self.on_ask_question_resolved.lock().unwrap() = Some(Box::new(cb));
    }

    // ===== Public resolution API (called from Web UI via WebSocket gateway) =====

    /// Resolve a pending permission request. First responder wins.
    /// Returns `false` if the request was already consumed.
    pub fn resolve_permission(&self, request_id: &str, option_key: &str) -> bool {
        let pending = {
            let mut map = self.pending_permissions.lock().unwrap();
            map.remove(request_id)
        };
        let Some(pending) = pending else {
            tracing::warn!(
                "[PermissionBridge] resolve permission ignored: unknown request id={}",
                request_id
            );
            return false;
        };

        tracing::info!(
            "[PermissionBridge] resolve permission id={} group_jid={} chat_jid={} tool={} option={}",
            request_id,
            pending.group_jid,
            pending.chat_jid,
            pending.tool_name,
            option_key
        );
        self.fire_activity(&pending.chat_jid);
        self.api
            .respond_to_tool_permission(&pending.group_jid, &pending.tool_name, option_key);
        self.record_gate_answer(&pending, option_key);

        if option_key == "allow" {
            if let Some(cb) = self.on_tool_allowed.lock().unwrap().as_ref() {
                cb(&pending.group_jid, &pending.permission_key);
            }
            self.install_approval_rule(&pending);
        }

        let label = capitalize_first(option_key);
        if let Some(cb) = self.on_permission_resolved.lock().unwrap().as_ref() {
            cb(&pending.chat_jid, request_id, option_key, &label);
        }
        true
    }

    /// Hand the person's answer to the gate that judged this request, so its
    /// audit log can say whether they agreed.
    fn record_gate_answer(&self, pending: &PendingPermission, option_key: &str) {
        if let Some(id) = pending.gate_log_id {
            if let Some(gate) = self.tool_gate.lock().unwrap().as_ref() {
                gate.record_answer(id, option_key);
            }
        }
    }

    /// Batch-answer a pending ask-question request (Web UI path).
    ///
    /// `answers`: `{ qi: oi }` single-select or `{ qi: [oi, ...] }` multi-select.
    /// `-1` = "Other" option.
    /// `other_texts`: `{ qi: "custom text" }` for the Other option.
    ///
    /// Returns `false` if the request was already consumed.
    pub fn resolve_ask_question_batch(
        &self,
        request_id: &str,
        answers: &serde_json::Value,
        other_texts: Option<&serde_json::Value>,
    ) -> bool {
        const OTHER_INDEX: i64 = -1;

        let pending = {
            let mut map = self.pending_ask_questions.lock().unwrap();
            map.remove(request_id)
        };
        let Some(pending) = pending else {
            return false;
        };

        let mut resolved: HashMap<String, String> = HashMap::new();

        if let Some(obj) = answers.as_object() {
            for (qi_str, selection) in obj {
                let qi: usize = match qi_str.parse() {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let question = match pending.questions.get(qi) {
                    Some(q) => q,
                    None => continue,
                };

                let resolve_option = |oi: i64| -> String {
                    if oi == OTHER_INDEX {
                        return other_texts
                            .and_then(|ot| ot.get(qi_str))
                            .and_then(|v| v.as_str())
                            .unwrap_or("Other")
                            .to_string();
                    }
                    question
                        .options
                        .get(oi as usize)
                        .map(|o| o.label.clone())
                        .unwrap_or_default()
                };

                let label = if let Some(arr) = selection.as_array() {
                    let labels: Vec<String> = arr
                        .iter()
                        .filter_map(|v| v.as_i64().map(resolve_option))
                        .filter(|s| !s.is_empty())
                        .collect();
                    labels.join(",")
                } else if let Some(n) = selection.as_i64() {
                    resolve_option(n)
                } else {
                    continue;
                };

                resolved.insert(question.question.clone(), label);
            }
        }

        self.fire_activity(&pending.chat_jid);
        self.api
            .respond_to_ask_question(&pending.group_jid, &pending.agent_id, resolved.clone());

        if let Some(cb) = self.on_ask_question_resolved.lock().unwrap().as_ref() {
            cb(&pending.chat_jid, request_id, resolved);
        }
        true
    }

    /// Resolve a pending FormUI request (Web UI / app path). `submitted = false`
    /// means the user skipped the form. First responder wins — returns `false`
    /// if the request was already consumed.
    pub fn resolve_form(
        &self,
        request_id: &str,
        values: HashMap<String, serde_json::Value>,
        submitted: bool,
    ) -> bool {
        let pending = {
            let mut map = self.pending_forms.lock().unwrap();
            map.remove(request_id)
        };
        let Some(pending) = pending else {
            tracing::warn!(
                "[PermissionBridge] resolve form ignored: unknown request id={}",
                request_id
            );
            return false;
        };

        tracing::info!(
            "[PermissionBridge] resolve form id={} group_jid={} chat_jid={} submitted={} values={}",
            request_id,
            pending.group_jid,
            pending.chat_jid,
            submitted,
            values.len()
        );
        self.fire_activity(&pending.chat_jid);
        self.api.respond_to_form(
            &pending.group_jid,
            &pending.agent_id,
            values.clone(),
            submitted,
        );

        if let Some(cb) = self.on_form_resolved.lock().unwrap().as_ref() {
            cb(&pending.chat_jid, request_id, values);
        }
        true
    }

    // ===== Incoming request handlers (called when sema-core fires events) =====

    /// Handle a `tool:permission:request` event from sema-core.
    /// Sends inline keyboard to channel (if supported) and notifies Web UI via callback.
    #[allow(clippy::too_many_arguments)]
    pub fn handle_permission_request(
        &self,
        tool_name: &str,
        permission_key: &str,
        title: &str,
        content: &serde_json::Value,
        options: &HashMap<String, String>,
        group_jid: &str,
        chat_jid: &str,
        bot_token: Option<&str>,
    ) {
        if self.auto_accept(tool_name, content, group_jid, chat_jid) {
            return;
        }
        self.prompt_permission(
            tool_name,
            permission_key,
            title,
            content,
            options,
            group_jid,
            chat_jid,
            bot_token,
            None,
        );
    }

    /// [`Self::handle_permission_request`] with the tool gate consulted first:
    /// a request no rule accepted, that the gate wants, is judged in the
    /// background and either approved once (`agree` — never a saved grant) or
    /// shown to the person as usual. Without a gate, or outside a runtime, it
    /// is exactly the ungated path.
    #[allow(clippy::too_many_arguments)]
    pub fn handle_permission_request_gated(
        self: &Arc<Self>,
        tool_name: &str,
        permission_key: &str,
        title: &str,
        content: &serde_json::Value,
        options: &HashMap<String, String>,
        group_jid: &str,
        chat_jid: &str,
        bot_token: Option<&str>,
    ) {
        if self.auto_accept(tool_name, content, group_jid, chat_jid) {
            return;
        }
        let gate = self
            .tool_gate
            .lock()
            .unwrap()
            .clone()
            .filter(|g| g.wants(tool_name, content));
        let (Some(gate), Ok(rt)) = (gate, tokio::runtime::Handle::try_current()) else {
            self.prompt_permission(
                tool_name,
                permission_key,
                title,
                content,
                options,
                group_jid,
                chat_jid,
                bot_token,
                None,
            );
            return;
        };

        let this = Arc::clone(self);
        let (tool_name, permission_key, title, content, options, group_jid, chat_jid, bot_token) = (
            tool_name.to_string(),
            permission_key.to_string(),
            title.to_string(),
            content.clone(),
            options.clone(),
            group_jid.to_string(),
            chat_jid.to_string(),
            bot_token.map(str::to_string),
        );
        rt.spawn(async move {
            // Judged in a task of its own: a gate that panics must still
            // leave the person a prompt, or the agent waits forever.
            let judged = {
                let (tool_name, content, chat_jid) = (tool_name.clone(), content.clone(), chat_jid.clone());
                tokio::spawn(async move { gate.judge(&tool_name, &content, &chat_jid).await })
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!("[PermissionBridge] tool gate failed ({e}); prompting");
                        GateCall::default()
                    })
            };
            if judged.allow {
                tracing::info!("[PermissionBridge] tool gate approved tool={tool_name} group={group_jid}");
                this.api.respond_to_tool_permission(&group_jid, &tool_name, "agree");
                this.fire_activity(&chat_jid);
                return;
            }
            this.prompt_permission(
                &tool_name,
                &permission_key,
                &title,
                &content,
                &options,
                &group_jid,
                &chat_jid,
                bot_token.as_deref(),
                judged.log_id,
            );
        });
    }

    /// Accept the request on a matching rule (no prompt). Returns whether it did.
    fn auto_accept(&self, tool_name: &str, content: &serde_json::Value, group_jid: &str, chat_jid: &str) -> bool {
        let rule_count = self.tool_rules.lock().unwrap().len();
        let accept_all = *self.accept_all.lock().unwrap();
        tracing::info!("[PermissionBridge] permission request tool={tool_name} accept_all={accept_all} rules={rule_count}");
        if !self.should_auto_accept(tool_name, content) {
            return false;
        }
        tracing::info!("[PermissionBridge] auto-accepting tool={tool_name} group={group_jid}");
        self.api.respond_to_tool_permission(group_jid, tool_name, "allow");
        self.fire_activity(chat_jid);
        true
    }

    /// Show the request to the person: channel buttons and/or the Web UI.
    #[allow(clippy::too_many_arguments)]
    fn prompt_permission(
        &self,
        tool_name: &str,
        permission_key: &str,
        title: &str,
        content: &serde_json::Value,
        options: &HashMap<String, String>,
        group_jid: &str,
        chat_jid: &str,
        bot_token: Option<&str>,
        gate_log_id: Option<i64>,
    ) {
        let request_id = short_id();
        {
            let mut map = self.pending_permissions.lock().unwrap();
            map.insert(
                request_id.clone(),
                PendingPermission {
                    tool_name: tool_name.to_string(),
                    permission_key: if permission_key.is_empty() {
                        tool_name.to_string()
                    } else {
                        permission_key.to_string()
                    },
                    chat_jid: chat_jid.to_string(),
                    group_jid: group_jid.to_string(),
                    gate_log_id,
                },
            );
        }

        let raw_content = format_content(content);
        let content_str = truncate_content(&raw_content, self.max_content_length);

        let text =
            format!("🔐 *Permission Request*\n\nTool: {tool_name}\n{title}\n\n{content_str}");

        let buttons: Vec<InlineButton> = options
            .iter()
            .map(|(key, label)| InlineButton {
                label: label.clone(),
                callback_data: format!("{PREFIX_PERM}:{request_id}:{key}"),
            })
            .collect();

        let is_web = self.api.is_web_jid(chat_jid);
        tracing::info!(
            "[PermissionBridge] request id={} group_jid={} chat_jid={} tool={} is_web={} options={}",
            request_id,
            group_jid,
            chat_jid,
            tool_name,
            is_web,
            options.len()
        );

        // Try channel send with buttons
        if !is_web {
            if self.api.supports_buttons(chat_jid) {
                if let Err(e) = self
                    .api
                    .send_with_buttons(chat_jid, &text, &buttons, bot_token)
                {
                    tracing::warn!(
                        "[PermissionBridge] send_with_buttons failed for {chat_jid}: {e}"
                    );
                }
            } else if self.on_permission_request.lock().unwrap().is_none() {
                // No WS sink → downgrade to plain text + auto-deny
                let option_lines: String = buttons
                    .iter()
                    .map(|b| format!("• {}", b.label))
                    .collect::<Vec<_>>()
                    .join("\n");
                let _ = self.api.send_message(
                    chat_jid,
                    &format!(
                        "{text}\n\nOptions:\n{option_lines}\n\n\
                         (This channel does not support interactive buttons. \
                         Please contact the administrator for configuration)"
                    ),
                    bot_token,
                );
                self.api
                    .respond_to_tool_permission(group_jid, tool_name, "refuse");
                self.pending_permissions.lock().unwrap().remove(&request_id);
                return;
            }
        }

        // Notify Web UI
        if let Some(cb) = self.on_permission_request.lock().unwrap().as_ref() {
            tracing::info!(
                "[PermissionBridge] notify UI request id={} chat_jid={} tool={}",
                request_id,
                chat_jid,
                tool_name
            );
            cb(
                chat_jid,
                &request_id,
                PermissionPayload {
                    tool_name: tool_name.to_string(),
                    title: title.to_string(),
                    content: raw_content,
                    options: options
                        .iter()
                        .map(|(key, label)| PermissionOption {
                            key: key.clone(),
                            label: label.clone(),
                        })
                        .collect(),
                },
            );
        }

        self.fire_activity(chat_jid);
    }

    /// Handle an `ask:question:request` event from sema-core.
    /// Sends each question as an inline-keyboard message and notifies Web UI.
    pub fn handle_ask_question_request(
        &self,
        agent_id: &str,
        questions: Vec<AskQuestionData>,
        group_jid: &str,
        chat_jid: &str,
        bot_token: Option<&str>,
    ) {
        let request_id = short_id();
        let pending_count = questions.len();
        {
            let mut map = self.pending_ask_questions.lock().unwrap();
            map.insert(
                request_id.clone(),
                PendingAskQuestion {
                    agent_id: agent_id.to_string(),
                    chat_jid: chat_jid.to_string(),
                    group_jid: group_jid.to_string(),
                    questions: questions.clone(),
                    answers: HashMap::new(),
                    pending_count,
                },
            );
        }

        let is_web = self.api.is_web_jid(chat_jid);

        // Send each question to the channel if buttons supported
        if !is_web && self.api.supports_buttons(chat_jid) {
            for (qi, q) in questions.iter().enumerate() {
                let text = format!("❓ *{}*\n\n{}", q.header, q.question);
                let buttons: Vec<InlineButton> = q
                    .options
                    .iter()
                    .enumerate()
                    .map(|(oi, opt)| InlineButton {
                        label: opt.label.clone(),
                        callback_data: format!("{PREFIX_ASK}:{request_id}:{qi}:{oi}"),
                    })
                    .collect();
                if let Err(e) = self
                    .api
                    .send_with_buttons(chat_jid, &text, &buttons, bot_token)
                {
                    tracing::warn!(
                        "[PermissionBridge] ask-question send_with_buttons failed for {chat_jid}: {e}"
                    );
                }
            }
        }

        // Notify Web UI
        if let Some(cb) = self.on_ask_question_request.lock().unwrap().as_ref() {
            cb(
                chat_jid,
                &request_id,
                AskQuestionPayload {
                    agent_id: agent_id.to_string(),
                    questions,
                },
            );
        }

        self.fire_activity(chat_jid);
    }

    /// Handle a `form:request` event from sema-core.
    ///
    /// Rich forms can't be expressed as channel inline keyboards. Non-web
    /// chats with no WS sink degrade to a read-only text snapshot plus an
    /// auto-submit of each field's default (`submitted = false`) so the agent
    /// never blocks forever. Otherwise the full form structure is pushed to
    /// the Web UI / app via the `on_form_request` callback.
    pub fn handle_form_request(
        &self,
        request: &crate::zen_core::FormRequestData,
        group_jid: &str,
        chat_jid: &str,
        bot_token: Option<&str>,
    ) {
        let request_id = short_id();
        {
            let mut map = self.pending_forms.lock().unwrap();
            map.insert(
                request_id.clone(),
                PendingForm {
                    agent_id: request.agent_id.clone(),
                    chat_jid: chat_jid.to_string(),
                    group_jid: group_jid.to_string(),
                },
            );
        }

        let is_web = self.api.is_web_jid(chat_jid);
        tracing::info!(
            "[PermissionBridge] form request id={} group_jid={} chat_jid={} title={} surface={} fields={} is_web={}",
            request_id,
            group_jid,
            chat_jid,
            request.title,
            request.surface,
            request.fields.len(),
            is_web
        );

        // Non-web channel with no WS sink → degrade: send read-only snapshot,
        // auto-submit defaults so the agent isn't blocked forever.
        if !is_web && self.on_form_request.lock().unwrap().is_none() {
            let snapshot = format_form_snapshot(request);
            if let Err(e) = self.api.send_message(chat_jid, &snapshot, bot_token) {
                tracing::warn!("[PermissionBridge] form snapshot send failed for {chat_jid}: {e}");
            }
            self.pending_forms.lock().unwrap().remove(&request_id);
            self.api.respond_to_form(
                group_jid,
                &request.agent_id,
                build_default_values(&request.fields),
                false,
            );
            return;
        }

        // Notify Web UI / app with the full form structure.
        if let Some(cb) = self.on_form_request.lock().unwrap().as_ref() {
            cb(
                chat_jid,
                &request_id,
                FormPayload {
                    agent_id: request.agent_id.clone(),
                    title: request.title.clone(),
                    surface: request.surface.clone(),
                    submit_label: request.submit_label.clone(),
                    fields: request.fields.clone(),
                },
            );
        }

        // Form pushed → reset the group idle timer (agent is waiting on the user).
        self.fire_activity(chat_jid);
    }

    // ===== Callback routing (called when a channel receives an inline-button press) =====

    /// Route an inline-keyboard callback press. Returns confirmation text to show
    /// the user (e.g. via Telegram `answerCallbackQuery`), or `None` if unknown.
    pub fn handle_callback(&self, callback_data: &str, _chat_jid: &str) -> Option<String> {
        if let Some(rest) = callback_data.strip_prefix(&format!("{PREFIX_PERM}:")) {
            return self.handle_permission_callback(rest);
        }
        if let Some(rest) = callback_data.strip_prefix(&format!("{PREFIX_ASK}:")) {
            return self.handle_ask_question_callback(rest);
        }
        None
    }

    // ===== Internal =====

    fn fire_activity(&self, chat_jid: &str) {
        if let Some(cb) = self.on_activity.lock().unwrap().as_ref() {
            cb(chat_jid);
        }
    }

    /// Parse `P:{requestId}:{optionKey}` (prefix already stripped → `{requestId}:{optionKey}`).
    fn handle_permission_callback(&self, rest: &str) -> Option<String> {
        let colon = rest.find(':')?;
        let request_id = &rest[..colon];
        let option_key = &rest[colon + 1..];

        let pending = self
            .pending_permissions
            .lock()
            .unwrap()
            .remove(request_id)?;

        self.fire_activity(&pending.chat_jid);

        self.api
            .respond_to_tool_permission(&pending.group_jid, &pending.tool_name, option_key);
        self.record_gate_answer(&pending, option_key);

        if option_key == "allow" {
            if let Some(cb) = self.on_tool_allowed.lock().unwrap().as_ref() {
                cb(&pending.group_jid, &pending.permission_key);
            }
            self.install_approval_rule(&pending);
        }

        let label = capitalize_first(option_key);
        if let Some(cb) = self.on_permission_resolved.lock().unwrap().as_ref() {
            cb(&pending.chat_jid, request_id, option_key, &label);
        }

        Some(format!("✅ Selected: {label}"))
    }

    /// Parse `Q:{requestId}:{qi}:{oi}` (prefix already stripped → `{requestId}:{qi}:{oi}`).
    fn handle_ask_question_callback(&self, rest: &str) -> Option<String> {
        let mut parts = rest.splitn(3, ':');
        let request_id = parts.next()?;
        let qi: usize = parts.next()?.parse().ok()?;
        let oi: usize = parts.next()?.parse().ok()?;

        let question_label = {
            let map = self.pending_ask_questions.lock().unwrap();
            let pending = map.get(request_id)?;
            let question = pending.questions.get(qi)?;
            let option = question.options.get(oi)?;
            option.label.clone()
        };

        // Re-acquire for mutation
        let (should_resolve, resolved_answers, chat_jid, group_jid, agent_id) = {
            let mut map = self.pending_ask_questions.lock().unwrap();
            let pending = match map.get_mut(request_id) {
                Some(p) => p,
                None => return Some(format!("✅ Selected: {question_label}")),
            };

            let question = match pending.questions.get(qi) {
                Some(q) => q,
                None => return Some(format!("✅ Selected: {question_label}")),
            };

            pending
                .answers
                .insert(question.question.clone(), question_label.clone());
            pending.pending_count = pending.pending_count.saturating_sub(1);

            self.fire_activity(&pending.chat_jid);

            if pending.pending_count == 0 {
                let answers = pending.answers.clone();
                let chat_jid = pending.chat_jid.clone();
                let group_jid = pending.group_jid.clone();
                let agent_id = pending.agent_id.clone();
                map.remove(request_id);
                (true, answers, chat_jid, group_jid, agent_id)
            } else {
                return Some(format!("✅ Selected: {question_label}"));
            }
        };

        if should_resolve {
            self.api
                .respond_to_ask_question(&group_jid, &agent_id, resolved_answers.clone());
            if let Some(cb) = self.on_ask_question_resolved.lock().unwrap().as_ref() {
                cb(&chat_jid, request_id, resolved_answers);
            }
        }

        Some(format!("✅ Selected: {question_label}"))
    }
}

/// Initial `values` assembled from each field's declared default — used by the
/// degraded (no interactive UI) auto-submit path.
fn build_default_values(
    fields: &[crate::zen_core::FormField],
) -> HashMap<String, serde_json::Value> {
    let mut values = HashMap::new();
    for f in fields {
        if let (Some(key), Some(default)) = (f.key(), f.default_value()) {
            values.insert(key.to_string(), default);
        }
    }
    values
}

/// Render the form as a read-only text snapshot (degraded channels).
fn format_form_snapshot(request: &crate::zen_core::FormRequestData) -> String {
    let lines: Vec<String> = request
        .fields
        .iter()
        .map(|f| match f {
            crate::zen_core::FormField::StaticText { text, .. } => text.clone(),
            _ => {
                let label = f.label().unwrap_or_default();
                let required = if f.required() { " *" } else { "" };
                let default = f
                    .default_value()
                    .map(|v| format!(" (default: {v})"))
                    .unwrap_or_default();
                format!("• {label}{required}{default}")
            }
        })
        .collect();
    format!(
        "📋 *{}*\n\n{}\n\n(This channel does not support interactive forms; \
         defaults were used. Please use the Web UI to fill it in.)",
        request.title,
        lines.join("\n")
    )
}

fn default_tool_rules() -> Vec<ToolAutoAcceptRule> {
    Vec::new()
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_dp(&p, &t)
}

fn glob_dp(p: &[char], t: &[char]) -> bool {
    let (m, n) = (p.len(), t.len());
    let mut dp = vec![vec![false; n + 1]; m + 1];
    dp[0][0] = true;
    for i in 1..=m {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=m {
        for j in 1..=n {
            if p[i - 1] == '*' {
                dp[i][j] = dp[i - 1][j] || dp[i][j - 1];
            } else if p[i - 1] == '?' || p[i - 1] == t[j - 1] {
                dp[i][j] = dp[i - 1][j - 1];
            }
        }
    }
    dp[m][n]
}
