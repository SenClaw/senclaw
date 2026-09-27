//! Browser automation module — Chrome extension bridge, tab registry,
//! crawl engine, and HTML compression.
//!
//! Communicates with the SenClaw Chrome Extension via WebSocket to
//! provide remote browser control for SenClaw agents.

pub mod bridge;
pub mod crawl_engine;
pub mod html_compressor;
pub mod protocol;
pub mod tab_registry;
pub mod types;
