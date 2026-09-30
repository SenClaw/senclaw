---
name: agent-browser
description: Use the web through SenClaw's browser (`senclaw-browser` MCP server) — search, read pages, and carry out whole tasks on a site (click, type, choose, fill forms) in one call. Use whenever a task needs live web content (current prices, news, docs, web research) or interaction with a web app.
version: 2.0.0
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

SenClaw's browser runs a decision loop inside the daemon: you state the goal, the loop watches the page, picks each step in a fraction of a second and checks the result. **Every tool call costs you a whole turn — so a job is one call, not one call per click.**

The tools are already in your tool list as `mcp__core__browser_<verb>` (`mcp__senclaw-browser__browser_<verb>` names the same tools). Call them directly: no `ToolSearch` first.

## Pick the call by the job

| The person wants… | One call |
|---|---|
| something found on the web | `browser_search { "query": "…" }` → `results` (title, url) plus the results page's text |
| a page read, or a question answered from it | `browser_read { "url": "…" }` → text and links; add `"question"` for an answer quoted from the page |
| something **done** on a site — open it, click, type, choose, scroll, press a button, fill a form, several steps in a row | `browser_task { "goal": "…", "url": "…", "done_criteria": ["…"] }` |
| to see the page | `browser_screenshot {}` |

`browser_open`, `browser_look` and `browser_do` drive the page one step at a time. Use them only when `browser_task` could not do the job: each step is a turn of yours, where the loop takes a fraction of a second.

## browser_search, then browser_read

```
mcp__senclaw-browser__browser_search { "query": "giá vàng SJC hôm nay" }
→ { "engine": "DuckDuckGo", "results": [ { "title": "…", "url": "https://…" }, … ], "text": "…" }

mcp__senclaw-browser__browser_read { "url": "<the best result's url>", "question": "Giá vàng SJC mua vào / bán ra hôm nay?" }
→ { "url": "…", "title": "…", "answer": "…" }
```

Search snippets are leads, not sources: read the page before you state a figure, and read a second one when a number needs confirming. `browser_read` with a `url` opens the page and reads it in the same call — never `browser_open` followed by `browser_read`.

## browser_task

```
mcp__senclaw-browser__browser_task {
  "goal": "Open the Deals page, then show Business class deals to London",
  "url": "https://example.com/",
  "done_criteria": ["The page shows \"Business class\"", "The page shows \"deals to London\""]
}
```

- **`goal`**: everything in one sentence or two, with every value a form will need (names, dates, amounts). The loop never invents a value.
- **`url`**: where to start. Leave it out to go on in this chat's current tab.
- **`done_criteria`**: what the page visibly shows when the goal is reached, quoted text where you can. Always give them: they are what accepts a DONE, and without them a model has to write them first.
- **`question`**: asked of the final page; the reply comes back in `answer`.

What comes back, by `status`:

| status | What to do |
|---|---|
| `done` | Report it, with `url` (and `answer` if you asked). `evidence` lists the criteria checked. |
| `needs_approval` | Show `pending.action` to the person. Call `browser_approve { approval_id, approve: true }` **only after they say yes** to that action; `approve: false` declines. |
| `needs_user` | A sign-in, a one-time code or a CAPTCHA: `browser_handover { "action": "start" }`, tell the person where the tab is, wait for them, then `browser_handover { "action": "done" }` and `browser_resume { task_id }`. |
| `needs_input` | The goal lacks a value. Ask the person, then run the task again with it. |
| `budget` | It ran out of steps: `browser_resume { task_id }` to go on. |
| `blocked`, `unverified` | Say what the page shows (`message`, `url`); do not claim it worked. |

`notes`, when present, tells you what made the run slow (a decision model that is not installed, for one). Pass it on to the person.

## Which browser

Tasks run in SenClaw's own Chrome profile: nothing of the person's is signed in there. For a site that needs **their** account, add `"browser": "extension"` — their own Chrome, through the SenClaw extension — and only when they ask for it or the task plainly needs their sign-in. A tab they shared from the side panel is listed by `browser_tabs` (`extension.shared_tabs`); pass its `tab` as `shared_tab`.

## Rules

- One job, one call. Never click through a page with `browser_do` when `browser_task` can take the goal.
- `browser_open` and `browser_do` already return the page: do not `browser_look` after them.
- Buying, paying, sending, posting, deleting: the loop stops and asks. Never approve for the person.
- Passwords, codes and CAPTCHAs are the person's to enter: hand the tab over.
- Give the URL of every page you relied on.

## When it fails

| What you get | Why | Do |
|---|---|---|
| `code: "blocked"` from `browser_search`, or a page that is a bot check | The site turned SenClaw's browser away | Run it in the person's Chrome (`"browser": "extension"`), or hand the tab over |
| `extension_not_connected` | Their Chrome's SenClaw extension is not connected | Ask them to open its side panel, or use SenClaw's own browser |
| `code: "timeout"` | The task is still running in SenClaw | `browser_tabs`, then `browser_resume` — do not start it again |
| `runtime_not_installed` | The browser runtime is missing | Tell the person: Settings → Runtimes → sen-browser |
| `stale_page` from `browser_do` | The page changed since you looked | `browser_look`, then act on the new table |
