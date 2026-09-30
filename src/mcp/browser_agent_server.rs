//! `senclaw-browser` (engine v2) — the agent's browser tools over the
//! Jev + LLM browser loop.
//!
//! Every tool is a thin loopback call to the daemon's `/api/browser-agent/*`
//! (see [`crate::browser_agent::rest`]): the loop needs the decision client,
//! the LLM configs and the extension hub, all of which live in the daemon
//! process. The chat that owns the tab comes from the env
//! (`SENCLAW_AGENT_ID`, set per chat by `browser_mcp_config`), never from a
//! tool parameter.

use std::time::Duration;

use anyhow::Result;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content};
use serde_json::{json, Value};

/// A whole task can take minutes (slow sites, a local decision model on CPU).
const TASK_TIMEOUT: Duration = Duration::from_secs(900);
const STEP_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct TaskParams {
    /// What to achieve, in plain words, with every value the page will need
    /// ("one-way flights Zurich → London on 20 Sep 2026, 1 adult, economy").
    goal: String,
    /// Where to start. Omit to continue in this chat's current tab (or start at a search engine).
    #[serde(default)]
    url: Option<String>,
    /// A question to answer from the final page (the answer comes back in `answer`).
    #[serde(default)]
    question: Option<String>,
    /// What the page visibly shows once the goal is reached ("The page shows \"Order placed\"");
    /// checked before DONE is accepted. Give them: otherwise an LLM has to write them first.
    #[serde(default)]
    done_criteria: Vec<String>,
    /// Step budget (default 40).
    #[serde(default)]
    max_steps: Option<u32>,
    /// "managed" (SenClaw's own Chrome profile, default), "extension" (the
    /// person's Chrome with their sign-ins — only when they ask for it), or "auto".
    #[serde(default)]
    browser: Option<String>,
    /// Work in a tab the person shared from the SenClaw side panel (its `tab`
    /// from browser_tabs → extension.shared_tabs). Implies browser="extension".
    #[serde(default)]
    shared_tab: Option<i64>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct ApproveParams {
    /// The `pending.approval_id` from a `needs_approval` result.
    approval_id: String,
    /// true only after the person explicitly agreed in chat; false declines.
    approve: bool,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct ResumeParams {
    /// The `task_id` of a task that stopped with needs_user / budget / needs_input.
    task_id: String,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct BrowserParam {
    /// "managed" (default) or "extension".
    #[serde(default)]
    browser: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct DoParams {
    /// `observation_id` from the browser_look (or browser_do) result you are acting on.
    observation_id: u64,
    /// One of the offered operations: CLICK, TYPE_TEXT, SELECT, SCROLL_DOWN, SCROLL_UP, WAIT, KEY_ENTER, GO_BACK, DIALOG_ACCEPT, DIALOG_DISMISS.
    operation: String,
    /// Element index from that result's `elements` ("7", or "4:2" for a dropdown option). Omit for controls.
    #[serde(default)]
    target: Option<String>,
    /// The text for TYPE_TEXT (replaces the field's content).
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    browser: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct OpenParams {
    /// An http(s) URL.
    url: String,
    #[serde(default)]
    browser: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct ReadParams {
    /// Open this http(s) URL first, then read it — one call instead of browser_open + browser_read.
    /// Omit to read this chat's current page.
    #[serde(default)]
    url: Option<String>,
    /// Ask a question about the page; omit to get its text and links.
    #[serde(default)]
    question: Option<String>,
    /// Characters of page text to return (default 20000).
    #[serde(default)]
    max_chars: Option<u64>,
    #[serde(default)]
    browser: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct HandoverParams {
    /// "start" (default): give the tab to the person. "done": take it back after they said they finished.
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    browser: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
struct SearchParams {
    /// What to search the web for.
    query: String,
}

#[derive(Clone)]
pub struct McpBrowserAgentServer {
    base_url: String,
    owner: String,
    token: Option<String>,
    http: reqwest::Client,
}

impl McpBrowserAgentServer {
    /// Built when the browser engine is v2 (`SENCLAW_BROWSER_ENGINE=v2`);
    /// otherwise the legacy extension server takes the `senclaw-browser` slot.
    pub fn from_env() -> Result<Option<Self>> {
        if std::env::var("SENCLAW_BROWSER_ENGINE").ok().as_deref() != Some("v2") {
            return Ok(None);
        }
        Ok(Some(Self::new(
            std::env::var("SENCLAW_BROWSER_API_URL").ok(),
            std::env::var("SENCLAW_AGENT_ID").ok(),
        )))
    }

    pub fn new(base_url: Option<String>, owner: Option<String>) -> Self {
        let base_url = base_url.map(|s| s.trim().trim_end_matches('/').to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| {
            let port = std::env::var("SENCLAW_UI_PORT").ok().and_then(|p| p.trim().parse::<u16>().ok()).unwrap_or(18788);
            format!("http://127.0.0.1:{port}")
        });
        Self {
            base_url,
            owner: owner.filter(|o| !o.is_empty()).unwrap_or_else(|| "default".into()),
            token: std::env::var("SENCLAW_API_TOKEN").ok().filter(|t| !t.is_empty()),
            http: reqwest::Client::new(),
        }
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>, timeout: Duration) -> Value {
        let url = format!("{}{path}", self.base_url);
        let mut req = self.http.request(method, &url).timeout(timeout);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        match req.send().await {
            Ok(resp) => resp.json::<Value>().await.unwrap_or_else(|e| json!({ "error": format!("unreadable answer: {e}") })),
            Err(e) if e.is_timeout() => json!({
                "error": "The browser task is still running in SenClaw; check back with browser_tabs rather than starting it again.",
                "code": "timeout"
            }),
            Err(e) => json!({ "error": format!("cannot reach SenClaw at {url}: {e}"), "code": "unreachable" }),
        }
    }

    async fn post(&self, path: &str, mut body: Value, timeout: Duration) -> String {
        body["chat_jid"] = json!(self.owner);
        self.call(reqwest::Method::POST, path, Some(body), timeout).await.to_string()
    }
}

#[rmcp::tool_router(server_handler, vis = "pub")]
impl McpBrowserAgentServer {
    #[rmcp::tool(
        description = "Do a whole task on the web in ONE call — open a page, click, type, choose, scroll — instead of one call per step: SenClaw's browser loop observes the page, a decision model picks each step in a fraction of a second, an LLM writes field values, and DONE is only accepted after the page is checked. Use it for anything beyond reading a page, and pass `done_criteria`. Returns status: done (with `answer` if you asked a `question`), needs_approval (show `pending.action` to the person and call browser_approve only after they agree), needs_user (sign-in, code or CAPTCHA: call browser_handover), needs_input (ask the person for the missing value), blocked, budget or unverified; `notes`, when present, says what made the run slow. Runs in SenClaw's own Chrome profile unless browser=\"extension\" (the person's Chrome — only when they ask for their own account)."
    )]
    async fn browser_task(&self, Parameters(p): Parameters<TaskParams>) -> String {
        let browser = if p.shared_tab.is_some() { Some("extension".to_string()) } else { p.browser };
        let body = serde_json::to_value(&json!({
            "goal": p.goal, "url": p.url, "question": p.question, "done_criteria": p.done_criteria,
            "max_steps": p.max_steps, "browser": browser, "ext_tab": p.shared_tab,
        }))
        .unwrap_or_default();
        self.post("/api/browser-agent/tasks", body, TASK_TIMEOUT).await
    }

    #[rmcp::tool(
        description = "Answer a pending approval from browser_task (purchase, payment, send, post, delete, confirm dialog). Call with approve=true ONLY after the person explicitly agreed in chat to that exact action; approve=false declines. The task then continues and returns like browser_task."
    )]
    async fn browser_approve(&self, Parameters(p): Parameters<ApproveParams>) -> String {
        let path = format!("/api/browser-agent/approvals/{}", p.approval_id);
        self.post(&path, json!({ "approve": p.approve }), TASK_TIMEOUT).await
    }

    #[rmcp::tool(description = "Continue a browser task that stopped (after the person finished a handover, or supplied what was missing). Returns like browser_task.")]
    async fn browser_resume(&self, Parameters(p): Parameters<ResumeParams>) -> String {
        let path = format!("/api/browser-agent/tasks/{}/resume", p.task_id);
        self.post(&path, json!({}), TASK_TIMEOUT).await
    }

    #[rmcp::tool(
        description = "Show this chat's current page as an indexed table: elements (index, role, label, value, supported operations), offered operations, visible text and observation_id. For acting by hand with browser_do when browser_task cannot do it — every step then costs a turn of yours. browser_open and browser_do already return this table: look again only when the page changed by itself."
    )]
    async fn browser_look(&self, Parameters(p): Parameters<BrowserParam>) -> String {
        self.post("/api/browser-agent/look", json!({ "browser": p.browser }), STEP_TIMEOUT).await
    }

    #[rmcp::tool(
        description = "Execute one operation on the page from the latest browser_look: CLICK/TYPE_TEXT/SELECT need a `target` index, controls (SCROLL_DOWN, WAIT, KEY_ENTER, GO_BACK…) do not. The action is refused if the page changed since that look (look again) and risky actions (buy, pay, send, delete…) are refused here: use browser_task so the person can approve. Returns the new page table."
    )]
    async fn browser_do(&self, Parameters(p): Parameters<DoParams>) -> String {
        let body = json!({ "observation_id": p.observation_id, "operation": p.operation, "target": p.target, "text": p.text, "browser": p.browser });
        self.post("/api/browser-agent/do", body, STEP_TIMEOUT).await
    }

    #[rmcp::tool(
        description = "Open a URL in this chat's browser tab and return the page table once the page has loaded (same shape as browser_look — no need to look again). To read a page, call browser_read with `url` instead; to do something on it, browser_task with `url`."
    )]
    async fn browser_open(&self, Parameters(p): Parameters<OpenParams>) -> String {
        self.post("/api/browser-agent/open", json!({ "url": p.url, "browser": p.browser }), STEP_TIMEOUT).await
    }

    #[rmcp::tool(
        description = "Read a page: its full text and links, or — with `question` — an answer grounded in the page (figures quoted from it). With `url` it opens that page first, in the same call; without, it reads this chat's current page."
    )]
    async fn browser_read(&self, Parameters(p): Parameters<ReadParams>) -> String {
        let body = json!({ "url": p.url, "question": p.question, "max_chars": p.max_chars, "browser": p.browser });
        self.post("/api/browser-agent/read", body, STEP_TIMEOUT).await
    }

    #[rmcp::tool(description = "List the browser sessions and tabs, this chat's current tab per browser, and whether the person's Chrome extension is connected.")]
    async fn browser_tabs(&self) -> String {
        let path = format!("/api/browser-agent/tabs?chat_jid={}", urlencoding(&self.owner));
        self.call(reqwest::Method::GET, &path, None, STEP_TIMEOUT).await.to_string()
    }

    #[rmcp::tool(description = "A screenshot (JPEG) of this chat's current page, for charts, maps, canvas or anything the text does not carry.")]
    async fn browser_screenshot(&self, Parameters(p): Parameters<BrowserParam>) -> CallToolResult {
        let v: Value = serde_json::from_str(&self.post("/api/browser-agent/screenshot", json!({ "browser": p.browser }), STEP_TIMEOUT).await)
            .unwrap_or_default();
        match v.get("data").and_then(Value::as_str) {
            Some(data) => CallToolResult::success(vec![Content::image(data.to_string(), "image/jpeg".to_string())]),
            None => CallToolResult::error(vec![Content::text(v.to_string())]),
        }
    }

    #[rmcp::tool(
        description = "Hand this chat's tab to the person for a sign-in, one-time code, CAPTCHA or anything they must do themselves (action=\"start\"): SenClaw stops reading and acting on it. Tell them where the tab is, wait until they say they finished, then call again with action=\"done\" and continue with browser_resume."
    )]
    async fn browser_handover(&self, Parameters(p): Parameters<HandoverParams>) -> String {
        self.post("/api/browser-agent/handover", json!({ "action": p.action, "browser": p.browser }), STEP_TIMEOUT).await
    }

    #[rmcp::tool(
        description = "Search the web in SenClaw's own browser (DuckDuckGo, then Bing if DuckDuckGo shows a bot check) and return the results page's text and links, with `engine` naming the one that answered."
    )]
    async fn browser_search(&self, Parameters(p): Parameters<SearchParams>) -> String {
        let mut walls = Vec::new();
        for (engine, base) in SEARCH_ENGINES {
            let url = format!("{base}{}", urlencoding(&p.query));
            let opened = self.post("/api/browser-agent/open", json!({ "url": url, "browser": "managed" }), STEP_TIMEOUT).await;
            if opened.contains("\"error\"") {
                return opened;
            }
            let read = self.post("/api/browser-agent/read", json!({ "max_chars": 8000, "browser": "managed" }), STEP_TIMEOUT).await;
            let Ok(mut page) = serde_json::from_str::<Value>(&read) else { return read };
            if page.get("error").is_some() {
                return read;
            }
            if !is_bot_wall(page.get("text").and_then(Value::as_str).unwrap_or_default()) {
                page["engine"] = json!(engine);
                // The results themselves, instead of every link on the page.
                page["results"] = json!(result_links(page.get("links").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default()));
                if let Some(o) = page.as_object_mut() {
                    o.remove("links");
                }
                return page.to_string();
            }
            walls.push(*engine);
        }
        json!({
            "error": format!("{} answered with a bot check instead of results. Search in the person's own Chrome (browser_open with browser \"extension\"), or hand the tab over (browser_handover).", walls.join(" and ")),
            "code": "blocked",
        })
        .to_string()
    }
}

/// Tried in order; the first that answers with results wins.
const SEARCH_ENGINES: &[(&str, &str)] = &[
    ("DuckDuckGo", "https://html.duckduckgo.com/html/?q="),
    ("Bing", "https://www.bing.com/search?q="),
];

/// The page a search engine shows a browser it takes for a bot, instead of
/// results. Only the top of the page is read: that is where the wall says so,
/// while results further down may quote the same words.
fn is_bot_wall(text: &str) -> bool {
    let top: String = text.chars().take(1500).collect::<String>().to_lowercase();
    [
        "bots use duckduckgo",
        "please complete the following challenge",
        "detected unusual traffic",
        "verify you are human",
        "checking if the site connection is secure",
    ]
    .iter()
    .any(|w| top.contains(w))
}

/// `{title, url}` for each result, in page order: the engines' redirect links
/// unwrapped to the site they lead to (DuckDuckGo `/l/?uddg=`, Bing
/// `/ck/a?…&u=a1<base64>`), ads and the engine's own pages dropped.
fn result_links(links: &[Value]) -> Vec<Value> {
    use base64::Engine;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for link in links {
        let Some(href) = link.get("href").and_then(Value::as_str) else { continue };
        let Ok(url) = reqwest::Url::parse(href) else { continue };
        let host = url.host_str().unwrap_or_default().trim_start_matches("www.").to_string();
        let param = |name: &str| url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned());
        let target = match host.as_str() {
            "duckduckgo.com" | "html.duckduckgo.com" => param("uddg"),
            "bing.com" if url.path().starts_with("/ck/") => param("u").and_then(|u| {
                let b64 = u.strip_prefix("a1")?;
                let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(b64.trim_end_matches('=')).ok()?;
                String::from_utf8(bytes).ok()
            }),
            "bing.com" | "go.microsoft.com" => None,
            _ => Some(href.to_string()),
        };
        let Some(target) = target.filter(|t| t.starts_with("http")) else { continue };
        let Ok(t) = reqwest::Url::parse(&target) else { continue };
        // Ads come back through the engine itself (`duckduckgo.com/y.js?ad_domain=…`).
        let t_host = t.host_str().unwrap_or_default().trim_start_matches("www.");
        if matches!(t_host, "duckduckgo.com" | "bing.com") {
            continue;
        }
        let title = link.get("text").and_then(Value::as_str).unwrap_or_default().trim().to_string();
        if seen.insert(target.clone()) {
            out.push(json!({ "title": title, "url": target }));
        }
    }
    out.truncate(20);
    out
}

fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_lead_to_the_sites_not_the_engine() {
        let links = vec![
            json!({ "text": "All Regions", "href": "https://html.duckduckgo.com/html/?kl=wt-wt" }),
            json!({ "text": "Klook", "href": "https://duckduckgo.com/l/?uddg=https%3A%2F%2Fduckduckgo.com%2Fy.js%3Fad_domain%3Dklook.com" }),
            json!({ "text": "Hanoi Weather | AccuWeather", "href": "https://duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.accuweather.com%2Fen%2Fvn%2Fhanoi&rut=x" }),
            json!({ "text": "www.accuweather.com/en/vn/hanoi", "href": "https://duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.accuweather.com%2Fen%2Fvn%2Fhanoi&rut=x" }),
            // Bing: `u=a1` + base64url("https://www.bbc.com/weather/1581130")
            json!({ "text": "Hanoi - BBC Weather", "href": "https://www.bing.com/ck/a?!&&p=1&u=a1aHR0cHM6Ly93d3cuYmJjLmNvbS93ZWF0aGVyLzE1ODExMzA&ntb=1" }),
            json!({ "text": "Privacy", "href": "https://go.microsoft.com/fwlink/?LinkId=521839" }),
        ];
        let results = result_links(&links);
        let urls: Vec<&str> = results.iter().filter_map(|r| r["url"].as_str()).collect();
        assert_eq!(urls, vec!["https://www.accuweather.com/en/vn/hanoi", "https://www.bbc.com/weather/1581130"]);
        assert_eq!(results[0]["title"], "Hanoi Weather | AccuWeather", "the first link to a site names it");
    }

    #[test]
    fn a_bot_check_is_not_taken_for_results() {
        assert!(is_bot_wall("DuckDuckGo\n\nUnfortunately, bots use DuckDuckGo too. Please complete the following challenge"));
        assert!(is_bot_wall("Our systems have detected unusual traffic from your computer network."));
        assert!(!is_bot_wall("giá vàng hôm nay at DuckDuckGo\nGiá vàng SJC hôm nay 29/9 ..."));
        let deep = format!("{}how bots use DuckDuckGo", "result ".repeat(400));
        assert!(!is_bot_wall(&deep), "results that merely quote the words further down");
    }

    /// The skills written for this engine name only tools this engine has. A
    /// skill that names the other engine's tools sends the model looking for
    /// them: a turn spent on ToolSearch, then a page driven one step per turn.
    #[test]
    fn the_engines_skills_name_only_its_tools() {
        let tools: Vec<String> = McpBrowserAgentServer::tool_router().list_all().into_iter().map(|t| t.name.to_string()).collect();
        let named = regex::Regex::new(r"browser_[a-z]+(?:_[a-z]+)*").unwrap();
        let skills = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("skills");
        for skill in ["agent-browser", "web-research"] {
            let text = std::fs::read_to_string(skills.join(skill).join("SKILL.browser-v2.md")).unwrap_or_else(|e| panic!("{skill}: {e}"));
            let mentioned: std::collections::BTreeSet<&str> = named.find_iter(&text).map(|m| m.as_str()).collect();
            for tool in &mentioned {
                assert!(tools.iter().any(|t| t == tool), "{skill} names `{tool}`, which engine v2 does not have ({tools:?})");
            }
            for needed in ["browser_search", "browser_read", "browser_task"] {
                assert!(mentioned.contains(needed), "{skill} never mentions {needed}");
            }
            assert!(!text.contains("ToolSearch {"), "{skill} must not send the model to ToolSearch for tools it already has");
        }
    }

    #[test]
    fn tools_are_registered() {
        let names: Vec<String> = McpBrowserAgentServer::tool_router().list_all().into_iter().map(|t| t.name.to_string()).collect();
        for expected in [
            "browser_task", "browser_approve", "browser_resume", "browser_look", "browser_do", "browser_open",
            "browser_read", "browser_tabs", "browser_screenshot", "browser_handover", "browser_search",
        ] {
            assert!(names.iter().any(|n| n == expected), "{expected} missing from {names:?}");
        }
        assert_eq!(urlencoding("giá vàng SJC"), "gi%C3%A1+v%C3%A0ng+SJC");
        // Engine v1 keeps the legacy server; only v2 builds this one.
        std::env::remove_var("SENCLAW_BROWSER_ENGINE");
        assert!(McpBrowserAgentServer::from_env().unwrap().is_none());
    }
}
