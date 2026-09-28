//! Group binding registry, directory management, and global config persistence.
//! Mirrors `src-old/gateway/GroupManager.ts`.

pub mod apps;
pub mod browser_agent;
pub mod chat;
pub(crate) mod config;
pub mod control_plane;
pub mod dirs;
pub mod llm;
pub mod manager;
pub(crate) mod soul;
#[cfg(test)]
mod tests;
pub mod types;

// Re-exports for external consumers
pub use apps::{delete_feishu_app, get_feishu_apps, save_feishu_app};
pub use browser_agent::save_browser_agent_settings;
pub use chat::{delete_telegram_bot, get_telegram_bots, get_wechat_accounts, save_telegram_bot};
pub use control_plane::{load_control_plane_settings, save_control_plane_settings};
pub use dirs::{ensure_agent_dirs, read_memory_md, read_soul_md, write_memory_md, write_soul_md};
pub use llm::{
    get_admin_permissions_config, get_after_process_enabled, get_defaults_config,
    get_dispatch_enabled, get_memory_recall_enabled, get_pre_cognitive_enabled,
    get_pre_trigger_skill_enabled, get_thinking_enabled, load_cognitive_config,
    load_decision_settings, load_embedding_config, load_llm_configs, remove_llm_config,
    save_admin_permissions_config, save_after_process_enabled, save_cognitive_config,
    save_decision_settings, save_defaults_config, save_dispatch_enabled, save_embedding_config,
    save_llm_config, save_memory_recall_enabled, save_pre_cognitive_enabled,
    save_pre_trigger_skill_enabled, save_thinking_enabled, set_active_cognitive_llm_config,
    set_active_llm_config, set_active_quick_llm_config, set_widget_disabled,
};
pub use manager::{ensure_app_group, ensure_wechat_admin_group, GroupManager};
pub use types::{
    AdminPermissions, DefaultsConfig, EmbeddingConfig, FeishuAppConfig, GroupBindingUpdate,
    LlmConfig, LlmConfigResult, PersistedCognitiveConfig, TelegramBotConfig, WechatAccountConfig,
};

pub use config::{get_agent_allowed_work_dirs, sync_groups_from_config};
