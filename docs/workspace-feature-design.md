# Workspace — Tài liệu Thiết kế Tính năng

> **Phiên bản:** 0.1-draft · **Ngày:** 2026-05-04  
> **Tác giả:** SenClaw Core Team  
> **Trạng thái:** RFC (Request for Comments)

---

## 1. Tổng quan

**Workspace** là không gian làm việc cá nhân tích hợp trực tiếp vào thanh điều hướng sidebar của SenClaw Web UI. Nó bổ sung hàng dọc mới bên cạnh nút **Chat**, tập trung phục vụ năng suất hàng ngày của người dùng — ghi chú, lịch trình, nhắc nhở và email — với AI agent sẵn sàng hỗ trợ ở mọi màn hình. Kiến trúc được thiết kế để mở rộng thông qua mô hình **micro-frontend**, cho phép các module bên ngoài (viết bằng NestJS hoặc bất kỳ framework nào) tích hợp vào SenClaw qua một hợp đồng rõ ràng.

### Mục tiêu


| Mục tiêu                | Mô tả                                                                                                      |
| ----------------------- | ---------------------------------------------------------------------------------------------------------- |
| **Năng suất hàng ngày** | Ghi chú nhanh, xem lịch ngày, nhắc nhở công việc, đọc email — ngay trong cùng ứng dụng với AI              |
| **AI-first**            | Agent có thể đọc/ghi note, tạo sự kiện, gửi nhắc nhở qua MCP tool — không cần người dùng thao tác thủ công |
| **Khả năng mở rộng**    | Micro-frontend API cho phép plugin bên thứ ba (NestJS app, React module, iframe) nhúng vào tab Workspace   |
| **Offline capable**     | Note và calendar cơ bản hoạt động offline, đồng bộ khi có kết nối                                          |


---

## 2. Vị trí trong UI hiện tại

### 2.1 Sidebar hiện tại (`Sidebar.tsx`)

Sidebar có 3 zone:

- **TOP**: Logo + status dot
- **TOP MENU**: Tab ngang `Chat | Cowork | Code`
- **MIDDLE**: Nội dung động (inject từ page)
- **BOTTOM MENU**: Icon `Wiki | Plugins | Settings | Toggle Theme`

### 2.2 Vị trí thêm `Workspace`

Thêm nút **Workspace** vào **TOP MENU** — hàng tab ngang, đứng sau `Code`:

```
[ Chat ] [ Workspace ] [ Cowork ]  [ Code ]
```

Route mới: `/workspace` và các sub-route `/workspace/notes`, `/workspace/calendar`, `/workspace/email`, `/workspace/apps/:moduleId`.

### 2.3 Layout màn hình Workspace

```
┌─────────────────────────────────────────────────────────────────┐
│ SIDEBAR (300px)              │ WORKSPACE CONTENT                │
│ ┌──────────────────────────┐ │ ┌──────────┬────────────────────┐│
│ │ Logo · SenClaw  ●        │ │ │ SUB-NAV  │                    ││
│ ├──────────────────────────┤ │ │          │   MODULE CONTENT    ││
│ │ [Chat][Cowork][Code][WS] │ │ │ 📝 Notes │                    ││
│ ├──────────────────────────┤ │ │ 📅 Cal   │                    ││
│ │                          │ │ │ 📧 Email │                    ││
│ │  Workspace Sub-Sidebar   │ │ │ 🔌 Apps  │                    ││
│ │  (quick notes list /     │ │ │          │                    ││
│ │   today events)          │ │ └──────────┴────────────────────┘│
│ │                          │ │                                  │
│ ├──────────────────────────┤ │                                  │
│ │ Wiki │ Plugins │ Settings│ │                                  │
│ └──────────────────────────┘ │                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## 3. Modules chức năng

### 3.1 Notes — Ghi chú

**Mô tả:** Editor rich-text nhẹ (Markdown + code blocks), tổ chức theo tag và folder, tìm kiếm full-text.

**Tính năng cốt lõi:**

- Tạo / sửa / xoá note với editor Markdown (dùng `@uiw/react-md-editor` hoặc `tiptap`)
- Phân loại bằng **tags** và **folders** (lưu DB)
- Tìm kiếm FTS5 (tái dùng infrastructure memory đã có trong SenClaw)
- Pin note lên đầu
- AI agent có thể đọc/ghi/tìm note qua MCP tool `workspace:notes`
- **Quick capture**: phím tắt toàn cục `Cmd+Shift+N` mở popup ghi note nhanh

**Dữ liệu:**

```sql
CREATE TABLE ws_notes (
    id          TEXT PRIMARY KEY,          -- uuid v7
    title       TEXT NOT NULL DEFAULT '',
    body        TEXT NOT NULL DEFAULT '',  -- Markdown raw
    body_html   TEXT,                      -- rendered cache
    tags        TEXT NOT NULL DEFAULT '[]',-- JSON array of strings
    folder_id   TEXT,                      -- FK ws_note_folders.id nullable
    pinned      INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,          -- unix ms
    updated_at  INTEGER NOT NULL,
    deleted_at  INTEGER                    -- soft delete
);

CREATE TABLE ws_note_folders (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    parent_id   TEXT,                      -- self-referencing tree
    created_at  INTEGER NOT NULL
);

CREATE VIRTUAL TABLE ws_notes_fts USING fts5(
    id UNINDEXED, title, body,
    content=ws_notes, content_rowid=rowid
);
```

**API REST** (prefix `/api/workspace/notes`):


| Method | Path         | Mô tả                                        |
| ------ | ------------ | -------------------------------------------- |
| GET    | `/`          | Danh sách note (filter: tag, folder, search) |
| POST   | `/`          | Tạo note mới                                 |
| GET    | `/:id`       | Lấy note theo id                             |
| PUT    | `/:id`       | Cập nhật                                     |
| DELETE | `/:id`       | Soft delete                                  |
| GET    | `/search?q=` | FTS search                                   |
| GET    | `/folders`   | Danh sách folder                             |
| POST   | `/folders`   | Tạo folder                                   |


**MCP Tool** (thêm vào `workspace_server.rs`):

```
workspace:note_create   { title, body, tags?, folder_id? }
workspace:note_update   { id, title?, body?, tags? }
workspace:note_search   { query, limit? }
workspace:note_list     { folder_id?, tag? }
```

---

### 3.2 Calendar & Timeline — Lịch trình & Nhắc nhở

**Mô tả:** Lịch cá nhân theo ngày/tuần/tháng, nhắc nhở có tích hợp với TaskScheduler của SenClaw, hiển thị timeline công việc trong ngày.

**Tính năng cốt lõi:**

- View: **Day** (timeline giờ), **Week**, **Month**
- Tạo sự kiện: tiêu đề, thời gian bắt đầu/kết thúc, lặp lại (daily/weekly/monthly), nhắc nhở (X phút trước)
- **Tích hợp Scheduler**: khi tạo nhắc nhở → sinh `scheduled_task` loại `notify` gửi push qua WebSocket + Telegram
- **Tích hợp `cowork_tasks`**: task từ Cowork hiển thị trên calendar nếu có deadline
- Import/Export iCal (`.ics`)
- AI agent có thể đọc lịch, tạo sự kiện, set reminder qua MCP

**Dữ liệu:**

```sql
CREATE TABLE ws_events (
    id              TEXT PRIMARY KEY,
    title           TEXT NOT NULL,
    description     TEXT,
    start_at        INTEGER NOT NULL,   -- unix ms
    end_at          INTEGER NOT NULL,
    all_day         INTEGER DEFAULT 0,
    location        TEXT,
    color           TEXT,               -- hex color for UI
    recurrence      TEXT,               -- JSON: { freq, interval, until, byday }
    reminder_min    INTEGER,            -- minutes before start, nullable
    task_id         TEXT,               -- FK scheduled_tasks.id nullable
    source          TEXT DEFAULT 'manual', -- 'manual'|'cowork'|'ical'|'agent'
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    deleted_at      INTEGER
);
```

**Tích hợp Scheduler:** Khi `reminder_min IS NOT NULL`, backend tự động tạo/cập nhật một `scheduled_task` loại `notify`:

- `run_at = start_at - reminder_min * 60 * 1000`
- `context_mode = "notify"` → gửi push qua WS event `workspace:reminder`
- Xoá sự kiện → xoá scheduled_task tương ứng

**API REST** (prefix `/api/workspace/calendar`):


| Method | Path                | Mô tả                               |
| ------ | ------------------- | ----------------------------------- |
| GET    | `/events?from=&to=` | Lấy sự kiện trong khoảng thời gian  |
| POST   | `/events`           | Tạo sự kiện                         |
| PUT    | `/events/:id`       | Cập nhật                            |
| DELETE | `/events/:id`       | Xoá (+ huỷ reminder task)           |
| GET    | `/today`            | Tóm tắt hôm nay: events + reminders |
| POST   | `/import`           | Import iCal                         |
| GET    | `/export`           | Export iCal                         |


**WebSocket Events:**

```
Server → Client:
  workspace:reminder    { event_id, title, start_at, minutes_left }
  workspace:event_updated { event }
  workspace:event_deleted { event_id }
```

**MCP Tool:**

```
workspace:event_create  { title, start_at, end_at, description?, reminder_min? }
workspace:event_list    { from, to }
workspace:today_summary {}   → trả về danh sách event + tasks hôm nay
```

---

### 3.3 Email

**Mô tả:** Client email nhẹ tích hợp với IMAP/SMTP, cho phép đọc inbox và soạn thảo email, với AI hỗ trợ viết/tóm tắt/phân loại.

**Phạm vi Phase 1 (MVP):**

- Cấu hình tài khoản IMAP/SMTP (lưu encrypted)
- Xem inbox, đọc email, soạn thảo, reply, forward
- Tìm kiếm email
- AI: tóm tắt thread dài, soạn thảo nháp từ prompt

**Phạm vi Phase 2:**

- Nhiều tài khoản email
- Labels/folders
- Lọc thông minh (AI phân loại: quan trọng / newsletter / giao dịch)
- Rule tự động (forward email → agent action)

**Dữ liệu:**

```sql
CREATE TABLE ws_email_accounts (
    id          TEXT PRIMARY KEY,
    label       TEXT NOT NULL,
    email       TEXT NOT NULL,
    imap_host   TEXT NOT NULL,
    imap_port   INTEGER NOT NULL DEFAULT 993,
    smtp_host   TEXT NOT NULL,
    smtp_port   INTEGER NOT NULL DEFAULT 587,
    username    TEXT NOT NULL,
    password    TEXT NOT NULL,           -- AES-GCM encrypted, key từ SENCLAW_SECRET
    use_tls     INTEGER DEFAULT 1,
    created_at  INTEGER NOT NULL
);

CREATE TABLE ws_email_cache (
    id          TEXT PRIMARY KEY,        -- message-id header
    account_id  TEXT NOT NULL,
    folder      TEXT NOT NULL DEFAULT 'INBOX',
    subject     TEXT,
    from_addr   TEXT,
    to_addrs    TEXT,                    -- JSON array
    date        INTEGER,
    body_text   TEXT,
    body_html   TEXT,
    flags       TEXT DEFAULT '[]',      -- JSON: ['\\Seen','\\Flagged',...]
    synced_at   INTEGER NOT NULL
);
```

**API REST** (prefix `/api/workspace/email`):


| Method | Path                            | Mô tả                 |
| ------ | ------------------------------- | --------------------- |
| GET    | `/accounts`                     | Danh sách tài khoản   |
| POST   | `/accounts`                     | Thêm tài khoản        |
| DELETE | `/accounts/:id`                 | Xoá tài khoản         |
| GET    | `/accounts/:id/messages`        | Inbox (paginated)     |
| GET    | `/accounts/:id/messages/:msgId` | Đọc email             |
| POST   | `/accounts/:id/send`            | Gửi email             |
| POST   | `/accounts/:id/sync`            | Trigger sync thủ công |
| GET    | `/accounts/:id/search?q=`       | Tìm kiếm              |


**MCP Tool:**

```
workspace:email_inbox   { account_id?, limit? }
workspace:email_read    { message_id }
workspace:email_compose { to, subject, body }
workspace:email_search  { query, account_id? }
workspace:email_summary { message_id }  → AI tóm tắt thread
```

---

## 4. Kiến trúc Micro-Frontend

### 4.1 Vấn đề cần giải quyết

Workspace sẽ phát triển với nhiều module khác nhau (Finance, CRM, Analytics, v.v.) — không thể tất cả đóng gói vào monolith. Cần một **plugin system** cho phép:

- Module bên ngoài viết bằng bất kỳ framework nào (React, Vue, Angular, plain JS)
- NestJS microservice cung cấp API và UI module cùng lúc
- Tích hợp không cần rebuild SenClaw core

### 4.2 Mô hình tích hợp — 3 cấp độ

```
┌─────────────────────────────────────────────────────────────────┐
│                     SENCLAW WORKSPACE                           │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │  Module Registry (JSON manifest từ /api/workspace/apps)  │   │
│  └──────────────────────────────────────────────────────────┘   │
│         │                │                    │                  │
│   Level 1: iframe   Level 2: Web Component  Level 3: ESM Remote │
│   ─────────────────  ─────────────────────  ──────────────────  │
│   Sandbox tốt nhất   Custom Element,        Module Federation    │
│   DX đơn giản nhất   postMessage API        (Vite / Webpack 5)  │
│   Phù hợp NestJS     Phù hợp Vue/React      Best performance    │
│   + serve static     (same domain)          (same domain, CSR)  │
└─────────────────────────────────────────────────────────────────┘
```

### 4.3 App Manifest

Mỗi micro-frontend đăng ký bằng một **manifest JSON**:

```json
{
  "id": "finance-tracker",
  "name": "Finance Tracker",
  "version": "1.2.0",
  "icon": "💰",
  "description": "Theo dõi thu chi cá nhân",
  "integration": {
    "type": "iframe",
    "url": "http://localhost:3100/workspace-embed",
    "sandbox": "allow-scripts allow-same-origin allow-forms"
  },
  "mcp_tools": [
    {
      "name": "finance:add_transaction",
      "description": "Thêm giao dịch tài chính",
      "input_schema": { "type": "object", "properties": { "amount": { "type": "number" }, "category": { "type": "string" } } }
    }
  ],
  "permissions": ["workspace:read", "workspace:write"],
  "senclaw_events": ["workspace:reminder", "agent:reply"]
}
```

**API REST** (prefix `/api/workspace/apps`):


| Method | Path            | Mô tả                       |
| ------ | --------------- | --------------------------- |
| GET    | `/`             | Danh sách app đã cài        |
| POST   | `/register`     | Đăng ký app từ manifest URL |
| DELETE | `/:id`          | Gỡ app                      |
| GET    | `/:id/manifest` | Lấy manifest hiện tại       |


**DB:**

```sql
CREATE TABLE ws_apps (
    id          TEXT PRIMARY KEY,
    manifest    TEXT NOT NULL,           -- JSON manifest
    enabled     INTEGER DEFAULT 1,
    installed_at INTEGER NOT NULL,
    last_seen_at INTEGER
);
```

### 4.4 Bridge Protocol — SenClaw ↔ Micro-Frontend

Với `type: "iframe"`, hai bên giao tiếp qua `window.postMessage`:

```typescript
// Giao thức từ SenClaw → iframe
type SencClawToApp =
  | { type: 'senclaw:init'; payload: { token: string; theme: 'dark' | 'light'; wsUrl: string } }
  | { type: 'senclaw:theme'; payload: { theme: 'dark' | 'light' } }
  | { type: 'senclaw:event'; payload: { event: string; data: unknown } }  // forward WS events
  | { type: 'senclaw:mcp:response'; payload: { callId: string; result: unknown } }

// Giao thức từ iframe → SenClaw  
type AppToSenclaw =
  | { type: 'app:ready' }
  | { type: 'app:resize'; payload: { height: number } }
  | { type: 'app:navigate'; payload: { path: string } }   // deep link trong WS
  | { type: 'app:mcp:call'; payload: { callId: string; tool: string; args: unknown } }
  | { type: 'app:notify'; payload: { title: string; body: string; icon?: string } }
```

### 4.5 NestJS Micro-Frontend — Template tích hợp

Một NestJS app muốn tích hợp với SenClaw cần:

**1. Serve manifest endpoint:**

```typescript
// GET /senclaw-manifest.json
@Get('senclaw-manifest.json')
manifest() {
  return {
    id: 'my-nestjs-app',
    name: 'My App',
    integration: { type: 'iframe', url: `${this.config.publicUrl}/embed` },
    mcp_tools: [...],
  };
}
```

**2. Serve embed route (React/plain HTML):**

```typescript
// GET /embed → trả về HTML page nhúng được trong iframe
// Nhận senclaw:init message, lấy token + wsUrl để kết nối WS SenClaw nếu cần
```

**3. Gọi SenClaw API từ backend NestJS** (qua HTTP internal hoặc token):

```typescript
// NestJS có thể gọi SenClaw REST API với token
// hoặc mở WebSocket WS kết nối như một "service client"
const response = await fetch(`${senclaw_url}/api/workspace/notes`, {
  headers: { Authorization: `Bearer ${ws_token}` }
});
```

**4. Expose MCP tools để SenClaw agent dùng:**

- Khai báo trong manifest → SenClaw sẽ proxy call MCP đến NestJS endpoint
- NestJS implement endpoint `POST /mcp/call` nhận `{ tool, args }` và trả về kết quả

### 4.6 Sơ đồ luồng đầy đủ

```
User → SenClaw UI (React)
         │
         ├── [Built-in modules] Notes/Calendar/Email
         │         │
         │         ▼
         │    Axum REST /api/workspace/* (Rust)
         │         │
         │         ▼
         │    SQLite (ws_notes, ws_events, ws_email_*)
         │
         └── [Micro-Frontend Apps]
                   │
                   ├── iframe embed (NestJS static serve)
                   │         │
                   │         ▼ postMessage bridge
                   │    NestJS Backend ──► SenClaw REST API
                   │
                   └── MCP Tool Proxy
                             │
                             ▼
                        SenClaw Agent Pool
                             │
                        NestJS /mcp/call endpoint
```

---

## 5. Tích hợp với AI Agent

### 5.1 MCP Server mới: `workspace_daily_server`

Thêm vào `src/mcp/workspace_daily_server.rs`, expose tất cả tool của Notes + Calendar + Email:

```
Tool namespace: workspace:*
  workspace:notes_create
  workspace:notes_search
  workspace:notes_list
  workspace:event_create
  workspace:event_list
  workspace:today_summary      ← Agent dùng khi user hỏi "hôm nay tôi có gì?"
  workspace:email_inbox
  workspace:email_compose
  workspace:email_summary
```

### 5.2 Context injection vào daily session

Khi agent bắt đầu session mới (hoặc nhận lệnh), tự động inject context từ `today_summary`:

- Số note chưa xem
- Các sự kiện trong 2 giờ tới
- Email chưa đọc (nếu cấu hình)

Trigger: WS event `workspace:morning_brief` lúc 7:00 sáng (scheduled task hằng ngày).

---

## 6. Database Migration

Thêm vào `src/db/schema.rs` function `apply_workspace_tables(conn)`:

```rust
pub fn apply_workspace_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS ws_notes ( ... );
        CREATE TABLE IF NOT EXISTS ws_note_folders ( ... );
        CREATE VIRTUAL TABLE IF NOT EXISTS ws_notes_fts USING fts5( ... );
        CREATE TABLE IF NOT EXISTS ws_events ( ... );
        CREATE TABLE IF NOT EXISTS ws_email_accounts ( ... );
        CREATE TABLE IF NOT EXISTS ws_email_cache ( ... );
        CREATE TABLE IF NOT EXISTS ws_apps ( ... );
    ")?;
    Ok(())
}
```

Gọi trong `Db::open()` sau `apply_memory_tables`.

---

## 7. Kế hoạch triển khai (Roadmap)

### Phase 0 — Foundation (2 tuần)

- DB schema: tạo `apply_workspace_tables`
- Sidebar UI: thêm button Workspace + route `/workspace`
- `WorkspacePage` skeleton với sub-nav (Notes / Calendar / Email / Apps)
- API skeleton: các endpoint trả 501 Not Implemented

### Phase 1 — Notes MVP (2 tuần)

- Backend: `src/gateway/ui_server/workspace_notes.rs` — CRUD + FTS search
- Frontend: `WorkspaceNotesPage` — danh sách, editor Markdown, tags, folders
- MCP tool: `workspace:note_*` trong `workspace_daily_server.rs`
- Quick capture popup

### Phase 2 — Calendar MVP (3 tuần)

- Backend: `src/gateway/ui_server/workspace_calendar.rs` — CRUD events
- Tích hợp Scheduler: auto-create reminder tasks
- Frontend: day/week/month views (dùng `@fullcalendar/react` hoặc tự build)
- WS events: `workspace:reminder`
- MCP tool: `workspace:event_*`, `workspace:today_summary`
- Morning brief scheduled task

### Phase 3 — Email MVP (3 tuần)

- Backend: IMAP/SMTP client (crate `imap` + `lettre`)
- Sync service: background worker fetch inbox
- Frontend: inbox list, thread view, compose
- MCP tool: `workspace:email_*`
- Encryption: password AES-GCM với `SENCLAW_SECRET`

### Phase 4 — Micro-Frontend Platform (4 tuần)

- App registry API: `/api/workspace/apps`
- iframe host component: `WorkspaceAppFrame.tsx`
- postMessage bridge implementation
- MCP proxy: forward tool call từ manifest đến NestJS endpoint
- NestJS integration template + README
- Developer docs

### Phase 5 — Polish & Integration (2 tuần)

- Offline support (IndexedDB cache cho Notes + Calendar)
- iCal import/export
- Keyboard shortcuts
- Mobile-responsive layout
- E2E tests (Playwright)

---

## 8. Cấu trúc file sẽ thêm mới

### Rust backend (`src/`)

```
src/
├── gateway/ui_server/
│   ├── workspace_notes.rs       -- Note CRUD + FTS
│   ├── workspace_calendar.rs    -- Event CRUD + scheduler integration
│   ├── workspace_email.rs       -- IMAP/SMTP + email cache
│   └── workspace_apps.rs        -- Micro-frontend registry
├── mcp/
│   └── workspace_daily_server.rs -- MCP tools cho Workspace
└── db/
    └── workspace.rs              -- DB helpers cho workspace tables
```

### Web frontend (`web/src/`)

```
web/src/
├── pages/
│   └── WorkspacePage.tsx
├── components/workspace/
│   ├── WorkspaceSidebar.tsx      -- Sub-nav + quick preview
│   ├── WorkspaceLayout.tsx       -- Layout wrapper
│   ├── notes/
│   │   ├── NotesList.tsx
│   │   ├── NoteEditor.tsx
│   │   ├── NoteItem.tsx
│   │   └── QuickCapture.tsx
│   ├── calendar/
│   │   ├── CalendarView.tsx
│   │   ├── DayTimeline.tsx
│   │   ├── EventModal.tsx
│   │   └── ReminderBadge.tsx
│   ├── email/
│   │   ├── InboxList.tsx
│   │   ├── ThreadView.tsx
│   │   ├── ComposeModal.tsx
│   │   └── AccountSetup.tsx
│   └── apps/
│       ├── AppGallery.tsx
│       ├── AppFrame.tsx          -- iframe host + postMessage bridge
│       └── AppManifestCard.tsx
└── hooks/
    ├── useNotes.ts
    ├── useCalendar.ts
    ├── useEmail.ts
    └── useWorkspaceApps.ts
```

---

## 9. Cân nhắc kỹ thuật

### Security


| Rủi ro                | Biện pháp                                                                              |
| --------------------- | -------------------------------------------------------------------------------------- |
| Email password lưu DB | AES-GCM encrypt với key `SENCLAW_SECRET` (env var); không bao giờ log                  |
| iframe XSS            | `sandbox` attribute giới hạn quyền; CSP header; chỉ `postMessage` với origin whitelist |
| MCP proxy injection   | Validate manifest `mcp_tools` schema trước khi proxy; rate limit                       |
| IMAP credentials      | Không cache raw credential trong memory sau login                                      |


### Performance


| Vấn đề                              | Giải pháp                                                       |
| ----------------------------------- | --------------------------------------------------------------- |
| FTS notes search                    | FTS5 trigger maintain `ws_notes_fts` tự động                    |
| Email sync lag                      | Background worker với 5 phút interval; manual refresh available |
| Calendar với nhiều recurring events | Expand recurring events lazily chỉ trong khoảng query (from/to) |
| Micro-frontend load                 | Lazy load iframe chỉ khi user navigate vào app tab              |


### Offline

- **Notes**: Draft lưu `localStorage` → sync khi online
- **Calendar**: Cache 30 ngày tới trong `localStorage` (JSON)  
- **Email**: Cache 50 email gần nhất trong `localStorage`
- **Apps**: Hiện thông báo "requires internet connection" nếu iframe fail load

---

## 10. Tích hợp với tính năng hiện có


| Tính năng SenClaw    | Tích hợp Workspace                                               |
| --------------------- | ---------------------------------------------------------------- |
| **TaskScheduler**     | Calendar reminder → `scheduled_task` loại `notify`               |
| **Cowork tasks**      | Task có deadline → hiện trên Calendar view                       |
| **Wiki**              | Note có thể "publish to Wiki" → tạo wiki entry                   |
| **Agent Chat**        | AI đọc/ghi note, tạo event từ chat (`@workspace create note...`) |
| **MCP tools**         | Agent gọi `workspace:today_summary` để biết context hàng ngày    |
| **WebSocket Gateway** | Push reminder event `workspace:reminder` real-time               |
| **Telegram channel**  | Reminder gửi Telegram notification qua channel adapter           |


---

## 11. Câu hỏi mở (Open Questions)

1. **Email encryption key management**: Dùng per-user key hay server-wide `SENCLAW_SECRET`? Cần cân nhắc khi có multi-user.
2. **Micro-frontend auth**: Iframe nhận WS token từ `senclaw:init` — token này có expiry không? Cần refresh mechanism.
3. **Calendar sync với external**: Có cần 2-way sync với Google Calendar / Apple Calendar không? Nếu có → Phase 6.
4. **Note collaboration**: Note hiện tại per-user local. Có cần share note trong Cowork workspace không?
5. **Mobile app**: Workspace có cần Progressive Web App manifest để dùng trên mobile?

---

*Tài liệu này là living document — cập nhật khi có quyết định design mới.*