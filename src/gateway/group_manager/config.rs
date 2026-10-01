//! Global config load/save and group config persistence.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::db::Db;
use crate::types::GroupBinding;

use super::dirs::ensure_agent_dirs;
use super::manager::GroupManager;
use super::types::{GlobalConfig, GroupConfigEntry};

pub(super) fn load_global_config(path: &Path) -> GlobalConfig {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => GlobalConfig::default(),
    }
}

pub(super) fn save_global_config(path: &Path, cfg: &GlobalConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create dir {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(cfg)?;
    fs::write(path, json)?;
    // config.json holds every LLM `apiKey` in cleartext; it was created 0644.
    crate::util::file_perms::restrict_best_effort(path);
    Ok(())
}

pub(super) fn save_group_to_config(config_path: &Path, binding: &GroupBinding) {
    // The implicit "main" group is never persisted to config.json — it is
    // recreated on boot. (Previously gated on `is_admin`, which only ever meant
    // `folder == "main"`.)
    if binding.folder == "main" {
        return;
    }
    let mut cfg = load_global_config(config_path);
    let groups = cfg.groups.get_or_insert_with(Vec::new);
    let entry = GroupConfigEntry {
        jid: binding.jid.clone(),
        folder: binding.folder.clone(),
        name: binding.name.clone(),
        channel: Some(binding.channel.clone()).filter(|c| !c.is_empty()),
        group_type: Some(binding.group_type.clone()).filter(|t| t != "chat"),
        requires_trigger: Some(binding.requires_trigger),
        allowed_tools: binding.allowed_tools.clone(),
        allowed_paths: binding.allowed_paths.clone(),
        allowed_work_dirs: binding.allowed_work_dirs.clone(),
        bot_token: binding.bot_token.clone(),
        max_messages: binding.max_messages,
        llm_config_id: binding.llm_config_id.clone(),
    };
    if let Some(existing) = groups.iter_mut().find(|g| g.jid == entry.jid) {
        *existing = entry;
    } else {
        groups.push(entry);
    }
    let _ = save_global_config(config_path, &cfg);
}

pub(super) fn remove_group_from_config(config_path: &Path, jid: &str) {
    let mut cfg = load_global_config(config_path);
    if let Some(ref mut groups) = cfg.groups {
        groups.retain(|g| g.jid != jid);
    }
    let _ = save_global_config(config_path, &cfg);
}

/// Reserved jid prefixes for dynamic, DB-only chat sessions that are created at
/// runtime (not via config.json) and must never be deleted by config
/// reconciliation: recurring schedules, cowork teams, web chat sessions, and
/// virtual agents.
fn is_dynamic_system_jid(jid: &str) -> bool {
    jid.starts_with("schedule:")
        || jid.starts_with("cowork:")
        || jid.starts_with("web:")
        || jid.starts_with("virtual:")
}

pub fn sync_groups_from_config(
    db: &Db,
    gm: &GroupManager,
    config: &Config,
) -> (usize, usize, usize) {
    let cfg = load_global_config(&config.paths.global_config_path);
    let config_groups = cfg.groups.unwrap_or_default();
    let config_jids: HashSet<&str> = config_groups.iter().map(|g| g.jid.as_str()).collect();
    let now = super::manager::chrono_now();
    let mut added = 0usize;
    let mut updated = 0usize;
    let mut removed = 0usize;

    for entry in &config_groups {
        // Prevent folder conflicts among config-managed groups. Delete the
        // conflicting rows by jid — never by folder: dynamic system sessions
        // (schedule:/cowork:/web:/virtual:) legitimately share a config
        // folder, since a schedule bound to an agent profile IS that folder.
        // The old delete_group_by_folder sweep wiped those sessions on boot,
        // which orphaned the schedule's chat and made later profile edits
        // silently update zero rows.
        if let Ok(all) = db.list_groups() {
            for conflict in all.iter().filter(|g| {
                g.folder == entry.folder && g.jid != entry.jid && !is_dynamic_system_jid(&g.jid)
            }) {
                let _ = db.delete_group(&conflict.jid);
            }
        }

        let existing = gm.get(db, &entry.jid);
        let binding = GroupBinding {
            jid: entry.jid.clone(),
            folder: entry.folder.clone(),
            name: entry.name.clone(),
            channel: entry.channel.clone().unwrap_or_default(),
            group_type: entry
                .group_type
                .clone()
                .unwrap_or_else(|| "chat".to_string()),
            requires_trigger: entry.requires_trigger.unwrap_or(true),
            allowed_tools: entry.allowed_tools.clone(),
            allowed_paths: None,
            allowed_work_dirs: entry.allowed_work_dirs.clone(),
            bot_token: entry.bot_token.clone(),
            max_messages: entry.max_messages,
            llm_config_id: entry
                .llm_config_id
                .clone()
                .or_else(|| existing.as_ref().and_then(|e| e.llm_config_id.clone())),
            last_active: existing.as_ref().and_then(|e| e.last_active.clone()),
            added_at: existing
                .as_ref()
                .map(|e| e.added_at.clone())
                .unwrap_or_else(|| now.clone()),
        };

        ensure_agent_dirs(config, &binding.folder, &binding.name);
        let _ = db.upsert_group(&binding);
        if existing.is_some() {
            updated += 1;
        } else {
            added += 1;
        }
    }

    // Delete config-managed channel groups that the user removed from
    // config.json. Dynamic, DB-only system groups are NEVER written to
    // config.json and must survive reconciliation — otherwise e.g. a recurring
    // schedule's chat session (`schedule:<id>`) gets wiped on boot and the
    // scheduler later fails with "chat session not found". (These were
    // previously protected by `is_admin`; that flag is gone, so match on the
    // reserved jid prefixes / the implicit "main" folder instead.)
    if let Ok(all) = gm.list(db) {
        for db_group in &all {
            if db_group.folder == "main" || is_dynamic_system_jid(&db_group.jid) {
                continue;
            }
            if !config_jids.contains(db_group.jid.as_str()) {
                let _ = db.delete_group(&db_group.jid);
                removed += 1;
            }
        }
    }

    (added, updated, removed)
}

pub fn get_agent_allowed_work_dirs(
    config_path: &Path,
    folder: &str,
) -> Option<Option<Vec<String>>> {
    let cfg = load_global_config(config_path);
    match cfg.agents.and_then(|a| a.get(folder).cloned()) {
        None => None,                                 // not present in config
        Some(entry) => Some(entry.allowed_work_dirs), // null = switching disallowed
    }
}

#[cfg(test)]
mod tests {
    use super::is_dynamic_system_jid;

    /// `decisionConfig` also carries `backend`/`local`/`online` — sen-sysone's
    /// own sub-keys since the runtime split. The daemon reads/writes only
    /// `.gate`/`.skills` (`crate::decision::settings`), so this section is kept
    /// as raw JSON and must round-trip byte-for-byte through an unrelated save.
    #[test]
    fn decision_config_round_trips_sen_sysones_sub_keys_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"groups":[{"jid":"web:a","folder":"a","name":"A"}],
                "decisionConfig":{"backend":"online","local":{"idleUnloadMinutes":30}}}"#,
        )
        .unwrap();
        let cfg = super::load_global_config(&path);
        assert_eq!(cfg.decision_config.as_ref().and_then(|v| v.get("backend").cloned()), Some(serde_json::json!("online")));
        assert_eq!(cfg.groups.as_ref().map(Vec::len), Some(1), "the rest of the file survives");
        // And a save writes the rest back instead of an empty config.
        super::save_global_config(&path, &cfg).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("web:a"));
        assert!(text.contains("\"backend\": \"online\""), "{text}");
    }

    #[test]
    fn conflict_sweep_spares_dynamic_sessions_sharing_a_profile_folder() {
        use crate::db::Db;
        use crate::types::GroupBinding;

        let tmp = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::from_env();
        config.paths.global_config_path = tmp.path().join("config.json");
        config.paths.profiles_dir = tmp.path().join("agents");
        config.paths.workspace_dir = tmp.path().join("workspace");
        std::fs::write(
            &config.paths.global_config_path,
            r#"{"groups":[{"jid":"web:ssh","folder":"ssh","name":"SSH"}]}"#,
        )
        .unwrap();

        let db = Db::open_in_memory(&config).unwrap();
        let mk = |jid: &str, folder: &str| GroupBinding {
            jid: jid.into(),
            folder: folder.into(),
            name: String::new(),
            channel: String::new(),
            group_type: "chat".into(),
            requires_trigger: false,
            allowed_tools: None,
            allowed_paths: None,
            allowed_work_dirs: None,
            bot_token: None,
            max_messages: None,
            llm_config_id: None,
            last_active: None,
            added_at: "2026-07-18T00:00:00Z".into(),
        };
        // A schedule session bound to the "ssh" profile folder, plus a stale
        // config-managed channel row that genuinely conflicts.
        db.upsert_group(&mk("schedule:abc", "ssh")).unwrap();
        db.upsert_group(&mk("tg:group:9", "ssh")).unwrap();

        let gm = super::GroupManager::new();
        super::sync_groups_from_config(&db, &gm, &config);

        // The schedule's chat session survives reconciliation; the stale
        // channel row is swept; the config-managed profile is (re)created.
        assert!(db.get_group("schedule:abc").unwrap().is_some());
        assert!(db.get_group("tg:group:9").unwrap().is_none());
        assert!(db.get_group("web:ssh").unwrap().is_some());
    }

    #[test]
    fn dynamic_system_jids_are_protected_from_reconciliation() {
        // Must survive config reconciliation (DB-only, never in config.json).
        assert!(is_dynamic_system_jid("schedule:21843f68-1449-4a4d-b273"));
        assert!(is_dynamic_system_jid("cowork:736ef25b-98d3-4938"));
        assert!(is_dynamic_system_jid("web:main:mquiu8os-osyxge"));
        assert!(is_dynamic_system_jid("virtual:code-reviewer"));
        // Config-managed channel groups are NOT dynamic — they may be reconciled.
        assert!(!is_dynamic_system_jid("tg:group:12345"));
        assert!(!is_dynamic_system_jid("feishu:oc_abc"));
        assert!(!is_dynamic_system_jid("app:ch_x:user:y"));
    }
}
