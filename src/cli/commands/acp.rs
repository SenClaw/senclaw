//! `senclaw acp` — run SenClaw as an Agent Client Protocol agent on stdio,
//! for Zed / JetBrains / Kiro and any other ACP client. Needs a running
//! daemon; the process is a translator, not an engine (see [`crate::acp`]).

use anyhow::Result;
use clap::Args;

#[derive(Args, Debug)]
pub struct AcpCmd {
    /// WebSocket gateway of the daemon.
    #[arg(long, default_value = "ws://127.0.0.1:18789")]
    pub gateway: String,
    /// Gateway token (default: `SENCLAW_WS_TOKEN`; unset when the daemon has none).
    #[arg(long)]
    pub token: Option<String>,
}

pub async fn run(cmd: AcpCmd) -> Result<()> {
    // Logs must not touch stdout — that is the protocol channel.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();
    let token = cmd.token.or_else(|| std::env::var("SENCLAW_WS_TOKEN").ok()).filter(|t| !t.is_empty());
    crate::acp::run(&cmd.gateway, token.as_deref()).await
}
