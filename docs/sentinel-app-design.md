# Sentinel Space App — giám sát & điều tra bảo mật AI Agent

> **Trạng thái:** ĐÃ IMPLEMENT & kiểm chứng sống · **Ngày:** 2026-07-31
> **App:** `apps/sentinel` · cổng **4680** · MCP `sentinel-mcp` · prefix tool `sen_`
> **Thực tế đã build:** 32 luật · 27 MCP tool · 114 test · zip 3.3M · bind `127.0.0.1`
> **Quan hệ tài liệu:** [docs/prompt-injection-security.md](prompt-injection-security.md) là lớp **phòng ngừa**
> (preventive). Tài liệu này là lớp **phát hiện & điều tra** (detective/forensic). Hai lớp bổ sung
> nhau, không thay thế nhau.

---

## Mục lục

1. [Vì sao cần app này](#1-vì-sao-cần-app-này)
2. [Bằng chứng khảo sát trên máy đang chạy](#2-bằng-chứng-khảo-sát-trên-máy-đang-chạy)
3. [Nguồn dữ liệu — cái gì có, cái gì không](#3-nguồn-dữ-liệu--cái-gì-có-cái-gì-không)
4. [Quyết định kiến trúc](#4-quyết-định-kiến-trúc)
5. [Mô hình dữ liệu (SQLite riêng của app)](#5-mô-hình-dữ-liệu-sqlite-riêng-của-app)
6. [Bộ luật phát hiện](#6-bộ-luật-phát-hiện) · [ánh xạ chuẩn](#67-ánh-xạ-sang-chuẩn-mối-đe-dọa)
7. [Ảnh chụp & so sánh cấu hình (baseline diff)](#7-ảnh-chụp--so-sánh-cấu-hình-baseline-diff)
8. [Quy trình điều tra: dòng thời gian → phát hiện → hồ sơ vụ việc](#8-quy-trình-điều-tra-dòng-thời-gian--phát-hiện--hồ-sơ-vụ-việc)
9. [Vai trò của AI trong app](#9-vai-trò-của-ai-trong-app)
10. [REST + MCP surface](#10-rest--mcp-surface)
11. [Web UI](#11-web-ui)
12. [Bảo mật của chính app này](#12-bảo-mật-của-chính-app-này)
13. [Kế hoạch triển khai theo phase](#13-kế-hoạch-triển-khai-theo-phase)
14. [Rủi ro đã biết](#14-rủi-ro-đã-biết)

---

## 1. Vì sao cần app này

SenClaw đã có nhiều lớp phòng thủ tốt: permission gate 4 tầng
(`src/zen_core/permissions.rs:372`), human-in-the-loop qua PermissionBridge
(`src/agent/permission_bridge/bridge.rs:446`), SSRF guard
(`src/util/fetch_safety.rs:16`), shell-safety classifier
(`src/util/shell_safety.rs:17`), workspace path containment
(`src/mcp/workspace_server.rs:147`), 3 guard của background task
(`src/mcp/background_server.rs:246`).

Cái **không** có là lớp thứ hai: khi một lớp phòng thủ bị vô hiệu hoá hoặc bị vượt
qua, không có gì phát hiện ra điều đó.

Cụ thể, `grep -ri 'audit' src/` không trả về bảng audit nào. Hệ quả trực tiếp:

- **Không ghi vết đối tượng thực hiện.** `scheduled_tasks`, `task_run_logs`,
  `tool_rules`, `plans`, `background_*` đều không có cột `created_by`/`actor`.
  Không thể phân biệt "người dùng đặt lịch này" với "agent tự đặt lịch cho chính nó".
- **Không lưu lịch sử thay đổi cấu hình.** `groups.allowed_tools`,
  `groups.approved_tools`, `tool_rules`, `hooks.json`, danh sách MCP server — tất cả
  ghi đè tại chỗ (`src/gateway/group_manager/manager.rs:75`). Không biết cấu hình
  hôm qua khác hôm nay chỗ nào.
- **Quyết định auto-approve không để lại dấu vết.** `should_auto_accept`
  (`bridge.rs:115`) trả về sớm tại `:460` trước khi callback ghi `chat_events`. Một
  tool được cho qua nhờ `tool_rules` hoặc nhờ cờ skip-permissions thì **không có bản
  ghi phê duyệt nào cả** — chỉ một dòng `tracing::info!` ra stdout.
- **Lịch sử bị xoá dần.** `tool_executions` bị FIFO-trim theo `groups.max_messages`
  (`src/db/tool_executions.rs:71`), `dispatch_activity` cắt còn 500 dòng/task
  (`src/db/dispatch_activity.rs:24`), `background_*` prune sau 30 ngày
  (`src/config.rs:574`).

Sentinel lấp đúng bốn khoảng trống đó: **trích xuất → bảo toàn → phát hiện → điều tra**.

Phạm vi có chủ đích: **detective, không phải preventive**. App không đứng chắn trên
đường thực thi tool (đó là việc của `permissions.rs` và hook `PreToolUse`). App đọc
dấu vết sau khi sự việc xảy ra, chấm điểm rủi ro, và cho con người điều tra.

---

## 2. Bằng chứng khảo sát trên máy đang chạy

Toàn bộ số liệu dưới đây lấy từ `~/.senclaw/senclaw.db` và daemon đang chạy ngày
2026-07-31 — không phải giả định. Chúng vừa chứng minh nhu cầu, vừa là bộ dữ liệu
kiểm thử sẵn có cho app.

| Quan sát | Số liệu | Ý nghĩa |
|---|---|---|
| `tool_executions` còn lại / đã từng có | **1 268 / 3 950** (`MAX(id)`) | **~68% lịch sử tool-call đã bị FIFO xoá vĩnh viễn.** Không phục hồi được từ DB. |
| Tool thuộc nhóm cần phê duyệt đã chạy | **895** | `mcp__*`, `Bash`, `Write`, `Edit`, `Skill` |
| Sự kiện `permission:request` được ghi | **83** | **~91% lệnh có đặc quyền không để lại bản ghi phê duyệt nào.** |
| `/api/admin-permissions` | `{"skipAllAgentsPermissions":true,"skipMainAgentPermissions":true}` | **HITL đang tắt toàn cục** — giải thích chính xác khoảng cách 895/83. |
| Luật auto-accept dạng wildcard | `mcp:senclaw-browser:*`, `mcp:senclaw-code:*`, `mcp:senclaw-dispatch:*`, … | `browser_execute_js` (JS tuỳ ý trong Chrome đã đăng nhập) và `senclaw-code` `bash`/`write_file` chạy **không hỏi**. |
| Lịch không nằm dưới folder `schedule_*` | 6/13 (`default`, `main`, `reminders`) | Được tạo qua `senclaw-schedule` chứ không qua Space UI — đường mà agent dùng được. |
| `task_run_logs` mồ côi | 1 `task_id` | Một lịch đã tồn tại, đã chạy, rồi **bị xoá** — chỉ còn log là bằng chứng. |
| Bề mặt tool | **52 MCP server / 757 tool** (35 server đang `connected`) | Trong đó `crm-mcp` 83 tool, `moltbook-mcp` 44, `mini-browser-mcp` 35, `facebook-mcp` 35, `video-flow-mcp` 35, `senclaw-browser` 30. |
| Lệnh shell trên **máy từ xa** đã chạy | **81** `ssh_execute_command` + `ssh_start_connect*` | Qua `ssh-manager-mcp`. Không có bản ghi phê duyệt nào cho bất kỳ lệnh nào. |
| `chat_events` | 1 192 (996 `agent:state`, 83 `permission:request`, 82 `permission:resolved`, 16+15 `question:*`) | |
| `~/.senclaw/llm_logs/` | **214 MB**, 30 file ngày | Nơi **duy nhất** có tham số tool đầy đủ + system prompt. |
| Hook đang cài | `PostToolUse` matcher `Bash` → `echo done` | Hook chạy lệnh shell tuỳ ý; không có lịch sử thay đổi. |

Kết luận đọc được ngay từ bảng trên: trên máy này, **lớp human-in-the-loop hiện
không hoạt động**, agent có 757 tool trong tầm với, trong đó có đường thực thi shell
trên máy từ xa đã dùng 81 lần — và không có cơ chế nào báo cho người dùng biết điều
đó. Đó chính là màn hình đầu tiên Sentinel phải hiển thị.

Cần nói rõ: đây **không** phải bằng chứng hệ thống đã bị xâm nhập. Đây là bằng chứng
rằng nếu có xâm nhập thì hiện không ai biết được — và sau khi FIFO xoá 68% lịch sử,
phần lớn cũng không còn điều tra được nữa.

---

## 3. Nguồn dữ liệu — cái gì có, cái gì không

### 3.1 Có trong `~/.senclaw/senclaw.db`

| Bảng | Dùng cho | Hạn chế cần biết |
|---|---|---|
| `tool_executions` (`src/db/schema.rs:181`) | Xương sống của dòng thời gian: `chat_jid`, `agent_id`, `tool_name`, `ok`, `timestamp`, `content_json` | `content_json` là **kết quả**, không phải tham số. Mọi `gen_tool_result_message` bỏ `input` (`src/mcp/bridge.rs:82`). |
| — riêng `Bash` | `content_json.title` = **lệnh thật**, cắt 100 ký tự (`src/tools/bash.rs:243`) | Chỉ Bash có. Đủ để bắt `curl`/`base64`/`rm -rf`. |
| `chat_events` (`:131`) | `permission:request` / `permission:resolved` / `question:*` / `agent:state` | Payload dùng `toolName` (camelCase) và `key`. Không ghi lần auto-approve. |
| `scheduled_tasks` (`:226`) | Lịch sử lịch: `context_mode`, `schedule_type`, `group_folder`, `script_path`, `status` | Không có `created_by`. Cột DB tên `script_path` nhưng field Rust là `script_command` (`src/db/scheduled_tasks.rs:31`). |
| `task_run_logs` (`:245`) | Mỗi lần chạy: `run_at`, `status`, `result`, `error` | `duration_ms` **luôn NULL** (`src/scheduler/executor.rs:66`). Không lưu lệnh shell đã chạy. |
| `background_tasks` / `background_runs` / `background_activity` (`:260`,`:302`,`:323`) | Tác vụ nền không giám sát; `owner_kind`/`owner_id` là **cột ownership duy nhất** trong toàn bộ hệ | Prune sau 30 ngày. |
| `dispatch_activity` (`:208`) | Hoạt động của sub-agent DAG | Cắt 500 dòng/task. |
| `tool_rules` (`:175`) | Luật auto-accept hiện hành | Last-write-wins, không version, không `changed_at` thực. |
| `groups` (`:10`) | `allowed_tools`, `approved_tools`, `allowed_work_dirs`, `max_messages` | Không lịch sử thay đổi. |
| `group_messages` (`:86`) | Bản ghi hội thoại (văn bản) | FIFO theo `max_messages`. |
| `memory_chunks` (`src/memory/schema.rs:52`) | Quét memory poisoning | |
| `installed_skills` / `installed_plugins` (`:699`,`:714`) | Trạng thái hiện tại | Không có lịch sử cài đặt. |

### 3.2 Có ngoài DB

- **`~/.senclaw/llm_logs/YYYY-MM-DD.log`** (`src/util/llm_log.rs:125`) — nguồn **duy
  nhất** có `toolCalls: [{name, args}]` đầy đủ và toàn văn system prompt. Định dạng
  `[HH:MM:SS]{json}` mỗi dòng (**không phải JSONL thuần** — phải cắt tiền tố 10 ký
  tự trước khi parse). Giữ 30 file gần nhất (`:55`), tắt được bằng
  `SENCLAW_LLM_LOG=0`. Đây cũng là bề mặt lộ bí mật lớn nhất của hệ thống.
- **REST daemon** `127.0.0.1:18788` — `GET /api/mcp-servers` (kèm `tools[]` và
  `description`), `/api/config`, `/api/admin-permissions`, `/api/hooks`,
  `/api/space/apps`, `/api/space/schedules`, `/api/skills`, `/api/plugins`. Đã
  verify cả 6 endpoint trả 200 với daemon hiện tại.
- `~/.senclaw/workflow-runs.json`, `~/.senclaw/dispatch-state.json`,
  `~/.senclaw/hooks.json`, `~/.senclaw/config.json`.

### 3.3 Không tồn tại ở đâu cả — Sentinel phải tự dựng

1. Tham số tool của **tool không phải Bash** (trừ khi đối chiếu được với `llm_logs`).
2. Lệnh shell của lịch `script`/`script-agent` — chỉ nằm ở `scheduled_tasks.script_path`,
   không bao giờ vào `task_run_logs`.
3. Bản ghi cho lần **auto-approve** và **luật nào đã khớp**.
4. **Lịch sử** của `tool_rules`, `groups.*`, `hooks.json`, danh sách MCP server,
   skill/plugin đã cài → phải **tự chụp ảnh định kỳ và so sánh**.
5. Bản ghi việc **xoá** lịch (`delete_task` xoá cứng, `src/mcp/schedule_server.rs:393`).
6. Hành động browser chi tiết (`browser_server.rs` không ghi DB) và sự kiện gửi tin
   (`send_server.rs` không có bảng `sent_messages`).

---

## 4. Quyết định kiến trúc

### 4.1 Đọc thẳng SQLite của daemon ở chế độ chỉ-đọc — cố ý lệch quy ước

Quy ước hiện tại: **không app nào mở DB của daemon**; app chỉ có DB riêng và gọi REST.
Sentinel phá lệ này, có lý do và có giới hạn rõ ràng:

- Không có REST endpoint nào expose `tool_executions`, `tool_rules`, `chat_events`
  xuyên chat. Không đọc trực tiếp thì không có dữ liệu để điều tra.
- Thêm endpoint mới vào daemon là sửa core — nằm ngoài phạm vi một Space App, và
  ngược đời: công cụ pháp chứng không nên yêu cầu sửa chính hệ thống bị điều tra.

**Ràng buộc bắt buộc** (đã verify chạy được trên DB WAL sống, 4 ms, ghi bị chặn ở
tầng SQLite):

```rust
// Mở bằng URI mode=ro + query_only. WAL cho phép nhiều reader song song với writer.
let conn = Connection::open_with_flags(
    format!("file:{}?mode=ro", db_path.display()),
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
)?;
conn.execute_batch("PRAGMA query_only=ON;")?;
```

Không `ATTACH`, không transaction dài, mọi truy vấn có `LIMIT`. Nếu mở thất bại
(daemon đổi đường dẫn, DB bị khoá) → app vẫn chạy ở chế độ suy giảm, chỉ dùng REST,
và hiển thị rõ nguồn nào đang mất.

### 4.2 Trích xuất một chiều vào kho riêng, append-only, có chuỗi băm

Sentinel **không** truy vấn trực tiếp DB daemon mỗi lần vẽ giao diện. Nó chạy ingest
định kỳ (mặc định 60 s) chép sự kiện mới sang `~/.senclaw/apps/sentinel/sentinel.db`,
chuẩn hoá về một bảng `events` duy nhất. Ba lý do:

1. **Bảo toàn.** Bản chép sống sót qua FIFO-trim của daemon — chính là 68% lịch sử
   đang mất hôm nay.
2. **Chống sửa vết.** Mỗi sự kiện mang `prev_hash`/`hash` (SHA-256 trên bộ trường
   chuẩn hoá). Sửa hoặc xoá một dòng trong quá khứ làm gãy chuỗi và `sen_verify_chain`
   chỉ ra đúng vị trí gãy. Không chống nổi kẻ có quyền ghi file, nhưng phát hiện
   được việc sửa lặng lẽ — đúng mục tiêu tamper-**evident**, không phải tamper-proof.
3. **Tốc độ.** Chỉ mục theo `(ts)`, `(actor, ts)`, `(kind, ts)` phục vụ dòng thời
   gian và tương quan mà không đụng vào DB daemon.

Con trỏ ingest lưu theo từng nguồn (`ingest_cursor`), dùng khoá tăng dần của nguồn
(`tool_executions.id`, `chat_events.id`, `task_run_logs.id`) chứ không dùng thời gian
— tránh mất sự kiện khi đồng hồ nhảy.

### 4.3 Ba tầng, tách bạch

```
                 ┌─────────────── nguồn (chỉ đọc) ────────────────┐
                 │ senclaw.db (ro)   REST 18788   llm_logs/*.log  │
                 └───────────────────────┬────────────────────────┘
                                         │ ingest 60s (một chiều)
                 ┌───────────────────────▼────────────────────────┐
   Tầng 1  KHO   │ events (append-only, hash-chained) + snapshots │
                 └───────────────────────┬────────────────────────┘
                                         │ rule engine (thuần Rust, tất định)
                 ┌───────────────────────▼────────────────────────┐
   Tầng 2  PHÁT  │ findings (mức, điểm, chứng cứ) → cases         │
         HIỆN    └───────────────────────┬────────────────────────┘
                                         │ AI chỉ diễn giải, không chấm điểm
                 ┌───────────────────────▼────────────────────────┐
   Tầng 3  ĐIỀU  │ dòng thời gian · pivot theo actor · hồ sơ vụ   │
          TRA    │ việc · báo cáo                                 │
                 └────────────────────────────────────────────────┘
```

**Ranh giới quan trọng:** luật phát hiện là mã Rust tất định, có unit test, không gọi
LLM. AI chỉ dùng để *diễn giải* phát hiện đã có (giải thích, dựng giả thuyết, viết
báo cáo). Lý do: một hệ thống bảo mật mà mức nghiêm trọng do LLM tự chấm thì vừa
không tái lập được, vừa chính là mục tiêu ngon cho prompt injection — dữ liệu đầu vào
của app **là** nội dung không tin cậy do agent sinh ra.

### 4.4 Mặc định chỉ quan sát, hành động phải chọn riêng

Phase 1–3 app **không ghi gì** vào daemon. Phase 4 mở một tập hành động hẹp, mỗi
hành động là một nút bấm của con người, không phải MCP tool để agent tự gọi:

- tạm dừng một scheduled task (`PATCH /api/space/schedules/:id`),
- tắt một MCP server (`POST /api/mcp-servers/:name/enabled`),
- xoá một luật auto-accept quá rộng.

Theo đúng khuôn autonomy-gate của moltbook (`apps/moltbook/src/engine.rs:503`): một
đường ghi duy nhất, mặc định an toàn.

---

## 5. Mô hình dữ liệu (SQLite riêng của app)

`~/.senclaw/apps/sentinel/sentinel.db` (ghi đè bằng `SENCLAW_DATA_DIR`), WAL.

```sql
-- Sự kiện chuẩn hoá, chỉ thêm, có chuỗi băm
CREATE TABLE events (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  ts          TEXT NOT NULL,           -- RFC3339 UTC
  source      TEXT NOT NULL,           -- tool_executions | chat_events | task_run_logs | llm_log | rest | snapshot
  kind        TEXT NOT NULL,           -- tool_call | permission_request | permission_resolved |
                                       -- schedule_run | schedule_defined | config_change | message | agent_state
  actor       TEXT NOT NULL,           -- chat_jid hoặc schedule:<id> hoặc bg:<task_id>
  agent_id    TEXT NOT NULL DEFAULT 'main',
  tool_name   TEXT,
  ok          INTEGER,
  summary     TEXT NOT NULL DEFAULT '',
  detail_json TEXT NOT NULL DEFAULT '{}',   -- đã lọc bí mật trước khi ghi
  src_key     TEXT,                    -- '<source>:<id gốc>' để chống trùng
  prev_hash   TEXT NOT NULL DEFAULT '',
  hash        TEXT NOT NULL
);
CREATE UNIQUE INDEX idx_events_srckey ON events(src_key) WHERE src_key IS NOT NULL;
CREATE INDEX idx_events_ts     ON events(ts);
CREATE INDEX idx_events_actor  ON events(actor, ts);
CREATE INDEX idx_events_kind   ON events(kind, ts);
CREATE INDEX idx_events_tool   ON events(tool_name, ts);

CREATE VIRTUAL TABLE events_fts USING fts5(summary, detail_json, content='events', content_rowid='id');

CREATE TABLE ingest_cursor (
  source     TEXT PRIMARY KEY,
  last_key   TEXT NOT NULL,     -- id cuối đã đọc (chuỗi để dùng chung cho id số và offset file)
  last_run   TEXT NOT NULL,
  ok         INTEGER NOT NULL DEFAULT 1,
  error      TEXT
);

-- Ảnh chụp cấu hình để so sánh (daemon không lưu lịch sử)
CREATE TABLE snapshots (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  taken_at   TEXT NOT NULL,
  kind       TEXT NOT NULL,     -- mcp_servers | tool_rules | groups | hooks | admin_permissions |
                                -- skills | plugins | schedules | mcp_tool_manifest
  body_json  TEXT NOT NULL,
  body_hash  TEXT NOT NULL
);
CREATE INDEX idx_snapshots_kind ON snapshots(kind, taken_at);

CREATE TABLE snapshot_diffs (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  kind       TEXT NOT NULL,
  from_id    INTEGER NOT NULL,
  to_id      INTEGER NOT NULL,
  added      TEXT NOT NULL DEFAULT '[]',
  removed    TEXT NOT NULL DEFAULT '[]',
  changed    TEXT NOT NULL DEFAULT '[]',
  detected_at TEXT NOT NULL
);

-- Kết quả luật
CREATE TABLE findings (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  rule_id     TEXT NOT NULL,          -- SEN-PERSIST-02 …
  severity    TEXT NOT NULL,          -- critical | high | medium | low | info
  score       INTEGER NOT NULL,       -- 0..100
  title       TEXT NOT NULL,
  detail      TEXT NOT NULL DEFAULT '',
  actor       TEXT,
  first_ts    TEXT NOT NULL,
  last_ts     TEXT NOT NULL,
  evidence    TEXT NOT NULL DEFAULT '[]',  -- [event.id]
  status      TEXT NOT NULL DEFAULT 'open', -- open | triaged | accepted_risk | false_positive | resolved
  dedupe_key  TEXT NOT NULL,
  case_id     INTEGER,
  note        TEXT NOT NULL DEFAULT '',
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);
CREATE UNIQUE INDEX idx_findings_dedupe ON findings(dedupe_key);

CREATE TABLE cases (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  title       TEXT NOT NULL,
  summary     TEXT NOT NULL DEFAULT '',
  status      TEXT NOT NULL DEFAULT 'open',   -- open | investigating | closed
  severity    TEXT NOT NULL DEFAULT 'medium',
  hypothesis  TEXT NOT NULL DEFAULT '',       -- do AI đề xuất, người sửa
  timeline    TEXT NOT NULL DEFAULT '[]',
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  closed_at   TEXT
);

CREATE TABLE case_notes (
  id       INTEGER PRIMARY KEY AUTOINCREMENT,
  case_id  INTEGER NOT NULL,
  author   TEXT NOT NULL,        -- 'user' | 'ai'
  body     TEXT NOT NULL,
  ts       TEXT NOT NULL
);

-- Bật/tắt và chỉnh ngưỡng luật; luật do người dùng thêm
CREATE TABLE rule_config (
  rule_id    TEXT PRIMARY KEY,
  enabled    INTEGER NOT NULL DEFAULT 1,
  severity   TEXT,
  params     TEXT NOT NULL DEFAULT '{}',
  updated_at TEXT NOT NULL
);

CREATE TABLE suppressions (      -- giảm nhiễu: bỏ qua có chủ đích, có hạn
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  rule_id    TEXT NOT NULL,
  match_json TEXT NOT NULL,      -- {actor?, tool_name?, contains?}
  reason     TEXT NOT NULL,
  until      TEXT,
  created_at TEXT NOT NULL
);

CREATE TABLE settings (k TEXT PRIMARY KEY, v TEXT NOT NULL);
```

**Lọc bí mật trước khi ghi.** `detail_json` đi qua `redact()` trước khi vào DB: mẫu
khoá API (`sk-`, `ghp_`, `AKIA`, JWT ba đoạn), `Authorization:`, chuỗi base64 dài
bất thường, và tên trường nhạy cảm (`password`, `token`, `secret`, `cookie`,
`api_key`). Mượn thẳng danh sách của mini-browser
(`apps/mini-browser/src/session.rs:1023`). Bản gốc **không** được chép sang; nếu điều
tra viên cần nguyên văn, app chỉ ra vị trí trong nguồn (`src_key`, offset file log).

---

## 6. Bộ luật phát hiện

Mỗi luật: mã ổn định, tín hiệu, nguồn dữ liệu, mức mặc định. Luật là hàm Rust thuần
`fn(&Store, &RuleCtx) -> Vec<Finding>`, có unit test, có `dedupe_key` để chạy lại
không sinh trùng.

### 6.1 Persistence — agent tự tạo chỗ đứng chân

| Mã | Tín hiệu | Nguồn | Mức |
|---|---|---|---|
| `SEN-PERSIST-01` | `scheduled_tasks.group_folder` **không** khớp `schedule\_%` → lịch được tạo qua `senclaw-schedule` (đường agent dùng được), không qua Space UI | `scheduled_tasks` | high |
| `SEN-PERSIST-02` | `context_mode IN ('script','script-agent')` → `bash -c` tuỳ ý, **không** qua `shell_safety`, không `BANNED_COMMANDS`, không hỏi phê duyệt (`src/scheduler/executor.rs:111`,`:146`) | `scheduled_tasks` | **critical** |
| `SEN-PERSIST-03` | `script_path` khớp mẫu nguy hiểm: `curl`,`wget`,`base64 -d`,`nc `,`chmod +x`,`>> ~/.`,`launchctl`,`crontab` | `scheduled_tasks` | **critical** |
| `SEN-PERSIST-04` | `task_run_logs.task_id` không còn trong `scheduled_tasks` → lịch đã bị **xoá cứng** sau khi chạy (xoá vết, hoặc `once_delete`) | join 2 bảng | high |
| `SEN-PERSIST-05` | Lịch mới xuất hiện trong vòng N phút sau một tool-call có dấu hiệu injection (`SEN-INJECT-01`) | tương quan | **critical** |
| `SEN-PERSIST-06` | `background_tasks` có `owner_kind` ≠ `user` **và** dùng tool outward-facing **và** status ≠ paused → vượt Guard 3 (`background_server.rs:283`) | `background_tasks` | high |
| `SEN-PERSIST-07` | Lịch `context_mode='isolated'` báo `success` — nhưng executor là stub, **không bao giờ chạy thật** (`executor.rs:47`) | `scheduled_tasks`+`task_run_logs` | medium |

### 6.2 Vô hiệu hoá kiểm soát

| Mã | Tín hiệu | Nguồn | Mức |
|---|---|---|---|
| `SEN-CTRL-01` | `skipAllAgentsPermissions` hoặc `skipMainAgentPermissions` = true → **HITL tắt toàn cục** | `/api/admin-permissions` | **critical** |
| `SEN-CTRL-02` | Khoảng cách phê duyệt: số tool thuộc nhóm bị kiểm soát đã chạy ÷ số `permission:request`. > 3× ⇒ cảnh báo, kèm con số thật | `events` | high |
| `SEN-CTRL-03` | Luật auto-accept dạng wildcard cho server rủi ro (`senclaw-browser`, `senclaw-code`, `senclaw-js`, `senclaw-send`, `senclaw-schedule`) | `tool_rules` | high |
| `SEN-CTRL-04` | Luật auto-accept **mới xuất hiện** rồi tool thuộc luật đó chạy dồn ngay sau — thứ tự "mở cửa rồi đi qua" | `snapshot_diffs`+`events` | **critical** |
| `SEN-CTRL-05` | Một tool bị `refuse`, rồi chính tool đó chạy thành công trong vòng N phút | `chat_events`+`events` | high |
| `SEN-CTRL-06` | `hooks.json` đổi: thêm hook, hoặc lệnh hook chứa mẫu nguy hiểm (hook chạy shell tuỳ ý) | `/api/hooks` snapshot | high |
| `SEN-CTRL-07` | `groups.allowed_tools`/`approved_tools` **mở rộng** giữa hai lần chụp | `groups` snapshot | medium |
| `SEN-CTRL-08` | `SENCLAW_LLM_LOG=0` hoặc file log ngày hôm nay ngừng tăng khi vẫn có tool-call → mất nguồn chứng cứ | fs + `events` | medium |

### 6.3 Rò rỉ dữ liệu

| Mã | Tín hiệu | Nguồn | Mức |
|---|---|---|---|
| `SEN-EXFIL-01` | Chuỗi trong cửa sổ N phút cùng `actor`: đọc nguồn nhạy cảm (`Read`/`Bash cat`/`workspace_*` chạm `.ssh`,`.env`,`credential`,`token`) **rồi** hành vi ra ngoài (`send_*`, `browser_navigate` host lạ, `WebFetch`, `browser_fill_form`) | `events` tương quan | **critical** |
| `SEN-EXFIL-02` | Bất kỳ `send_file` nào | `events` | high |
| `SEN-EXFIL-03` | `Bash.title` chứa `curl`/`wget`/`nc`/`base64` — dù `BANNED_COMMANDS` chặn `BashTool`, `senclaw-js.bash_run` và lịch script **không** bị chặn | `events` | high |
| `SEN-EXFIL-04` | `browser_navigate`/`browser_fill_form` tới host chưa từng thấy, trong phiên có đăng nhập | `events` | medium |
| `SEN-EXFIL-05` | Chuỗi giống bí mật xuất hiện trong `detail_json` hoặc `llm_logs` (chạy trên bản đã redact: đếm số lần khớp, không lưu giá trị) | `events`+fs | high |

### 6.4 Prompt injection & đầu độc

| Mã | Tín hiệu | Nguồn | Mức |
|---|---|---|---|
| `SEN-INJECT-01` | Kết quả tool chứa cụm chỉ thị: `ignore previous`, `disregard`, `you are now`, `your real task`, `system:`, `[SYSTEM]`, `bỏ qua hướng dẫn`, `never ask`, `without asking`, `do not tell` — dùng lại danh sách `INJECTION` của mini-browser (`apps/mini-browser/src/llm.rs:1391`) | `tool_executions.content_json` | high |
| `SEN-INJECT-02` | **Tool poisoning**: `description` của MCP tool chứa cụm chỉ thị | `/api/mcp-servers/:name/tools` | **critical** |
| `SEN-INJECT-03` | **Rug pull**: băm manifest tool của một server đổi so với ảnh chụp trước, mà không có lần cài/cập nhật tương ứng | `mcp_tool_manifest` snapshot | **critical** |
| `SEN-INJECT-04` | **Memory poisoning**: `memory_chunks.text` hoặc node cognitive chứa cụm chỉ thị | `memory_chunks` | high |
| `SEN-INJECT-05` | Tin nhắn kênh vào chứa cụm chỉ thị **và** sau đó có tool-call đặc quyền cùng `actor` trong N phút | `group_messages`+`events` | **critical** |

### 6.5 Bất thường hành vi

| Mã | Tín hiệu | Nguồn | Mức |
|---|---|---|---|
| `SEN-ANOM-01` | Hoạt động ngoài giờ: cụm tool-call trong khung giờ người dùng đặt (mặc định 00:00–05:00 giờ địa phương) | `events` | medium |
| `SEN-ANOM-02` | Bùng nổ tần suất: số tool-call/giờ của một `actor` vượt `mean + 3σ` của 14 ngày trước (cần ≥7 ngày dữ liệu, nếu không thì im lặng) | `events` | medium |
| `SEN-ANOM-03` | Tool mới lần đầu xuất hiện với một `actor` đã có lịch sử ổn định | `events` | low |
| `SEN-ANOM-04` | Lặp lỗi: ≥5 lần `ok=0` liên tiếp cùng tool — tương ứng error-loop guard (`src/zen_core/conversation.rs:1643`); nếu **không** thấy `SessionError` đi kèm thì guard đã không bắt được | `events` | medium |
| `SEN-ANOM-05` | Lịch `cron` chạy sai nhịp so với `schedule_value` (chạy dày hơn khai báo) | `task_run_logs` | medium |
| `SEN-ANOM-06` | Một `actor` dùng nhiều họ tool khác thường trong một phiên (browser + shell + send) — mẫu "read → act → exfil" | `events` | high |

### 6.6 Tư thế bảo mật (chạy theo lịch, không theo sự kiện)

| Mã | Tín hiệu | Mức |
|---|---|---|
| `SEN-POSTURE-01` | Danh sách MCP server đang bật, đánh dấu server rủi ro cao (`senclaw-js`, `senclaw-browser`, `senclaw-code`, `senclaw-send`, `senclaw-schedule`) | info |
| `SEN-POSTURE-02` | `SENCLAW_WS_TOKEN` chưa đặt → WebSocket không xác thực | medium |
| `SEN-POSTURE-03` | Space App bind `0.0.0.0` → lộ ra LAN (đúng với **mọi** app hiện tại, kể cả app này nếu không sửa) | high |
| `SEN-POSTURE-04` | Skill/plugin mới cài kể từ ảnh chụp trước | medium |
| `SEN-POSTURE-05` | Nhóm có `allowed_work_dirs` rộng (`/`, `$HOME`) | medium |
| `SEN-POSTURE-06` | `llm_logs` chứa system prompt + tham số tool ở dạng văn bản thuần, quyền file rộng | medium |

### 6.7 Ánh xạ sang chuẩn mối đe dọa

Mỗi luật mang thêm trường `standards: []` để báo cáo nói được cùng ngôn ngữ với hồ sơ
tuân thủ, và để người đọc ngoài dự án hiểu được. Hai khung tham chiếu chính:

- **OWASP Top 10 for LLM Applications (2025)** — `LLM01` Prompt Injection,
  `LLM02` Sensitive Information Disclosure, `LLM03` Supply Chain,
  `LLM04` Data & Model Poisoning, `LLM06` Excessive Agency,
  `LLM07` System Prompt Leakage.
- **OWASP Agentic AI — Threats and Mitigations** — `T1` Memory Poisoning,
  `T2` Tool Misuse, `T3` Privilege Compromise, `T6` Intent Breaking & Goal
  Manipulation, `T8` Repudiation & Untraceability, `T10` Overwhelming
  Human-in-the-Loop, `T11` Unexpected RCE, `T13` Rogue Agents.

| Nhóm luật | OWASP LLM | Agentic | Chiến thuật ATLAS |
|---|---|---|---|
| `SEN-PERSIST-*` | LLM06 | T2, T6 | Persistence |
| `SEN-CTRL-*` | LLM06 | T3, T10 | Defense Evasion, Privilege Escalation |
| `SEN-EXFIL-*` | LLM02 | T2 | Collection, Exfiltration, Credential Access |
| `SEN-INJECT-01/05` | LLM01 | T6 | Initial Access (indirect prompt injection) |
| `SEN-INJECT-02/03` | LLM01, LLM03 | T2, T13 | Supply-chain / plugin compromise |
| `SEN-INJECT-04` | LLM04 | T1 | Persistence (qua memory) |
| `SEN-ANOM-*` | LLM06 | T6, T13 | Discovery, Execution |
| `SEN-POSTURE-*` | LLM03, LLM07 | T3 | — |

`T8` (Repudiation & Untraceability) không ánh xạ vào một luật cụ thể nào — **nó là
toàn bộ lý do app này tồn tại**. Kho append-only có chuỗi băm (§4.2), việc ghi lại
thuộc tính hành động, và ảnh chụp cấu hình (§7) chính là phần đáp ứng `T8`.

### 6.8 Chấm điểm

`score = base(severity) × độ_tin_cậy × hệ_số_gần_đây`, làm tròn 0–100. `base`:
critical 90, high 70, medium 45, low 20, info 5. Độ tin cậy giảm khi luật dựa trên
suy đoán (ví dụ heuristic folder của `SEN-PERSIST-01`), tăng khi có nhiều mảnh chứng
cứ độc lập. Điểm chỉ để **xếp thứ tự hàng đợi phân loại**, không tự động kích hoạt
hành động nào.

---

## 7. Ảnh chụp & so sánh cấu hình (baseline diff)

Đây là phần Sentinel tạo ra dữ liệu mà daemon **không hề có**: lịch sử cấu hình.

Mỗi 15 phút (và khi bấm nút), app chụp 9 nhóm: `mcp_servers`, `mcp_tool_manifest`
(tên + băm description từng tool), `tool_rules`, `groups`, `hooks`,
`admin_permissions`, `skills`, `plugins`, `schedules`. Mỗi ảnh lưu `body_json` +
`body_hash`. Nếu `body_hash` trùng ảnh trước → không lưu thêm dòng, chỉ cập nhật
`taken_at` (tránh phình DB).

Khi băm đổi → sinh `snapshot_diffs` với ba danh sách `added`/`removed`/`changed`, rồi
nạp vào luật `SEN-CTRL-03…07`, `SEN-INJECT-02/03`, `SEN-POSTURE-04`.

Ảnh chụp đầu tiên **không** sinh phát hiện thay đổi (không có gì để so) nhưng vẫn
chạy các luật trạng thái tĩnh — vì vậy `SEN-CTRL-01` báo ngay từ phút đầu trên máy
hiện tại.

`mcp_tool_manifest` là mấu chốt chống rug-pull: đây chính là khoảng trống §4.9 của
[prompt-injection-security.md](prompt-injection-security.md) — không có cơ chế phát
hiện MCP server đổi hành vi sau khi đã được tin tưởng. Sentinel không ngăn được, nhưng
**phát hiện được**, và đó là bước đầu tiên.

---

## 8. Quy trình điều tra: dòng thời gian → phát hiện → hồ sơ vụ việc

Ba nguyên hàm, đủ dùng, không cố làm SIEM đầy đủ.

**Dòng thời gian.** Một luồng sự kiện hợp nhất, lọc theo khoảng thời gian, `actor`,
`kind`, `tool_name`, và tìm toàn văn (FTS5). Sự kiện có phát hiện gắn kèm được tô
màu. Đây là màn hình dùng nhiều nhất.

**Pivot.** Từ một sự kiện, nhảy sang: mọi việc cùng `actor` đó ±30 phút; mọi lần dùng
cùng `tool_name`; lịch đã sinh ra `actor` này (khi `actor` dạng `schedule:<id>`);
tin nhắn kênh ngay trước đó (ứng viên nguồn injection). Pivot là thao tác điều tra
thật sự — trả lời "cái gì dẫn đến việc này".

**Hồ sơ vụ việc.** Gom nhiều phát hiện + sự kiện thành `case`, có giả thuyết, ghi
chú, dòng thời gian, và xuất báo cáo Markdown. Trạng thái: `open` →
`investigating` → `closed`. Phát hiện có `status` riêng (`triaged`,
`accepted_risk`, `false_positive`) để hàng đợi không phình mãi.

**Giảm nhiễu.** `suppressions` cho phép tắt có chủ đích, kèm lý do và hạn dùng — bắt
buộc có lý do, để sáu tháng sau còn biết vì sao. Hết hạn thì luật bật lại.

---

## 9. Vai trò của AI trong app

Qua `SpaceClient::llm_request_full`; luôn kiểm tra `finish == "length"` khi cần JSON
(quy ước bắt buộc: `apps/thinking/src/llm.rs:41`).

1. **Giải thích phát hiện** — dịch bằng chứng kỹ thuật sang lời thường: điều gì đã
   xảy ra, vì sao đáng lo, kiểm tra tiếp thế nào.
2. **Dựng giả thuyết cho vụ việc** — từ dòng thời gian, đề xuất chuỗi nhân quả và
   nêu rõ chứng cứ nào còn thiếu. Là *bản nháp cho người sửa*, không phải kết luận.
3. **Phân loại sơ bộ** — gợi ý "có vẻ dương tính giả vì…", người quyết định.
4. **Báo cáo** — Markdown/HTML từ một `case` hoặc từ khoảng thời gian.
5. **Hỏi bằng lời thường** — "tuần trước có gì bất thường không?" → app tự dịch sang
   truy vấn có sẵn (khoảng thời gian, actor, kind) rồi mới đưa kết quả cho LLM tóm
   tắt. LLM **không** sinh SQL.

**Điều AI tuyệt đối không làm:** không chấm mức nghiêm trọng, không đóng phát hiện,
không quyết định dương tính giả, không kích hoạt hành động.

**Bọc nội dung không tin cậy.** Mọi nội dung lấy từ log agent (kết quả tool, tin nhắn,
description MCP) đưa vào prompt phải bọc giữa `BEGIN_UNTRUSTED_EVIDENCE` /
`END_UNTRUSTED_EVIDENCE` kèm câu dặn không thực thi chỉ thị bên trong — đúng khuôn
mini-browser đã dùng (`apps/mini-browser/src/llm.rs:627`). Đây là yêu cầu sống còn:
app này *chuyên* đọc nội dung có thể chứa injection; một app phân tích injection mà
bị chính injection đó điều khiển thì tệ hơn là không có app.

---

## 10. REST + MCP surface

Mọi handler REST uỷ quyền cho `pub(crate) fn *_value(...) -> Value`; MCP `call_tool`
gọi **cùng** hàm đó — bất biến của kiến trúc Space App hiện tại
(`apps/thinking/src/api.rs:1`).

### REST (`/api`)

```
GET  /status                      ok, phiên bản, tình trạng từng nguồn ingest
GET  /dashboard                   thẻ tư thế, đếm phát hiện theo mức, xu hướng 14 ngày
POST /ingest/run                  chạy ingest ngay
GET  /events?from&to&actor&kind&tool&q&limit&cursor
GET  /events/:id                  chi tiết + phát hiện liên quan
GET  /events/:id/pivot?mode=actor|tool|schedule|preceding
GET  /findings?status&severity&rule&limit
POST /findings/:id/status         {status, note}
GET  /rules                       danh sách luật + trạng thái + tham số
POST /rules/:id                   {enabled, severity, params}
POST /scan                        chạy toàn bộ luật ngay (chọn khoảng thời gian)
GET  /snapshots?kind
POST /snapshots/take              chụp ngay
GET  /snapshots/diff?kind&from&to
GET  /cases         POST /cases   GET|POST /cases/:id   POST /cases/:id/notes
POST /cases/:id/report            sinh báo cáo Markdown
POST /explain                     {finding_id} → giải thích bằng AI
POST /ask                         {question, from?, to?} → hỏi bằng lời thường
GET  /verify-chain                kiểm tra chuỗi băm, trả vị trí gãy nếu có
GET  /suppressions  POST /suppressions  DELETE /suppressions/:id
GET  /settings      POST /settings
```

### MCP `sentinel-mcp` — 27 tool, prefix `sen_`

Chỉ-đọc và phân tích. **Không** có tool nào sửa được trạng thái daemon; hành động
đáp ứng (phase 4) chỉ nằm trên UI.

```
sen_status         sen_dashboard        sen_sources
sen_events         sen_event_detail     sen_pivot
sen_findings       sen_finding_detail   sen_finding_explain   sen_finding_status
sen_scan           sen_ingest
sen_rules          sen_rule_config      sen_suppress
sen_snapshots      sen_snapshot_take    sen_snapshot_diff
sen_cases          sen_case_open        sen_case_detail
sen_case_note      sen_case_hypothesis  sen_case_report
sen_ask            sen_tool_args        sen_verify_chain
```

`sen_tool_args` là tool phát sinh trong lúc implement, không có trong bản thiết
kế đầu: nó đọc `~/.senclaw/llm_logs` để **khôi phục đối số tool** — thứ mà
`tool_executions` của daemon vứt đi. Đây hoá ra là tool có giá trị pháp chứng
cao nhất, vì không có nó thì không trả lời được câu "agent đã đọc file nào, gọi
URL nào".

Một bài test bất biến (`no_tool_mutates_daemon_state`) sẽ gãy nếu ai đó thêm
tool có tên chứa `pause`/`delete`/`disable`/… vào MCP surface.

Ba bài test hợp đồng bắt buộc (mọi app mới đều có): đếm đúng số tool + tên duy nhất +
cùng prefix; mọi tool có `description` > 20 ký tự và `inputSchema.type == "object"`;
schema khớp hằng số Rust.

---

## 11. Web UI

React 19 + AntD 6 + Vite 8, `base: './'` (bắt buộc — iframe chạy cả ở origin app lẫn
qua proxy daemon).

**Chủ đề sáng/tối/theo hệ thống** (`web/src/theme.tsx`, theo khuôn `apps/news`):
`ThemeProvider` chọn `darkAlgorithm`/`defaultAlgorithm` của AntD, lưu lựa chọn ở
`localStorage` và đồng bộ lên `POST /api/settings` (tiện ích, hỏng thì bỏ qua chứ
không chặn việc đổi giao diện). Chế độ `system` bám `prefers-color-scheme` và đổi
ngay khi OS đổi — listener luôn bật kể cả lúc đang ở chế độ cố định, để quay lại
`system` là đúng ngay. Provider đặt `data-theme` + `color-scheme` trên `<html>`;
phần tự vẽ (biểu đồ hoạt động, viền header) đọc biến CSS trong `index.css` nên
không có màu nào bị kẹt một chế độ. `message` lấy qua `App.useApp()` để thông báo
cũng ăn theo chủ đề. Nút chọn là `Segmented` ba biểu tượng ở header.

Sáu tab:

1. **Tổng quan** — thẻ tư thế lớn (HITL bật/tắt, số server rủi ro, số luật wildcard,
   tình trạng chuỗi băm), đếm phát hiện theo mức, biểu đồ hoạt động 14 ngày, 5 phát
   hiện điểm cao nhất.
2. **Dòng thời gian** — bảng ảo hoá, bộ lọc trái, chi tiết phải, nút pivot.
3. **Phát hiện** — hàng đợi phân loại xếp theo điểm, thao tác hàng loạt, nút "Giải
   thích bằng AI", nút "Tạo vụ việc".
4. **Vụ việc** — danh sách + trang chi tiết (giả thuyết, dòng thời gian, ghi chú,
   xuất báo cáo).
5. **Luật** — bật/tắt, chỉnh ngưỡng, xem lần khớp gần nhất, quản lý suppression.
6. **Cấu hình & Ảnh chụp** — lịch sử ảnh chụp, xem diff kiểu git, nút chụp ngay.

Tab "Hỏi" gộp vào Tổng quan dưới dạng một ô nhập: "tuần này có gì bất thường?".

---

## 12. Bảo mật của chính app này

App tập trung dữ liệu nhạy cảm nhất trong hệ thống. Nó là mục tiêu giá trị cao, và
phải được thiết kế như vậy.

| Vấn đề | Xử lý |
|---|---|
| **Mọi Space App hiện bind `0.0.0.0`** (`TcpListener::bind(format!("0.0.0.0:{port}"))`) → lộ toàn bộ lịch sử agent ra LAN | Sentinel bind **`127.0.0.1`**, cố ý lệch khuôn mẫu. Có ghi chú rõ trong `main.rs`. Đồng thời `SEN-POSTURE-03` cảnh báo về các app khác. |
| Lộ bí mật trong bản chép | `redact()` chạy **trước** khi ghi `events`; không bao giờ lưu bản gốc. |
| App bị injection qua chính dữ liệu nó phân tích | Mọi nội dung agent-sinh vào prompt đều bọc `BEGIN_UNTRUSTED_EVIDENCE`; luật phát hiện là mã Rust, không phải LLM; LLM không sinh SQL, không gọi tool. |
| Ghi nhầm vào DB daemon | `mode=ro` + `query_only=ON`; không có đường ghi nào trong mã. |
| Sửa vết trong kho của app | Chuỗi băm + `sen_verify_chain`; thẻ trạng thái chuỗi hiện ngay trang Tổng quan. |
| App tự tạo nhiễu cho chính nó | Hoạt động của `sentinel` (`actor` = `sentinel*`) bị loại khỏi luật hành vi, nhưng vẫn ghi vào `events` để kiểm chứng chéo được. |
| CORS | Không dùng `CorsLayer::permissive()` như các app khác; chỉ cho phép origin của chính app. |

---

## 13. Kế hoạch triển khai theo phase

| Phase | Nội dung | Test |
|---|---|---|
| **1 — Kho & ingest** | `db.rs` (schema §5), đầu đọc chỉ-đọc cho `tool_executions`/`chat_events`/`scheduled_tasks`/`task_run_logs`, chuẩn hoá `events`, chuỗi băm, `redact()`, con trỏ ingest, `/status` `/events` `/verify-chain` | round-trip schema, tính bất biến chuỗi băm (sửa 1 dòng ⇒ phát hiện), chống trùng qua `src_key`, redact 8 mẫu bí mật |
| **2 — Luật & ảnh chụp** | Rule engine + 6 nhóm luật §6, ảnh chụp 9 nhóm + diff, `/findings` `/scan` `/snapshots` | mỗi luật ≥1 test dương + 1 test âm trên dữ liệu dựng sẵn; diff added/removed/changed |
| **3 — Điều tra & AI** | Pivot, `cases`, `case_notes`, báo cáo, `/explain` `/ask`, MCP 22 tool, Web UI 6 tab | test hợp đồng MCP, `extract_json` + xử lý `finish=="length"`, bọc untrusted |
| **4 — Đáp ứng (tuỳ chọn)** | Tạm dừng lịch, tắt MCP server, xoá luật quá rộng — chỉ trên UI, có xác nhận, ghi vào `events` | test đường ghi duy nhất, mặc định tắt |

Mục tiêu ~45–60 test, ngang mức các app mới nhất (news 79, autotest 37, capital 34).

**Điểm neo kiểm chứng phase 1:** chạy trên `~/.senclaw/senclaw.db` thật phải ra đúng
1 268 `tool_executions`, 1 192 `chat_events`, 13 `scheduled_tasks`, 363
`task_run_logs`. **Phase 2** phải tự phát hiện được 5 điều đã biết trước trên máy này:
`SEN-CTRL-01` (HITL tắt), `SEN-CTRL-03` (wildcard browser/code), `SEN-PERSIST-01`
(6 lịch ngoài `schedule_*`), `SEN-PERSIST-04` (1 log mồ côi), `SEN-POSTURE-03`
(app bind 0.0.0.0). Nếu không ra đủ 5 → luật sai, không phải máy sạch.

**Điểm đăng ký bắt buộc:** thêm `"apps/sentinel"` vào `members` của
[Cargo.toml](Cargo.toml); còn lại (MCP, skill, persona) tự động qua manifest.

---

## 14. Rủi ro đã biết

1. **Phụ thuộc schema daemon.** Đọc trực tiếp SQLite nghĩa là daemon đổi schema thì
   app gãy. Giảm thiểu: mỗi đầu đọc kiểm tra cột cần dùng qua `PRAGMA table_info` khi
   khởi động, thiếu cột thì tắt riêng nguồn đó và báo trên `/status`, không sập app.
2. **Dương tính giả.** `SEN-PERSIST-01` dựa trên heuristic tiền tố folder —
   `default`/`main` cũng có thể do người dùng tạo qua CLI. Vì vậy mức `high` chứ
   không `critical`, và mô tả phát hiện phải nói rõ đây là suy đoán.
3. **Đến sau sự việc.** Ingest 60 s và luật chạy theo mẻ. Không ngăn được gì đang
   xảy ra. Đó là lựa chọn có chủ ý (§4.4), nhưng phải nói rõ trên UI để không tạo
   cảm giác an toàn giả.
4. **`llm_logs` vừa là chứng cứ tốt nhất vừa là rủi ro lớn nhất.** 214 MB văn bản
   thuần chứa system prompt và tham số tool. Parse nó cho phép khôi phục tham số tool
   — nhưng cũng nhân đôi bề mặt lộ nếu app lưu lại. Quy tắc: **chỉ lưu chỉ mục và
   hàm băm, không chép nội dung**; hiển thị theo yêu cầu, đọc trực tiếp từ file.
5. **Ngưỡng bất thường cần dữ liệu nền.** `SEN-ANOM-02` cần ≥7 ngày; trước đó phải
   im lặng thay vì cảnh báo bừa.
6. **Không phải EDR.** App chỉ thấy cái daemon ghi lại. Agent gọi tool qua đường
   không ghi log (ví dụ `browser_server` không ghi DB) thì chỉ còn thấy dòng
   `tool_executions` chung chung. Cách khắc phục thật sự là thêm hook `PostToolUse`
   ghi tham số — thuộc về core, không thuộc app này; app nên **đề xuất** cấu hình hook
   đó cho người dùng.
