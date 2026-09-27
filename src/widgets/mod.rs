//! Widget registry — one catalog for every chat-box / dashboard widget the
//! daemon knows about.
//!
//! Three sources feed the catalog:
//! 1. **Built-in template kinds** (`chart`, `image`, `clock`, `weather`,
//!    `video`, `audio`) — rendered natively by each client from `data`.
//! 2. **Space Apps** — the `widgets[]` section of `senclaw-manifest.json`
//!    (read from the `space_apps` DB registry, same rows
//!    `space_mcp::autoregister_installed` scans). These are `url` widgets:
//!    an iframe/webview pointed at the app's `entryUrl`.
//! 3. **Plugins** — `<pluginDir>/widgets/widgets.json` (same entry schema as
//!    the manifest section), served via the daemon's plugin static route.
//!
//! The registry is the single lookup used by the `emit_widget` tool (kind
//! `app`), the `widget_list` tool, and `GET /api/widgets`. Enable/disable
//! state and the flow defaults live in the `defaults` section of
//! `~/.senclaw/config.json` (see `gateway::group_manager::DefaultsConfig`).
//!
//! See `WIDGET_CONTRACT.md` for the payload contract each client renders.

pub mod chart_data;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;
use serde_json::Value;

use crate::db::Db;
use crate::gateway::group_manager::get_defaults_config;
use crate::marketplace::manager::MarketplaceManager;
use crate::zen_core::WidgetSpec;

/// Built-in template widget kinds rendered natively by the clients.
pub const BUILTIN_KINDS: [&str; 6] = ["chart", "image", "clock", "weather", "video", "audio"];

/// One catalog entry. `id` is globally unique: the bare kind name for
/// built-ins (`"chart"`), `"<app_id>.<widget_id>"` for Space-App widgets and
/// `"plugin:<name>.<widget_id>"`-sourced ids stay `"<plugin>.<widget_id>"`.
#[derive(Debug, Clone, Serialize)]
pub struct WidgetDef {
    pub id: String,
    /// `"builtin"` | `"app:<app_id>"` | `"plugin:<plugin_name>"`
    pub source: String,
    /// `"template"` (client renders `data` natively) | `"url"` (iframe/webview).
    pub kind: String,
    pub name: String,
    pub description: String,
    /// Where this widget may appear: `"chat"` and/or `"dashboard"`.
    pub surfaces: Vec<String>,
    /// JSON-Schema (object) for the params the agent fills when emitting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// App-relative entry (`/widget/foo.html`) as declared in the manifest.
    #[serde(skip_serializing_if = "Option::is_none", rename = "entryUrl")]
    pub entry_url: Option<String>,
    /// Resolved entry the client can load directly: the app's stamped
    /// `runtime.url` origin when available, else the daemon proxy path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "refreshMs")]
    pub refresh_ms: Option<u64>,
    /// Template rendered for text-only channels; `{param}` placeholders are
    /// substituted from the emit params.
    #[serde(skip_serializing_if = "Option::is_none", rename = "textFallback")]
    pub text_fallback: Option<String>,
    /// Flow intents this widget can serve as a default handler for.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub intents: Vec<String>,
    pub enabled: bool,
}

pub struct WidgetRegistry {
    db: Option<Arc<Db>>,
    config_path: PathBuf,
    marketplace: Option<Arc<Mutex<MarketplaceManager>>>,
}

static GLOBAL: OnceLock<Arc<WidgetRegistry>> = OnceLock::new();

/// Install the process-wide registry (called once from `run_daemon`). Tools
/// (`emit_widget` kind `app`, `widget_list`) read it via [`global`]; in
/// standalone runtimes (tests, MCP subprocess binaries) it stays unset and the
/// tools degrade to the built-in-only catalog.
pub fn init_global(registry: WidgetRegistry) {
    let _ = GLOBAL.set(Arc::new(registry));
}

pub fn global() -> Option<Arc<WidgetRegistry>> {
    GLOBAL.get().cloned()
}

impl WidgetRegistry {
    pub fn new(
        db: Option<Arc<Db>>,
        config_path: PathBuf,
        marketplace: Option<Arc<Mutex<MarketplaceManager>>>,
    ) -> Self {
        Self {
            db,
            config_path,
            marketplace,
        }
    }

    /// The full catalog (built-in + apps + plugins) with `enabled` applied
    /// from the `defaults.disabledWidgets` config list. Recomputed per call —
    /// the sources (SQLite row scan + one config-file read) are cheap and this
    /// keeps app installs/updates visible without invalidation plumbing.
    pub fn catalog(&self) -> Vec<WidgetDef> {
        let disabled = get_defaults_config(&self.config_path)
            .disabled_widgets
            .unwrap_or_default();
        let mut out = builtin_defs();
        out.extend(self.app_defs());
        out.extend(self.plugin_defs());
        for def in &mut out {
            def.enabled = !disabled.iter().any(|d| d == &def.id);
        }
        out
    }

    /// Look one widget up by its full id.
    pub fn find(&self, id: &str) -> Option<WidgetDef> {
        self.catalog().into_iter().find(|d| d.id == id)
    }

    fn app_defs(&self) -> Vec<WidgetDef> {
        let Some(db) = &self.db else {
            return Vec::new();
        };
        let rows: Vec<(String, Value)> = match db.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT id, manifest FROM space_apps WHERE enabled = 1")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .filter_map(|r| r.ok())
                .filter_map(|(id, m)| serde_json::from_str::<Value>(&m).ok().map(|v| (id, v)))
                .collect::<Vec<_>>();
            Ok(rows)
        }) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("[widgets] could not list space apps: {e}");
                return Vec::new();
            }
        };
        rows.iter()
            .flat_map(|(app_id, manifest)| parse_manifest_widgets(app_id, manifest))
            .collect()
    }

    /// Plugin-shipped widgets (`<pluginDir>/widgets/widgets.json`).
    fn plugin_defs(&self) -> Vec<WidgetDef> {
        let Some(mkt) = &self.marketplace else {
            return Vec::new();
        };
        let plugin_dirs: Vec<(String, PathBuf)> = match mkt.lock() {
            Ok(m) => m.enabled_plugin_dirs(),
            Err(_) => return Vec::new(),
        };
        plugin_dirs
            .iter()
            .flat_map(|(name, dir)| parse_plugin_widgets(name, dir))
            .collect()
    }
}

/// Built-in template kinds as catalog entries. `data` shapes live in
/// `WIDGET_CONTRACT.md` and the `emit_widget` tool description.
fn builtin_defs() -> Vec<WidgetDef> {
    let mk = |id: &str, name: &str, description: &str| WidgetDef {
        id: id.to_string(),
        source: "builtin".to_string(),
        kind: "template".to_string(),
        name: name.to_string(),
        description: description.to_string(),
        surfaces: vec!["chat".to_string()],
        params: None,
        entry_url: None,
        entry: None,
        size: None,
        refresh_ms: None,
        text_fallback: None,
        intents: Vec::new(),
        enabled: true,
    };
    vec![
        mk(
            "chart",
            "Biểu đồ",
            "Bar/line/area/pie/scatter chart từ series số liệu",
        ),
        mk(
            "image",
            "Ảnh",
            "Hiển thị một ảnh (url hoặc dataUrl) kèm caption",
        ),
        mk("clock", "Đồng hồ", "Đồng hồ sống theo timezone"),
        mk(
            "weather",
            "Thời tiết",
            "Thẻ thời tiết hiện tại + dự báo ngày",
        ),
        mk(
            "video",
            "Video",
            "Phát video từ một http(s) URL ngay trong chat",
        ),
        mk(
            "audio",
            "Âm thanh",
            "Phát audio từ một http(s) URL ngay trong chat",
        ),
    ]
}

/// Parse the `widgets[]` section of one app manifest into catalog entries.
/// Unknown/missing fields degrade instead of erroring: widgets existed in 10
/// manifests before this registry and must keep loading untouched.
pub fn parse_manifest_widgets(app_id: &str, manifest: &Value) -> Vec<WidgetDef> {
    let Some(items) = manifest.get("widgets").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    // Entry base: the daemon stamps `runtime.url` on spawn; fall back to the
    // reverse proxy so a not-yet-started app still resolves somewhere the UI
    // can reach (the proxy lazily boots the app on first hit).
    let base = manifest
        .get("runtime")
        .and_then(|r| r.get("url"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|| format!("/api/space/apps/{app_id}/proxy"));
    items
        .iter()
        .filter_map(|w| {
            let short_id = w.get("id").and_then(|v| v.as_str())?.trim();
            if short_id.is_empty() {
                return None;
            }
            let entry_url = w
                .get("entryUrl")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let entry = entry_url.as_deref().map(|e| {
                if e.starts_with('/') {
                    format!("{base}{e}")
                } else {
                    format!("{base}/{e}")
                }
            });
            let surfaces = w
                .get("surfaces")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str())
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                })
                // Pre-registry manifests declared dashboard widgets only —
                // keep that exact behavior when `surfaces` is absent.
                .unwrap_or_else(|| vec!["dashboard".to_string()]);
            Some(WidgetDef {
                id: format!("{app_id}.{short_id}"),
                source: format!("app:{app_id}"),
                kind: "url".to_string(),
                name: w
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(short_id)
                    .to_string(),
                description: w
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                surfaces,
                params: w.get("params").filter(|p| p.is_object()).cloned(),
                entry_url,
                entry,
                size: w.get("size").and_then(|v| v.as_str()).map(String::from),
                refresh_ms: w.get("refreshMs").and_then(|v| v.as_u64()),
                text_fallback: w
                    .get("textFallback")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                intents: w
                    .get("intents")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str())
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default(),
                enabled: true,
            })
        })
        .collect()
}

/// Parse `<pluginDir>/widgets/widgets.json` (array of the same entry schema as
/// the manifest `widgets[]` section). Entries resolve against the daemon's
/// plugin static route since plugins have no server of their own.
pub fn parse_plugin_widgets(plugin_name: &str, plugin_dir: &Path) -> Vec<WidgetDef> {
    let path = plugin_dir.join("widgets").join("widgets.json");
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(items) = serde_json::from_str::<Value>(&raw) else {
        tracing::warn!("[widgets] malformed widgets.json for plugin '{plugin_name}'");
        return Vec::new();
    };
    let Some(items) = items.as_array() else {
        return Vec::new();
    };
    // NOT `/api/plugins/...` — that namespace belongs to the clawhub plugin
    // system; marketplace plugin assets live under `/api/marketplace/...`.
    let base = format!("/api/marketplace/plugins/{plugin_name}/widget-static");
    items
        .iter()
        .filter_map(|w| {
            let short_id = w.get("id").and_then(|v| v.as_str())?.trim();
            if short_id.is_empty() {
                return None;
            }
            let entry_url = w
                .get("entryUrl")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let entry = entry_url.as_deref().map(|e| {
                let e = e.trim_start_matches('/');
                format!("{base}/{e}")
            });
            Some(WidgetDef {
                id: format!("{plugin_name}.{short_id}"),
                source: format!("plugin:{plugin_name}"),
                kind: "url".to_string(),
                name: w
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(short_id)
                    .to_string(),
                description: w
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                surfaces: w
                    .get("surfaces")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s.as_str())
                            .map(String::from)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_else(|| vec!["chat".to_string()]),
                params: w.get("params").filter(|p| p.is_object()).cloned(),
                entry_url,
                entry,
                size: w.get("size").and_then(|v| v.as_str()).map(String::from),
                refresh_ms: w.get("refreshMs").and_then(|v| v.as_u64()),
                text_fallback: w
                    .get("textFallback")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                intents: Vec::new(),
                enabled: true,
            })
        })
        .collect()
}

/// Minimal JSON-Schema check for widget emit params: `required` presence plus
/// primitive `type` agreement per declared property. Deliberately lenient —
/// undeclared params pass (an app may accept more than it documents).
pub fn validate_params(schema: &Value, params: &Value) -> Result<(), String> {
    let props = schema.get("properties").and_then(|v| v.as_object());
    if let Some(required) = schema.get("required").and_then(|v| v.as_array()) {
        for key in required.iter().filter_map(|k| k.as_str()) {
            if params.get(key).is_none() {
                return Err(format!("missing required param \"{key}\""));
            }
        }
    }
    if let (Some(props), Some(obj)) = (props, params.as_object()) {
        for (key, value) in obj {
            let Some(decl) = props.get(key) else { continue };
            let Some(ty) = decl.get("type").and_then(|t| t.as_str()) else {
                continue;
            };
            let ok = match ty {
                "string" => value.is_string(),
                "number" => value.is_number(),
                "integer" => value.is_i64() || value.is_u64(),
                "boolean" => value.is_boolean(),
                "array" => value.is_array(),
                "object" => value.is_object(),
                _ => true,
            };
            if !ok {
                return Err(format!("param \"{key}\" must be of type {ty}"));
            }
        }
    }
    Ok(())
}

/// The user's configured `browser_search` engine (`defaults.searchEngine`),
/// falling back to google. Called from the browser MCP server — which may run
/// as a standalone stateless subprocess — so it re-derives the config path via
/// `Config::from_env()` instead of threading daemon state; the cost is one env
/// scan + one small file read per search, dwarfed by the search itself.
pub fn configured_search_engine() -> String {
    let cfg = crate::config::Config::from_env();
    get_defaults_config(&cfg.paths.global_config_path)
        .effective_search_engine()
        .to_string()
}

/// Substitute `{key}` placeholders from `params`; unresolved placeholders
/// render as empty string so a fallback never leaks raw braces to a channel.
pub fn render_text_fallback(template: &str, params: &Value) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) => {
                let key = &after[..end];
                if let Some(v) = params.get(key) {
                    match v {
                        Value::String(s) => out.push_str(s),
                        Value::Null => {}
                        other => out.push_str(&other.to_string()),
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Render a widget as one line of plain text for text-only messaging channels
/// (Telegram/Feishu/WeChat…). The WS `chat:widget` broadcast never reaches
/// them, so this line is what the channel user gets instead of silence.
pub fn fallback_text(spec: &WidgetSpec) -> String {
    let title = spec.title.as_deref().unwrap_or("").trim();
    let d = &spec.data;
    let s = |key: &str| {
        d.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    match spec.kind.as_str() {
        "chart" => {
            let n = d
                .get("series")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let t = if title.is_empty() {
                "Biểu đồ"
            } else {
                title
            };
            format!("📊 {t} ({n} chuỗi số liệu) — xem trên SenClaw Web/Desktop")
        }
        "image" => {
            let cap = if !s("caption").is_empty() {
                s("caption")
            } else {
                title.to_string()
            };
            let url = s("url");
            match (cap.is_empty(), url.is_empty()) {
                (false, false) => format!("🖼 {cap}: {url}"),
                (false, true) => format!("🖼 {cap}"),
                (true, false) => format!("🖼 {url}"),
                (true, true) => "🖼 (ảnh — xem trên SenClaw Web/Desktop)".to_string(),
            }
        }
        "clock" => {
            let label = if !s("label").is_empty() {
                s("label")
            } else {
                s("tz")
            };
            if label.is_empty() {
                "🕐 Đồng hồ — xem trên SenClaw Web/Desktop".to_string()
            } else {
                format!("🕐 Đồng hồ: {label}")
            }
        }
        "weather" => {
            let loc = s("location");
            let (temp, cond) = d
                .get("current")
                .map(|c| {
                    (
                        c.get("temp")
                            .map(|t| t.to_string().trim_matches('"').to_string())
                            .unwrap_or_default(),
                        c.get("condition")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    )
                })
                .unwrap_or_default();
            format!("🌤 Thời tiết {loc}: {temp}° {cond}")
                .trim()
                .to_string()
        }
        "video" | "audio" => {
            let icon = if spec.kind == "video" { "🎬" } else { "🎵" };
            let cap = if !s("caption").is_empty() {
                s("caption")
            } else {
                title.to_string()
            };
            let url = s("url");
            if cap.is_empty() {
                format!("{icon} {url}")
            } else {
                format!("{icon} {cap}: {url}")
            }
        }
        "app" => {
            let rendered = s("textFallback");
            if !rendered.is_empty() {
                return rendered;
            }
            let t = if title.is_empty() { "Widget" } else { title };
            let app = s("app");
            if app.is_empty() {
                format!("{t} — mở SenClaw để xem chi tiết")
            } else {
                format!("{t} — mở SenClaw → /space/app/{app} để xem chi tiết")
            }
        }
        _ => {
            if title.is_empty() {
                String::new()
            } else {
                format!("{title} — xem trên SenClaw Web/Desktop")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest_with_widgets(widgets: Value) -> Value {
        json!({
            "id": "crm",
            "runtime": { "kind": "server", "port": 4390 },
            "widgets": widgets
        })
    }

    #[test]
    fn parse_manifest_defaults_to_dashboard_surface_and_proxy_entry() {
        let m = manifest_with_widgets(json!([
            { "id": "pipeline", "name": "Phễu", "description": "Phễu deal", "entryUrl": "/widget/pipeline.html", "size": "medium", "refreshMs": 30000 }
        ]));
        let defs = parse_manifest_widgets("crm", &m);
        assert_eq!(defs.len(), 1);
        let d = &defs[0];
        assert_eq!(d.id, "crm.pipeline");
        assert_eq!(d.source, "app:crm");
        assert_eq!(d.kind, "url");
        // Pre-registry manifests must keep meaning "dashboard only".
        assert_eq!(d.surfaces, vec!["dashboard"]);
        // No stamped runtime.url → proxy path.
        assert_eq!(
            d.entry.as_deref(),
            Some("/api/space/apps/crm/proxy/widget/pipeline.html")
        );
        assert_eq!(d.refresh_ms, Some(30000));
    }

    #[test]
    fn parse_manifest_uses_stamped_runtime_url_and_new_fields() {
        let mut m = manifest_with_widgets(json!([
            {
                "id": "board",
                "name": "Bảng",
                "entryUrl": "/widget/board.html",
                "surfaces": ["chat", "dashboard"],
                "params": { "type": "object", "properties": { "stage": { "type": "string" } }, "required": ["stage"] },
                "textFallback": "Bảng giai đoạn {stage}",
                "intents": ["media"]
            }
        ]));
        m["runtime"]["url"] = json!("http://127.0.0.1:4390");
        let defs = parse_manifest_widgets("crm", &m);
        let d = &defs[0];
        assert_eq!(
            d.entry.as_deref(),
            Some("http://127.0.0.1:4390/widget/board.html")
        );
        assert_eq!(d.surfaces, vec!["chat", "dashboard"]);
        assert!(d.params.is_some());
        assert_eq!(d.text_fallback.as_deref(), Some("Bảng giai đoạn {stage}"));
        assert_eq!(d.intents, vec!["media"]);
    }

    #[test]
    fn parse_manifest_skips_entries_without_id() {
        let m = manifest_with_widgets(json!([{ "name": "vô danh" }, { "id": "" }]));
        assert!(parse_manifest_widgets("x", &m).is_empty());
        assert!(parse_manifest_widgets("x", &json!({"id": "x"})).is_empty());
    }

    #[test]
    fn validate_params_checks_required_and_types() {
        let schema = json!({
            "type": "object",
            "properties": { "stage": { "type": "string" }, "limit": { "type": "integer" } },
            "required": ["stage"]
        });
        assert!(validate_params(&schema, &json!({ "stage": "won" })).is_ok());
        assert!(validate_params(&schema, &json!({ "stage": "won", "limit": 5 })).is_ok());
        // Undeclared params pass (lenient).
        assert!(validate_params(&schema, &json!({ "stage": "won", "extra": true })).is_ok());
        let err = validate_params(&schema, &json!({})).unwrap_err();
        assert!(err.contains("stage"), "{err}");
        let err = validate_params(&schema, &json!({ "stage": 3 })).unwrap_err();
        assert!(err.contains("string"), "{err}");
        let err = validate_params(&schema, &json!({ "stage": "a", "limit": "nope" })).unwrap_err();
        assert!(err.contains("integer"), "{err}");
        // No schema at all → anything goes.
        assert!(validate_params(&json!({}), &json!({ "x": 1 })).is_ok());
    }

    #[test]
    fn render_text_fallback_substitutes_and_drops_unknown() {
        let out = render_text_fallback(
            "Phễu {stage} có {count} deal {missing}",
            &json!({ "stage": "won", "count": 12 }),
        );
        assert_eq!(out, "Phễu won có 12 deal");
        // Unclosed brace stays literal (never panics).
        assert_eq!(render_text_fallback("a {b", &json!({})), "a {b");
    }

    #[test]
    fn fallback_text_covers_builtin_and_app_kinds() {
        let chart = WidgetSpec {
            kind: "chart".into(),
            title: Some("Doanh thu".into()),
            data: json!({ "series": [{"name": "Q1"}, {"name": "Q2"}] }),
        };
        assert!(fallback_text(&chart).contains("Doanh thu"));
        assert!(fallback_text(&chart).contains("2"));

        let video = WidgetSpec {
            kind: "video".into(),
            title: None,
            data: json!({ "url": "https://x/v.mp4", "caption": "clip" }),
        };
        assert_eq!(fallback_text(&video), "🎬 clip: https://x/v.mp4");

        let app = WidgetSpec {
            kind: "app".into(),
            title: Some("Phễu Q3".into()),
            data: json!({ "app": "crm", "widget": "pipeline", "textFallback": "Phễu won — mở CRM" }),
        };
        assert_eq!(fallback_text(&app), "Phễu won — mở CRM");

        let app_no_fb = WidgetSpec {
            kind: "app".into(),
            title: Some("Phễu".into()),
            data: json!({ "app": "crm", "widget": "pipeline" }),
        };
        let t = fallback_text(&app_no_fb);
        assert!(t.contains("/space/app/crm"), "{t}");
    }

    #[test]
    fn parse_plugin_widgets_resolves_against_marketplace_static_route() {
        let tmp = tempfile::TempDir::new().unwrap();
        let wdir = tmp.path().join("widgets");
        std::fs::create_dir_all(&wdir).unwrap();
        std::fs::write(
            wdir.join("widgets.json"),
            serde_json::to_string(&json!([
                { "id": "hello", "name": "Hello", "description": "d", "entryUrl": "/hello.html" },
                { "name": "no-id" }
            ]))
            .unwrap(),
        )
        .unwrap();
        let defs = parse_plugin_widgets("my-plugin", tmp.path());
        assert_eq!(defs.len(), 1);
        let d = &defs[0];
        assert_eq!(d.id, "my-plugin.hello");
        assert_eq!(d.source, "plugin:my-plugin");
        // Plugins default to the chat surface (they exist for the chat box).
        assert_eq!(d.surfaces, vec!["chat"]);
        assert_eq!(
            d.entry.as_deref(),
            Some("/api/marketplace/plugins/my-plugin/widget-static/hello.html")
        );
        // Missing dir / malformed file degrade to empty, never error.
        assert!(parse_plugin_widgets("x", &tmp.path().join("nope")).is_empty());
    }

    #[test]
    fn builtin_defs_match_builtin_kinds() {
        let defs = builtin_defs();
        assert_eq!(defs.len(), BUILTIN_KINDS.len());
        for kind in BUILTIN_KINDS {
            assert!(defs.iter().any(|d| d.id == kind && d.source == "builtin"));
        }
    }
}
