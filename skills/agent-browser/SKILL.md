---
name: agent-browser
description: Drive the connected browser via the `senclaw-browser` MCP server — search the web, navigate sites, extract structured data, fill forms, take screenshots. Use whenever a task needs live web content (current prices, news, docs, web research) or interaction with a web app.
version: 1.2.0
when-to-use: any request that needs fresh web content, web automation, or page screenshots (e.g. "tìm giá vàng hôm nay", "screenshot github trending", "fill this form", "extract product list from amazon")
triggers:
  # --- Web search & research ---
  - search
  - tìm
  - tìm kiếm
  - tra cứu
  - look up
  - google
  - bing
  - research
  - nghiên cứu
  # --- Current/live data ---
  - giá
  - price
  - tỷ giá
  - exchange rate
  - thời tiết
  - weather
  - tin tức
  - news
  - hôm nay
  - today
  - hiện tại
  - current
  - latest
  - mới nhất
  - live
  - real-time
  # --- Web navigation ---
  - mở trang
  - open page
  - navigate
  - go to
  - truy cập
  - visit
  - website
  - url
  - link
  # --- Content extraction ---
  - extract
  - trích xuất
  - lấy nội dung
  - crawl
  - scrape
  - đọc trang
  - read page
  - nội dung trang
  - page content
  - tổng hợp
  - summarize site
  # --- Screenshot ---
  - screenshot
  - chụp màn hình
  - chụp trang
  - capture
  # --- Form interaction ---
  - fill form
  - điền form
  - đăng nhập
  - login
  - sign in
  - submit
  - gửi form
  # --- Comparison & aggregation ---
  - so sánh
  - compare
  - đánh giá
  - review
  - top
  - best
  - ranking
  - xếp hạng
---

# Agent Browser Skill

A connected Chrome instance is exposed through the **`senclaw-browser`** MCP server (backed by `src/mcp/browser_server.rs` + the `senclaw-extension-chrome` WebSocket bridge). The browser runs in the user's actual session — pages they're logged in to are accessible.

---

## When to Activate This Skill

Use this skill whenever the task requires **information or interaction that only a live browser can provide**. The key question: "Does this need the real, current web — or can I answer from my training data?"

### ACTIVATE when the user:

| Intent | Example phrases (VI/EN) | Why browser needed |
|--------|------------------------|--------------------|
| **Asks for current/live data** | "giá vàng hôm nay", "bitcoin price now", "tỷ giá USD" | Training data is stale; needs real-time source |
| **Wants web search results** | "tìm kiếm X", "google Y", "search for Z", "tra cứu" | Needs SERP results from a search engine |
| **Needs content from a URL** | "đọc trang này", "mở link này", "what does this page say" | Content lives on the web, not in training data |
| **Requests a screenshot** | "chụp trang", "screenshot of X", "show me what Y looks like" | Visual capture requires a browser |
| **Wants structured extraction** | "lấy danh sách sản phẩm", "extract table from", "scrape prices" | Structured data from a live DOM |
| **Needs to interact with a web app** | "điền form", "đăng nhập vào", "submit this", "click the button" | Form filling and clicks require a browser session |
| **Compares or aggregates from multiple sites** | "so sánh giá", "tổng hợp đánh giá", "which is cheaper" | Cross-site data aggregation |
| **Asks about recent events/news** | "tin tức mới nhất", "what happened today", "latest release" | Current events not in training data |
| **Wants to crawl/index a site** | "crawl documentation site", "index all blog posts" | Multi-page sweep |
| **Checks website status or appearance** | "trang này còn hoạt động không", "how does this page look" | Needs live HTTP request / rendering |

### DO NOT activate when:

| Situation | Use instead |
|-----------|-------------|
| User asks a general knowledge question answerable from training | Answer directly |
| User wants to edit local files or code | `code` skill or file tools |
| User wants to manage notes/calendar/email | `space` or `note` skill |
| User wants wiki/knowledge base operations | `wiki` skill |
| User provides the full text and just asks to process it | Process directly, no browser needed |
| User asks about a concept, definition, or how-to | Answer from training data |

### Priority signals (strongest → weakest):

1. **Explicit URL** in the message → always activate
2. **Time-sensitive keywords** ("hôm nay", "hiện tại", "latest", "now", "current") → likely activate
3. **Search verbs** ("tìm", "search", "google", "tra cứu", "look up") → activate
4. **Price/rate/weather keywords** ("giá", "tỷ giá", "thời tiết", "price") → activate
5. **Screenshot/capture** ("chụp", "screenshot", "capture") → always activate
6. **General question without time/web signals** → probably don't activate

---

## Tool names

The MCP server registers under the name **`senclaw-browser`**, so each tool's full identifier is:

```
mcp__senclaw-browser__browser_<verb>
```

Examples: `mcp__senclaw-browser__browser_search`, `mcp__senclaw-browser__browser_navigate`, `mcp__senclaw-browser__browser_snapshot`.

`mcp__senclaw-browser__browser_<verb>` is the canonical and **only** form that resolves. There is **no** `mcp__browser__*` resolver in plain Claude Code — never use a "short" tool name, and never substitute another browser MCP (Playwright, Claude-in-Chrome) for `senclaw-browser`.

Inside SenClaw the built-in tools run bundled, so the same tools are listed as `mcp__core__browser_<verb>` (`mcp__core__browser_search`, …); the `mcp__senclaw-browser__…` names above resolve to them there too.

## Step 0 — Load the tools FIRST (only if they are not in your tool list)

**Already in your tool list?** SenClaw loads the browser tools together with this skill (as `mcp__core__browser_<verb>`). If you can see them, skip this step and call them directly — a `ToolSearch` here only costs a round trip.

Otherwise the tools are **deferred** to save tokens: their schemas are NOT in the prompt, so **calling one before loading it fails with `InputValidationError`**. Run `ToolSearch` first, in the same turn, before any `senclaw-browser` call.

Bulk-load by exact name (most reliable):

```
ToolSearch {
  "query": "select:mcp__senclaw-browser__browser_search,mcp__senclaw-browser__browser_navigate,mcp__senclaw-browser__browser_snapshot,mcp__senclaw-browser__browser_click,mcp__senclaw-browser__browser_type,mcp__senclaw-browser__browser_extract_text,mcp__senclaw-browser__browser_extract_structured,mcp__senclaw-browser__browser_screenshot,mcp__senclaw-browser__browser_fill_form,mcp__senclaw-browser__browser_click_and_wait,mcp__senclaw-browser__browser_wait,mcp__senclaw-browser__browser_new_tab,mcp__senclaw-browser__browser_close_tab",
  "max_results": 30
}
```

Or load by keyword (matches the server name substring in every tool name, so one query returns the whole toolkit):

```
ToolSearch { "query": "senclaw-browser", "max_results": 30 }
```

Need a tool not in the common set (e.g. `crawl`, `extract_table`)? Load it the same way: `ToolSearch { query: "select:mcp__senclaw-browser__browser_crawl,mcp__senclaw-browser__browser_crawl_status" }`.

Only after ToolSearch returns the matched schemas may you call the tool directly.

## Core workflows

### 1. Quick web search + detail extraction (preferred for "find/search X")

Use **`browser_search`** — returns ranked SERP results without opening a tab. Cheapest path for fact-finding.

```
mcp__senclaw-browser__browser_search {
  "query": "<user query in the user's language>",
  "engine": "google",            // or "bing"; omit → the user's configured default engine
  "num_results": 10,             // default 10
  "language": "vi"               // optional
}
```

**User defaults**: if the system prompt carries a `## User defaults` block naming a
preferred search tool (e.g. the Search app's `mcp__search-mcp__search_query`), use
that tool for searches instead of `browser_search`. Omit `engine` unless the user
asks for a specific one — the daemon fills in their configured default.

Returns structured results (`title`, `url`, `snippet`, …). Treat snippets as a lead, not the final source, whenever the user asks for current data, prices, news, comparisons, summaries, or "tổng hợp".

After search:

1. Pick the most relevant 1-3 result URLs. Prefer official, primary, or high-authority sources.
2. Navigate to each selected URL with `browser_navigate`.
3. Extract the page content with `browser_extract_text` or `browser_extract_structured`.
4. Pull out the concrete facts, numbers, timestamps, source names, and any disagreement between sources.
5. Close tabs you opened if the task is done.
6. Answer from the extracted page content, citing each URL.

Only answer directly from snippets when the user explicitly asks for search-result links/snippets, or when navigation/extraction fails and you clearly say the answer is based on search snippets only. Do not ask the user whether to open the pages; open the relevant result pages yourself.

### 2. Open a page + read content

```
mcp__senclaw-browser__browser_navigate { "url": "https://example.com/article" }
↓ (response includes a tab_id)
mcp__senclaw-browser__browser_snapshot { "tab_id": "<from navigate response>" }
↓ pick element indices from the snapshot
mcp__senclaw-browser__browser_extract_text { "tab_id": "...", "selector": "article" }   // selector is optional CSS; omit for whole page
```

`browser_snapshot` returns a compact accessibility tree with **numbered indices** for every interactive element. Those indices are the `index` values passed to `click`, `type`, `hover`, `click_and_wait`, etc. `tab_id` is optional on most tools — omit it to act on the active tab.

### 3. Structured data extraction

When the page has tabular / list data, skip manual parsing — let the model map it via schema:

```
mcp__senclaw-browser__browser_extract_structured {
  "tab_id": "...",
  "schema": {
    "type": "array",
    "items": {
      "type": "object",
      "properties": {
        "title":  { "type": "string" },
        "price":  { "type": "number" },
        "url":    { "type": "string" }
      }
    }
  },
  "selector": "...",     // optional: scope to a container
  "max_items": 50        // optional cap
}
```

For HTML tables specifically use `browser_extract_table` — faster, no schema needed.

### 4. Fill forms / interact

```
mcp__senclaw-browser__browser_fill_form {
  "tab_id": "...",
  "fields": [
    { "target": "Email",    "value": "alice@example.com", "type": "text" },
    { "target": "Password", "value": "...",                "type": "password" }
  ],
  "submit": true
}
```

Each field's `target` auto-matches by label / placeholder / name / CSS selector. For a single field, use `browser_type { "tab_id": "...", "index": <n>, "text": "...", "submit": false }` after a snapshot.

For navigation-triggering clicks (login, submit), prefer **`browser_click_and_wait { "index": <n> }`** so the next step doesn't race the new page load.

### 5. Screenshot

```
mcp__senclaw-browser__browser_screenshot {
  "tab_id": "...",
  "full_page": false,                 // false = viewport (default); true = full page
  "element_selector": "#hero",        // optional: shoot just one element
  "format": "png"                     // or "jpeg" (+ optional "quality")
}
```

Returns image data via the workbench (not inlined, to save tokens). Tell the user "screenshot saved to workbench".

### 6. Crawl multiple pages

```
mcp__senclaw-browser__browser_crawl {
  "start_url": "https://blog.example.com",
  "max_pages": 20,                    // default 50
  "depth": 2,                         // default 2
  "extract_type": "markdown",         // "text" (default) | "markdown" | ...
  "link_patterns": [".*/posts/.*"],   // regexes a link must match to follow
  "exclude_patterns": ["/tag/.*"],    // optional
  "same_domain": true                 // default true
}
```

Returns a `job_id`. Poll with `browser_crawl_status { "job_id": "..." }`. Stop a stuck per-tab task with `browser_stop_task { "tab_id": "..." }`.

## Decision tree

```
User asks for…
├── A specific fact / list / "current X today"
│       → browser_search (fastest)
├── Content of a known URL
│       → browser_navigate + browser_extract_text
├── Structured data from a page
│       → browser_extract_structured (schema) or browser_extract_table
├── Multi-page sweep of a site
│       → browser_crawl + browser_crawl_status
├── Login / sign-up / form submission
│       → browser_fill_form (submit: true)
├── Visual proof / share with user
│       → browser_screenshot
└── Page state debug
        → browser_snapshot
```

## Rules

- **ToolSearch before the first call** (Step 0). A direct call to a not-yet-loaded `senclaw-browser` tool fails with `InputValidationError`.
- **Search first** for general questions — use `browser_search` before navigating blindly.
- **Do not stop at SERP snippets for synthesis/current data** — after search, open the selected result URLs and extract page content before answering.
- **Single tab per task** — open with `browser_new_tab`, close with `browser_close_tab` when done. `tab_id` is optional elsewhere (defaults to the active tab).
- **Indices come from the snapshot** — never invent element numbers. If you need to click and don't have a current snapshot, run `browser_snapshot` first; re-snapshot after the DOM changes.
- **Wait when navigating** — use `browser_click_and_wait` for nav-triggering clicks; `browser_wait` after manual navigation if the page is slow.
- **Respect login boundaries** — the browser uses the user's real cookies. Don't post / pay / send messages without explicit user confirmation.
- **Report the URL** in the final response so the user can verify the source.
- **No emojis** in tool-output relays unless the user asks.

## Failure handling

| Symptom | Cause | Fix |
|---|---|---|
| `InputValidationError` on a browser tool | Tool not loaded yet | Run the Step 0 `ToolSearch` first, then retry |
| `senclaw-browser` server not in the MCP list | Daemon not running, or extension not connected | Start the SenClaw daemon and open the SenClaw browser tab in Chrome |
| `Extension not connected` | Chrome extension offline | Ask user to open the SenClaw browser tab |
| `Tab not found` after navigate | Tab closed mid-task | Re-create with `browser_new_tab` |
| `Element not found` | Index stale after DOM change | Re-run `browser_snapshot` then retry |
| Search returns empty | Engine rate-limited | Switch `engine` to the other (google ↔ bing) |
| Crawl stuck | Task hung | `browser_stop_task { tab_id }`, then narrow `link_patterns` and re-crawl |

## Examples

**Query**: "tìm giá vàng hôm nay"

```
1. ToolSearch { query: "select:mcp__senclaw-browser__browser_search,mcp__senclaw-browser__browser_navigate,mcp__senclaw-browser__browser_extract_text,mcp__senclaw-browser__browser_close_tab" }
2. mcp__senclaw-browser__browser_search {
     "query": "giá vàng hôm nay",
     "num_results": 5
   }
3. Pick the top relevant source URLs (for example 24h, PNJ, SJC/DOJI/BTMC if present).
4. mcp__senclaw-browser__browser_navigate { "url": "<selected result URL>" }
5. mcp__senclaw-browser__browser_extract_text { "tab_id": "<from navigate response>" }
6. Repeat for 1-2 additional high-value sources if the figures need corroboration.
7. Summarize the extracted prices, units, update time/source, and cite each URL. Mention if sources disagree.
```

**Query**: "screenshot github.com/trending"

```
1. ToolSearch { query: "select:mcp__senclaw-browser__browser_navigate,mcp__senclaw-browser__browser_screenshot" }
2. mcp__senclaw-browser__browser_navigate { "url": "https://github.com/trending" }
   → { tab_id: "tab-1", ... }
3. mcp__senclaw-browser__browser_screenshot { "tab_id": "tab-1", "full_page": false }
   → "screenshot saved to workbench"
4. Reply: "Screenshot of github.com/trending saved. [workbench link]"
```

**Query**: "extract top 10 hacker news front-page posts as JSON"

```
1. ToolSearch { query: "select:mcp__senclaw-browser__browser_navigate,mcp__senclaw-browser__browser_extract_structured" }
2. mcp__senclaw-browser__browser_navigate { "url": "https://news.ycombinator.com" }
3. mcp__senclaw-browser__browser_extract_structured {
     "tab_id": "...",
     "schema": {
       "type": "array",
       "items": {
         "type": "object",
         "properties": {
           "rank":      { "type": "number" },
           "title":     { "type": "string" },
           "url":       { "type": "string" },
           "points":    { "type": "number" },
           "comments":  { "type": "number" }
         }
       }
     },
     "max_items": 10
   }
4. Return the JSON to the user.
```

## Tools available (full list)

All registered as `mcp__senclaw-browser__browser_<name>` (see `src/mcp/browser_server.rs`):

Navigation: `navigate`, `new_tab`, `close_tab`, `list_tabs`, `switch_tab`, `go_back`, `go_forward`, `reload`

Interaction: `click`, `type`, `select_option`, `scroll`, `hover`, `press_key`, `upload_file`, `fill_form`, `click_and_wait`, `execute_js`, `wait`

Inspection: `snapshot`, `screenshot`, `get_status`

Extraction: `extract_text`, `extract_links`, `extract_table`, `extract_structured`

Research: `search`, `crawl`, `crawl_status`, `stop_task`
