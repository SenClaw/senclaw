//! WeChat and Telegram configuration functions.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use super::config::{load_global_config, save_global_config};
use super::types::{TelegramBotConfig, WechatAccountConfig};

// ===== WeChat accounts =====

pub fn get_wechat_accounts(config_path: &Path) -> HashMap<String, WechatAccountConfig> {
    load_global_config(config_path)
        .wechat_accounts
        .unwrap_or_default()
}

// ===== Telegram multi-bot config =====

pub fn save_telegram_bot(config_path: &Path, entry: TelegramBotConfig) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    let bots = cfg.telegram_bots.get_or_insert_with(Vec::new);
    if let Some(existing) = bots.iter_mut().find(|b| b.token == entry.token) {
        *existing = entry;
    } else {
        bots.push(entry);
    }
    save_global_config(config_path, &cfg)
}

pub fn delete_telegram_bot(config_path: &Path, token: &str) -> Result<()> {
    let mut cfg = load_global_config(config_path);
    if let Some(ref mut bots) = cfg.telegram_bots {
        bots.retain(|b| b.token != token);
    }
    save_global_config(config_path, &cfg)
}

pub fn get_telegram_bots(config_path: &Path) -> Vec<TelegramBotConfig> {
    load_global_config(config_path)
        .telegram_bots
        .unwrap_or_default()
}
