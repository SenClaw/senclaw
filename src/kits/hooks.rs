//! Hooks a kit registers, written as one file per kit.
//!
//! `<kits_dir>/hooks/<kit_id>.json` holds exactly the shape
//! `agent::hook_config_loader` already reads, and those files are handed to
//! the loader as *extra files* — the same slot marketplace plugin hooks use.
//! Two consequences, both deliberate:
//!
//! * **Uninstall is a delete.** A kit never edits the user's own `hooks.json`,
//!   so removing a kit can't mangle hooks the user wrote by hand.
//! * **Kits inherit the untrusted-source policy.** The loader refuses
//!   `type: "command"` from extra files unless the operator opted in, because
//!   a command hook is `sh -c` at daemon privilege — a supply-chain RCE and a
//!   restart-surviving foothold. This module refuses to *write* one at all, so
//!   the rejection is visible at install time instead of silently at load.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::manifest::{safe_segment, KitManifest};

/// Events the engine's hook loader accepts. Kept in sync with
/// `hook_config_loader::VALID_HOOK_EVENTS`; an unknown name here would make
/// the loader drop the whole group at load time with only a log line.
pub const KIT_HOOK_EVENTS: &[&str] = &[
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
    "SessionStart",
    "SessionEnd",
    "PreCompact",
    "PostCompact",
];

/// Default timeout for a kit's prompt hook, in seconds. A hook that never
/// returns would otherwise hold the agent loop for as long as the model wants.
const DEFAULT_TIMEOUT_SECS: u32 = 30;

#[derive(Debug, Serialize)]
struct HookFile {
    hooks: BTreeMap<String, Vec<EventGroup>>,
}

#[derive(Debug, Serialize)]
struct EventGroup {
    #[serde(skip_serializing_if = "Option::is_none")]
    matcher: Option<String>,
    #[serde(rename = "if", skip_serializing_if = "Option::is_none")]
    condition: Option<String>,
    hooks: Vec<HookEntry>,
}

#[derive(Debug, Serialize)]
struct HookEntry {
    #[serde(rename = "type")]
    hook_type: &'static str,
    prompt: String,
    timeout: u32,
    blocking: bool,
}

pub enum KitHookOutcome {
    Written { path: PathBuf, accepted: usize },
    Rejected(String),
}

/// Directory holding one hook file per kit.
pub fn kit_hooks_dir(kits_dir: &Path) -> PathBuf {
    kits_dir.join("hooks")
}

/// Path this kit's hooks live at.
pub fn kit_hook_path(kits_dir: &Path, kit_id: &str) -> PathBuf {
    kit_hooks_dir(kits_dir).join(format!("{}.json", safe_segment(kit_id)))
}

/// Every kit hook file on disk, sorted for a stable load order.
///
/// This is what the daemon feeds to the hook loader alongside the user's own
/// `hooks.json`.
pub fn kit_hook_files(kits_dir: &Path) -> Vec<PathBuf> {
    let dir = kit_hooks_dir(kits_dir);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    files.sort();
    files
}

/// Write this kit's hooks. Returns how many entries were accepted.
///
/// Entries naming an event the engine does not know are dropped with a warning
/// rather than failing the install — one bad hook should not cost the user the
/// rest of the kit. If *nothing* survives, that is a rejection: writing an
/// empty file would claim hooks were installed when none were.
pub fn write_kit_hooks(kits_dir: &Path, kit: &KitManifest) -> KitHookOutcome {
    let mut grouped: BTreeMap<String, Vec<EventGroup>> = BTreeMap::new();
    let mut accepted = 0usize;

    for hook in &kit.hooks {
        if !KIT_HOOK_EVENTS.contains(&hook.event.as_str()) {
            tracing::warn!(
                "[kits] {} declares hook event '{}', which this build does not know — skipped",
                kit.id,
                hook.event
            );
            continue;
        }
        if hook.prompt.trim().is_empty() {
            tracing::warn!("[kits] {} has a hook with an empty prompt — skipped", kit.id);
            continue;
        }
        grouped
            .entry(hook.event.clone())
            .or_default()
            .push(EventGroup {
                matcher: hook.matcher.clone(),
                condition: hook.if_condition.clone(),
                hooks: vec![HookEntry {
                    // Prompt only, always. See the module docs: a kit is an
                    // untrusted bundle, and shell at daemon privilege is not
                    // something one tap should be able to install.
                    hook_type: "prompt",
                    prompt: hook.prompt.clone(),
                    timeout: hook.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS),
                    blocking: hook.blocking,
                }],
            });
        accepted += 1;
    }

    if accepted == 0 {
        return KitHookOutcome::Rejected(
            "no usable hooks (unknown event names or empty prompts)".into(),
        );
    }

    let path = kit_hook_path(kits_dir, &kit.id);
    let body = match serde_json::to_string_pretty(&HookFile { hooks: grouped }) {
        Ok(body) => body,
        Err(e) => return KitHookOutcome::Rejected(e.to_string()),
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return KitHookOutcome::Rejected(e.to_string());
        }
    }
    match fs::write(&path, body) {
        Ok(()) => KitHookOutcome::Written { path, accepted },
        Err(e) => KitHookOutcome::Rejected(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kits::manifest::KitHook;

    fn kit_with(hooks: Vec<KitHook>) -> KitManifest {
        KitManifest {
            manifest: 2,
            id: "demo".into(),
            name: "Demo".into(),
            version: "1.0.0".into(),
            hooks,
            ..Default::default()
        }
    }

    fn hook(event: &str) -> KitHook {
        KitHook {
            event: event.into(),
            matcher: None,
            if_condition: None,
            prompt: "check it".into(),
            timeout: None,
            blocking: false,
        }
    }

    #[test]
    fn writes_the_shape_the_loader_reads() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = hook("PreToolUse");
        h.matcher = Some("Bash".into());
        h.if_condition = Some("rm -rf".into());

        let outcome = write_kit_hooks(dir.path(), &kit_with(vec![h]));

        let KitHookOutcome::Written { path, accepted } = outcome else {
            panic!("expected the hooks to be written");
        };
        assert_eq!(accepted, 1);

        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let group = &value["hooks"]["PreToolUse"][0];
        assert_eq!(group["matcher"], "Bash");
        assert_eq!(group["if"], "rm -rf");
        assert_eq!(group["hooks"][0]["type"], "prompt");
        assert_eq!(group["hooks"][0]["timeout"], DEFAULT_TIMEOUT_SECS);
        assert_eq!(group["hooks"][0]["blocking"], false);
    }

    #[test]
    fn a_kit_can_never_register_shell() {
        // The manifest has no field for it, and the writer only ever emits
        // `type: "prompt"` — so even a hostile kit cannot get `sh -c` at
        // daemon privilege through this path.
        let dir = tempfile::tempdir().unwrap();
        write_kit_hooks(dir.path(), &kit_with(vec![hook("SessionStart")]));

        let body = fs::read_to_string(kit_hook_path(dir.path(), "demo")).unwrap();
        assert!(!body.contains("\"command\""));
        assert!(body.contains("\"prompt\""));
    }

    #[test]
    fn unknown_events_are_dropped_but_the_rest_survives() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = write_kit_hooks(
            dir.path(),
            &kit_with(vec![hook("NotAnEvent"), hook("Stop")]),
        );

        let KitHookOutcome::Written { accepted, path } = outcome else {
            panic!("one good hook should still install");
        };
        assert_eq!(accepted, 1);
        let body = fs::read_to_string(path).unwrap();
        assert!(body.contains("Stop"));
        assert!(!body.contains("NotAnEvent"));
    }

    #[test]
    fn rejects_when_nothing_usable_remains() {
        let dir = tempfile::tempdir().unwrap();
        let mut empty = hook("Stop");
        empty.prompt = "   ".into();

        let outcome = write_kit_hooks(dir.path(), &kit_with(vec![hook("Nope"), empty]));

        assert!(matches!(outcome, KitHookOutcome::Rejected(_)));
        // An empty file would claim hooks were installed when none were.
        assert!(!kit_hook_path(dir.path(), "demo").exists());
    }

    #[test]
    fn several_hooks_on_one_event_are_all_kept() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = write_kit_hooks(
            dir.path(),
            &kit_with(vec![hook("PreToolUse"), hook("PreToolUse")]),
        );

        let KitHookOutcome::Written { path, accepted } = outcome else {
            panic!("expected written");
        };
        assert_eq!(accepted, 2);
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(value["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn kit_hook_files_lists_only_json_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        write_kit_hooks(dir.path(), &kit_with(vec![hook("Stop")]));
        let mut other = kit_with(vec![hook("Stop")]);
        other.id = "another".into();
        write_kit_hooks(dir.path(), &other);
        fs::write(kit_hooks_dir(dir.path()).join("notes.txt"), "ignore me").unwrap();

        let files = kit_hook_files(dir.path());

        assert_eq!(files.len(), 2);
        assert!(files[0].ends_with("another.json"));
        assert!(files[1].ends_with("demo.json"));
    }

    #[test]
    fn missing_directory_lists_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(kit_hook_files(dir.path()).is_empty());
    }

    #[test]
    fn kit_id_cannot_escape_the_hooks_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = kit_hook_path(dir.path(), "../../evil");

        assert_eq!(path.parent().unwrap(), kit_hooks_dir(dir.path()));
        assert_eq!(path.file_name().unwrap(), "evil.json");
    }

    /// Mắt xích cuối của chuỗi "kit cài hook → hook chạy": file kit ghi ra phải
    /// được chính loader của engine đọc vào. Hai nửa từng được test riêng —
    /// module này ghi đúng shape, loader đọc đúng shape — mà không ai nối lại,
    /// nên một đổi tên ở hai bên vẫn để cả hai test xanh và hook thì im lặng
    /// không bao giờ chạy.
    #[test]
    fn the_engine_loader_reads_a_kit_hook_file() {
        let dir = tempfile::tempdir().unwrap();
        let kits_dir = dir.path().join("kits");
        let kit = KitManifest::parse(&serde_json::json!({
            "manifest": 2,
            "id": "loader-check",
            "hooks": [
                { "event": "UserPromptSubmit", "prompt": "trả lời bằng tiếng Việt" },
                { "event": "PreToolUse", "matcher": "Bash", "prompt": "soát lệnh" }
            ]
        }))
        .unwrap();

        assert!(matches!(
            write_kit_hooks(&kits_dir, &kit),
            KitHookOutcome::Written { accepted: 2, .. }
        ));

        let files = kit_hook_files(&kits_dir);
        assert_eq!(files.len(), 1, "installed kit must contribute one file");

        // `global_config_dir` trỏ vào thư mục trống: chứng minh hook đến TỪ kit,
        // không phải từ hooks.json của người dùng.
        let loaded = crate::agent::load_zen_hook_config(
            dir.path(),
            None,
            Some(&files),
            crate::agent::MarketplaceHookPolicy {
                allow_command_hooks: false,
                ..Default::default()
            },
        )
        .expect("loader must return a config built from the kit file alone");

        use crate::zen_core::hooks::types::HookEvent;
        assert!(loaded.hooks.contains_key(&HookEvent::UserPromptSubmit));
        assert!(loaded.hooks.contains_key(&HookEvent::PreToolUse));
    }

}
