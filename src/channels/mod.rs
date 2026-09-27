//! Channel adapter trait + re-exports. Port targets: src-old/types.ts (IChannel) + src-old/channels/*.ts

pub mod app;
pub mod feishu;
pub mod feishu_ws;
pub(crate) mod media;
pub mod telegram;
pub mod wechat;

use async_trait::async_trait;

use crate::types::{ChatMeta, IncomingMessage, InlineButton};

/// Callback invoked when a message arrives from any channel.
/// The implementation should be non-blocking (spawn if async work needed).
pub type MessageCallback = Box<dyn Fn(IncomingMessage) + Send + Sync + 'static>;

/// Callback invoked when chat metadata is updated.
pub type MetadataCallback = Box<dyn Fn(ChatMeta) + Send + Sync + 'static>;

/// Generic channel adapter interface.
/// Mirrors the TS `IChannel` interface.
#[async_trait]
pub trait Channel: Send + Sync {
    /// Channel identifier: "telegram", "feishu", "wechat"
    fn id(&self) -> &'static str;

    /// Establish connection and start background message reception.
    async fn connect(&self) -> anyhow::Result<()>;

    /// Gracefully shut down.
    async fn disconnect(&self) -> anyhow::Result<()>;

    fn is_connected(&self) -> bool;

    /// Send a text message to a chat.
    async fn send_message(
        &self,
        chat_jid: &str,
        text: &str,
        bot_token: Option<&str>,
    ) -> anyhow::Result<()>;

    /// Send a file to a chat.
    async fn send_file(
        &self,
        chat_jid: &str,
        file_path: &str,
        caption: Option<&str>,
        bot_token: Option<&str>,
    ) -> anyhow::Result<()>;

    /// Whether this channel owns the given JID.
    fn owns_jid(&self, chat_jid: &str) -> bool;

    /// Register a callback for incoming messages.
    fn on_message(&self, handler: MessageCallback);

    /// Register a callback for chat metadata updates.
    fn on_metadata(&self, handler: MetadataCallback);

    /// Get the bot username for a given token (Telegram-specific).
    fn get_bot_username(&self, _bot_token: Option<&str>) -> Option<String> {
        None
    }

    /// Send a message with inline buttons (for permission interactions).
    async fn send_with_buttons(
        &self,
        _chat_jid: &str,
        _text: &str,
        _buttons: &[InlineButton],
        _bot_token: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Set typing indicator.
    async fn set_typing(
        &self,
        _chat_jid: &str,
        _active: bool,
        _bot_token: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}
