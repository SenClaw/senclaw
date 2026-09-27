//! HookManager — matches incoming events to configured hooks.
//!
//! Port of TS `hooks/HookManager.ts`.

use std::sync::RwLock;

use regex::Regex;
use tracing::warn;

use super::types::{HookConfig, HookDefinition, HookEvent, HookInput};

pub struct HookManager {
    config: RwLock<HookConfig>,
}

impl HookManager {
    pub fn new(config: HookConfig) -> Self {
        Self {
            config: RwLock::new(config),
        }
    }

    pub fn empty() -> Self {
        Self::new(HookConfig::default())
    }

    pub fn update_config(&self, config: HookConfig) {
        *self.config.write().unwrap() = config;
    }

    pub fn get_config(&self) -> HookConfig {
        self.config.read().unwrap().clone()
    }

    pub fn has_hooks_for_event(&self, event: &HookEvent) -> bool {
        self.config
            .read()
            .unwrap()
            .hooks
            .get(event)
            .map_or(false, |v| !v.is_empty())
    }

    /// Return all `HookDefinition`s that match the given event and input.
    ///
    /// Matching rules (same as TS):
    /// 1. `matcher` — glob pattern against `match_query` (tool name, notification type).
    ///    If the event config has no matcher, it matches everything.
    /// 2. `if` — regex applied to the serialised `tool_input` / message text.
    ///    If the event config has no `if`, it matches everything.
    pub fn get_matching_hooks(&self, event: &HookEvent, input: &HookInput) -> Vec<HookDefinition> {
        let config = self.config.read().unwrap();
        let event_configs = match config.hooks.get(event) {
            Some(v) => v,
            None => return vec![],
        };

        let match_query = input.match_query();
        let match_text = input.extract_match_text();
        let mut matched = Vec::new();

        for event_cfg in event_configs {
            // Step 1: matcher glob
            if let Some(ref pattern) = event_cfg.matcher {
                match match_query {
                    Some(q) if matches_glob(pattern, q) => {}
                    Some(_) => continue, // pattern present, didn't match
                    None => continue,    // pattern present but no query to match against
                }
            }

            // Step 2: `if` regex
            if let Some(ref pattern) = event_cfg.if_condition {
                if !matches_regex(pattern, &match_text) {
                    continue;
                }
            }

            matched.extend(event_cfg.hooks.iter().cloned());
        }

        matched
    }
}

// ============================================================================
// Matching helpers
// ============================================================================

/// Glob pattern matching: supports `*` wildcard and comma-separated alternatives.
///
/// Examples:
///   `"Bash"` → matches exactly "Bash"
///   `"Bash,Write"` → matches "Bash" or "Write"
///   `"Bash*"` → matches anything starting with "Bash"
///   `"*"` → matches everything
fn matches_glob(pattern: &str, query: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    pattern.split(',').map(str::trim).any(|p| {
        if p.contains('*') {
            // Convert glob to regex: escape special chars except *, replace * with .*
            let escaped = regex::escape(p).replace("\\*", ".*");
            Regex::new(&format!("^{escaped}$"))
                .map(|re| re.is_match(query))
                .unwrap_or(false)
        } else {
            p == query
        }
    })
}

/// Regex condition matching applied to the input text.
fn matches_regex(pattern: &str, text: &str) -> bool {
    match Regex::new(pattern) {
        Ok(re) => re.is_match(text),
        Err(e) => {
            warn!("[hooks] Invalid 'if' regex pattern '{pattern}': {e}");
            false
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zen_core::hooks::types::{
        HookDefinition, HookEventConfig, HookInput, HookInputBase, HookType, PreToolUseInput,
    };
    use chrono::Utc;
    use std::collections::HashMap;

    fn base(event: HookEvent) -> HookInputBase {
        HookInputBase {
            hook_event_name: event,
            session_id: "s1".into(),
            agent_id: "main".into(),
            timestamp: Utc::now().to_rfc3339(),
            cwd: "/tmp".into(),
        }
    }

    fn bash_hook() -> HookDefinition {
        HookDefinition {
            hook_type: HookType::Command,
            command: Some("echo ok".into()),
            prompt: None,
            timeout: None,
            blocking: None,
            is_async: None,
            include_history: None,
            history_limit: None,
        }
    }

    #[test]
    fn matches_exact_tool_name() {
        let mut hooks: HashMap<HookEvent, Vec<HookEventConfig>> = HashMap::new();
        hooks.insert(
            HookEvent::PreToolUse,
            vec![HookEventConfig {
                matcher: Some("Bash".into()),
                if_condition: None,
                hooks: vec![bash_hook()],
            }],
        );
        let mgr = HookManager::new(HookConfig { hooks });

        let input = HookInput::PreToolUse(PreToolUseInput {
            base: base(HookEvent::PreToolUse),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
        });

        assert_eq!(
            mgr.get_matching_hooks(&HookEvent::PreToolUse, &input).len(),
            1
        );
    }

    #[test]
    fn no_match_on_wrong_tool_name() {
        let mut hooks: HashMap<HookEvent, Vec<HookEventConfig>> = HashMap::new();
        hooks.insert(
            HookEvent::PreToolUse,
            vec![HookEventConfig {
                matcher: Some("Write".into()),
                if_condition: None,
                hooks: vec![bash_hook()],
            }],
        );
        let mgr = HookManager::new(HookConfig { hooks });

        let input = HookInput::PreToolUse(PreToolUseInput {
            base: base(HookEvent::PreToolUse),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
        });

        assert!(mgr
            .get_matching_hooks(&HookEvent::PreToolUse, &input)
            .is_empty());
    }

    #[test]
    fn if_condition_filters_by_command_content() {
        let mut hooks: HashMap<HookEvent, Vec<HookEventConfig>> = HashMap::new();
        hooks.insert(
            HookEvent::PreToolUse,
            vec![HookEventConfig {
                matcher: Some("Bash".into()),
                if_condition: Some("git commit".into()),
                hooks: vec![bash_hook()],
            }],
        );
        let mgr = HookManager::new(HookConfig { hooks });

        let matching = HookInput::PreToolUse(PreToolUseInput {
            base: base(HookEvent::PreToolUse),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "git commit -m 'test'"}),
        });
        let non_matching = HookInput::PreToolUse(PreToolUseInput {
            base: base(HookEvent::PreToolUse),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls -la"}),
        });

        assert_eq!(
            mgr.get_matching_hooks(&HookEvent::PreToolUse, &matching)
                .len(),
            1
        );
        assert!(mgr
            .get_matching_hooks(&HookEvent::PreToolUse, &non_matching)
            .is_empty());
    }

    #[test]
    fn pre_permission_event_matches_by_tool_name() {
        use crate::zen_core::hooks::types::PrePermissionInput;
        let mut hooks: HashMap<HookEvent, Vec<HookEventConfig>> = HashMap::new();
        hooks.insert(
            HookEvent::PrePermission,
            vec![HookEventConfig {
                matcher: Some("Bash".into()),
                if_condition: None,
                hooks: vec![bash_hook()],
            }],
        );
        let mgr = HookManager::new(HookConfig { hooks });

        let input = HookInput::PrePermission(PrePermissionInput {
            base: base(HookEvent::PrePermission),
            tool_name: "Bash".into(),
            tool_input: serde_json::json!({"command": "ls"}),
        });
        assert_eq!(
            mgr.get_matching_hooks(&HookEvent::PrePermission, &input)
                .len(),
            1
        );
    }

    #[test]
    fn output_filter_event_matches_by_tool_name() {
        use crate::zen_core::hooks::types::OutputFilterInput;
        let mut hooks: HashMap<HookEvent, Vec<HookEventConfig>> = HashMap::new();
        hooks.insert(
            HookEvent::OutputFilter,
            vec![HookEventConfig {
                matcher: Some("Read".into()),
                if_condition: None,
                hooks: vec![bash_hook()],
            }],
        );
        let mgr = HookManager::new(HookConfig { hooks });

        let input = HookInput::OutputFilter(OutputFilterInput {
            base: base(HookEvent::OutputFilter),
            tool_name: "Read".into(),
            tool_input: serde_json::json!({"path": "x"}),
            tool_output: serde_json::json!({"content": "abc"}),
        });
        assert_eq!(
            mgr.get_matching_hooks(&HookEvent::OutputFilter, &input)
                .len(),
            1
        );
        assert!(mgr.has_hooks_for_event(&HookEvent::OutputFilter));
    }

    #[test]
    fn empty_manager_has_no_hooks_for_new_events() {
        let mgr = HookManager::empty();
        assert!(!mgr.has_hooks_for_event(&HookEvent::PrePermission));
        assert!(!mgr.has_hooks_for_event(&HookEvent::OutputFilter));
    }

    #[test]
    fn wildcard_matcher_matches_all_tools() {
        let mut hooks: HashMap<HookEvent, Vec<HookEventConfig>> = HashMap::new();
        hooks.insert(
            HookEvent::PreToolUse,
            vec![HookEventConfig {
                matcher: Some("*".into()),
                if_condition: None,
                hooks: vec![bash_hook()],
            }],
        );
        let mgr = HookManager::new(HookConfig { hooks });

        for tool in &["Bash", "Write", "Read", "Edit"] {
            let input = HookInput::PreToolUse(PreToolUseInput {
                base: base(HookEvent::PreToolUse),
                tool_name: tool.to_string(),
                tool_input: serde_json::json!({}),
            });
            assert_eq!(
                mgr.get_matching_hooks(&HookEvent::PreToolUse, &input).len(),
                1,
                "Expected match for tool {tool}"
            );
        }
    }
}
