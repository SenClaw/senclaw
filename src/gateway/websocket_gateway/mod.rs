//! WebSocket gateway. Port target: src-old/gateway/WebSocketGateway.ts
//!
//! Provides real-time event streaming and bidirectional interaction for Web UI / CLI.
//! Listens on 127.0.0.1:{port} (default 18789), not exposed externally.
//!
//! Client → Server protocol:
//!   { type: 'connect', token?: string }
//!   { type: 'subscribe', groupJid: string }
//!   { type: 'unsubscribe', groupJid: string }
//!   { type: 'message', groupJid: string, text: string }
//!   { type: 'list:groups' }
//!   { type: 'register:group', jid, folder, name, ... }
//!   { type: 'unregister:group', jid }
//!   { type: 'update:group', jid, ...fields }
//!   { type: 'list:tasks', groupJid?: string }
//!   { type: 'list:task-logs', taskId: string, limit?: number }
//!   { type: 'manage:task', taskId: string, action: 'pause'|'resume'|'cancel' }
//!   { type: 'permission:response', requestId, optionKey }
//!   { type: 'question:response', requestId, answers, otherTexts? }
//!   { type: 'register:feishu-app', appId, appSecret, domain? }
//!   { type: 'unregister:feishu-app', appId }
//!   { type: 'list:feishu-apps' }
//!   { type: 'list:dispatch' }
//!   { type: 'agent:control', groupJid, action, query? }

mod browser;
mod connection;
mod entity_handlers;
mod gateway;
mod handlers;
mod helpers;
mod notify;
mod state;
#[cfg(test)]
mod tests;
mod wire;

// Re-export public API
pub(crate) use browser::BrowserRelay;
pub use gateway::{WebSocketGateway, WsGatewayApi};
pub use state::WsState;
