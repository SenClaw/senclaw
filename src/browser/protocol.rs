//! WebSocket message types for daemon ↔ Chrome extension communication.

use serde::{Deserialize, Serialize};

use super::types::*;

// ===== Daemon → Extension =====

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonMessage {
    // Tab management
    Navigate {
        request_id: RequestId,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
    },
    NewTab {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    CloseTab {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        tab_id: TabId,
    },
    SwitchTab {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        tab_id: TabId,
    },
    GoBack {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
    },
    GoForward {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
    },
    Reload {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
    },

    // DOM interaction
    Click {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
        index: u32,
    },
    Type {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
        index: u32,
        text: String,
        #[serde(default)]
        submit: bool,
    },
    SelectOption {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
        index: u32,
        option_text: String,
    },
    Scroll {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
        direction: String,
        amount: ScrollAmount,
        /// Scroll the scrollable sub-container at this snapshot index instead of
        /// the page itself (e.g. a modal or dropdown pane).
        #[serde(skip_serializing_if = "Option::is_none")]
        container_index: Option<u32>,
    },
    Hover {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
        index: u32,
    },
    PressKey {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
        key: String,
    },
    UploadFile {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
        index: u32,
        file_paths: Vec<String>,
    },
    ExecuteJs {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
        script: String,
    },
    WaitFor {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
        condition: WaitCondition,
    },

    // Observation
    GetSnapshot {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        depth: Option<u8>,
        #[serde(default)]
        compress_html: bool,
        /// Pixels beyond the viewport to include. -1 = strict viewport.
        #[serde(skip_serializing_if = "Option::is_none")]
        viewport_expansion: Option<i32>,
        /// Hard cap on interactive elements indexed.
        #[serde(skip_serializing_if = "Option::is_none")]
        max_interactive: Option<u16>,
        /// Walk same-origin iframes (default true).
        #[serde(skip_serializing_if = "Option::is_none")]
        walk_iframes: Option<bool>,
        /// Walk shadow DOM (default true).
        #[serde(skip_serializing_if = "Option::is_none")]
        walk_shadow: Option<bool>,
        /// Draw numbered badge overlay on the page (default false).
        #[serde(skip_serializing_if = "Option::is_none")]
        highlight: Option<bool>,
    },
    GetScreenshot {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
        #[serde(default)]
        full_page: bool,
        #[serde(default)]
        format: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        quality: Option<u8>,
    },
    ExtractText {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
    },
    ExtractLinks {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
    },
    ExtractTable {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
    },

    // Search
    Search {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        query: String,
        #[serde(default = "default_search_engine")]
        engine: String,
        #[serde(default = "default_num_results")]
        num_results: u8,
        #[serde(skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        /// Run in a throwaway tab that is closed after extraction (default).
        /// Lets any number of searches run in parallel without touching the
        /// agent's persistent tab. false = search in the agent's own tab.
        #[serde(default = "default_true")]
        ephemeral: bool,
    },

    // Crawl control
    CrawlStart {
        job_id: JobId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        start_url: String,
        depth: u8,
        max_pages: u16,
        #[serde(default)]
        link_patterns: Vec<String>,
        #[serde(default)]
        exclude_patterns: Vec<String>,
        #[serde(default = "default_true")]
        same_domain: bool,
    },
    CrawlPause {
        job_id: JobId,
    },
    CrawlResume {
        job_id: JobId,
    },
    CrawlStop {
        job_id: JobId,
    },

    // Fill form
    FillForm {
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tab_id: Option<TabId>,
        fields: Vec<FormField>,
        #[serde(default)]
        submit: bool,
    },

    // List tabs
    ListTabs {
        request_id: RequestId,
    },

    // Get status
    GetStatus {
        request_id: RequestId,
    },
}

impl DaemonMessage {
    /// Stamp the originating agent's identity onto the message. The extension
    /// maps each agent_id to its own tab (TabGroupController), so this is what
    /// keeps concurrent agents from clobbering one another's tabs.
    pub fn with_agent_id(mut self, id: Option<String>) -> Self {
        use DaemonMessage::*;
        match &mut self {
            Navigate { agent_id, .. }
            | NewTab { agent_id, .. }
            | CloseTab { agent_id, .. }
            | SwitchTab { agent_id, .. }
            | GoBack { agent_id, .. }
            | GoForward { agent_id, .. }
            | Reload { agent_id, .. }
            | Click { agent_id, .. }
            | Type { agent_id, .. }
            | SelectOption { agent_id, .. }
            | Scroll { agent_id, .. }
            | Hover { agent_id, .. }
            | PressKey { agent_id, .. }
            | UploadFile { agent_id, .. }
            | ExecuteJs { agent_id, .. }
            | WaitFor { agent_id, .. }
            | GetSnapshot { agent_id, .. }
            | GetScreenshot { agent_id, .. }
            | ExtractText { agent_id, .. }
            | ExtractLinks { agent_id, .. }
            | ExtractTable { agent_id, .. }
            | Search { agent_id, .. }
            | CrawlStart { agent_id, .. }
            | FillForm { agent_id, .. } => *agent_id = id,
            CrawlPause { .. }
            | CrawlResume { .. }
            | CrawlStop { .. }
            | ListTabs { .. }
            | GetStatus { .. } => {}
        }
        self
    }
}

fn default_search_engine() -> String {
    "google".into()
}

fn default_num_results() -> u8 {
    10
}

fn default_true() -> bool {
    true
}

// ===== Extension → Daemon =====

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ExtensionMessage {
    // Response to requests
    Response {
        request_id: RequestId,
        #[serde(flatten)]
        result: ActionResult,
    },

    // Tab events
    TabCreated {
        tab_id: TabId,
        url: String,
        window_id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
    },
    TabUpdated {
        tab_id: TabId,
        url: String,
        title: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
    },
    TabClosed {
        tab_id: TabId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
    },

    // Crawl progress
    CrawlProgress {
        job_id: JobId,
        pages_crawled: u16,
        pages_total: u16,
        current_url: String,
    },
    CrawlResult {
        job_id: JobId,
        page_result: CrawlPageResult,
    },
    CrawlComplete {
        job_id: JobId,
        total_pages: u16,
        duration_ms: u64,
    },

    // Screenshot frame (for Web UI preview)
    ScreenshotFrame {
        tab_id: TabId,
        data: String,
        format: String,
    },

    // Heartbeat
    Heartbeat {
        tab_count: u16,
        active_tab_id: Option<TabId>,
        /// Live agent → tab map; lets the daemon registry resync after a
        /// restart or missed tab events.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_tabs: Option<std::collections::HashMap<String, TabId>>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_serialized_when_set() {
        let msg = DaemonMessage::Search {
            request_id: "r1".into(),
            agent_id: None,
            query: "rust".into(),
            engine: "google".into(),
            num_results: 5,
            language: None,
            ephemeral: true,
        }
        .with_agent_id(Some("agent-a@group".into()));

        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "Search");
        assert_eq!(json["agent_id"], "agent-a@group");
    }

    #[test]
    fn agent_id_omitted_when_none() {
        let msg = DaemonMessage::Navigate {
            request_id: "r2".into(),
            agent_id: None,
            url: "https://example.com".into(),
            tab_id: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert!(json.get("agent_id").is_none());
    }

    #[test]
    fn legacy_daemon_message_without_agent_id_deserializes() {
        // Messages produced by pre-agent_id builds must keep parsing.
        let json = r#"{"type":"Search","request_id":"r3","query":"hello","engine":"bing","num_results":3}"#;
        let msg: DaemonMessage = serde_json::from_str(json).unwrap();
        match msg {
            DaemonMessage::Search {
                agent_id,
                query,
                ephemeral,
                ..
            } => {
                assert_eq!(agent_id, None);
                assert_eq!(query, "hello");
                assert!(ephemeral, "ephemeral must default to true");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn extension_tab_event_with_agent_id_deserializes() {
        // The TS extension already sends agent_id on tab lifecycle events.
        let json = r#"{"type":"TabCreated","tab_id":"42","url":"https://example.com","window_id":1,"agent_id":"agent-b"}"#;
        let msg: ExtensionMessage = serde_json::from_str(json).unwrap();
        match msg {
            ExtensionMessage::TabCreated {
                tab_id, agent_id, ..
            } => {
                assert_eq!(tab_id, "42");
                assert_eq!(agent_id.as_deref(), Some("agent-b"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn extension_response_with_extra_agent_id_field_deserializes() {
        // TS Response carries agent_id; it must not break the flattened ActionResult.
        let json = r#"{"type":"Response","request_id":"r4","agent_id":"agent-c","status":"ok","data":{"x":1}}"#;
        let msg: ExtensionMessage = serde_json::from_str(json).unwrap();
        match msg {
            ExtensionMessage::Response { request_id, result } => {
                assert_eq!(request_id, "r4");
                assert!(matches!(result, ActionResult::Ok { .. }));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn heartbeat_with_and_without_agent_tabs_deserializes() {
        // Old extension: no agent_tabs field.
        let legacy = r#"{"type":"Heartbeat","tab_count":3,"active_tab_id":"7"}"#;
        let msg: ExtensionMessage = serde_json::from_str(legacy).unwrap();
        match msg {
            ExtensionMessage::Heartbeat { agent_tabs, .. } => assert!(agent_tabs.is_none()),
            other => panic!("unexpected variant: {other:?}"),
        }

        // New extension: agent → tab map included.
        let new = r#"{"type":"Heartbeat","tab_count":3,"active_tab_id":"7","agent_tabs":{"agent-a":"7","agent-b":"9"}}"#;
        let msg: ExtensionMessage = serde_json::from_str(new).unwrap();
        match msg {
            ExtensionMessage::Heartbeat { agent_tabs, .. } => {
                let map = agent_tabs.expect("agent_tabs present");
                assert_eq!(map.get("agent-a").map(String::as_str), Some("7"));
                assert_eq!(map.get("agent-b").map(String::as_str), Some("9"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn with_agent_id_noop_on_untargeted_variants() {
        let msg = DaemonMessage::ListTabs {
            request_id: "r5".into(),
        }
        .with_agent_id(Some("agent-d".into()));
        let json = serde_json::to_value(&msg).unwrap();
        assert!(json.get("agent_id").is_none());
    }
}
