//! GroupManager struct, impl, helper functions, and ensure_* group functions.

use std::collections::HashSet;
use std::sync::Mutex;

use crate::config::Config;
use crate::db::Db;
use crate::types::GroupBinding;
use anyhow::Result;

use super::config::{remove_group_from_config, save_group_to_config};
use super::dirs::ensure_agent_dirs;
use super::types::GroupBindingUpdate;

pub struct GroupManager {
    pub(super) on_groups_changed: Mutex<Option<Box<dyn Fn() + Send + 'static>>>,
}

impl GroupManager {
    pub fn new() -> Self {
        Self {
            on_groups_changed: Mutex::new(None),
        }
    }

    pub fn set_on_groups_changed(&self, cb: Box<dyn Fn() + Send + 'static>) {
        if let Ok(mut guard) = self.on_groups_changed.lock() {
            *guard = Some(cb);
        }
    }

    pub fn register(&self, db: &Db, config: &Config, binding: &GroupBinding) {
        ensure_agent_dirs(config, &binding.folder, &binding.name);
        if let Err(e) = db.upsert_group(binding) {
            // Loud failure: silently swallowing here means the WS handler
            // broadcasts a `group:registered` event for a row that doesn't
            // exist, and the next message to that JID 404s with "Group not
            // found". Log so future schema regressions surface immediately.
            tracing::error!(
                jid = %binding.jid,
                folder = %binding.folder,
                error = %e,
                "[GroupManager] upsert_group failed — group will not be reachable"
            );
        }
        save_group_to_config(&config.paths.global_config_path, binding);
        self.fire_changed();
    }

    pub fn unregister(&self, db: &Db, config: &Config, jid: &str) {
        let _ = db.delete_group(jid);
        remove_group_from_config(&config.paths.global_config_path, jid);
        self.fire_changed();
    }

    pub fn update(
        &self,
        db: &Db,
        config: &Config,
        jid: &str,
        updates: GroupBindingUpdate,
    ) -> Result<GroupBinding> {
        let existing = db
            .get_group(jid)?
            .ok_or_else(|| anyhow::anyhow!("Group not found: {jid}"))?;
        let updated = GroupBinding {
            jid: existing.jid.clone(),
            folder: updates.folder.unwrap_or(existing.folder),
            name: updates.name.unwrap_or(existing.name),
            channel: updates.channel.unwrap_or(existing.channel),
            group_type: updates.group_type.unwrap_or(existing.group_type),
            requires_trigger: updates
                .requires_trigger
                .unwrap_or(existing.requires_trigger),
            allowed_tools: merge_opt_opt(updates.allowed_tools, existing.allowed_tools),
            allowed_paths: merge_opt_opt(updates.allowed_paths, existing.allowed_paths),
            allowed_work_dirs: merge_opt_opt(updates.allowed_work_dirs, existing.allowed_work_dirs),
            bot_token: merge_opt_opt(updates.bot_token, existing.bot_token),
            max_messages: merge_opt_opt(updates.max_messages, existing.max_messages),
            llm_config_id: merge_opt_opt(updates.llm_config_id, existing.llm_config_id),
            last_active: existing.last_active,
            added_at: existing.added_at,
        };
        db.upsert_group(&updated)?;
        save_group_to_config(&config.paths.global_config_path, &updated);
        self.fire_changed();
        Ok(updated)
    }

    pub fn get(&self, db: &Db, jid: &str) -> Option<GroupBinding> {
        db.get_group(jid).ok().flatten()
    }

    pub fn list(&self, db: &Db) -> Result<Vec<GroupBinding>> {
        db.list_groups()
    }

    pub fn touch_active(&self, db: &Db, jid: &str, timestamp: &str) {
        let _ = db.touch_group_active(jid, timestamp);
    }

    pub fn find_pending_feishu_binding(&self, db: &Db, app_id: &str) -> Option<GroupBinding> {
        if app_id.is_empty() {
            return None;
        }
        db.get_group(&format!("feishu:pending:{app_id}"))
            .ok()
            .flatten()
    }

    pub fn find_pending_wechat_binding(&self, db: &Db, folder: &str) -> Option<GroupBinding> {
        if folder.is_empty() {
            return None;
        }
        db.get_group(&format!("wx:pending:{folder}")).ok().flatten()
    }

    pub fn migrate_jid(
        &self,
        db: &Db,
        config_path: &std::path::Path,
        old_jid: &str,
        new_jid: &str,
    ) -> Option<GroupBinding> {
        let new_binding = db.rename_group_jid(old_jid, new_jid).ok().flatten()?;
        remove_group_from_config(config_path, old_jid);
        save_group_to_config(config_path, &new_binding);
        self.fire_changed();
        Some(new_binding)
    }

    pub fn get_extra_bot_tokens(&self, db: &Db, default_token: &str) -> Vec<String> {
        let mut tokens = HashSet::new();
        if let Ok(all) = db.list_groups() {
            for g in &all {
                if let Some(ref tok) = g.bot_token {
                    if tok != default_token {
                        tokens.insert(tok.clone());
                    }
                }
            }
        }
        tokens.into_iter().collect()
    }

    pub fn get_channel_for_jid(jid: &str) -> Option<&'static str> {
        if jid.starts_with("tg:") {
            Some("telegram")
        } else if jid.starts_with("feishu:") {
            Some("feishu")
        } else if jid.starts_with("wx:") {
            Some("wechat")
        } else if jid.ends_with("@s.whatsapp.net") || jid.ends_with("@g.us") {
            Some("whatsapp")
        } else {
            None
        }
    }

    pub(super) fn fire_changed(&self) {
        if let Ok(guard) = self.on_groups_changed.lock() {
            if let Some(ref cb) = *guard {
                cb();
            }
        }
    }
}

impl Default for GroupManager {
    fn default() -> Self {
        Self::new()
    }
}

// ===== Helpers =====

/// Merge an `Option<Option<T>>` update with an existing `Option<T>`.
/// - `None` = don't update (keep existing)
/// - `Some(None)` = set to null
/// - `Some(Some(v))` = set to value
fn merge_opt_opt<T>(update: Option<Option<T>>, existing: Option<T>) -> Option<T> {
    match update {
        None => existing,
        Some(v) => v,
    }
}

pub(super) fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format_unix_timestamp(now.as_secs())
}

fn format_unix_timestamp(secs: u64) -> String {
    let days_since_epoch = secs / 86400;
    let time_of_day = secs % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;

    let (year, month, day) = days_to_ymd(days_since_epoch as i64);
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.000Z")
}

fn days_to_ymd(mut days: i64) -> (i64, u32, u32) {
    days += 719468;
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = (days - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

pub fn ensure_wechat_admin_group(
    db: &Db,
    gm: &GroupManager,
    config: &Config,
    owner_jid: &str,
    folder: &str,
) {
    let now = chrono_now();

    // folder UNIQUE constraint
    let existing_by_folder = gm
        .list(db)
        .ok()
        .and_then(|all| all.into_iter().find(|g| g.folder == *folder));
    if let Some(ref existing) = existing_by_folder {
        if existing.jid != owner_jid {
            gm.unregister(db, config, &existing.jid);
            remove_group_from_config(&config.paths.global_config_path, &existing.jid);
            tracing::info!(
                "[GroupManager] Migrated WeChat group from {} to {owner_jid}",
                existing.jid
            );
        }
    }

    let existing = gm.get(db, owner_jid).or(existing_by_folder);

    let binding = GroupBinding {
        jid: owner_jid.to_string(),
        folder: folder.to_string(),
        name: existing
            .as_ref()
            .map(|e| e.name.clone())
            .unwrap_or_else(|| folder.to_string()),
        channel: "wechat".to_string(),
        group_type: existing
            .as_ref()
            .map(|e| e.group_type.clone())
            .unwrap_or_else(|| "chat".to_string()),
        requires_trigger: false,
        // Every chat has full privileges now — no admin/non-admin split, so all
        // tools are allowed by default.
        allowed_tools: None,
        allowed_paths: existing.as_ref().and_then(|e| e.allowed_paths.clone()),
        allowed_work_dirs: existing.as_ref().and_then(|e| e.allowed_work_dirs.clone()),
        bot_token: None,
        max_messages: existing.as_ref().and_then(|e| e.max_messages),
        llm_config_id: existing.as_ref().and_then(|e| e.llm_config_id.clone()),
        last_active: existing.as_ref().and_then(|e| e.last_active.clone()),
        added_at: existing
            .as_ref()
            .map(|e| e.added_at.clone())
            .unwrap_or_else(|| now.clone()),
    };

    gm.register(db, config, &binding);

    let action = if existing.is_some() {
        "updated"
    } else {
        "registered"
    };
    tracing::info!("[GroupManager] WeChat group {action}: {owner_jid} → profiles/{folder}/");
}

/// Auto-register an app-channel JID as a group on first contact, mirroring the
/// WeChat pattern. The folder name is derived from the sender_id portion of the
/// JID (`app:{channel_id}:user:{sender_id}` → folder `app-{sender_id}`).
pub fn ensure_app_group(db: &Db, gm: &GroupManager, config: &Config, chat_jid: &str) {
    let now = chrono_now();

    // Derive a stable folder name from the sender portion of the JID.
    let folder = {
        let parts: Vec<&str> = chat_jid.split(':').collect();
        // JID: app:{channel_id}:user:{sender_id}
        if parts.len() >= 4 && parts[2] == "user" {
            format!("app-{}", parts[3])
        } else {
            format!("app-{}", chat_jid.replace(':', "-"))
        }
    };

    let existing = gm.get(db, chat_jid);

    let binding = GroupBinding {
        jid: chat_jid.to_string(),
        folder: folder.clone(),
        name: existing
            .as_ref()
            .map(|e| e.name.clone())
            .unwrap_or_else(|| folder.clone()),
        channel: "app".to_string(),
        group_type: existing
            .as_ref()
            .map(|e| e.group_type.clone())
            .unwrap_or_else(|| "chat".to_string()),
        requires_trigger: false,
        allowed_tools: existing.as_ref().and_then(|e| e.allowed_tools.clone()),
        allowed_paths: existing.as_ref().and_then(|e| e.allowed_paths.clone()),
        allowed_work_dirs: existing.as_ref().and_then(|e| e.allowed_work_dirs.clone()),
        bot_token: None,
        max_messages: existing.as_ref().and_then(|e| e.max_messages),
        llm_config_id: existing.as_ref().and_then(|e| e.llm_config_id.clone()),
        last_active: existing.as_ref().and_then(|e| e.last_active.clone()),
        added_at: existing
            .as_ref()
            .map(|e| e.added_at.clone())
            .unwrap_or_else(|| now.clone()),
    };

    gm.register(db, config, &binding);

    let action = if existing.is_some() {
        "updated"
    } else {
        "registered"
    };
    tracing::info!("[GroupManager] App group {action}: {chat_jid} → profiles/{folder}/");
}
