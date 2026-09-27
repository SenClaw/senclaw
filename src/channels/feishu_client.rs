//! Feishu REST API client helpers.
//! Port target: src-old/channels/feishu-client.ts
//!
//! In the Rust port, the Feishu HTTP client logic is inlined directly into
//! [`crate::channels::feishu::FeishuChannel`] rather than extracted into a
//! separate module. The TS module wraps `@larksuiteoapi/node-sdk`; the Rust
//! port uses raw `reqwest` calls instead.
