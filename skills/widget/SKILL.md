---
name: widget
description: Render rich inline widgets in the chat box — charts, images, clock, weather, video/audio players, and widgets provided by installed Space Apps — via emit_widget + widget_list
version: 1.0.0
when-to-use: When a visual rendering communicates better than text — plotting numbers, showing an image/video/audio, a live clock or weather card, or embedding an installed Space App's widget (kanban board, CRM pipeline, calendar…) inline in the chat
triggers:
  - widget
  - biểu đồ
  - chart
  - đồ thị
  - vẽ biểu đồ
  - hiển thị
  - phát video
  - phát nhạc
  - phát audio
  - xem ảnh
  - đồng hồ
  - thời tiết
allowed-tools:
  - emit_widget
  - widget_list
---

# Widget — Rich Inline Chat Widgets

You can push **display-only** widgets into the chat box with `emit_widget`. They render
inline next to your text reply. They are ONE-WAY: the user cannot interact back through
them (for input, use FormUI / AskUserQuestion instead).

## When to use a widget

- The user asks to **see** something: a trend, a comparison, an image, a video.
- Numbers you are reporting would be clearer as a chart than a table.
- The user asks to "play" a media URL → an inline `video`/`audio` widget.
- An installed Space App has a widget that answers the request better than prose
  (a kanban board snapshot, a CRM pipeline, a calendar…).

Don't emit a widget for information a single sentence already conveys, and don't
emit more than 2–3 widgets per reply.

## Built-in kinds

Call `emit_widget` with `kind` + `data`:

| kind | data shape |
|------|-----------|
| `chart` | `{ chartType: bar\|line\|area\|pie\|scatter, xLabel?, yLabel?, stacked? }` + the data in ONE of three forms: canonical `series`, **`rows`** (tabular), or `labels`+`values` — see "Preparing chart data" below |
| `image` | `{ url? \| dataUrl?, caption?, alt? }` (one of url/dataUrl required) |
| `clock` | `{ tz?, label?, showSeconds?, showDate?, format24h? }` |
| `weather` | `{ location, unit: C\|F, current: {temp,condition,icon,humidity,wind}, daily?: [{day,hi,lo,icon}] }` |
| `video` | `{ url, poster?, caption?, mime?, autoplay? }` (url required, http(s) only) |
| `audio` | `{ url, caption?, mime? }` (url required, http(s) only) |

`video`/`audio` URLs must be **fetchable http(s)** URLs — a local filesystem path
renders a dead card. Space Apps that store media expose URLs (e.g. the TikTok
downloader returns `file_urls`).

## Preparing chart data — NO temp files

**Hard rule: do all math/unit conversion inline in the tool call.** Never write
a script to `/tmp` or run `bash`/`node` just to reshape data for a widget — it
wastes a permission round-trip and is entirely unnecessary, because the daemon
normalizes these raw shapes itself:

1. **`rows` (preferred for tabular data)** — an array of flat objects, one per
   x point; **every numeric column automatically becomes a series**; the x
   column is auto-detected (`x`/`date`/`day`/`label`/`name`/`ngày`…) or named
   explicitly with `"x": "<column>"`:

```
emit_widget {
  "kind": "chart", "title": "Nhiệt độ Hà Nội 7 ngày",
  "data": {
    "chartType": "line", "xLabel": "Ngày", "yLabel": "°C",
    "rows": [
      { "date": "26/07", "cao": 37, "thấp": 26 },
      { "date": "27/07", "cao": 34, "thấp": 28 }
    ]
  }
}
```

   (Need °F→°C? Compute `(F-32)*5/9` in your head and fill in the converted
   numbers — no code needed.)

2. **`labels` + `values`** — two parallel arrays, one series (great for `pie`):
   `{ "chartType": "pie", "labels": ["A","B"], "values": [60, 40] }`
3. **Canonical `series`** — when you need exact control of series
   names/colors/order; `points` accepts `{x,y}`, `[x,y]` pairs, or bare
   numbers (x = index).

Numeric strings ("37", "33,5") parse automatically. These shortcuts apply to
**both the `emit_widget` tool and fences** — the daemon normalizes the tool
path, the renderers normalize the fence path, and both display identically.

## Space-App widgets (kind `app`)

Installed Space Apps and plugins can provide their own widgets. Workflow:

1. Call `widget_list` — it returns every chat-capable widget with its full id,
   description, and params schema.
2. Emit with `kind: "app"`, passing `widget` (the full id) and `params` — NOT `data`:

```
emit_widget { "kind": "app", "widget": "widget-pack.countdown", "params": { "to": "2026-12-31", "label": "Tết Dương lịch" } }
emit_widget { "kind": "app", "widget": "widget-pack.progress", "params": { "value": 68, "max": 100, "label": "Hoàn thành sổ tay" } }
emit_widget { "kind": "app", "widget": "widget-pack.table", "params": { "title": "Menu", "cols": "Món,Giá", "rows": "[[\"Cà phê\",25000],[\"Bạc xỉu\",30000]]" } }
```

(The bundled `widget-pack` app ships these plus iframe chart/image/video
variants; any other installed app's widgets appear in `widget_list`
automatically.)

If `emit_widget` reports the widget as unknown or disabled, trust the error —
re-run `widget_list` rather than guessing ids.

## Inserting a widget INSIDE your reply (fence — preferred for charts)

**The simplest way to show a chart**: write a fenced block at the exact spot in
your reply where the chart belongs. Every client's renderer detects it and
replaces the block with the widget — the chart sits inside the flowing text, is
not split into a separate card, and needs no tool call:

````markdown
Nhiệt độ Hà Nội 7 ngày qua giảm dần:

```chart
{ "chartType": "line", "xLabel": "Ngày", "yLabel": "°C",
  "rows": [
    { "date": "26/07", "cao": 37, "thấp": 26 },
    { "date": "27/07", "cao": 34, "thấp": 28 }
  ] }
```

Nắng nóng nhất là ngày 26/07.
````

Fence languages: `widget` (full spec `{kind,title,data}`), or `chart` /
`weather` / `clock` / `video` / `audio` / `image` (body = the `data` object).
Fences accept **all the same data shortcuts** as the tool (`rows`,
`labels`+`values`, `[x,y]` pairs / bare-number points) — the renderer
normalizes them.

**Rule**: if you write "here is the chart", you MUST include the fence (or an
emit_widget call) in that same turn — never just list the numbers as bullets
and skip the chart.

Fence or tool?
- **Fence** — the chart/media is part of the reply you are writing (default
  for charts).
- **`emit_widget`** — you need a standalone card outside the text flow, a
  different `chat_jid`, or a **Space-App widget (kind `app` — tool required**,
  since the daemon registry resolves it).

## Messaging channels

On Telegram/Zalo/QQ/Feishu/WeChat the rich card cannot render. The daemon
automatically delivers a **one-line text summary** to the channel instead; the full
widget shows on the SenClaw Web/Desktop UI. So on channel chats, make sure your
accompanying text carries the essential information — the widget is a bonus there,
not the message.

## User defaults

If the system prompt contains a `## User defaults` block, honor it: it tells you the
user's preferred media handler (inline widget vs browser), search tool, and note
store. When it says media = inline widget, "phát bài này" means `emit_widget`
kind `video`/`audio`, not a link.
