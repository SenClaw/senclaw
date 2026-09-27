---
name: calendar
description: Calendar event management — create, update, search, delete events with reminders, conflict detection, and schedule organization via Space MCP
version: 1.0.0
when-to-use: When the user wants to create, view, search, update, or delete calendar events. Also use for setting reminders, checking for schedule conflicts, finding free time slots, organizing daily/weekly schedules, or asking what's happening today.
triggers:
  # --- Event CRUD (Vietnamese) ---
  - lịch
  - sự kiện
  - event
  - thêm sự kiện
  - tạo sự kiện
  - xoá sự kiện
  - sửa sự kiện
  - dời sự kiện
  - cập nhật sự kiện
  # --- Reminders (Vietnamese) ---
  - nhắc
  - nhắc nhở
  - nhắc tôi
  - reminder
  - remind
  - báo thức
  - nhắc trước
  - nhắc liên tục
  # --- Schedule queries (Vietnamese) ---
  - hôm nay có gì
  - lịch hôm nay
  - lịch ngày mai
  - lịch tuần này
  - lịch tuần sau
  - tuần này
  - ngày mai
  # --- Time / availability ---
  - rảnh
  - bận
  - có rảnh không
  - am I free
  - free time
  - available
  - busy
  - trùng lịch
  - conflict
  - overlap
  # --- Organization ---
  - sắp xếp lịch
  - organize schedule
  - xem lịch
  - view calendar
  - today
  - schedule
  - what's on
  - what's happening
  # --- Event types ---
  - họp
  - meeting
  - cuộc họp
  - hẹn
  - appointment
  - deadline
  - hạn chót
  - sinh nhật
  - birthday
  - kỷ niệm
  - anniversary
  # --- English CRUD ---
  - add event
  - create event
  - delete event
  - move event
  - reschedule
  - update event
  - set reminder
  - calendar
  - today summary
mcp_servers:
  - senclaw-space
---

# Calendar — Event Management & Notifications

Manage calendar events with automatic notifications, conflict detection, and schedule organization. Tools register as `space_event_*` on the `senclaw-space` MCP server. Call them by the **canonical bridge name** `mcp__space__<verb>` — the resolver strips the redundant `space_` prefix once (e.g. `space_event_create` → `mcp__space__event_create`). The bare `space_<verb>(...)` notation used below maps to the same tool.

## Required Tool Discovery

Before calling any calendar action, make sure the concrete MCP tool is visible. If not, call `ToolSearch` first:

```
ToolSearch { query: "select:mcp__space__event_create" }
ToolSearch { query: "select:mcp__space__event_list" }
ToolSearch { query: "select:mcp__space__current_time" }
```

If an exact `select:` query returns no match, search by keywords such as `space event create`, then call the exact tool name returned.

Only tell the user the event was created after the concrete tool call returns a success result.

---

## Current Time — always call first!

```
space_current_time()
```

Returns current local system time with pre-computed ranges. **Always call this before any time-relative query.**

Response includes:
- `now_ms` — current Unix timestamp (ms)
- `display` — formatted Vietnamese string, e.g. "Thứ ba, 05/05/2026 14:30"
- `timezone` — e.g. "UTC+7"
- `today.start_ms` / `today.end_ms` — start/end of today
- `this_week.start_ms` / `this_week.end_ms` — this week (Sun–Sat)
- `this_month.start_ms` / `this_month.end_ms` — this month
- `tomorrow.start_ms`, `yesterday.start_ms`

```
// Example: list all events today
const t = await space_current_time();
space_event_list({ from: t.today.start_ms, to: t.today.end_ms })

// Example: event at 3pm today
space_event_create({ start_at: t.today.start_ms + 15*3600*1000, ... })
```

---

## Create an event

```
space_event_create(title, start_at, end_at, description?, location?, all_day?, reminder_min?, renotify_min?, color?)
```

| Parameter | Required | Description |
|-----------|----------|-------------|
| `title` | yes | Event name |
| `start_at` | yes | Unix milliseconds — when the event starts |
| `end_at` | yes | Unix milliseconds — when the event ends |
| `description` | no | Detailed notes about the event |
| `location` | no | Where the event takes place |
| `all_day` | no | `true` for full-day events (set `start_at`/`end_at` to day boundaries) |
| `reminder_min` | no | Minutes **before** start to send a reminder notification (e.g. `5`, `15`, `30`, `60`) |
| `renotify_min` | no | Repeat notification every N minutes **while the event is ongoing** |
| `color` | no | Hex color code for event chip in UI |

**Default duration:** if user doesn't specify end time, use 1 hour (60 min).

**IMPORTANT:** Always check for conflicts before creating (see "Conflict detection" below).

---

## Notification System — How Events Notify the User

The `EventNotifier` daemon polls every 60 seconds and fires **three types** of notifications, all pushed via WebSocket (`space:event:reminder`) to the UI:

### 1. Reminder notification (before start)
- **When:** `reminder_min` minutes before `start_at` (i.e. `start_at - reminder_min * 60000 <= now`)
- **Fired exactly once.** After firing, `reminder_sent_at` is set — it won't fire again
- **If not set:** no pre-event reminder (but see "start notification" below)
- **Example:** `reminder_min: 5` → notifies 5 minutes before the event starts

### 2. Start notification (at event time)
- **When:** `start_at <= now` (event has started)
- **ALWAYS fires for every event**, even if `reminder_min` is not set. This is the baseline guarantee: the user always gets notified when an event begins
- **Fired exactly once** (`start_sent_at` tracks this)
- **This means:** an event created without any reminder (e.g. "thêm sự kiện Đi Uniqlo lúc 14h") will still notify at 14h

### 3. Re-notification (during ongoing event)
- **When:** event is `ongoing` AND `renotify_min` is set
- **Repeats** every `renotify_min` minutes until the event ends
- **Use case:** meetings or tasks where periodic pinging prevents the user from forgetting
- **Example:** `renotify_min: 15` → after the event starts, notifies again every 15 minutes

### Notification flow diagram
```
  reminder_min before    start_at         every renotify_min       end_at
       |                    |                    |                    |
       v                    v                    v                    v
  [reminder]            [start]            [re-notify]...         (stops)
  (once, opt.)          (always, once)     (repeating, opt.)
```

### What the UI receives (WebSocket)
Each notification is pushed as:
```json
{
  "type": "space:event:reminder",
  "id": "<notification_id>",
  "eventId": "<event_id>",
  "title": "Họp team",
  "startAt": 1717488000000,
  "kind": "reminder" | "start" | "renotify",
  "firedAt": 1717487700000,
  "delayedMs": 0
}
```
- `kind`: `"reminder"` (before start), `"start"` (at start), `"renotify"` (during ongoing)
- `delayedMs`: non-zero if the daemon was offline when the notification should have fired (the UI can show a "late" badge)

### Event status transitions (automatic)
The notifier also transitions event `status` automatically:
- `upcoming` → `ongoing`: when `start_at <= now < end_at`
- `ongoing` → `done`: when `end_at <= now`

---

## Recommended Reminder Defaults

When the user doesn't specify a reminder preference, apply these defaults:

| Event type | `reminder_min` | `renotify_min` |
|------------|----------------|----------------|
| Meeting / call | `5` | `null` |
| Short task (< 30min) | `5` | `null` |
| Important / exam / flight | `30` | `null` |
| All-day event | `60` (1 hour before) | `null` |
| Long running task (>1h) | `5` | `30` |

| User says | What to set |
|-----------|-------------|
| "nhắc trước 5 phút" | `reminder_min: 5` |
| "nhắc trước nửa tiếng" | `reminder_min: 30` |
| "nhắc tôi liên tục" / "keep reminding" | `reminder_min: 5` + `renotify_min: 10` |
| "không cần nhắc" | omit both (start notification still fires) |

---

## Update an event

```
space_event_update(event_id, title?, description?, start_at?, end_at?, location?, all_day?, color?, reminder_min?, renotify_min?, reset_reminder?)
```
- Only provided fields are changed — omit fields you don't want to modify
- `reset_reminder`: set to `true` to clear `reminder_sent_at`, `renotify_sent_at`, `start_sent_at` — this makes all notifications fire again (useful after changing `start_at` or `reminder_min`)
- **Changing `start_at` automatically resets** the reminder tracking (the reminder will fire relative to the new start time)

---

## Set or Update a Reminder

```
space_set_reminder(event_id, reminder_min)
```
Shortcut to set/update `reminder_min` on an existing event. Also creates a one-time scheduled task as a backup notification.

---

## Search Events

```
space_event_search(query?, date?, from?, to?, limit?)
```

| Parameter | Description |
|-----------|-------------|
| `query` | Keyword search in title, description, location |
| `date` | Natural language: `"today"`, `"tomorrow"`, `"yesterday"`, `"hôm nay"`, `"ngày mai"`, `"next Monday"`, or ISO `"2026-05-10"` |
| `from`/`to` | Unix ms range (overrides `date` if both provided) |
| `limit` | Max results (default 50) |

---

## List Events in a Range

```
space_event_list(from, to)
```
Both in Unix ms. Typically use start-of-day / end-of-day from `space_current_time()`.

---

## Delete an Event

```
space_event_delete(event_id)
```
Soft-deletes the event (sets `deleted_at`) and cancels all pending notifications.

---

## Today's Summary

```
space_today_summary()
```
Returns today's events and recent notes as a compact briefing. Use this to answer "hôm nay có gì" / "what's on my schedule today".

---

## Importing an outside calendar

Two sources import into the same Space calendar. Both return the same shape:

```json
{ "synced": 12, "created": 3, "updated": 9, "errors": [], "needsReauth": false }
```

```
space_sync_google_calendar(token: "<google access token>", days: 30)
space_sync_apple_calendar(username: "<apple id>", token: "<app-specific password>", days: 30)
```

**Rules:**

- **`needsReauth: true` means stop.** The credential is dead; calling again
  burns the account's quota and cannot succeed. Ask the user for a new token
  (Google) or a new app-specific password (iCloud), and say which.
- **Google is incremental after the first run.** `days` only shapes the first
  call; afterwards the stored sync token decides the window, so a small `days`
  does not make a later run cheaper.
- **iCloud needs an app-specific password**, not the account password — the
  server rejects the latter whenever two-factor auth is on, which is always.
  It also needs `username`; a password alone cannot identify whose calendars
  to read.
- **Imported events are read-only here.** A recurring series is stored with
  its original `RRULE` and never expanded into occurrences, so editing one
  locally would not reach the server. Create events people should see
  elsewhere in the source calendar, not in Space.
- **`errors` can be non-empty on a successful sync.** One unreadable event
  does not fail the other forty; report what came back rather than claiming
  everything imported.

---

## Conflict Detection — Checking for Overlapping Events

**The system does NOT automatically prevent overlapping events.** The agent must check manually before creating or moving events.

### How to check for conflicts:
1. Before creating/moving an event, call `space_event_list(from, to)` where `from`=proposed `start_at` and `to`=proposed `end_at`
2. Filter results: any existing event where `existing.start_at < proposed.end_at AND existing.end_at > proposed.start_at` is a conflict
3. Report conflicts to the user and ask how to proceed

### Conflict resolution options:
| Strategy | Action |
|----------|--------|
| **Keep both** | Create the event anyway (both will notify independently) |
| **Move new event** | Adjust `start_at`/`end_at` to fit after the conflicting event |
| **Move existing event** | Update the existing event via `space_event_update` |
| **Replace** | Delete the existing event and create the new one |
| **Shorten** | Adjust `end_at` of the earlier event or `start_at` of the later event |

### Conflict check workflow:
```
// User wants to add "Họp team 14:00-15:00"
1. space_current_time() → get today's start_ms
2. proposed_start = today.start_ms + 14*3600*1000
   proposed_end   = today.start_ms + 15*3600*1000
3. space_event_list({ from: proposed_start, to: proposed_end })
4. If results contain overlapping events:
   → "Bạn đã có sự kiện 'Họp ABC' từ 13:30-14:30, bị trùng 30 phút.
      Bạn muốn: (1) giữ cả hai, (2) dời sự kiện mới, hay (3) dời sự kiện cũ?"
5. Act on user's choice
```

**Always check for conflicts when:**
- Creating a new event
- Moving an event (changing `start_at`/`end_at`)
- User asks "tôi có rảnh lúc X không?" / "am I free at X?"

---

## Schedule Organization

### Viewing and sorting events:
- **Today's schedule:** `space_today_summary()` — quick overview
- **Specific day:** `space_event_search({ date: "2026-06-10" })`
- **This week:** use `space_current_time()` → `space_event_list({ from: this_week.start_ms, to: this_week.end_ms })`
- **Keyword search:** `space_event_search({ query: "họp" })` — find all meetings

### When presenting events to the user:
1. Sort by `start_at` ascending (soonest first)
2. Group by day if spanning multiple days
3. Show time in local format (HH:MM), not Unix ms
4. Indicate status: upcoming / ongoing / done
5. Highlight conflicts (overlapping time ranges)
6. Show reminder status if set

### Scheduling assistance — when user asks "sắp xếp lịch":
1. Call `space_event_list` for the requested time range
2. Identify gaps (free time slots) between events
3. For each gap, compute: `gap_start = previous.end_at`, `gap_end = next.start_at`, `duration = gap_end - gap_start`
4. Present the schedule as a timeline with busy/free blocks
5. If user wants to add something, suggest available slots

### Free/busy check:
```
// "Tôi có rảnh chiều nay không?"
1. space_current_time() → get today ranges
2. afternoon_start = today.start_ms + 13*3600*1000
   afternoon_end   = today.start_ms + 18*3600*1000
3. space_event_list({ from: afternoon_start, to: afternoon_end })
4. If empty → "Chiều nay bạn rảnh hoàn toàn (13:00-18:00)"
   If events → show busy blocks and available gaps
```

---

## Time Parsing Reference

When user gives natural language times, convert to Unix ms:
- "lúc 7h sáng" → today at 07:00 local time
- "lúc 9 giờ tối" → today at 21:00 local time
- "ngày mai lúc 2pm" → tomorrow at 14:00
- "sau 30 phút" → now + 30 * 60 * 1000 ms
- "thứ 2 tuần sau" → next Monday at 09:00 (default start time)

Use `space_current_time()` for "now" reference. Default event duration is 1 hour when end time is not specified.

---

## When to Use Calendar Tools

### Creating events
| User says | Tool & parameters |
|-----------|-------------------|
| "nhắc tôi họp lúc 3pm" / "remind me meeting at 3pm" | `space_event_create` with `reminder_min: 5` |
| "thêm sự kiện lúc 2h chiều" / "add event at 2pm" | `space_event_create` — check conflicts first! |
| "hẹn bác sĩ ngày mai 9h sáng" / "doctor appointment tomorrow 9am" | `space_event_create` with `reminder_min: 30` |
| "họp team từ 14h đến 15h30" / "team meeting 2pm to 3:30pm" | `space_event_create` with explicit `start_at` + `end_at` |
| "sinh nhật An ngày 15/7" / "An's birthday July 15" | `space_event_create` with `all_day: true` |
| "deadline nộp báo cáo thứ 6" / "report deadline Friday" | `space_event_create` with `reminder_min: 60` |

### Reminders
| User says | Tool & parameters |
|-----------|-------------------|
| "nhắc trước 5 phút" / "remind 5min before" | `reminder_min: 5` |
| "nhắc trước nửa tiếng" / "remind 30min before" | `reminder_min: 30` |
| "nhắc trước 1 tiếng" / "remind 1 hour before" | `reminder_min: 60` |
| "nhắc tôi liên tục" / "keep reminding me" | `reminder_min: 5` + `renotify_min: 10` |
| "không cần nhắc" / "no reminder" | Omit `reminder_min` (start notification still fires) |
| "đặt nhắc cho sự kiện X" / "set reminder for event X" | `space_set_reminder(event_id, reminder_min)` |
| "đổi nhắc nhở" / "change reminder" | `space_event_update(event_id, reminder_min: N, reset_reminder: true)` |

### Viewing & searching
| User says | Tool & parameters |
|-----------|-------------------|
| "hôm nay có gì" / "today's schedule" / "what's on today" | `space_today_summary()` |
| "lịch ngày mai" / "tomorrow's events" | `space_event_search({ date: "tomorrow" })` |
| "lịch tuần này" / "this week's schedule" | `space_current_time()` → `space_event_list({ from: this_week.start_ms, to: this_week.end_ms })` |
| "lịch thứ 2 tuần sau" / "next Monday's schedule" | `space_event_search({ date: "next Monday" })` |
| "tìm cuộc họp" / "find meetings" | `space_event_search({ query: "họp" })` |
| "có sự kiện gì ngày 10/6?" / "any events on June 10?" | `space_event_search({ date: "2026-06-10" })` |
| "xem lịch" / "view calendar" / "show events" | `space_event_list` for the relevant range |

### Conflicts & availability
| User says | Tool & parameters |
|-----------|-------------------|
| "tôi có rảnh lúc 3pm không?" / "am I free at 3pm?" | `space_event_list` for 15:00-16:00 → check overlaps |
| "chiều nay rảnh không?" / "free this afternoon?" | `space_event_list` for 13:00-18:00 → show gaps |
| "có bị trùng lịch không?" / "any conflict?" | `space_event_list` for proposed time → overlap check |
| "sắp xếp lịch hôm nay" / "organize today's schedule" | `space_event_list` → show timeline with busy/free blocks |
| "khi nào rảnh trong tuần?" / "when am I free this week?" | `space_event_list` for the week → identify all gaps |

### Updating & deleting
| User says | Tool & parameters |
|-----------|-------------------|
| "dời họp sang 4pm" / "move meeting to 4pm" | `space_event_update(event_id, start_at, end_at)` + conflict check |
| "đổi tên sự kiện" / "rename event" | `space_event_update(event_id, title)` |
| "thêm ghi chú cho sự kiện" / "add note to event" | `space_event_update(event_id, description)` |
| "đổi địa điểm" / "change location" | `space_event_update(event_id, location)` |
| "huỷ sự kiện" / "cancel event" / "xoá sự kiện" | `space_event_delete(event_id)` |
| "kéo dài thêm 30 phút" / "extend by 30min" | `space_event_update(event_id, end_at: original + 30*60000)` |
