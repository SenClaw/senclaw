# WIDGET_CONTRACT — inline chat-widget payload contract

The contract between the `emit_widget` tool (backend, `src/tools/emit_widget.rs`),
the widget registry (`src/widgets/`), and the three renderers:
`web/src/components/WidgetCard.tsx`, `desktop_app/lib/features/chat/widgets/widget_card.dart`,
`channel_app/lib/widgets/widget_card.dart`.

## §1 WidgetSpec

```jsonc
{ "kind": "<kind>", "title": "optional card header", "data": { /* kind-specific */ } }
```

The backend keeps `data` opaque (`serde_json::Value`); clients validate & render.
Backend-enforced checks: `kind` known, `data` is an object, `video`/`audio` carry a
non-empty http(s) `url`, and kind `app` resolves in the widget registry.

## §2 Built-in kinds

| kind | data |
|------|------|
| `chart` | Canonical (what clients render & what's persisted): `{ chartType: bar\|line\|area\|pie\|scatter, series: [{ name, color?, points: [{x,y}] }], xLabel?, yLabel?, stacked? }`. The **tool input** additionally accepts `rows` (array of flat objects — every numeric column becomes a series, x column auto-detected or named via `x`), `labels`+`values`, and point shortcuts (`[x,y]` pairs, bare numbers, numeric strings incl. comma decimals) — normalized daemon-side by `src/widgets/chart_data.rs::normalize_chart_data` before persist/broadcast. Fenced ` ```chart ` blocks bypass the daemon → canonical only. |
| `image` | `{ url? \| dataUrl?, caption?, alt? }` — one of url/dataUrl required |
| `clock` | `{ tz?, label?, showSeconds?, showDate?, format24h? }` |
| `weather` | `{ location, unit: "C"\|"F", current: { temp, condition, icon, humidity?, wind? }, daily?: [{ day, hi, lo, icon }] }` — icon ∈ sunny\|partly_cloudy\|cloudy\|rain\|thunderstorm\|snow\|fog\|wind |
| `video` | `{ url, poster?, caption?, mime?, autoplay? }` — url required, http(s) |
| `audio` | `{ url, caption?, mime? }` — url required, http(s) |

## §3 Kind `app` — Space-App / plugin widgets

Emitted via `emit_widget { kind: "app", widget: "<full id>", params: {...} }`.
The daemon resolves the id in the widget registry (manifest `widgets[]` of enabled
Space Apps + enabled plugins' `widgets/widgets.json`) and builds:

```jsonc
{
  "kind": "app",
  "title": "widget name (or tool-supplied title)",
  "data": {
    "app": "crm",             // app id (deep link: /space/app/<app>)
    "widget": "pipeline",     // short id
    "id": "crm.pipeline",     // full id
    "params": { ... },        // as passed, validated against the widget's schema
    "entry": "http://127.0.0.1:4390/widget/pipeline.html?stage=won",
                              // resolved entry (runtime.url origin or
                              // /api/space/apps/<id>/proxy fallback) + params
                              // as a query string. Params NEVER alter the path.
    "size": "medium",         // optional: small|medium|large|tall
    "refreshMs": 30000,       // optional client reload hint
    "textFallback": "..."     // rendered {param} template for text channels
  }
}
```

Clients render `entry` in a sandboxed iframe (web) / embedded webview (desktop);
channel_app renders a text card with a deep link. Unknown `kind` values must render
an error chip, never crash — old clients meeting `app`/`audio` degrade this way.

### Manifest declaration (`senclaw-manifest.json` → `widgets[]`)

```jsonc
{
  "id": "pipeline",              // required
  "name": "Phễu bán hàng",
  "description": "…",            // shown to the agent via widget_list
  "entryUrl": "/widget/pipeline.html",
  "size": "medium", "refreshMs": 30000, "render": "client",
  "surfaces": ["dashboard", "chat"],  // DEFAULT ["dashboard"] (pre-registry behavior)
  "params": { "type": "object", "properties": { "stage": { "type": "string" } }, "required": ["stage"] },
  "textFallback": "Phễu giai đoạn {stage} — mở CRM để xem",
  "intents": ["media"]           // optional: candidate handler for default flows
}
```

Plugins use the same entry schema in `<pluginDir>/widgets/widgets.json`; their
entries resolve against `/api/marketplace/plugins/<name>/widget-static/`.

## §4 Transport & persistence

- Event: `EngineEvent::WidgetEmit` → bridge in `lib.rs` → `chat_widgets` table
  (FIFO per jid) → WS frame `{ "type": "chat:widget", "groupJid", "id", "widget", "ts" }`.
- `history:load` merges rows as `{ id, role: "widget", widget, timestamp }`.
- **Messaging channels** (jid not `web:`/`virtual:`/`app:`): the WS broadcast can't
  reach them, so the bridge additionally sends `widgets::fallback_text(spec)` as a
  plain channel message (same egress gate as replies). The tool result says so.

## §5 Markdown fence fallback

Fence languages `widget` | `chart` | `weather` | `clock` | `video` | `audio` | `image`
render as widgets in all three clients. ` ```widget ` takes a full spec (incl. kind
`app`); the others take the bare `data` object. Invalid/incomplete JSON falls back to
a plain code block (streaming-safe).

The fence is the **primary way to place a chart inline** in a flowing reply (the
renderer injects the widget at that exact spot). Chart fences accept the same
shortcut shapes as the tool (`rows`, `labels`+`values`, point pairs/bare numbers,
numeric strings) — each client's chart renderer normalizes them (`deriveChartSeries`
in web `WidgetCard.tsx`, `_seriesFromRows` in both Flutter `widget_card.dart`s), so
fence and tool render identically. Kind `app` still requires the tool (registry
resolution happens daemon-side).

## §6 Catalog & settings surfaces

- `GET /api/widgets` — full catalog (`WidgetDef` list) with `enabled` flags.
- `PUT /api/widgets/:id` — `{ enabled }` toggle (stored in `defaults.disabledWidgets`).
- `GET/PUT /api/defaults` — default flow handlers (open link / media / search / note).
- Tools: `widget_list` (deferred; discovery) and `emit_widget` (emission).
- UI: Plugins → Widget (catalog + defaults), web `WidgetsPanel.tsx`.
