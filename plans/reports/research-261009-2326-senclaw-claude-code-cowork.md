# Nghiên cứu: Cộng tác SenClaw + Claude Code, MCP hai chiều, sen-link (A2A)

Ngày: 2026-10-09 · Loại: research (chưa triển khai) · Phạm vi: senclaw daemon + repo anh em

## 1. Mục tiêu (outcome)

1. **SenClaw quản lý công việc → đẩy cho Claude Code làm** (worker), theo dõi tiến độ, nhận kết quả.
2. **SenClaw xác thực kết quả**: yêu cầu Claude Code phân tích/tự review, SenClaw chạy kiểm tra độc lập rồi mới đóng việc.
3. **MCP để Claude Code gọi ngược vào SenClaw** ở mức sâu: các MCP con (schedule, memory, wiki, dispatch, kanban, lịch…).
4. **sen-link**: kết nối SenClaw ↔ SenClaw (2 máy/2 người) qua giao thức A2A để đồng bộ và chia việc.

Non-goal giai đoạn đầu: không đưa engine/LLM vào daemon (giữ quyết định runtime split); không đụng token OAuth của Claude.

## 2. Hiện trạng codebase (đã kiểm chứng)

| Thành phần | Có sẵn | Ghi chú |
|---|---|---|
| ACP | `src/acp/` — `senclaw acp` là **ACP agent** cho Zed/JetBrains, relay sang WS gateway `:18789` | Chiều ngược lại: SenClaw **chưa spawn** Claude Code/agent CLI ngoài |
| MCP server stdio | `senclaw core-server` (`src/mcp/core_server.rs:416`), gộp ~19 server con: wiki, workspace, docs, memory, schedule, background, usage, dispatch, virtual, space, send, browser, ocr, patterns, litho, js, sandbox, user_profile | Mỗi con build từ env (`SENCLAW_GROUP_FOLDER`, `SENCLAW_CHAT_JID`, DB dùng chung qua `shared_env_db`) → **gắn với 1 chat, mở thẳng DB, không auth**. Không hợp để giao trực tiếp cho tiến trình ngoài |
| MCP qua HTTP | Kanban: `src/kanban/mcp.rs:71` `mcp_sse` + `/api/mcp/message` — SSE kiểu cũ, viết tay, protocol `2024-11-05` | Tiền lệ MCP-over-HTTP trong daemon; nên thay bằng Streamable HTTP chuẩn |
| rmcp | `rmcp = 1`, chỉ feature `transport-io` | Cần bật `transport-streamable-http-server` |
| Quản lý việc | Kanban (`src/kanban/`, SQLite, auto-dispatch Ready→In Progress), DAG dispatch (`src/agent/dispatch_bridge/`, `src/tools/dispatch.rs`), scheduler cron/interval/once + watch (`src/scheduler/`) | Có retry API `/api/dispatch/.../retry` |
| Cách ly | Worktree `~/.senclaw/worktrees/<repo-hash>/<name>` nhánh `senclaw/<name>` (`src/worktree/`), shadow checkpoints (`src/checkpoints/`) | Dùng lại nguyên cho worker Claude Code |
| Lịch | `src/mcp/space_sync/ical.rs` (parse iCal) + space_sync store | Lộ qua MCP space/space_sync |
| Auth | Token `~/.senclaw/api_token` / `SENCLAW_API_TOKEN`; Bearer/`X-SenClaw-Token`; `SENCLAW_AUTH_MODE` auto/always/off; token theo app `sca_…` (`docs/space-app-api-token.md`) | Mẫu `sca_` dùng được cho token theo-phiên của worker |
| Kết nối từ xa | `senclaw-hub-connector` (Go): relay WS `/v1/relay/ws`, frame mã hoá AES-GCM theo `channel_id`/`sender_id`; `senclaw-connect` pair bằng QR `senclaw://connect?...` | Thiết kế cho **client → 1 daemon**, chưa có khái niệm daemon↔daemon |
| A2A | **Không có** (không có agent card, peer, federation) | sen-link là hạ tầng mới |

## 3. Dữ kiện bên ngoài (nguồn chính thức, 2026-10)

**Điều khiển Claude Code** (code.claude.com/docs/en/cli-reference, /headless, /agent-view, /hooks):
- `claude -p --input-format stream-json --output-format stream-json --verbose` → tiến trình sống lâu, mỗi message stdin là một turn mới.
- `--session-id <uuid>` / `--resume <uuid>`, `--permission-mode` (default/acceptEdits/plan/auto/dontAsk/bypassPermissions), `--allowedTools`, `--max-turns`, `--max-budget-usd`, `--json-schema` (kết quả có cấu trúc được validate), `--append-system-prompt`, `--mcp-config … --strict-mcp-config`, `--permission-prompt-tool <mcp tool>`.
- `claude --bg` + `claude agents --json` / `claude logs` / `claude --resume <bg-id> "…"`: phương án đơn giản hơn nhưng ít kiểm chứng cho lập trình.
- Hooks có handler `http` (POST JSON kèm bearer): `Stop`, `SessionEnd`, `PostToolUse`, `TaskCompleted`, `PermissionRequest`… `Stop` có thể **chặn kết thúc** → ép "tự verify trước khi xong".
- Agent SDK chỉ có TS/Python → daemon Rust nên spawn CLI.
- Bỏ qua cho core: Channels (research preview, allowlist, không chạy với `-p`), ACP adapter (thêm lớp thừa), `claude mcp serve` (chỉ lộ tool Read/Edit, không phải API giao việc).
- **Chính sách**: chạy binary `claude` của chính người dùng, đã đăng nhập, trên máy của họ = an toàn. **Cấm** SenClaw đọc/lưu/chuyển token OAuth claude.ai, hay dùng subscription của người dùng phục vụ người khác (relevant cho sen-link). Vòng lặp tự động nặng có thể bị giới hạn theo gói → ghi rõ trong docs; hỗ trợ cả API key.

**Đăng ký MCP vào Claude Code** (/docs/en/mcp): `claude mcp add --transport http --scope user senclaw http://127.0.0.1:18788/mcp --header "Authorization: Bearer ${SENCLAW_TOKEN}"`; `.mcp.json` hỗ trợ `${VAR}`. Biến tên kiểu credential (`ANTHROPIC_API_KEY`) bị xoá trong header → dùng `SENCLAW_TOKEN`. Hỗ trợ elicitation, `list_changed`.

**A2A** (a2a-protocol.org, github.com/a2aproject/A2A): spec **v1.0.0**, Linux Foundation. Agent Card ở `/.well-known/agent-card.json` (+ `GetExtendedAgentCard` có auth). Method PascalCase: `SendMessage`, `SendStreamingMessage`, `GetTask`, `ListTasks`, `CancelTask`, `SubscribeToTask`, push-notification config. Binding JSON-RPC / gRPC / HTTP+REST. Task state: SUBMITTED, WORKING, INPUT_REQUIRED, AUTH_REQUIRED, COMPLETED, FAILED, CANCELED, REJECTED. Auth: API key, bearer, OAuth2, OIDC, mTLS. SDK Rust chính thức `a2aproject/a2a-rs` (crate `a2a-lf`, `a2a-server-lf`, `a2a-client-lf`) — độ chín vừa phải, cần spike + pin version. SDK cũ dùng `message/send` → không tương thích v1.

Phân vai giao thức: **SenClaw→Claude Code = spawn process**, **Claude Code→SenClaw = MCP**, **SenClaw↔SenClaw = A2A**.

## 4. Kiến trúc đề xuất

```
                 ┌──────────────────────── SenClaw daemon (máy A) ────────────────────────┐
 Kanban/DAG/     │  coder_worker (mới)            /mcp (Streamable HTTP, mới)   sen-link   │
 Schedule  ───►  │  spawn `claude -p stream-json` ◄── tool calls ───────────────  (A2A srv) │◄──► SenClaw máy B
                 │  worktree + session token       scoped by sca_ token          /a2a/v1   │     (agent card)
                 │  verify pipeline ◄── http hooks (Stop/SessionEnd) ──┐                    │
                 └──────────────────────────────┬──────────────────────┘────────────────────┘
                                                ▼
                                   Claude Code (binary của user, worktree riêng)
```

### 4.1 `coder_worker` — SenClaw giao việc cho Claude Code
- Module mới `src/coder_worker/` (Rust). Một "worker job" gắn với kanban card hoặc DAG task (`assignee: claude-code`).
- Vòng đời: `queued → preparing (tạo worktree) → running → verifying → needs_review | done | failed`.
- Spawn: `claude -p --input-format stream-json --output-format stream-json --verbose --session-id <uuid> --permission-mode <cfg> --max-turns N --max-budget-usd X --mcp-config <json tạm> --strict-mcp-config --append-system-prompt <brief>` với `cwd = worktree`.
- Brief = card + tiêu chí chấp nhận + yêu cầu kết thúc bằng JSON (`--json-schema`: `{status, summary, files_changed, tests_run, risks}`).
- Đọc stream-json → cập nhật card/chat (tool step display đã có), lưu `session_id` để `--resume` khi cần sửa.
- Quyền: mặc định `acceptEdits` + `allowedTools` hạn chế; yêu cầu quyền ngoài phạm vi → `--permission-prompt-tool senclaw.approve` → đẩy về UI SenClaw/Telegram (tái dùng luồng approve hiện có).
- Phát hiện binary: `which claude`, `claude --version`; trạng thái đăng nhập để người dùng tự xử lý.

### 4.2 Xác thực công việc (verify)
Ba lớp, không tin lời tự báo cáo của model:
1. **Gate trong phiên**: http hook `Stop` → `POST /api/coder/hooks/stop`; daemon kiểm tra đã có kết quả JSON hợp lệ + test đã chạy, nếu chưa → chặn và trả lý do (Claude tiếp tục).
2. **Kiểm tra độc lập** sau khi kết thúc: chạy lệnh verify của repo (cấu hình theo project: `cargo test`, `npm test`, lint) trong worktree; diff qua `/api/worktrees/.../diff`; checkpoint.
3. **Review**: phiên Claude Code thứ hai (hoặc agent nội bộ của SenClaw) ở chế độ `plan`/read-only nhận diff + tiêu chí → phán quyết `pass | changes_requested` có cấu trúc. `changes_requested` → `--resume` phiên gốc với feedback (giới hạn số vòng).
Kết quả → card chuyển `Review`; merge/PR vẫn do người dùng duyệt (API worktree merge/PR có sẵn).

### 4.3 MCP SenClaw cho Claude Code (gọi ngược sâu)
- Endpoint mới **`/mcp` Streamable HTTP** trên UI server `:18788`, dùng rmcp (`transport-streamable-http-server`), tái dùng `CoreServer` nhưng **mở rộng phạm vi**: thay vì env 1 chat, mỗi request mang token → xác định scope (profile/group/chat/worktree).
- Token: phát `sca_`-style token theo phiên worker (TTL, scope, danh sách tool cho phép), tiêm qua `--mcp-config`. Người dùng tự thêm thủ công bằng `claude mcp add --scope user` với token cá nhân có scope rộng hơn.
- Bộ tool đề xuất (namespace qua alias hiện có `docs/mcp-tool-alias.md`):
  - `task.*`: `get_assignment`, `report_progress`, `submit_result`, `request_input` (→ INPUT_REQUIRED), `ask_user`.
  - `kanban.*`: list/create/move/comment card (gom kanban SSE MCP cũ về đây, deprecate `/api/mcp/sse`).
  - `schedule.*`: tạo/list/hủy cron, once, watch (cho Claude hẹn "chạy lại test lúc 2h").
  - `calendar.*`: đọc/tạo sự kiện từ space_sync/iCal.
  - `memory.*`, `wiki.*`, `docs.*`: tra cứu ngữ cảnh dự án/người dùng.
  - `dispatch.*`: chia việc con cho agent SenClaw khác (cẩn thận vòng lặp đệ quy → giới hạn độ sâu).
  - `send.*`: gửi thông báo về Telegram/Zalo (qua approve).
- Ghi audit mọi tool call (control-plane trace có sẵn).
- Chính sách: tool có tác dụng phụ (send, schedule, dispatch) mặc định cần approve, trừ khi scope cho phép.

### 4.4 sen-link — SenClaw ↔ SenClaw qua A2A
- Crate/module mới `src/sen_link/` (hoặc repo `sen-link` nếu muốn tái dùng cho agent khác). Dùng `a2a-server-lf`/`a2a-client-lf` (spike trước, pin version), binding HTTP+JSON/JSON-RPC.
- **Agent Card** tại `/.well-known/agent-card.json`: tên profile, skills (`code.implement`, `code.review`, `research`, `schedule`…), securitySchemes; card mở rộng sau auth.
- **Pairing**: tái dùng mẫu QR + code 8 ký tự (`telegram-pairing.md`, senclaw-connect) → trao đổi khoá công khai + token; người dùng xác nhận 2 phía. Lưu peer trong bảng `peers` (id, card url, pubkey, scopes, trust level).
- **Transport**: LAN/Tailscale → gọi trực tiếp HTTPS (mTLS hoặc bearer). Sau NAT → đi qua **hub-connector**: cần mở rộng relay để định tuyến theo `peer_id` (hiện chỉ `channel_id/sender_id`), payload A2A nằm trong frame mã hoá — hub không đọc được.
- **Ánh xạ**: A2A Task ↔ DAG task/kanban card; `INPUT_REQUIRED` ↔ approve/ask_user; streaming SSE ↔ tool step; push notification ↔ webhook về daemon gửi việc. Artifacts = patch/diff, file, báo cáo.
- **Đồng bộ**: chỉ đồng bộ những gì được chia sẻ rõ ràng (một board kanban "shared", task được giao); không replicate toàn bộ DB. Code chuyển bằng git (push nhánh/bundle), không chuyển cả workspace. Mỗi bên giữ quyền phê duyệt trên máy mình.
- **An toàn**: message từ peer là dữ liệu không tin cậy (prompt-injection) → đi qua control-plane + approve; giới hạn tool theo trust level; peer B dùng Claude Code/subscription **của chính B** — không bao giờ chuyển token Claude giữa hai máy.

## 5. Lộ trình đề xuất

| Phase | Nội dung | Phụ thuộc | Tiêu chí chấp nhận |
|---|---|---|---|
| P0 Spike | Spawn `claude -p` stream-json từ Rust; chụp schema event + payload hook `Stop`; spike `a2a-lf` | — | Ghi lại schema thật; 1 task chạy end-to-end trong worktree |
| P1 coder_worker | Module job, worktree, stream → card/chat, kết quả `--json-schema`, resume, cancel | P0 | Card `assignee=claude-code` chạy xong, diff hiện trong UI |
| P2 MCP `/mcp` | Streamable HTTP + token theo phiên, CoreServer theo scope, tool `task.*`/kanban/schedule/calendar/memory | P1 (token) | `claude mcp add` kết nối được; worker gọi `task.submit_result` |
| P3 Verify | Hook Stop gate, verify command theo project, review session, vòng changes_requested | P1, P2 | Task có test fail không thể sang Done |
| P4 sen-link | Agent card, pairing, A2A server/client, ánh xạ task, direct transport | P2 | 2 daemon (scratch HOME, cổng 28788/28789) giao/nhận 1 task |
| P5 Relay + sync | Hub-connector định tuyến peer, board chia sẻ, push notification | P4 | Giao việc qua NAT, board đồng bộ 2 chiều |
| UI | Desktop (Flutter) + web-app: tab worker, approve, danh sách peer | song song P1+ | — |

Kiểm thử theo memory: daemon thật chạy 18788/18789 + `~/.senclaw` → test bằng `SENCLAW_HOME`/HOME tạm và cổng 28788/28789.

## 6. Rủi ro

- Schema stream-json/hook chưa kiểm chứng chi tiết → P0 bắt buộc.
- Giới hạn subscription khi chạy tự động liên tục; cần hỗ trợ API key và cấu hình budget.
- `CoreServer` hiện phụ thuộc env 1 chat → refactor thành theo-request là phần công việc lớn nhất của P2.
- `a2a-rs` mới, ít người dùng; có thể phải tự viết lớp JSON-RPC mỏng theo spec v1.
- Đệ quy dispatch (Claude → SenClaw dispatch → Claude…) → giới hạn độ sâu và budget tổng.
- Prompt injection qua peer và qua nội dung repo.

## Câu hỏi chưa giải quyết

1. Chế độ quyền mặc định của worker: `acceptEdits` + approve relay, hay `auto`/`dontAsk` trong worktree?
2. Reviewer ở bước verify: phiên Claude Code thứ hai hay agent nội bộ SenClaw (model local)?
3. sen-link phạm vi đầu: chỉ giao task, hay cả đồng bộ kanban/memory/lịch?
4. sen-link nằm trong repo senclaw hay repo riêng `sen-link` (theo mô hình sen-* anh em)?
5. Transport ưu tiên: trực tiếp (LAN/Tailscale) trước, hay relay qua hub-connector ngay từ đầu?
6. Có cần hỗ trợ worker khác (Codex CLI, Gemini CLI) cùng giao diện `coder_worker` không?
