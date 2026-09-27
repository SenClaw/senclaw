# Hướng dẫn sử dụng Background tasks & Scheduled tasks

SenClaw có **hai** hệ thống chạy việc tự động, tách biệt hoàn toàn (hai bảng DB,
hai scheduler, hai UI). Chọn đúng công cụ:

| | **Scheduled task** (lịch định kỳ) | **Background task** (tác vụ nền) |
|---|---|---|
| Chạy ở đâu | Trong một **chat session** riêng của lịch — agent **trả lời bạn** trong chat đó | **Không có chat** — chạy không người trông, ghi vào **run record** |
| Hợp với | "Mỗi sáng 7h tóm tin nhắn cho tôi", báo cáo định kỳ bạn muốn đọc trong chat | Việc ngầm: dọn dẹp, thu thập, kiểm tra — chỉ cần xem lại lịch sử khi cần |
| Tạo ở đâu | Chat ("đặt lịch mỗi sáng…"), Web **Space → Định kỳ**, Desktop **Plugins → Schedules** | Chat, Desktop **Background**, Mobile "Tác vụ nền" — **Web UI chưa có màn hình này** |
| Kết quả | Tin nhắn trong chat của lịch | Run record (status + result + transcript) trong màn hình Background |

Phần C nói về **Reminders** (nhắc việc lịch/calendar) — hệ thứ ba, hay bị nhầm
với hai cái trên. Phần D nói về **Watch**, thứ tư và khác hẳn: không phải lịch
người dùng đặt, mà là cách **agent tự chờ** một việc chạy lâu rồi quay lại trả
kết quả vào đúng cuộc trò chuyện đang dở.

---

## PHẦN A — Scheduled tasks (lịch định kỳ)

### A1. Cách tạo

**Cách 1 — nói với agent trong chat** (dễ nhất): *"đặt lịch mỗi sáng 7h tóm tắt
tin tức"*, *"mỗi thứ 2 lúc 9h nhắc tôi họp"*, *"9h tối nay chạy một lần rồi
thôi"*. Agent dùng bộ tool `space_recurring_*` tạo lịch kèm chat session riêng.

**Cách 2 — Web UI**: **Space → tab "Định kỳ"** → "Thêm lịch định kỳ":

- **Yêu cầu cho agent** (prompt — bắt buộc) + **Tên gọi** (tuỳ chọn).
- **Tần suất**: Mỗi ngày / Thứ 2–6 / Hàng tuần (chọn thứ) / Hàng tháng (ngày
  1–28) + **Giờ chạy (theo giờ máy)**. Mục gập **"Nâng cao: Cron expression"**
  cho lịch phức tạp.
- **Chế độ chạy** Agent/DAG/Plan (xem lưu ý §A5).

**Cách 3 — Desktop**: **Plugins → Schedules** (hoặc: dialog New chat có công tắc
tạo schedule; dashboard có panel "Upcoming schedules"). Form thêm được
**Profile** (persona/skills/MCP của chat lịch) và **Model** riêng. Tần suất có
thêm `once` / `once_delete` — chạy một lần (giữ lại lịch sử) / chạy một lần rồi
tự xoá lịch.

**Cron expression** (khi dùng Nâng cao): 5 trường `phút giờ ngày tháng thứ`,
tính theo **giờ địa phương của máy chạy daemon**. Ví dụ: `0 7 * * *` (7h sáng
hằng ngày), `0 9 * * 1-5` (9h thứ 2–6), `0 */6 * * *` (mỗi 6 tiếng).

### A2. Lịch chạy như thế nào

- Scheduler quét mỗi **30 giây**; đến giờ là đẩy prompt vào **chat session của
  lịch** (jid `schedule:<id>`) — agent chạy và trả lời trong đó như một chat
  bình thường (dùng persona/skills của Profile nếu có chọn).
- **Xem kết quả**: mở chat session của lịch (nút **"Mở chat session"** trên thẻ
  lịch), hoặc **"Lịch sử chạy"** trong drawer chi tiết — từng lần chạy kèm
  ✓ OK / ✗ Lỗi, thời lượng, kết quả.
- Không có notification hệ thống khi lịch chạy xong — kết quả nằm trong chat.

### A3. Quản lý

- **Chạy ngay**: nút "Chạy ngay" — vào hàng đợi, chạy trong vòng ≤30 giây.
- **Tạm dừng / Kích hoạt**: nút Pause/Play. ⚠️ Lịch tạm dừng lâu ngày khi bật
  lại **có thể bắn ngay một phát** (mốc chạy kế còn ở quá khứ) — muốn sạch,
  sửa lại giờ chạy sau khi bật.
- **Sửa**: đổi prompt/tên/lịch/model thoải mái — đổi lịch là mốc chạy kế được
  tính lại theo giờ địa phương.
- **Xoá**: xoá lịch **kèm cả chat session** của nó — không hoàn tác được (UI có
  hỏi xác nhận).

### A4. Daemon tắt/restart — lần chạy bị lỡ

- Lịch **cron**: **không chạy bù** — bỏ qua các mốc đã lỡ, chờ mốc tương lai
  kế tiếp.
- Lịch **interval**: chỉ dời tới một cửa sổ; nếu daemon tắt lâu, sau khi bật có
  thể bắn dồn vài lần liên tiếp (mỗi tick một lần) cho đến khi đuổi kịp.

### A5. Lưu ý & giới hạn

1. **Chế độ chạy DAG/Plan hiện chỉ là nhãn** — lịch luôn chạy chế độ agent
   thường; dropdown được lưu nhưng chưa áp dụng khi thực thi.
2. Ô **"Lần cuối"** trên thẻ lịch hiện luôn trống (bug đã biết) — trạng thái
   lần chạy gần nhất xem ở icon ✓/✗ và "Lịch sử chạy" (nguồn này đúng).
3. Tạo lịch nên đi qua chat (`space_recurring_*`) hoặc UI. Bộ tool thô
   `senclaw-schedule` (`schedule_task`…) là hàng nội bộ: lịch tạo bằng nó với
   group folder thường sẽ bị migration ở lần daemon restart kế **tắt im lặng**
   (chỉ folder `schedule_*` được giữ); ngoài ra lịch tạo qua MCP có thể chạy
   **lần đầu tiên theo giờ UTC** (lệch −7h so với VN) rồi các lần sau mới đúng
   giờ địa phương.
4. Ngày trong tháng chỉ nhận **1–28** (tránh tháng thiếu ngày).
5. Các task đến hạn cùng lúc chạy **tuần tự** — một task chậm làm trễ các task
   sau trong cùng tick.

---

## PHẦN B — Background tasks (tác vụ nền)

### B1. Khái niệm

Task nền chạy **không có chat**: mỗi lần chạy là một **run record** với status,
kết quả, transcript, token đã tiêu. Phù hợp cho việc lặp ngầm: thu thập dữ liệu,
kiểm tra định kỳ, dọn dẹp.

Một task gồm:

- **Lịch chạy**: Hourly / Daily / Weekly / Monthly / Every N minutes /
  Advanced cron / Once at a time / **Manual only** (chỉ chạy khi bấm Run now).
- **Nguồn prompt**:
  - **Static** — prompt cố định.
  - **Template** — trước khi chạy GET một **Context URL** (JSON) và thay
    `{{biến}}` vào prompt; **JSON rỗng ⇒ tự skip, không tốn token** (rất hợp
    kiểu "có đơn hàng mới thì xử lý").
  - **Generated** — một lượt LLM viết prompt thật từ mô tả (tốn gấp đôi token).
- **🔔 Chỉ thông báo** (`notify`): không chạy agent — đến giờ đẩy thẳng OS
  notification với title + nội dung. 0 token, chắc chắn, hợp làm reminder thô.
- **Memory across runs**: **Fresh** (mặc định — mỗi lần chạy sạch) hoặc
  **Remembers** (nhét tóm tắt 5 run gần nhất vào context — cho task kiểu theo
  dõi tiến triển).
- **Nếu run trước chưa xong**: **Skip** (mặc định, ghi một run `skipped`) /
  **Wait** / **Cancel it**.
- **Catch up after downtime**: bật thì chạy bù một lần sau khi daemon tắt lâu
  (>5 phút so với mốc hẹn); tắt thì ghi một run `skipped` giải thích lý do.
- **Persona** + **Tools** (giới hạn tool agent được dùng).

### B2. Màn hình Background (desktop)

Nav **Background** — *"Tasks SenClaw runs by itself — no chat, no reply"*:

- **Quick task**: mô tả một câu tiếng Việt → AI điền sẵn form → **bạn duyệt lại
  rồi mới tạo** (cố ý 2 bước).
- **New task**: form đầy đủ như §B1.
- Cột trái: thống kê **Runs / Success / Avg / Tokens** theo cửa sổ 24h/7d/30d,
  băng "attention" (task đang lỗi), lọc theo status, danh sách task.
- Cột phải: prompt, **Run history**, nút **Pause/Resume · Run now · Edit ·
  Delete · Cancel run**.
- Click một run mở **Background session dialog**: status, thời lượng, trigger,
  số turn, token, **"Prompt sent"** (prompt thật sau khi resolve
  template/generator), **Result** / "Why it skipped" / **Error**, nút **Cancel**
  khi đang chạy. Task đang chạy hiển thị live (stream transcript qua WebSocket).
- Mobile (channel_app): mục **"Tác vụ nền"** trong drawer. **Web UI chưa có màn
  hình Background** — dùng desktop/mobile hoặc REST.

Run thường **không** bắn notification khi xong (chỉ task 🔔 notify mới bắn, và
desktop chỉ hiện toast khi cửa sổ không focus) — theo dõi bằng Run history.

### B3. Nhờ agent tạo task nền trong chat

MCP server **`senclaw-background`** có mặt trong mọi chat: `background_create`,
`background_list`, `background_get`, `background_pause`, `background_resume`,
`background_delete`, `background_run_now`, `background_stats`.

Ba lớp an toàn khi agent tự tạo task:

1. Tool của task phải là **tập con** tool mà chat đó được phép dùng.
2. Task "hướng ra ngoài" (prompt kiểu generated, hoặc tool send/browser/post/
   mail/message…) được tạo ở trạng thái **PAUSED** — bạn phải vào màn hình
   Background bật tay.
3. Quota mỗi owner tối đa **20 task**.

Lưu ý: `background_run_now` qua chat chỉ xếp hàng (chạy trong vài giây); nút
Run now trên UI chạy ngay và trả run id tức thì.

### B4. Vòng đời & tự bảo vệ

- Trạng thái run: `running · success · error · timeout · cancelled · skipped`
  (skipped **không** tính là lỗi). Trạng thái task: `active · paused ·
  completed · failed · cancelled`.
- Giới hạn mặc định: **3 run đồng thời toàn hệ thống, 1/owner**, timeout
  **5 phút**/run, tối đa 40 turn.
- **Lỗi liên tiếp → backoff luỹ tiến** (60s·2ⁿ⁻¹, trần 1h); đủ **5 lần lỗi liên
  tiếp → task tự chuyển `failed`** (tự cách ly) và hiện trong băng "attention" —
  bấm **Resume** để reset bộ đếm và chạy tiếp.
- Daemon restart giữa chừng: run dở được đánh dấu
  `error "daemon stopped while this run was in flight"`.
- Run history giữ **30 ngày** rồi tự dọn.

### B5. Tinh chỉnh (env)

`SENCLAW_BACKGROUND_ENABLED` (true) · `…_INTERVAL_SECS` (20, sàn 5) ·
`…_MAX_CONCURRENT` (3) · `…_PER_OWNER` (1) · `…_TIMEOUT_SECS` (300) ·
`…_MAX_TURNS` (40) · `…_RETENTION_DAYS` (30) · `…_MAX_TASKS_PER_OWNER` (20) ·
`…_BACKOFF_MAX_SECS` (3600).

### B6. REST API (tham khảo nhanh)

```
GET    /api/background/tasks              # ?status=&limit=&offset=… → {tasks, total}
POST   /api/background/tasks              # tạo (hỗ trợ "paused": true)
POST   /api/background/parse              # 1 câu mô tả → JSON spec (không tạo task)
GET    /api/background/tasks/:id          # {task, runs}
PATCH  /api/background/tasks/:id          # sửa; task của app/system chỉ đổi được status
DELETE /api/background/tasks/:id          # chỉ task user; huỷ run đang chạy trước
POST   /api/background/tasks/:id/run-now  # chạy ngay → run_id
GET    /api/background/tasks/:id/runs     # lịch sử (limit ≤500)
GET    /api/background/runs/:id           # {run, activity} — transcript
POST   /api/background/runs/:id/cancel
GET    /api/background/stats?window=24h|7d|30d
```

WS events cho client: `bg:run:started` · `bg:run:activity` · `bg:run:finished` ·
`bg:task:changed` · `notification` (kind `background`).

---

## PHẦN C — Reminders (nhắc việc theo lịch calendar)

Khác cả hai phần trên: đây là nhắc nhở cho **sự kiện lịch** (space events).

- Daemon quét mỗi 60 giây: bắn nhắc trước giờ theo cài đặt reminder của event,
  và bắn "sự kiện đang bắt đầu" đúng `start_at` cho mọi event.
- **Desktop**: toast OS có nút **Mở / Xoá**. Bấm Mở → **dialog nhắc việc tương
  tác**: chat với agent về event đó (mọi hội thoại reminder gom vào một chat
  cố định tên "Reminders"), kèm chip nhanh **"Nhắc lại sau 10 phút" · "Dời sang
  tối nay 20:00" · "Xoá nhắc nhở"** (+ "Mở <app>" nếu event gắn với một Space
  App), và **mic push-to-talk** — nói thay vì gõ, trả lời bằng giọng nói sẽ được
  đọc lên bằng TTS.
- Web nhận reminder qua WebSocket nhưng chưa có dialog tương tác + voice như
  desktop.
- Muốn "nhắc tôi lúc 9h tối" kiểu một lần: nói với agent trong chat — agent tạo
  event kèm reminder, hoặc lịch `once` (§A1), hoặc task nền 🔔 notify (§B1) —
  cả ba đều tới đích, khác nhau ở nơi bạn nhận kết quả.

---

## PHẦN D — Watch (chờ việc dài rồi tự trả kết quả)

### D1. Vấn đề nó giải quyết

Agent giao việc cho thứ chạy lâu — task AI Office, một lượt dispatch/DAG, job của
Space App, một lần build — và nhận về "đang chạy". Trước đây agent chỉ có hai
đường, cả hai đều đứt mạch:

- `sleep` rồi poll một lần trong lượt, chưa xong thì **bỏ cuộc**: *"bạn hỏi lại
  tôi sau nhé"*. Lượt kết thúc, không có gì tự quay lại.
- `background_*`: chạy tự trị nhưng **cố ý không trả lời chat được** — với tới
  người dùng chỉ qua một OS notification.

**Watch** là thứ còn thiếu: một dòng trong `scheduled_tasks` cứ định kỳ kiểm tra
một điều kiện, và ngay khi điều kiện đúng thì đẩy prompt vào **đúng chat gốc** —
cùng đường giao như `ContextMode::Group`. Cuộc trò chuyện tự chạy tiếp.

### D2. Rẻ ở chỗ nào

Mỗi lần kiểm tra là **một lời gọi MCP tool, không có LLM**. Watch chờ một tiếng
chỉ tốn mấy chục lời gọi tool. Chỉ lượt cuối — lúc điều kiện đúng, hoặc lúc bỏ
cuộc — mới tốn một lượt agent thật.

Không khai báo được `tool` thì watch rơi về **đánh thức agent mỗi kỳ** để tự kiểm
tra: vẫn chạy, nhưng mỗi kỳ tốn trọn một lượt.

### D3. Dùng

Agent gọi `mcp__senclaw-schedule__schedule_watch` rồi **kết thúc lượt**:

```json
{
  "label": "AI Office #27",
  "tool": "mcp__ai-office-mcp__office_get_task",
  "args": { "id": 27 },
  "done_path": "status",
  "done_op": "in",
  "done_values": ["done", "completed", "failed"],
  "interval_secs": 60,
  "timeout_secs": 3600,
  "resume_prompt": "Task AI Office #27 đã kết thúc. Kết quả: {{result}}. Lấy báo cáo đầy đủ rồi tổng hợp trả lời user."
}
```

`done_op`: `exists` (mặc định), `equals`, `contains`, `in` và các dạng phủ định —
đều không phân biệt hoa thường. `interval_secs` kẹp 15–3600, `timeout_secs` ≤
86400. Chủ sở hữu chat **lấy từ env**, không nhận qua tham số — watch nói vào một
cuộc trò chuyện, nên không thể để bên gọi chỉ định chat của người khác.

### D4. Bảo đảm

- **Luôn báo lại.** Xong việc, hết hạn, hay hỏng liên tiếp — chat đều được báo.
  Watch không bao giờ chết im lặng, vì im lặng thì y hệt cái hành vi bỏ cuộc mà
  nó sinh ra để thay thế.
- **Lỗi tạm thời không giết watch.** Space App kiểu `session` đang dừng là
  *trạng thái nghỉ bình thường*, không phải hỏng; phải hỏng liên tiếp nhiều lần
  mới dừng.
- **Hai phanh độc lập**: hạn chót (`deadline_at`) và trần số lần kiểm tra
  (`max_checks`).
- **Nhớ đưa cả trạng thái thất bại vào `done_values`** (`failed`, `error`,
  `cancelled`). Watch chỉ khớp `done` sẽ chờ hết hạn trên một job đã chết từ phút
  đầu.

---

## Tham chiếu code

Scheduled tasks: [`src/scheduler/task_scheduler.rs`](../src/scheduler/task_scheduler.rs) ·
[`src/scheduler/executor.rs`](../src/scheduler/executor.rs) ·
tool agent: [`src/mcp/space_server.rs`](../src/mcp/space_server.rs) (`space_recurring_*`) ·
skill: [`skills/schedule/SKILL.md`](../skills/schedule/SKILL.md) ·
UI web: [`web/src/components/space/schedules/`](../web/src/components/space/schedules/) ·
REST: `/api/space/schedules*` ([`src/gateway/ui_server/space.rs`](../src/gateway/ui_server/space.rs)).
Background: [`src/background/`](../src/background/) (scheduler/runner) ·
MCP: [`src/mcp/background_server.rs`](../src/mcp/background_server.rs) ·
REST: [`src/gateway/ui_server/background.rs`](../src/gateway/ui_server/background.rs) ·
UI desktop: [`desktop_app/lib/features/background/`](../desktop_app/lib/features/background/) ·
cấu hình: [`src/config.rs`](../src/config.rs).
Watch: [`src/scheduler/watch.rs`](../src/scheduler/watch.rs) (config + điều kiện) ·
`execute_watch` trong [`src/scheduler/executor.rs`](../src/scheduler/executor.rs) ·
tool agent: `schedule_watch` trong [`src/mcp/schedule_server.rs`](../src/mcp/schedule_server.rs) ·
hướng dẫn: [`skills/schedule/SKILL.md`](../skills/schedule/SKILL.md).
Reminders: [`src/scheduler/event_notifier.rs`](../src/scheduler/event_notifier.rs) ·
[`desktop_app/lib/features/chat/reminder_interaction.dart`](../desktop_app/lib/features/chat/reminder_interaction.dart).
