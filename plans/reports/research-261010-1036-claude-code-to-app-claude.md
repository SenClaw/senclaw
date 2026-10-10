# Nghiên cứu: tách Claude Code khỏi daemon → Space App `app-claude` (chạy nền)

Ngày: 2026-10-10 · Loại: research (chưa triển khai) · Nhánh hiện tại: `feat/claude-code-cowork` (chưa release)

## 1. Mục tiêu

- Toàn bộ logic Claude Code worker (spawn `claude`, verify, review, fix rounds, Stop hook, runs, settings, UI) nằm
  trong repo riêng `app-claude`, `runtime.mode: "background"` — daemon khởi động cùng lúc, giám sát, restart khi chết.
- Daemon chỉ giữ phần **chung**: dispatcher, worktree, MCP bridge `/mcp`, sen-link.
- Non-goal: đổi hành vi worker; data migration (tính năng chưa phát hành → không có dữ liệu người dùng cần chuyển).

## 2. Hiện trạng: `src/claude_code/` (2 229 dòng) dính daemon ở đâu

| Phần | Phụ thuộc nội bộ daemon | Sang app thế nào |
|---|---|---|
| `runner.rs`, `verify.rs` | `util::file_perms` | Copy nguyên (thuần tokio/process) |
| `worker.rs` | **được `MCPDispatcher::execute` gọi trực tiếp**; `worktree::diff`; `senclaw_mcp` | Cần giao thức app-worker (§4.1); diff tự chạy `git diff <base>` |
| `hooks.rs` | route trên UI server daemon, `auth::token_from_headers` | App tự host `/hooks/stop` trên cổng loopback của nó |
| `registry.rs` | `auth::ct_eq` | Copy (thay ct_eq bằng hàm cục bộ) |
| `senclaw_mcp.rs` | `Config`, `Db`, `GroupBinding`, `mcp::helper::core_mcp_config` | **Không chuyển được** — app không có Config/DB. Thay bằng MCP HTTP của daemon (§4.2) |
| `settings.rs` + `cowork_sections.rs` | `config.json` → `claudeCode`, `GlobalConfig` | Config riêng trong thư mục dữ liệu của app |
| `api.rs` | router daemon | REST của app (`/api/status`, `/api/runs`, `/api/settings`) |
| `mcp_bridge.rs` | Config, Db, API token | **Ở lại daemon** — dùng chung cho mọi client ngoài (Claude Code tương tác, app-claude, sau này app khác) |

Ràng buộc Space App (đã kiểm chứng):
- Token `sca_` của app chỉ mở các route `/api/space/apps/<id>/…` (bridge `llm.request`, config, sqlite…).
  Daemon **cố ý không** truyền `SENCLAW_API_TOKEN` cho tiến trình con. App gọi được `/api/kanban/*` chỉ vì loopback được
  miễn ở `SENCLAW_AUTH_MODE=auto` — vỡ ngay dưới `always`. Không dùng làm hợp đồng.
- SDK dispatch hiện chỉ cho app làm **nguồn** việc (`DispatchProvider`: poll/heartbeat/reclaim/finalize), không có
  vai **worker**.
- App tự claim card qua REST sẽ **tranh với `MCPDispatcher`** (nó cũng claim card Ready có assignee `claude-code`).

## 3. Phương án

| | A. App tự poll kanban | B. Giao thức **app worker** (khuyến nghị) | C. App thành DispatchSource |
|---|---|---|---|
| Ý tưởng | App gọi `/api/kanban/*`, tự claim | Manifest khai báo assignee; dispatcher đẩy WorkItem cho app, app trả Outcome | App liệt kê việc của nó |
| Auth | Vỡ dưới `always` | Daemon gọi app qua loopback (như health/MCP); token dispatch có scope | — |
| Tranh chấp | Đua với dispatcher | Không — một chỗ claim | Sai vai (app không phải nguồn) |
| Worktree/lease/retry | App tự làm lại | Dùng lại của dispatcher | — |
| Mở rộng | Riêng Claude | Codex/Gemini/… cùng giao thức | — |

## 4. Thiết kế đề xuất (B)

### 4.1 Giao thức app-worker (daemon + SDK)

Manifest:
```json
"runtime": { "kind": "server", "mode": "background", "start": "./app-claude", "port": 4810, "healthPath": "/health" },
"workers": [{ "assignee": "claude-code", "aliases": ["claude"], "path": "/api/worker" }]
```

Daemon (`MCPDispatcher`): thay hook cứng `set_claude_code` bằng `AppWorkerRegistry` (nạp từ manifest các app đã cài).
Khi `item.assignee` khớp:
1. `resolve_workspace` như cũ (tạo worktree `senclaw/card-<id>`), gắn `working_dir`, `worktree.base`.
2. `POST http://127.0.0.1:<port>/api/worker/runs` với `{ item, working_dir, worktree, mcp: [...], timeout_secs }`
   (kèm token app trong header — app chỉ chấp nhận daemon) → `{ run_id }`.
3. Poll `GET /api/worker/runs/:id` mỗi vài giây trong khi heartbeat lease (vòng heartbeat sẵn có) → khi terminal nhận
   `Outcome` (Completed/Blocked/Failed/TimedOut) → `finalize` như hiện tại (`with_worktree` giữ nguyên).
4. App chết/không trả lời → `Failed` có lý do; supervisor background tự restart app; lease hết hạn → reclaim.
5. Cancel: `DELETE /api/worker/runs/:id` khi item bị huỷ/timeout.

Async (run_id + poll) thay vì giữ một HTTP request suốt 15–60 phút: chịu được restart daemon/app và proxy timeout.
SDK (`app-space-sdk`): thêm `worker` module — trait `Worker { start(job) -> run_id; status(run_id) -> RunStatus; cancel }`
+ `worker_router()` mount các route trên, giống cách `dispatch_router` làm cho nguồn việc.

### 4.2 MCP cho worker = MCP HTTP của daemon
- Daemon cấp **dispatch token** theo item (scope: chat của board/`mcpChatJid`, TTL = timeout item, thu hồi khi finalize) —
  tổng quát hoá "run token" hiện nằm trong `claude_code::registry`, chuyển sang `src/mcp/http_bridge` của daemon.
- WorkItem gửi app chứa `McpServerSpec::Http { name: "senclaw-core", url: ".../mcp/core", headers }` và `senclaw-kanban`.
  App ghi thẳng vào `--mcp-config` của `claude` (`type: "http"`) — không cần `Config`/`Db`.
- `/mcp` bridge giữ nguyên trong daemon (kèm session cap, id rewriting, scope instructions).

### 4.3 Bên trong `app-claude`
```
app-claude/
  senclaw-manifest.json   # mode background, workers, integration iframe, mcp (tuỳ chọn: claude_status/claude_runs)
  senclaw-hub.json        # permissions: exec `claude`, network loopback
  Cargo.toml              # app-space-sdk (git), tokio, axum, serde
  src/main.rs             # bind SENCLAW_BIND_HOST (mặc định 127.0.0.1), PORT, kiểm SENCLAW_TOKEN_ACCESS_APP
  src/{runner,verify,worker,hooks,registry,settings}.rs   # port từ senclaw/src/claude_code (đổi crate:: → cục bộ)
  src/api.rs              # /health, /api/worker/*, /api/status, /api/runs, /api/settings, /hooks/stop
  web/                    # UI: trạng thái binary, lệnh `claude mcp add`, cài đặt worker, bảng runs (port từ web-app)
  scripts/pack.sh         # như app-news: build web + cargo release + zip theo nền tảng
```
- Stop hook trỏ `http://127.0.0.1:<port app>/hooks/stop` với run token — app tự xác thực (như hiện nay).
- Settings: file JSON trong thư mục dữ liệu app (hoặc route `/config` của SDK), cùng schema `ClaudeCodeSettings`.
- `requires`: **không** khai `bin: ["claude"]` (miss cứng = app không khởi động) — app vẫn chạy và báo "chưa cài/đăng nhập".
- `sandbox`: không bật mặc định (worker cần ghi worktree, chạy `claude`, mạng). Nêu rõ trong README.
- Không gọi `/api/kanban/*`: mọi thứ đến qua WorkItem; tiến độ hiển thị trong UI app; tóm tắt về card qua Outcome.

### 4.4 Phần gỡ khỏi daemon / client
- Xoá `src/claude_code/{runner,verify,worker,hooks,registry,settings,api,senclaw_mcp}.rs`, `cowork_sections` (`claudeCode`),
  route `/api/claude-code/*`, `/hooks/claude-code/stop`, field `GlobalConfig.claude_code`.
- Giữ & đổi chỗ: `mcp_bridge.rs` → `src/mcp/http_bridge.rs` (+ dispatch token).
- Desktop/web: bỏ section Settings → Claude Code (UI chuyển vào app, hiển thị qua iframe Space Apps). sen-link giữ nguyên.
- Docs: `claude-code-cowork.md` → chuyển sang README app-claude; daemon docs mô tả giao thức app-worker.

## 5. Lộ trình

| Phase | Việc | Repo | Chấp nhận |
|---|---|---|---|
| P1 | Giao thức app-worker: manifest `workers`, `AppWorkerRegistry`, dispatch qua HTTP, dispatch token cho `/mcp` | senclaw, app-space-sdk | Một app giả lập nhận item và trả Outcome; card Done/Blocked đúng |
| P2 | Scaffold `app-claude` (`senclaw create app app-claude`), port worker/verify/hooks/runner/settings, REST + UI | app-claude | E2E với `claude` giả: Stop hook, verify, review, fix rounds như hiện tại |
| P3 | Gỡ `claude_code` khỏi daemon, chuyển bridge, bỏ section UI desktop/web | senclaw, desktop, web-app | `cargo test`, flutter test, web build pass; không còn tham chiếu `claude_code` |
| P4 | Đóng gói/phát hành: pack.sh, catalog marketplace, docs | app-claude | Cài từ zip, app background tự khởi động, card `claude-code` chạy |

Thứ tự an toàn: P1 → P2 chạy song song bản built-in (dispatcher ưu tiên app worker nếu có, fallback built-in) → P3 gỡ.

## 6. Rủi ro
- Giao thức app-worker là public contract mới của SDK — cần version (`SENCLAW_API_VERSION`) và test hợp đồng.
- Process `claude` là con của app: app restart giữa chừng → run mất; dispatcher phải coi `404 run` là Failed để reclaim.
- Background app luôn chạy (chi phí nhỏ: một process Rust idle).
- Trust boundary không đổi: worker vẫn chạy bằng quyền người dùng; README app phải giữ cảnh báo hiện có.

## Câu hỏi chưa giải quyết
1. Chọn phương án B (giao thức app-worker tổng quát) hay A (app tự poll kanban, nhanh hơn nhưng vỡ dưới auth `always`)?
2. Bỏ section Claude Code trong desktop/web, chỉ dùng UI của app (iframe)?
3. `/mcp` bridge giữ trong daemon (đề xuất) hay cũng chuyển?
4. Phân phối: cài từ marketplace hay bundle sẵn cùng desktop?
5. Giữ bản built-in làm fallback trong giai đoạn chuyển tiếp, hay gỡ hẳn ngay ở P3?
