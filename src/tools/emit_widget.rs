//! `emit_widget` tool — push a one-way rich widget into the chat box.
//!
//! Unlike [`crate::tools::form_ui::FormUITool`] this is **display-only**: the
//! tool emits [`EngineEvent::WidgetEmit`] and returns immediately (no
//! [`crate::zen_core::ResponseRegistry`], no suspend, no response event). It
//! mirrors the one-way `tool:execution` push instead of the FormUI round-trip.
//!
//! See `WIDGET_CONTRACT.md` for the kind-specific `data` schemas. The backend
//! keeps `data` opaque — the web (`WidgetCard.tsx`) and desktop
//! (`widget_card.dart`) clients validate and render it.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::zen_core::{
    EngineEvent, Tool, ToolContext, ToolOutput, ToolResultMessage, WidgetEmitData, WidgetSpec,
};

/// The built-in template widget kinds (contract §1). Kind `app` (a Space-App
/// widget from the registry) is handled separately — see [`parse_app_spec`].
const KINDS: [&str; 6] = ["chart", "image", "clock", "weather", "video", "audio"];

const DESCRIPTION: &str = r#"# Rich chat widget (display-only)
Push a rich, non-interactive widget into the chat box for the user to see. This
is ONE-WAY — the widget is rendered inline; the user does NOT respond to it
(unlike FormUI/AskUserQuestion). Use it to show a chart, an image, a live clock,
a weather card, playable video/audio, or an installed Space App's own widget
alongside your text reply.

kind must be one of: chart | image | clock | weather | video | audio | app.
data is a kind-specific object (validated & rendered by the client):
- chart:   { chartType: bar|line|area|pie|scatter, xLabel?, yLabel?, stacked?, ...data }
           where ...data is ONE of (the daemon normalizes all of them):
             series: [{ name, color?, points: [{x,y}] }]   (points may also be [x,y] pairs or bare numbers)
             rows:   [{ date: "26/07", high: 37, low: 26 }, ...]   — one object per x;
                     EVERY numeric column becomes a series; optional "x" names the x column
             labels: [...] + values: [...]   — a single series
           Numeric strings ("37", "33,5") are parsed. Prefer `rows` when you have tabular data.
- image:   { url? | dataUrl?, caption?, alt? }   (one of url/dataUrl required)
- clock:   { tz?, label?, showSeconds?, showDate?, format24h? }
- weather: { location, unit: C|F, current: {temp,condition,icon,humidity,wind}, daily?: [{day,hi,lo,icon}] }
- video:   { url, poster?, caption?, mime?, autoplay? }   (url required)
- audio:   { url, caption?, mime? }   (url required)

Prepare widget data INLINE in this tool call: do unit conversions and math
yourself and pass the result directly (e.g. °F→°C, percentages). NEVER write a
temp script/file or run bash/node just to reshape data for a widget — the
`rows` shortcut above exists precisely so raw tabular data can be passed as-is.

For `video`/`audio` the url MUST be an http(s) URL the chat client can fetch —
a local filesystem path will NOT play. Space Apps that store media expose one
(e.g. the TikTok downloader returns `file_urls` on each finished download).

kind "app" embeds a widget provided by an installed Space App: pass `widget`
(its full id, e.g. "crm.pipeline") and `params` (per its schema) INSTEAD of
`data`. Call the `widget_list` tool first to see available app widgets, their
descriptions and params.

INLINE ALTERNATIVE: for a chart/media that belongs inside your flowing reply,
you may instead write a fenced block directly in your response text — e.g.
```chart with the same data object — and the client renders it in place (same
data shortcuts). Use this tool for standalone cards, another chat_jid, or app
widgets (kind "app" requires the tool).

On messaging channels (Telegram/Zalo/WeChat/…) the rich card cannot render — the
user automatically receives a one-line text summary instead. The full widget
shows on the SenClaw Web/Desktop UI.

Returns immediately after queuing the widget — do not expect a value back."#;

pub struct EmitWidgetTool;

/// Parse + validate the raw tool input into a [`WidgetSpec`]. Kept separate so
/// both `validate_input` and `call` share one code path.
fn parse_spec(input: &Value) -> std::result::Result<WidgetSpec, String> {
    let kind = input
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or("kind is required")?
        .to_string();
    if kind == "app" {
        return parse_app_spec(input);
    }
    if !KINDS.contains(&kind.as_str()) {
        return Err(format!(
            "Invalid kind \"{kind}\"; expected one of chart | image | clock | weather | video | audio | app"
        ));
    }
    let mut data = input.get("data").cloned().ok_or("data is required")?;
    if !data.is_object() {
        return Err("data must be an object".to_string());
    }
    // Chart data goes through the daemon-side normalizer: it accepts `rows` /
    // `labels`+`values` / point shortcuts and emits the one canonical shape
    // every client renders. This is the "data → widget" pipe that removes any
    // reason to shell out to a temp script just to reshape data.
    if kind == "chart" {
        data = crate::widgets::chart_data::normalize_chart_data(&data)?;
    }
    // Other kinds stay opaque (the clients own the rendering), with one
    // exception: video/audio without a fetchable url render as a dead card, so
    // catch it here where the model still gets a chance to correct itself.
    if kind == "video" || kind == "audio" {
        let url = data.get("url").and_then(|v| v.as_str()).unwrap_or("").trim();
        if url.is_empty() {
            return Err(format!("{kind} data requires a non-empty \"url\""));
        }
        let lower = url.to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            return Err(format!(
                "{kind} url must be an http(s) URL the chat client can fetch, got \"{url}\""
            ));
        }
    }
    let title = input
        .get("title")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(WidgetSpec { kind, title, data })
}

/// Kind `app`: resolve a Space-App widget from the registry and build the
/// client payload (`data = { app, widget, params, entry, … }`). The registry
/// is only present inside the daemon; standalone runtimes get a clear error.
fn parse_app_spec(input: &Value) -> std::result::Result<WidgetSpec, String> {
    let full_id = input
        .get("widget")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("kind \"app\" requires \"widget\" (full widget id, e.g. \"crm.pipeline\" — call widget_list to see available ids)")?;
    let params = match input.get("params") {
        None => Value::Object(Default::default()),
        Some(p) if p.is_object() => p.clone(),
        Some(_) => return Err("params must be an object".to_string()),
    };
    let registry = crate::widgets::global()
        .ok_or("the widget registry is not available in this runtime; only built-in kinds (chart/image/clock/weather/video/audio) work here")?;
    let def = registry.find(full_id).ok_or_else(|| {
        format!("unknown widget \"{full_id}\" — call widget_list for the available app widgets")
    })?;
    if !def.enabled {
        return Err(format!(
            "widget \"{full_id}\" is disabled in Plugins → Widget settings"
        ));
    }
    if !def.surfaces.iter().any(|s| s == "chat") {
        return Err(format!(
            "widget \"{full_id}\" does not support the chat surface (surfaces: {:?})",
            def.surfaces
        ));
    }
    if let Some(schema) = &def.params {
        crate::widgets::validate_params(schema, &params)?;
    }
    let (app_id, short_id) = full_id.split_once('.').unwrap_or((full_id, full_id));
    let mut data = serde_json::json!({
        "app": app_id,
        "widget": short_id,
        "id": full_id,
        "params": params,
    });
    if let Some(entry) = &def.entry {
        // Params travel as a query string only — the entry path itself is
        // fixed by the manifest, so the model can't point the iframe anywhere
        // else (same philosophy as `sanitize_event_link`).
        let mut url = entry.clone();
        if let Some(obj) = params.as_object() {
            let qs: Vec<String> = obj
                .iter()
                .map(|(k, v)| {
                    let val = match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    format!("{}={}", urlencoding::encode(k), urlencoding::encode(&val))
                })
                .collect();
            if !qs.is_empty() {
                url.push(if url.contains('?') { '&' } else { '?' });
                url.push_str(&qs.join("&"));
            }
        }
        data["entry"] = Value::String(url);
    }
    if let Some(size) = &def.size {
        data["size"] = Value::String(size.clone());
    }
    if let Some(ms) = def.refresh_ms {
        data["refreshMs"] = Value::from(ms);
    }
    if let Some(tpl) = &def.text_fallback {
        data["textFallback"] = Value::String(crate::widgets::render_text_fallback(tpl, &params));
    }
    let title = input
        .get("title")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| Some(def.name.clone()));
    Ok(WidgetSpec {
        kind: "app".to_string(),
        title,
        data,
    })
}

#[async_trait]
impl Tool for EmitWidgetTool {
    fn name(&self) -> &str {
        "emit_widget"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["chart", "image", "clock", "weather", "video", "audio", "app"],
                    "description": "Which widget to render."
                },
                "title": {
                    "type": "string",
                    "description": "Optional card header shown above the widget."
                },
                "data": {
                    "type": "object",
                    "description": "Kind-specific payload (built-in kinds only). See the tool description for each kind's shape."
                },
                "widget": {
                    "type": "string",
                    "description": "kind \"app\" only: full app-widget id (e.g. \"widget-pack.countdown\"). Call widget_list for available ids."
                },
                "params": {
                    "type": "object",
                    "description": "kind \"app\" only: params per the widget's declared schema (see widget_list)."
                },
                "chat_jid": {
                    "type": "string",
                    "description": "Optional target chat JID; defaults to the current chat."
                }
            },
            "required": ["kind"]
        })
    }

    fn is_read_only(&self) -> bool {
        // Display-only push; no filesystem/network side effects.
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolContext<'_>,
    ) -> std::result::Result<(), String> {
        parse_spec(input)?;
        Ok(())
    }

    async fn call(&self, input: Value, ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let widget = parse_spec(&input).map_err(|e| anyhow::anyhow!(e))?;
        let chat_jid = input
            .get("chat_jid")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let event_bus = ctx
            .event_bus
            .ok_or_else(|| anyhow::anyhow!("EventBus not available"))?;

        let id = format!("widget-{}", uuid::Uuid::new_v4());
        let kind = widget.kind.clone();
        event_bus.emit(EngineEvent::WidgetEmit(WidgetEmitData {
            agent_id: ctx.agent_id.to_string(),
            chat_jid,
            widget,
            id: id.clone(),
        }));

        let result_for_assistant = format!(
            "Rendered a {kind} widget in the chat. It is display-only; the user will not reply to it. \
             (On messaging channels the rich card cannot render — the user receives a one-line text summary instead; \
             the full widget shows on the SenClaw Web/Desktop UI.)"
        );
        Ok(vec![ToolOutput::Result {
            data: serde_json::json!({ "kind": kind, "id": id }),
            result_for_assistant,
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        let kind = data
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("widget");
        ToolResultMessage {
            title: "Widget".into(),
            summary: format!("Rendered {kind} widget"),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, input: &Value) -> String {
        match input.get("kind").and_then(|v| v.as_str()) {
            Some(kind) => format!("Widget: {kind}"),
            None => "Widget".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(
        bus: &'a crate::zen_core::EventBus,
        abort: &tokio_util::sync::CancellationToken,
    ) -> ToolContext<'a> {
        ToolContext {
            agent_id: "main",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: abort.clone(),
            event_bus: Some(bus),
            response_registry: None,
            edit_format: Default::default(),
        }
    }

    fn sample() -> Value {
        serde_json::json!({
            "kind": "chart",
            "title": "Doanh thu",
            "data": {
                "chartType": "bar",
                "series": [{"name": "Q1", "points": [{"x": "T1", "y": 30}]}]
            }
        })
    }

    #[test]
    fn parse_spec_ok() {
        let spec = parse_spec(&sample()).unwrap();
        assert_eq!(spec.kind, "chart");
        assert_eq!(spec.title.as_deref(), Some("Doanh thu"));
        assert!(spec.data.is_object());
    }

    #[test]
    fn parse_spec_rejects_bad_kind() {
        let bad = serde_json::json!({"kind": "hologram", "data": {}});
        assert!(parse_spec(&bad).unwrap_err().contains("Invalid kind"));
    }

    #[test]
    fn parse_spec_accepts_video_with_http_url() {
        let spec = parse_spec(&serde_json::json!({
            "kind": "video",
            "title": "Video TikTok",
            "data": {
                "url": "http://127.0.0.1:4670/api/downloads/3/file",
                "caption": "clip vừa tải"
            }
        }))
        .unwrap();
        assert_eq!(spec.kind, "video");
    }

    #[test]
    fn parse_spec_rejects_video_without_fetchable_url() {
        // Missing url.
        let err = parse_spec(&serde_json::json!({"kind": "video", "data": {}})).unwrap_err();
        assert!(err.contains("non-empty"), "{err}");
        // A local path is the mistake worth catching — it renders as a dead card.
        let err = parse_spec(&serde_json::json!({
            "kind": "video",
            "data": {"url": "/Users/me/Downloads/clip.mp4"}
        }))
        .unwrap_err();
        assert!(err.contains("http(s)"), "{err}");
        // So is a file:// URL.
        let err = parse_spec(&serde_json::json!({
            "kind": "video",
            "data": {"url": "file:///tmp/clip.mp4"}
        }))
        .unwrap_err();
        assert!(err.contains("http(s)"), "{err}");
    }

    #[test]
    fn parse_spec_validates_audio_like_video() {
        let ok = parse_spec(&serde_json::json!({
            "kind": "audio",
            "data": {"url": "https://x/a.mp3", "caption": "bài hát"}
        }))
        .unwrap();
        assert_eq!(ok.kind, "audio");
        let err = parse_spec(&serde_json::json!({"kind": "audio", "data": {}})).unwrap_err();
        assert!(err.contains("non-empty"), "{err}");
        let err = parse_spec(&serde_json::json!({
            "kind": "audio",
            "data": {"url": "/tmp/a.mp3"}
        }))
        .unwrap_err();
        assert!(err.contains("http(s)"), "{err}");
    }

    #[test]
    fn parse_spec_app_requires_widget_id_and_registry() {
        // Missing `widget`.
        let err = parse_spec(&serde_json::json!({"kind": "app"})).unwrap_err();
        assert!(err.contains("widget"), "{err}");
        // Non-object params.
        let err = parse_spec(&serde_json::json!({
            "kind": "app", "widget": "crm.pipeline", "params": [1]
        }))
        .unwrap_err();
        assert!(err.contains("params"), "{err}");
        // With `widget` set but no global registry installed in the test
        // process, the error must say the registry is unavailable (standalone
        // runtimes) — not pretend the widget doesn't exist.
        // NOTE: other tests never call `widgets::init_global`, so this holds
        // process-wide.
        let err = parse_spec(&serde_json::json!({
            "kind": "app", "widget": "crm.pipeline"
        }))
        .unwrap_err();
        assert!(err.contains("registry"), "{err}");
    }

    #[test]
    fn parse_spec_rejects_non_object_data() {
        let bad = serde_json::json!({"kind": "chart", "data": [1, 2, 3]});
        assert!(parse_spec(&bad)
            .unwrap_err()
            .contains("data must be an object"));
    }

    #[test]
    fn parse_spec_requires_kind_and_data() {
        assert!(parse_spec(&serde_json::json!({"data": {}})).is_err());
        assert!(parse_spec(&serde_json::json!({"kind": "clock"})).is_err());
    }

    #[tokio::test]
    async fn call_emits_widget_event_and_returns_immediately() {
        let bus = crate::zen_core::EventBus::new();
        let abort = tokio_util::sync::CancellationToken::new();
        let mut rx = bus.subscribe();

        let outputs = EmitWidgetTool
            .call(sample(), &ctx(&bus, &abort))
            .await
            .unwrap();

        // Returns a Result synchronously (no blocking).
        let ToolOutput::Result { data, .. } = &outputs[0] else {
            panic!("expected Result output");
        };
        assert_eq!(data["kind"], "chart");
        let id = data["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("widget-"));

        // And emitted exactly one WidgetEmit event carrying the spec.
        let event = rx.try_recv().unwrap();
        let EngineEvent::WidgetEmit(emitted) = event else {
            panic!("expected WidgetEmit");
        };
        assert_eq!(emitted.agent_id, "main");
        assert_eq!(emitted.widget.kind, "chart");
        assert_eq!(emitted.id, id);
        assert!(emitted.chat_jid.is_none());
    }

    #[tokio::test]
    async fn call_passes_through_chat_jid() {
        let bus = crate::zen_core::EventBus::new();
        let abort = tokio_util::sync::CancellationToken::new();
        let mut rx = bus.subscribe();
        let mut input = sample();
        input["chat_jid"] = serde_json::json!("telegram:42");
        EmitWidgetTool
            .call(input, &ctx(&bus, &abort))
            .await
            .unwrap();
        let EngineEvent::WidgetEmit(emitted) = rx.try_recv().unwrap() else {
            panic!("expected WidgetEmit");
        };
        assert_eq!(emitted.chat_jid.as_deref(), Some("telegram:42"));
    }
}
