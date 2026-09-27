//! WeChat iLink Bot channel adapter — re-exports.

mod api;
mod channel;
mod helpers;
mod media;
mod types;

#[cfg(test)]
mod tests;

pub use channel::WeChatChannel;
