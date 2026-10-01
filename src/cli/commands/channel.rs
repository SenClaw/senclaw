//! `senclaw channel ...`. Port target: src-old/cli/commands/channel.ts
//!
//! The TS implementation directly reads/writes `config.json` for channel
//! bindings (Telegram bots, Feishu apps, WeChat accounts, group entries).
//!
//! In the Rust port, channel configuration is driven by environment variables
//! (SENCLAW_TELEGRAM_BOT_TOKEN, etc.) read once at startup via
//! [`Config::from_env`]. There is no mutable JSON config store — channel
//! management is done by setting env vars and restarting the daemon.
//!
//! This command will be re-evaluated once the admin UI server is ported.

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum ChannelCmd {
    /// List configured channels (reads current env-based config)
    List,
    /// Show channel configuration status
    Status,
}

pub async fn run(cmd: ChannelCmd) -> Result<()> {
    match cmd {
        ChannelCmd::List => {
            let cfg = crate::config::Config::from_env();
            println!("=== Channel Status ===");
            println!(
                "Telegram:  {}",
                if cfg.telegram.bot_token.is_empty() {
                    "not configured"
                } else {
                    "configured"
                }
            );
        }
        ChannelCmd::Status => {
            let cfg = crate::config::Config::from_env();
            println!("Config path: {}", cfg.paths.global_config_path.display());
            println!("DB path:     {}", cfg.paths.db_path.display());
            println!("Agents dir:  {}", cfg.paths.profiles_dir.display());
        }
    }
    Ok(())
}
