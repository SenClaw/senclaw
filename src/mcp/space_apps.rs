//! Space App management for the `senclaw-space` MCP server — the `space_app_*`
//! tools that let a chat agent see which apps are installed, which are up, and
//! start or stop one on request.
//!
//! **Why these go over HTTP and the rest of `space_server` does not.** Notes and
//! calendar are rows in the shared SQLite file, so the MCP subprocess opens the
//! DB and is done. An app's *process* is not in any file: it lives in the
//! daemon's [`SpaceMcpLauncher`](crate::gateway::ui_server::space_mcp::SpaceMcpLauncher)
//! — a child-process map, a user-stopped set, a launch counter, all in memory in
//! a different process. Reading it from here is impossible and forking a second
//! launcher would fight the first one for ports. So every tool in this module is
//! a thin call to the daemon's own REST API on loopback, which is the same path
//! the Web UI's Space Apps page takes.
//!
//! Auth: loopback peers are exempt from the daemon's API token
//! ([`crate::gateway::ui_server::auth`]), and the app-token gate deliberately
//! covers only an app's *data* routes, never `/start` and `/stop`. So the
//! ordinary local case needs no credential. `SENCLAW_API_TOKEN` is forwarded
//! when set, for the operator who points `SENCLAW_SPACE_API_URL` at a daemon
//! that is not on this machine.

use std::time::Duration;

use serde_json::{json, Value};

use crate::mcp::schedule_server::ToolResult;

/// Where the daemon answers, and how long we are willing to wait for it.
pub struct SpaceAppsClient {
    base_url: String,
    http: reqwest::Client,
    token: Option<String>,
}

/// Starting an app can mean `npm ci`, a Python venv, and then a ~30s health
/// wait, so a client timeout anywhere near the health wait alone would report
/// failure for a start that is about to succeed. Past this the answer is not
/// "it failed" but "ask again later" — hanging a chat turn for longer is worse
/// than reporting the truth, which is that the daemon is still working on it.
const START_TIMEOUT: Duration = Duration::from_secs(120);
/// Reads are bookkeeping lookups; anything this slow is a daemon in trouble.
const READ_TIMEOUT: Duration = Duration::from_secs(15);

impl SpaceAppsClient {
    /// Built from `SENCLAW_SPACE_API_URL` (set by
    /// [`crate::mcp::helper::space_mcp_config`]), falling back to the daemon's
    /// documented loopback address so a hand-registered `space-server` in a
    /// `.mcp.json` still works.
    pub fn from_env() -> Self {
        let base_url = std::env::var("SENCLAW_SPACE_API_URL")
            .ok()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                let port = std::env::var("SENCLAW_UI_PORT")
                    .ok()
                    .and_then(|p| p.trim().parse::<u16>().ok())
                    .unwrap_or(18788);
                format!("http://127.0.0.1:{port}")
            });
        let token = std::env::var("SENCLAW_API_TOKEN")
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        let mut c = Self::with_base_url(base_url);
        c.token = token;
        c
    }

    /// Point the client at a specific daemon. Split out from [`Self::from_env`]
    /// so a test can drive the real tool bodies against a router it stood up,
    /// rather than mutating process-global env from parallel test threads.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(START_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http,
            token: None,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// GET a daemon JSON route.
    async fn get(&self, path: &str, timeout: Duration) -> Result<Value, String> {
        let mut req = self.http.get(self.url(path)).timeout(timeout);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        self.send(req, path).await
    }

    /// POST a daemon JSON route with no body.
    async fn post(&self, path: &str, timeout: Duration) -> Result<Value, String> {
        let mut req = self.http.post(self.url(path)).timeout(timeout);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        self.send(req, path).await
    }

    async fn send(&self, req: reqwest::RequestBuilder, path: &str) -> Result<Value, String> {
        let res = req.send().await.map_err(|e| {
            // Two very different failures, deliberately worded differently. A
            // timeout does NOT cancel the daemon's work — the install or health
            // wait carries on — so calling it a failure would have the agent
            // retry a start that is already in progress.
            if e.is_timeout() {
                format!(
                    "Hết thời gian chờ daemon trên {path}. Daemon VẪN đang xử lý \
                     (cài đặt phụ thuộc hoặc chờ app trả lời có thể lâu hơn thế). \
                     Đừng gọi lại ngay — chờ một lát rồi kiểm tra bằng space_app_list."
                )
            } else {
                // Told plainly, because the overwhelmingly likely cause is not a
                // bug in the call but a daemon that is not running — and "error
                // sending request" alone sends the agent hunting elsewhere.
                format!(
                    "Không gọi được daemon SenClaw tại {} ({e}). \
                     Daemon có đang chạy không? Đặt SENCLAW_SPACE_API_URL nếu nó ở cổng khác.",
                    self.base_url
                )
            }
        })?;
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!("{path} → HTTP {status}: {}", body.trim()));
        }
        serde_json::from_str(&body).map_err(|e| format!("Đáp án không phải JSON từ {path}: {e}"))
    }

    // ── Tools ──────────────────────────────────────────────────────────────

    /// Installed Space Apps with their lifecycle state.
    ///
    /// `status_filter` is applied here rather than by the daemon so the endpoint
    /// stays one shape for every caller; the list is tens of rows, not
    /// thousands.
    pub async fn list(
        &self,
        query: Option<String>,
        status_filter: Option<String>,
        probe: bool,
    ) -> ToolResult {
        let path = if probe {
            "/api/space/apps/status?probe=1"
        } else {
            "/api/space/apps/status"
        };
        let payload = match self.get(path, READ_TIMEOUT).await {
            Ok(v) => v,
            Err(e) => return ToolResult::err(e),
        };
        let all = payload
            .get("apps")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let needle = query
            .map(|q| q.trim().to_lowercase())
            .filter(|q| !q.is_empty());
        let want = status_filter
            .map(|s| s.trim().to_lowercase())
            .unwrap_or_else(|| "all".into());

        let apps: Vec<Value> = all
            .into_iter()
            .filter(|a| match &needle {
                None => true,
                Some(q) => {
                    let id = a["id"].as_str().unwrap_or("").to_lowercase();
                    let name = a["name"].as_str().unwrap_or("").to_lowercase();
                    id.contains(q) || name.contains(q)
                }
            })
            .filter(|a| match want.as_str() {
                "running" => a["running"] == json!(true),
                "stopped" => a["running"] != json!(true),
                // A misspelled filter must not silently mean "running": an agent
                // that asked for "up" and got an empty list would conclude the
                // machine has no apps.
                _ => true,
            })
            .collect();

        let running = apps.iter().filter(|a| a["running"] == json!(true)).count();
        ToolResult::ok(
            json!({
                "apps": apps,
                "count": apps.len(),
                "running": running,
                "filter": want,
                "probed": probe,
                "legend": {
                    "mode": "background = daemon giữ chạy liên tục; session = tự bật khi mở app hoặc gọi tool, tự tắt khi rảnh (idleTimeoutSecs).",
                    "running": "daemon này đang theo dõi một tiến trình sống. `ready` (chỉ có khi probe=true) mới là 'cổng đang trả lời'.",
                    "userStopped": "đã bị dừng tay; app background sẽ nằm im cho tới khi start lại.",
                },
            })
            .to_string(),
        )
    }

    /// Start one app now and wait until it answers.
    pub async fn start(&self, app_id: &str) -> ToolResult {
        let Some(id) = normalize_app_id(app_id) else {
            return ToolResult::err(BAD_ID.into());
        };
        match self
            .post(&format!("/api/space/apps/{id}/start"), START_TIMEOUT)
            .await
        {
            Ok(v) => ToolResult::ok(
                json!({
                    "success": true,
                    "appId": id,
                    "readiness": v,
                })
                .to_string(),
            ),
            Err(e) => ToolResult::err(e),
        }
    }

    /// Stop one app now.
    pub async fn stop(&self, app_id: &str) -> ToolResult {
        let Some(id) = normalize_app_id(app_id) else {
            return ToolResult::err(BAD_ID.into());
        };
        // The daemon's reply already carries the mode-dependent "what stopped
        // means" note, which is the part a user actually needs to hear.
        match self
            .post(&format!("/api/space/apps/{id}/stop"), READ_TIMEOUT)
            .await
        {
            Ok(v) => ToolResult::ok(v.to_string()),
            Err(e) => ToolResult::err(e),
        }
    }

    /// Kill and respawn one app, whether or not it was running.
    pub async fn restart(&self, app_id: &str) -> ToolResult {
        let Some(id) = normalize_app_id(app_id) else {
            return ToolResult::err(BAD_ID.into());
        };
        match self
            .post(&format!("/api/space/apps/{id}/restart"), START_TIMEOUT)
            .await
        {
            Ok(_) => ToolResult::ok(
                json!({ "success": true, "appId": id, "action": "restart" }).to_string(),
            ),
            Err(e) => ToolResult::err(e),
        }
    }

    /// Which MCP server each app registers, and what state it is in.
    ///
    /// Two requests for the whole fleet — the app list and the MCP registry —
    /// joined on the server name. Tool *names* are included for a single app, or
    /// when asked for explicitly: a fleet-wide listing with names in it runs to
    /// several hundred entries and buries the answer.
    pub async fn mcp_list(
        &self,
        app_id: Option<String>,
        include_tools: Option<bool>,
    ) -> ToolResult {
        let one = match app_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(raw) => match normalize_app_id(raw) {
                Some(id) => Some(id),
                None => return ToolResult::err(BAD_ID.into()),
            },
            None => None,
        };

        let apps = match self.get("/api/space/apps/status", READ_TIMEOUT).await {
            Ok(v) => v
                .get("apps")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            Err(e) => return ToolResult::err(e),
        };
        // The registry is an *enrichment*, not the answer. Which server an app
        // registers comes from its manifest; only live connection status and the
        // tool list come from here. Failing the whole call when the registry is
        // unreadable would withhold the part the caller most needs — the name to
        // call the tool by — over the part it can live without.
        let (servers, registry_error) = match self.get("/api/mcp-servers", READ_TIMEOUT).await {
            Ok(v) => (
                v.get("servers")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
                None,
            ),
            Err(e) => (Vec::new(), Some(e)),
        };

        if let Some(id) = &one {
            if !apps.iter().any(|a| a["id"].as_str() == Some(id.as_str())) {
                return ToolResult::err(format!("Không tìm thấy app đã cài với id `{id}`."));
            }
        }
        let with_tools = include_tools.unwrap_or(one.is_some());

        let rows: Vec<Value> = apps
            .iter()
            .filter(|a| match &one {
                Some(id) => a["id"].as_str() == Some(id.as_str()),
                None => true,
            })
            // An app with no `mcp` block exposes no tools; listing it as an MCP
            // row would imply an agent could call into it.
            .filter(|a| a["mcpName"].is_string())
            .map(|a| {
                let name = a["mcpName"].as_str().unwrap_or_default();
                let info = servers.iter().find(|s| s["name"].as_str() == Some(name));
                let tools = info.and_then(|s| s["tools"].as_array());
                let mut row = json!({
                    "appId":     a["id"],
                    "appName":   a["name"],
                    "appMode":   a["mode"],
                    "appRunning": a["running"],
                    "mcpName":   name,
                    // Registered = the daemon knows this server, whether or not
                    // the app behind it is up. A session app's tools stay in the
                    // roster while it sleeps; that is the design, not a fault.
                    // `null` when the registry could not be read at all — an
                    // unread registry is not evidence of an unregistered server.
                    "registered": if registry_error.is_some() { Value::Null } else { json!(info.is_some()) },
                    "status":    info.map(|s| s["status"].clone()).unwrap_or(Value::Null),
                    "toolCount": tools.map(|t| t.len()).unwrap_or(0),
                });
                if let Some(err) = info.and_then(|s| s["error"].as_str()) {
                    row["error"] = json!(err);
                }
                if with_tools {
                    row["tools"] = json!(tools
                        .map(|t| t
                            .iter()
                            .filter_map(|x| x["name"].as_str())
                            .collect::<Vec<_>>())
                        .unwrap_or_default());
                }
                row
            })
            .collect();

        let mut out = json!({
            "apps": rows,
            "count": rows.len(),
            "callFormat": "mcp__<mcpName>__<tool>",
            "note": "Một app chưa chạy vẫn giữ tool trong roster: lần gọi đầu sẽ tự bật app qua proxy của daemon.",
        });
        if let Some(e) = registry_error {
            // Said loudly, because every degraded field below is silent about
            // why it is empty: `toolCount: 0` on a healthy app is otherwise
            // indistinguishable from an app that really exposes nothing.
            out["registryError"] = json!(e);
            out["degraded"] = json!(
                "Không đọc được danh bạ MCP: `status`, `toolCount` và `tools` không đáng tin trong \
                 kết quả này. `mcpName` lấy từ manifest nên vẫn đúng."
            );
        }
        ToolResult::ok(out.to_string())
    }
}

const BAD_ID: &str =
    "app_id không hợp lệ. Chỉ chấp nhận chữ, số, `-` và `_` (tối đa 80 ký tự). Dùng space_app_list để lấy id đúng.";

/// Trim and validate an app id.
///
/// Same rule as the daemon's `valid_space_app_id`, applied here as well because
/// the id is interpolated into a URL path: a `../` or an encoded slash would
/// otherwise aim a POST at a route the caller did not name.
fn normalize_app_id(raw: &str) -> Option<String> {
    let id = raw.trim();
    if id.is_empty() || id.len() > 80 {
        return None;
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_ids_that_would_escape_the_route() {
        assert_eq!(normalize_app_id("kanban"), Some("kanban".into()));
        assert_eq!(
            normalize_app_id("  luna-calendar "),
            Some("luna-calendar".into())
        );
        assert_eq!(normalize_app_id("ai_office"), Some("ai_office".into()));

        // Path traversal and separators, raw or encoded — each of these would
        // otherwise POST to a different route than the caller named.
        assert_eq!(normalize_app_id("../../api/space/apps"), None);
        assert_eq!(normalize_app_id("kanban/stop"), None);
        assert_eq!(normalize_app_id("kanban%2Fstop"), None);
        assert_eq!(normalize_app_id("kanban?x=1"), None);
        assert_eq!(normalize_app_id(""), None);
        assert_eq!(normalize_app_id("   "), None);
        assert_eq!(normalize_app_id(&"a".repeat(81)), None);
    }

    #[test]
    fn base_url_comes_from_env_and_loses_its_trailing_slash() {
        // Serialised through one test because env is process-global.
        temp_env(
            "SENCLAW_SPACE_API_URL",
            Some("http://127.0.0.1:9999/"),
            || {
                assert_eq!(
                    SpaceAppsClient::from_env().base_url,
                    "http://127.0.0.1:9999"
                );
            },
        );
        temp_env("SENCLAW_SPACE_API_URL", None, || {
            temp_env("SENCLAW_UI_PORT", None, || {
                assert_eq!(
                    SpaceAppsClient::from_env().base_url,
                    "http://127.0.0.1:18788",
                    "an unset URL must fall back to the documented daemon port, \
                     so a hand-registered space-server still manages apps"
                );
            });
        });
    }

    fn temp_env(key: &str, value: Option<&str>, f: impl FnOnce()) {
        let prev = std::env::var(key).ok();
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        f();
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}
