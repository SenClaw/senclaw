---
name: space
description: Personal productivity tools — manage notes via the Space MCP server, and email via the Email Space App's `email-mcp` server. For calendar events see the "calendar" skill; for recurring schedules see the "schedule" skill.
version: 1.2.0
when-to-use: When the user wants to manage notes (create, search, tag, organize) or email (check inbox, read, compose, search). For calendar events and reminders use the "calendar" skill; for recurring scheduled tasks use the "schedule" skill.
triggers:
  # --- Notes (Vietnamese) ---
  - ghi chú
  - tạo ghi chú
  - note
  - ghi lại
  - lưu lại
  - todo
  - việc cần làm
  - ý tưởng
  - idea
  - meeting notes
  - biên bản
  - tóm tắt cuộc họp
  # --- Notes (English) ---
  - take note
  - create note
  - save note
  - find note
  - search notes
  - delete note
  - list notes
  - tag
  # --- Email (Vietnamese) ---
  - email
  - mail
  - thư
  - hộp thư
  - inbox
  - kiểm tra mail
  - kiểm tra email
  - đọc mail
  - viết email
  - soạn email
  - gửi email
  - trả lời email
  - tìm email
  - tóm tắt email
  # --- Email (English) ---
  - check email
  - read email
  - compose email
  - send email
  - reply email
  - search email
  - email summary
  # --- Sync ---
  - đồng bộ
  - sync
  - google calendar
  - apple notes
  - icloud
mcp_servers:
  - senclaw-space
---

# Space — Notes & Email

The Space feature gives you a personal productivity layer. This skill covers **notes** and **email**. Related skills:
- **calendar** — event management, reminders, conflict detection, schedule organization
- **schedule** — recurring agent tasks with Agent/DAG/Plan modes

**Notes** tools register as `space_<verb>` on the `senclaw-space` MCP server. Call them by the
**canonical bridge name** `mcp__space__<verb>` — the resolver strips the redundant `space_`
prefix once (e.g. `space_note_create` → `mcp__space__note_create`). The bare `space_<verb>(...)`
notation used below maps to the same tool.

**Email** is NOT on the space server — there are no `space_email_*` / `mcp__space__email_*`
tools. Email lives in the Email Space App, MCP server `email-mcp`, full tool names
`mcp__email-mcp__email_*` (see the Email section below).

## Required Tool Discovery

Loading this skill does not create notes or emails. Before calling any Space action, make sure the concrete MCP tool is visible. If it is not visible, call `ToolSearch` first.

Common discovery calls:

```
ToolSearch { query: "select:mcp__space__note_create" }
ToolSearch { query: "select:mcp__space__current_time" }
```

If an exact `select:` query returns no match, search by keywords such as `space note create`, then call the exact tool name returned by `ToolSearch`.

Only tell the user the item was created after the concrete tool call returns a success result.

---

## Notes

> **User defaults**: if the system prompt carries a `## User defaults` block naming a
> different default note store (`wiki_write` / `memory_save`), plain note requests go
> there instead of `space_note_create`.

### Create a note
```
space_note_create(title, body, tags?, folder_id?)
```
- `tags`: array of strings, e.g. `["todo", "meeting"]`
- Use tag `"todo"` for tasks, `"idea"` for ideas, `"meeting"` for meeting notes

### Update a note
```
space_note_update(id, title?, body?, tags?)
```

### Search notes (full-text)
```
space_note_search(query, limit?)
```
Returns notes ranked by FTS5 relevance.

### List notes
```
space_note_list(folder_id?, tag?)
```
Filter by folder or tag; omit both to list all recent notes.

### Delete a note
```
space_note_delete(id)
```

---

## Email

Email is served by the **Email Space App** (MCP server `email-mcp`). Discover the tools first:

```
ToolSearch { query: "select:mcp__email-mcp__email_inbox,mcp__email-mcp__email_compose" }
```

If the exact select misses, search keywords `email compose` and use the returned names.
There is a dedicated `email-reporting` skill installed by the Email App — prefer it for
report/digest flows.

### Inbox
```
mcp__email-mcp__email_inbox(account_id?, folder?, limit?)
```
Returns recent cached messages with subject, sender, and flags. `folder`: `INBOX` (default) or `Sent`.

### Read a message
```
mcp__email-mcp__email_read(message_id)
```
Returns full message with body text.

### Compose and send
```
mcp__email-mcp__email_compose(to?, subject, body, account_id?)
```
- `to` is **optional**: omitted/empty → the mail goes to the account's own address
  (self-report). Accepts several addresses separated by commas, and `"Name <a@b.c>"`.
- **Automated/scheduled runs**: generate subject/body from the task context and send
  immediately — never ask for confirmation and never wait for user input.
- **Interactive sends to a third party**: show the draft and confirm first
  (*"Đây là bản nháp email:"* → wait for "gửi đi" / "send it").
- "Gửi cho tôi" / "send me the report" → just send with `to` omitted, no confirmation.

### Search email
```
mcp__email-mcp__email_search(query, account_id?, limit?)
```

### Email summary
```
mcp__email-mcp__email_summary(message_id)
```
Returns the body plus a summarization instruction (key points, action items, sentiment).

---

## External Sync (Phase 3+)

```
space_sync_google_calendar(token, days?)
space_sync_apple_calendar(token, days?)
space_sync_apple_notes(token)
space_sync_gmail(token, days?)
```
All sync tools require an OAuth2 token. They are stubs — return instructions for the user when tokens are unavailable.

---

## When to Use Space Tools

### Notes
| User says | Tool & parameters |
|-----------|-------------------|
| "ghi chú lại" / "note this" / "#note" | `space_note_create(title, body)` |
| "ghi lại cuộc họp" / "meeting notes" | `space_note_create` with `tags: ["meeting"]` |
| "lưu ý tưởng này" / "save this idea" | `space_note_create` with `tags: ["idea"]` |
| "thêm vào todo" / "add to todo" | `space_note_create` with `tags: ["todo"]` |
| "việc cần làm" / "todo list" / "danh sách công việc" | `space_note_list(tag: "todo")` |
| "xem ghi chú" / "list notes" / "show my notes" | `space_note_list()` |
| "tìm ghi chú về X" / "find note about X" | `space_note_search(query)` |
| "sửa ghi chú" / "update note" | `space_note_update(id, title?, body?, tags?)` |
| "xoá ghi chú" / "delete note" | `space_note_delete(id)` |
| "gắn tag" / "add tag" | `space_note_update(id, tags: [...])` |

### Email (server `email-mcp`, tools `mcp__email-mcp__email_*`)
| User says | Tool & parameters |
|-----------|-------------------|
| "kiểm tra mail" / "check email" / "có mail mới không?" | `email_inbox()` |
| "đọc email từ X" / "read email from X" | `email_inbox()` → find → `email_read(message_id)` |
| "viết email cho X" / "soạn email" / "compose email" | Draft → show → confirm → `email_compose` |
| "trả lời email này" / "reply to this email" | Draft reply → show → confirm → `email_compose` |
| "gửi email cho tôi" / báo cáo tự động / scheduled report | `email_compose(subject, body)` — omit `to` (self), send ngay, không hỏi |
| "gửi email" / "send email" (người nhận khác) | `email_compose(to, subject, body)` — confirm khi chat trực tiếp |
| "tìm email về X" / "search email about X" | `email_search(query)` |
| "tóm tắt email" / "summarize email" | `email_summary(message_id)` |
| "có email quan trọng không?" / "any important emails?" | `email_inbox()` → highlight flagged/urgent |

### Sync
| User says | Tool & parameters |
|-----------|-------------------|
| "đồng bộ Google Calendar" / "sync Google Calendar" | `space_sync_google_calendar` (Phase 3+) |
| "đồng bộ Apple Notes" / "sync iCloud Notes" | `space_sync_apple_notes` (Phase 3+) |
| "đồng bộ Gmail" / "sync Gmail" | `space_sync_gmail` (Phase 3+) |

### Redirect to other skills
| User says | Redirect to |
|-----------|-------------|
| "thêm sự kiện" / "nhắc tôi" / "hôm nay có gì" / "rảnh không" | → **calendar** skill |
| "định kỳ" / "mỗi ngày" / "đặt lịch tự động" / "cron" | → **schedule** skill |
