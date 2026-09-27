# Hướng dẫn sử dụng Kanban

**Kanban** là bảng công việc lõi của SenClaw (mục **Kanban** trên sidebar, có cả
desktop app lẫn Web UI) với một điểm khác bảng kanban thường: **agent tự nhặt và
làm các card trong cột Ready** khi bạn bật dispatcher. Bạn xếp việc — AI làm —
kết quả quay lại board.

Dữ liệu nằm trong SQLite riêng `~/.senclaw/space-apps/kanban/kanban.db`,
độc lập với DB chính của daemon.

## 1. Khái niệm

**Board → Columns → Cards.** Mỗi board có thể gắn một **Workspace folder** —
thư mục làm việc của agent, file output sẽ rơi vào đây.

**6 cột mặc định** (workflow "Hermes" — template Standard):

| Cột | Vai trò (role) | Ý nghĩa |
|---|---|---|
| **Triage** 🟣 | `triage` | Chỗ ý tưởng chờ người duyệt. **Không bao giờ tự động chạy** — an toàn để chứa việc chưa chín. |
| **Todo** ⚪ | `todo` | Hàng chờ. Card hết phụ thuộc sẽ **tự trôi sang Ready** khi dispatcher bật. |
| **Ready** 🔵 | `ready` | **Agent nhặt việc từ đây.** Đưa card vào = giao việc cho AI. |
| **In Progress** 🔵 | `in_progress` | Agent đang chạy. |
| **Blocked** 🔴 | `blocked` | Cần người xử lý — agent bế tắc hoặc việc cần review. |
| **Done** 🟢 | `done` | Kéo card vào đây là tự đánh dấu hoàn thành (kéo ra là bỏ dấu). |

**Card** có: mô tả, **priority** (`low/medium/high/urgent` — quyết định thứ tự
agent nhặt), **assignee** (tên worker persona — tạo worker lanes), **labels**,
**tenant**, **dependencies** (card cha chưa xong → card con bị khoá 🔒, không
được nhặt), **subtasks** (hiện tiến độ `x/y`), và **comment thread** — nơi agent
báo cáo kết quả.

## 2. Tạo board

### New board

Bấm **New board** → nhập Title, chọn **Columns template**, tuỳ chọn **Workspace
folder** (nút Browse…). Ba template có sẵn:

| Template | Cột | Dùng khi |
|---|---|---|
| **Standard (Hermes)** | 6 cột trên | Mặc định — đầy đủ tự động hoá |
| **Advanced (review + WIP)** | 8 cột: thêm Backlog, Review; WIP limit trên Ready/In Progress/Review | Muốn có chặng review + cảnh báo quá tải |
| **Simple (classic)** | To Do → In Progress → Done | Bảng tay thuần — **không có cột Ready ⇒ agent không bao giờ chạy board này** |

### AI board

Bấm **AI board** → nhập **Goal** (ví dụ *"Plan a customer workshop in 6 weeks"*,
viết tiếng Việt thì board sinh ra cũng tiếng Việt) → **Generate**:

- Để mặc định "✨ AI generates columns" → AI thiết kế cả cột lẫn **8–16 card**
  (phần lớn ở Todo/Triage, các cột đang chạy để trống).
- Chọn một template → cột lấy từ template, AI chỉ sinh card và bỏ vào Todo.

Gọi **LLM đang active** của SenClaw (phải cấu hình model trước — Settings → LLM).
Chạy đồng bộ: dialog chờ đến khi xong rồi mở board; lỗi hiện "AI failed: …".

### AI Task (chỉ desktop, trong board đang mở)

Nút **AI Task** nhận một yêu cầu và tự break down thành các task đưa vào board
hiện tại. ⚠️ Lưu ý hiện tại nó chạy nhánh "AI sinh cột + card" — **cột mới sẽ
được thêm vào cuối board**; dùng lặp lại nhiều lần sẽ nhân bản cột.

## 3. Agent tự chạy card — cột Ready

### Bật/tắt

**Settings → Autonomous tasks → "Auto-run Kanban tasks (dispatcher)"** —
mặc định **TẮT** (*"Agents act unattended — leave OFF unless you want that"*).
Bật/tắt có hiệu lực ngay tick kế tiếp, áp dụng cho **mọi board cùng lúc**
(board không có đủ cặp cột Ready + In Progress thì được bỏ qua).

### Chu trình (mỗi 30 giây)

1. **Todo → Ready**: card ở Todo hết phụ thuộc tự thăng xuống cuối Ready.
   Triage đứng yên — muốn giữ việc lại cho người duyệt, để ở Triage.
2. **Nhặt việc**: card Ready không còn phụ thuộc mở, ưu tiên
   `urgent > high > medium > còn lại` rồi theo thứ tự trong cột. Giới hạn mặc
   định: **3 worker toàn hệ thống**, **1 việc/assignee** (card không gán
   assignee chỉ chịu giới hạn tổng).
3. **Chạy**: card chuyển sang **In Progress**; agent nhận prompt = tiêu đề +
   mô tả card, làm việc trong **Workspace folder** của board (không có thì dùng
   thư mục scratch riêng của card); card có assignee thì chạy bằng persona đó.
   Agent được yêu cầu xem chi tiết card trước và **bắt buộc chốt** bằng
   `kanban_complete` hoặc `kanban_block`; thay đổi code thì phải block với lý do
   `review-required: …` để người duyệt.

### Kết cục của một run

| Chuyện gì xảy ra | Card đi đâu | Ghi lại |
|---|---|---|
| Agent hoàn thành (`kanban_complete`) | **Done** | Comment `complete` chứa summary |
| Agent bế tắc (`kanban_block`) | **Blocked** | Comment `block` chứa lý do |
| Agent xong việc mà quên chốt | **Done** | Comment "auto-closed: agent returned without calling kanban_complete" |
| Agent lỗi / quá **10 phút** | **Blocked** | Comment "gave_up: …" — nằm đó tới khi bạn Unblock |
| Worker chết giữa chừng | về **Ready** sau 15 phút (hết lease) | Comment "stale: worker lease expired…" — sẽ được nhặt lại |

**Kết quả nằm trong comment thread của card** (mở card để đọc), file sinh ra nằm
trong Workspace folder. Không có notification hệ thống khi card xong — theo dõi
qua **Activity drawer** (⚡): mục *Running now* + *Recent worker feed* (30 dòng
gần nhất).

**Không có nút huỷ run đang chạy.** Tắt toggle chỉ chặn tick sau; run hiện tại
chạy đến khi xong hoặc hết 10 phút. Muốn chặn trước, kéo card khỏi Ready trước
khi bị nhặt.

### Tinh chỉnh qua biến môi trường

`SENCLAW_DISPATCH_ENABLED` (mặc định false) · `SENCLAW_DISPATCH_INTERVAL_SECS`
(30, sàn 5) · `SENCLAW_DISPATCH_MAX_CONCURRENT` (3) ·
`SENCLAW_DISPATCH_PER_ASSIGNEE` (1) · `SENCLAW_DISPATCH_MAX_TURNS` (40) ·
`SENCLAW_DISPATCH_TIMEOUT_SECS` (600).

## 4. Thao tác trên board

- **Kéo thả** card giữa các cột. Kéo vào Done = hoàn thành; kéo ra = mở lại.
- **Add card** cuối mỗi cột (gõ tiêu đề, Enter). **Add column** chọn Title +
  Type (role). **Xoá cột**: menu ⋯ trên cột — ⚠️ desktop **không hỏi xác nhận**
  và xoá luôn card bên trong. **Xoá board**: icon thùng rác ở danh sách board
  (có xác nhận; xoá sạch không hoàn tác).
- **Click card** mở chi tiết: nút **Complete** (hỏi summary) / **Block** (hỏi
  lý do) / **Unblock**; đổi Column, Priority, **Assignee** (chọn worker profile
  từ danh sách persona), Labels; **Break down (AI)** — AI chẻ card thành 4–8
  subtask cùng cột; **Comments** (ghi chú của bạn lưu tên "Bạn"); danh sách
  Dependencies (chỉ đọc trên UI — thêm/xoá bằng MCP/REST).
- **Worker lanes**: gom card theo assignee trong từng cột — desktop bật sẵn
  công tắc, web có nút toggle. Kèm dropdown lọc theo worker.
- **WIP limit** (template Advanced): chỉ **cảnh báo đỏ** khi vượt, không chặn thả.
- Khác biệt hai UI: **Rename board** chỉ có trên web (icon bút chì);
  **AI Task** chỉ có trên desktop. `due_date` có trong dữ liệu/API nhưng chưa
  có UI hiển thị/sửa.

Board tự cập nhật realtime qua WebSocket (daemon phát `kanban:update`, độ trễ
tối đa ~2 giây).

## 5. Templates cột (Plugins → Kanban)

Trang **Plugins → Kanban** quản lý **column template** tái sử dụng khi tạo board:

- **New template** tạo custom; **Import** dán JSON; mỗi template **Export**
  (copy JSON) để chia sẻ.
- Template builtin (Standard/Advanced/Simple) không sửa/xoá được — dùng
  **Duplicate as custom** rồi sửa bản sao.

## 6. Agent trong chat dùng Kanban — MCP tools

MCP server: **`senclaw-kanban`** (stdio, lệnh `senclaw kanban-server`), tool đầy
đủ dạng `mcp__senclaw-kanban__<tool>`.

> ⚠️ Server này **không nằm trong bộ MCP mặc định của agent chat** — nó chỉ được
> tự động cấp cho worker của dispatcher. Muốn agent trong chat thao tác board,
> đăng ký thêm MCP server trỏ tới `senclaw kanban-server` (hoặc dùng REST).

| Tool | Tác dụng |
|---|---|
| `kanban_list_boards` / `kanban_get_board` | Liệt kê board / đọc nguyên board (cột + card) |
| `kanban_create_board` / `kanban_delete_board` | Tạo (mặc định seed 6 cột) / xoá board |
| `kanban_add_column` / `kanban_update_column` / `kanban_delete_column` | Quản lý cột (title, role, màu, WIP) |
| `kanban_create` / `kanban_show` / `kanban_list` | Tạo card / xem chi tiết (kèm comments + links) / liệt kê có lọc |
| `kanban_update_card` / `kanban_move_card` / `kanban_delete_card` | Sửa / chuyển cột / xoá card |
| `kanban_complete` / `kanban_block` / `kanban_unblock` | Chốt việc: Done + summary / Blocked + lý do / trả về Ready |
| `kanban_comment` | Ghi chú bền vững vào thread (kênh agent ↔ người) |
| `kanban_link` | Thêm/xoá phụ thuộc parent → child (`remove=true`) |
| `kanban_generate_board` / `kanban_breakdown_card` | AI sinh board từ goal / AI chẻ card thành subtask |

## 7. REST API (tham khảo nhanh)

Mount tại `/api/kanban/*` trên daemon (port UI 18788):

```
GET  /api/kanban/boards            POST /api/kanban/boards            # list / create {title, template_id?, workspace_dir?}
GET  /api/kanban/board?id=         POST /api/kanban/board/rename|delete
GET  /api/kanban/templates         POST /api/kanban/templates(/delete)
POST /api/kanban/column/add|update|delete|reorder
GET  /api/kanban/card?id=          POST /api/kanban/card/add|update|move|delete
POST /api/kanban/card/complete|block|unblock|comment
POST /api/kanban/link/add|remove
POST /api/kanban/generate          # AI board {goal, template_id?, board_id?}
POST /api/kanban/breakdown         # AI subtasks {card_id}
GET  /api/kanban/activity?board_id=  # {running[], recent[]}
GET  /api/kanban/assignees?board_id=
GET/POST /api/dispatch-config      # {enabled} — công tắc dispatcher
```

(Còn nhóm `/api/kanban/chat*` — backend chat AI theo board đã có nhưng chưa có
UI nào dùng.)

## 8. Lưu ý & bẫy

1. **Muốn agent chạy → dùng template Standard hoặc Advanced.** Simple không có
   cột Ready nên dispatcher bỏ qua board.
2. **Dispatcher là công tắc toàn cục** — bật là áp dụng mọi board đủ điều kiện,
   không chọn từng board được.
3. **Todo tự trôi sang Ready** khi dispatcher bật — việc chưa muốn chạy hãy để
   ở **Triage**.
4. Card fail nằm ở **Blocked cho đến khi bạn Unblock** (đọc comment `gave_up:`
   trước). Card của worker chết quay lại Ready sau 15 phút và **được nhặt lại**
   — task luôn fail có thể lặp; sửa mô tả hoặc kéo về Triage để dừng.
5. **Xoá cột trên desktop không có xác nhận** và mang theo toàn bộ card bên trong.
6. **AI Task** (desktop) thêm cột mới vào board hiện có — dùng nhiều lần sẽ
   nhân bản cột.
7. WIP limit không chặn, chỉ cảnh báo. Phát hiện vòng phụ thuộc chỉ chặn cặp
   trực tiếp A↔B, chưa chặn vòng gián tiếp.
8. AI board/breakdown cần **LLM đã cấu hình** trong SenClaw; model trả JSON hỏng
   sẽ báo "AI failed: could not parse board JSON…".

## 9. Tham chiếu code

Engine: [`src/kanban/`](../src/kanban/) — DB/workflow: [`db.rs`](../src/kanban/db.rs) ·
REST: [`api.rs`](../src/kanban/api.rs) · dispatcher: [`dispatch.rs`](../src/kanban/dispatch.rs)
+ [`src/agent/mcp_dispatch/mod.rs`](../src/agent/mcp_dispatch/mod.rs) ·
AI: [`llm.rs`](../src/kanban/llm.rs) · MCP: [`mcp.rs`](../src/kanban/mcp.rs) ·
templates: [`templates.rs`](../src/kanban/templates.rs).
UI desktop: [`desktop_app/lib/features/kanban/`](../desktop_app/lib/features/kanban/) ·
UI web: [`web/src/pages/KanbanPage.tsx`](../web/src/pages/KanbanPage.tsx) ·
cấu hình dispatcher: [`src/config.rs`](../src/config.rs) (`SENCLAW_DISPATCH_*`).
