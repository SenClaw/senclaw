//! Shared types for browser automation.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Unique identifier for a browser tab.
pub type TabId = String;

/// Unique identifier for a crawl job.
pub type JobId = String;

/// Unique identifier for a pending request.
pub type RequestId = String;

/// Snapshot element representing an interactive DOM node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotElement {
    pub index: u32,
    pub tag: String,
    pub role: String,
    pub text: String,
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub attributes: HashMap<String, String>,
    pub bbox: BoundingBox,
    pub enabled: bool,
    pub selected: bool,
    /// True if not present in the previous snapshot at this URL.
    #[serde(default)]
    pub is_new: bool,
    /// "in" | "above" | "below" relative to current viewport.
    #[serde(default = "default_viewport_status")]
    pub viewport_status: String,
    /// Frame path for nested iframes; None if in top frame.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub frame_path: Option<String>,
}

fn default_viewport_status() -> String {
    "in".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundingBox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Viewport/scroll metadata returned alongside the element list.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ViewportInfo {
    pub width: f32,
    pub height: f32,
    pub scroll_x: f32,
    pub scroll_y: f32,
    pub document_width: f32,
    pub document_height: f32,
    /// Number of viewport-heights of content above the current scroll position.
    pub pages_above: f32,
    /// Number of viewport-heights of content below.
    pub pages_below: f32,
}

/// Three-section pre-rendered string view for the LLM.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FormattedState {
    pub header: String,
    pub content: String,
    pub footer: String,
}

/// Full page snapshot returned by content script.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageSnapshot {
    pub url: String,
    pub title: String,
    pub elements: Vec<SnapshotElement>,
    #[serde(default)]
    pub viewport: ViewportInfo,
    #[serde(default)]
    pub formatted: FormattedState,
    #[serde(default)]
    pub total_interactive: u32,
    #[serde(default)]
    pub capped: bool,
    pub text_content_summary: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub compressed_html: Option<String>,
}

/// Search result from Google/Bing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResultItem {
    pub position: u8,
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResults {
    pub results: Vec<SearchResultItem>,
    pub total_estimated: u64,
    pub search_url: String,
}

/// Tab state tracked by the registry.
#[derive(Debug, Clone, Serialize)]
pub struct TabState {
    pub tab_id: TabId,
    pub url: String,
    pub title: String,
    pub status: TabStatus,
    /// Agent that owns this tab (extension allocates one tab per agent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip)]
    pub created_at: std::time::Instant,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TabStatus {
    Loading,
    Complete,
    Error(String),
}

/// Crawl job configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlConfig {
    pub job_id: JobId,
    pub start_url: String,
    pub depth: u8,
    pub max_pages: u16,
    pub link_patterns: Vec<String>,
    pub exclude_patterns: Vec<String>,
    pub same_domain: bool,
    pub per_page_timeout_ms: u32,
    pub wait_between_pages_ms: u32,
}

/// Result from crawling a single page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlPageResult {
    pub url: String,
    pub title: String,
    pub text_content: String,
    pub extracted_data: Option<serde_json::Value>,
    pub links_found: u16,
    pub depth: u8,
    pub crawled_at: String,
}

/// Status of a crawl job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrawlJobStatus {
    pub job_id: JobId,
    pub status: String,
    pub pages_crawled: u16,
    pub pages_total: u16,
    pub results: Vec<CrawlPageResult>,
}

/// Scroll amount for scroll actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ScrollAmount {
    Pages(f32),
    Pixels(u32),
}

/// Wait condition for wait tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WaitCondition {
    #[serde(rename = "time")]
    Time { ms: u32 },
    #[serde(rename = "text")]
    Text { text: String, timeout_ms: u32 },
    #[serde(rename = "text_gone")]
    TextGone { text: String, timeout_ms: u32 },
    #[serde(rename = "navigation")]
    Navigation { timeout_ms: u32 },
}

/// Form field definition for fill_form.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FormField {
    pub target: String,
    pub value: String,
    #[serde(rename = "type")]
    pub field_type: String,
}

/// Result of a browser action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum ActionResult {
    #[serde(rename = "ok")]
    Ok { data: serde_json::Value },
    #[serde(rename = "error")]
    Error {
        message: String,
        code: Option<String>,
    },
}

impl ActionResult {
    pub fn ok_data(data: serde_json::Value) -> Self {
        Self::Ok { data }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
            code: None,
        }
    }
}

/// Screenshot format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenshotFormat {
    Png,
    Jpeg,
}
