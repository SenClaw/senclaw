# Hiện trạng cho tính năng Project (session chat + kanban)

Ngày 2026-10-18 · Phương pháp: **đọc code, chưa chạy daemon** · Phục vụ: [design.md](../261018-1618-project-feature/design.md)

Nền đọc: `main` @ ede1e16 (đã có memory engine; worktree `.worktrees/senclaw-extractor-key`, chỉ đọc), web-app f12b16b, desktop 3c70b29.
Số dòng dưới đây là của nền đó. Checkout `senclaw/` hiện ở `feat/datagen-import` @ fbe6230 **không có** `src/memory/engine/` — làm tính năng này phải cắt nhánh từ `main`.

## 1. Kết luận ngắn

- Chưa có thực thể Project. Gần nhất: `GroupBinding` (session), `boards.workspace_dir` (kanban), bank của kho fact (= agent folder). Ba thứ không biết nhau.
- Hai yêu cầu (root folder, facts + context riêng) **khả thi trên kiến trúc hiện có**, phần lớn là nối dây: `projectId` trên session và board, một bank fact mới, một khối context mới trong system prompt.
- Có **3 chỗ sẽ làm hỏng nếu nối ngây thơ** (F1, F2, F3 dưới) — nên có spike P0 trước khi code.

## 2. Session chat = `GroupBinding`

| Điều đọc được | Chứng cứ |
|---|---|
| Một session là một hàng `groups`: `jid`, `folder` (= **agent profile**, không phải session), `group_type` (`chat`/`code`/`cowork`), `allowed_work_dirs`, `llm_config_id` | `src/types.rs:60`, `src/db/schema.rs:10-31` |
| `folder` **không unique**: nhiều session dùng chung một profile (web tạo jid `web:<folder>:<time>-<rand>`) | `schema.rs:12-16`, `web-app/src/pages/ChatPage.tsx:131-145` |
| Tạo/sửa qua WS `register:group` / `update:group`; chat web-only để `channel = ""` | `src/gateway/websocket_gateway/handlers.rs:611`, `:884` |
| `update:group` không dựng lại core; model đổi có hiệu lực lượt sau vì pool đọc binding mỗi lượt | `handlers.rs:961-963` |
| Groups còn được **phản chiếu vào `config.json`** qua `GroupConfigEntry` | `src/gateway/group_manager/types.rs:14`, `config.rs:35` |
| Cột mới vào `groups` đi bằng `ALTER TABLE … ADD COLUMN` có guard | `schema.rs:609-621` |
| `destroy_agent_full(jid)` tồn tại — ứng viên để dựng lại session khi đổi project | `src/agent/agent_pool/pool.rs:2704` |
| Số vị trí khớp `GroupBinding {`: ≈24 trong 15 file (gồm khai báo/chữ ký; cộng ≈8 `OneShotOptions {`, ≈8 `RecallQuery {`) — thêm trường là sửa gần hết số đó | grep thô, chưa lọc |

Client: web `registerGroup({ groupType, allowedWorkDirs, modelId, … })` ở `ChatPage.tsx:162-172`; desktop `NewChatScreen` (`new_chat_dialog.dart`, có `_kind`, `_workDir`). Chỉ kind `code` mới nhận thư mục (`allowedWorkDirs: opts.kind === 'code' && opts.workDir ? [opts.workDir] : null`).

## 3. Thư mục làm việc của session

- Chat thường: `senclaw_data_home()/workspace/<folder>` (mặc định theo **profile**). Code chat: phần tử đầu của `allowed_work_dirs` (`pool.rs:996-1011`).
- Trạng thái workspace là file `~/.senclaw/workspace-state-<folder>.json` — **khoá theo folder, không theo chat** (`pool.rs:842`, `:1012`).
- Mỗi lần tạo core, **code chat ghi đè** file đó bằng workspace của mình (`pool.rs:1015-1021`).
- Engine giữ cwd theo `jid` (`set_working_dir(&binding.jid, …)`, `pool.rs:1039`) — phần này an toàn.
- `setup_workspace_watcher` (poll file 300 ms, áp cho `jid`) chỉ có **định nghĩa**, không có chỗ gọi (`pool.rs:3262`; grep toàn `src/`). Nên `workspace_switch` ghi file nhưng cwd của engine không tự đổi theo.
- Các MCP con đọc file state qua env lúc spawn: `workspace`, `docs`, `datagen`; DAG đọc bằng `admin_folder`:
  `src/agent/dispatch_bridge/bridge.rs:1187`, `src/mcp/dispatch_server.rs:214`, `src/mcp/virtual_server.rs:97`, `src/mcp/bridge_servers.rs:47`.

## 4. Kanban

- DB riêng `~/.senclaw/space-apps/kanban/kanban.db` (`docs/kanban-guide.md`); **không FK được** sang `senclaw.db`.
- `boards(id, title, description, workspace_dir, …)` — `src/kanban/db.rs:17-24`; cột thêm sau đi qua mảng `MIGRATIONS` (`db.rs:106-114`).
- `KanbanDispatchProvider::claim_ready` đổi card thành `WorkItem`: `prompt` = title + description, `guidance` = chỉ dẫn `kanban_show/complete/block`, `workspace` = `Dir(board.workspace_dir)` | `Worktree` (label `worktree`) | `Scratch` (`src/kanban/dispatch.rs:58-100`).
- `WorkItem` là kiểu của `app-space-sdk` (workspace Cargo riêng, các Space App phụ thuộc) — **không có chỗ** cho project; không nên sửa SDK.
- Worker chạy `run_one_shot(OneShotOptions { prompt, working_dir, system_prompt, mcp_configs, … })` (`src/agent/mcp_dispatch/mod.rs:214-225`). Không có chat jid, không recall fact, không profile.
- Worker **không có persona** thì `system_prompt = item.guidance` (`mod.rs:200`). `assemble_system_prompt` chỉ dùng `SYSTEM_PROMPT` mặc định khi base rỗng (`zen_core/engine.rs:2157`), nên base của worker này **chính là** đoạn guidance, thay hẳn prompt mặc định. Hành vi hiện tại, ngoài phạm vi; hệ quả cho tính năng: khối project phải **nối vào** guidance (như persona + guidance ở `mod.rs:194-197`), không thay nó.
- Dispatcher bật/tắt **toàn cục** (`Settings → Autonomous tasks`), chưa có theo board/project.
- Kanban UI: web `KanbanPage.tsx` (form `workspace_dir` ở `:417`, `:446`), desktop `features/kanban/kanban_dialogs.dart`. Backend chat AI theo board (`/api/kanban/chat*`) có mà chưa UI nào dùng.

## 5. Kho fact (memory engine)

- Bảng `mem_facts(bank, origin, source, superseded_by, slot_*, …)`; unique `(bank, content_hash)` (`src/memory/engine/facts/store.rs:35-57`). `bank` = agent folder; `origin` = jid chat (rỗng = cả agent).
- `Scope::{Agent, Chat(jid)}` áp ở **mọi** lần đọc (`store.rs:189-225`); `Scope::Chat` dùng `memory_session_rank` (cùng chat 2, rỗng/cùng folder 1, chat khác −1) — `src/memory/curated.rs:325`.
- Mặc định facts **theo chat** (`engine_facts_by_chat`): fact của chat A không hiện ở chat B dù cùng profile. Kho "chung của agent" (origin rỗng) gần như **không còn ai ghi** (chỉ fact cũ/legacy và chế độ `SENCLAW_MEMORY_FACTS_SCOPE=agent`) — `docs/memory-engine.md` mục "Bẫy".
- Ghi: `retain_text(config, bank, chat_jid, text, source)` (`facts/mod.rs:136`) gọi từ cửa sổ reflection (`agent_pool/reflection.rs:179`) và compaction (`pool.rs:3071`). `bank` đã là tham số → **dùng được bank khác agent folder** không đổi schema.
- Đọc: `RecallQuery { text, folder, chat_jid, facts_by_chat, limit, now }` (`engine/types.rs:25`), dựng ở `pool.rs:1855-1870`; bốn arm fact dùng `scope(q)` (`facts/arms.rs:65`). Arm chung nhãn `facts`, danh tính `(arm, key)` — arm mới trên cùng kho **phải** dùng lại nhãn đó (`CLAUDE.md` "Fusion identity").
- Kết quả recall là **ngữ cảnh của lượt** (`set_turn_context`), không vào lịch sử. `MEMORY.md` là ghim, không phải arm (`pool.rs:3497`).
- Tool agent `memory_search_facts` / `memory_forget_fact` giới hạn theo folder từ env của MCP (tiến trình riêng, không vector).
- REST `GET/DELETE /api/memory/facts`, `GET /api/memory/facts/banks`; chưa có POST (không thêm tay được).
- Cột `source` có sẵn → fact nhập tay đánh dấu `manual` được mà không đổi schema.

## 6. Chỗ nhét context vào prompt

| Chỗ | Đặc điểm | Hợp cho project? |
|---|---|---|
| `assemble_system_prompt` (`zen_core/engine.rs:2141`), AGENTS.md **sau cùng** vì là chữ người gõ; có test ghim thứ tự | đọc lại mỗi lượt (`engine.rs:1764`), cacheable | **Có** — thêm một tham số ngay trước `operating_rules` |
| `set_user_defaults(jid, block)` (`agent_pool/engine.rs:586`, gọi ở `pool.rs:1806`) | mẫu "pool đẩy khối đã render vào engine mỗi lượt" | Mẫu để copy cho `set_project_context` |
| `collect_first_turn_context` (`engine.rs:2805`): profile + `SENCLAW.md`/`CLAUDE.md` đi ngược từ cwd, cắt `MAX_FLAT_FILE_CHARS` | một lần mỗi phiên | Đã có sẵn cho repo; context project **bổ sung**, không thay |
| `project_docs_block(working_dir)` (`prompt.rs:174`) | chỉ khi root có `docs/` | Tự chạy khi cwd = root, không cần sửa |
| `OneShotOptions.custom_rules` | **xem F2** | Không dùng |

## 7. Tên gọi trùng — cần tránh nhầm

| Tên | Đang là | Hậu quả với "Project" mới |
|---|---|---|
| `/api/code/projects/:id/groups`, `project_id` trong `code_sessions.rs:444` | hợp đồng app mobile cũ; **project = một code session** (`id == project_id == jid`) | Không đổi. API mới dùng `/api/projects` |
| `~/.senclaw/project-config.json` (`zen_core/config_manager.rs`) | cấu hình theo **thư mục làm việc**: allowed_tools, history, rules; tối đa 20 mục, đẩy cái cũ ra | Không dùng lại (giới hạn 20, chứa history). Khoá theo đường dẫn → vẫn đúng nếu root trùng |
| `mcp::config::project_scope_dir` | thư mục `.senclaw/mcp.json` của **tiến trình daemon** (`src/lib.rs:1794`), không theo chat | Không liên quan; root project **không** đổi MCP project scope (MCP theo project là ngoài phạm vi) |
| "Workspace" | (a) thư mục làm việc của chat, (b) Cowork workspace `~/senclaw/workspace/{name}/` có board, member, memory pool, (c) RFC Workspace Notes/Calendar | Dùng từ **Project**; ghi rõ Cowork vẫn là kiểu chat riêng, không gộp |
| `~/senclaw/project/` (có sẵn trên máy, `research/`, `todo/`) | thư mục cá nhân, không do daemon tạo | Dùng `~/senclaw/projects/<id>/` (số nhiều); không đụng thư mục cũ |

## 8. Phát hiện ảnh hưởng thiết kế

| # | Phát hiện | Hậu quả | Xử lý trong design |
|---|---|---|---|
| **F1** | File state workspace khoá theo `folder`; code chat ghi đè; MCP con và DAG đọc nó | Hai session cùng profile ở hai project khác nhau có thể thấy nhầm thư mục (docs/datagen/DAG). **Đã dựng lại ở P0** | Khoá file theo chat cho session thuộc project (D6); P0 dựng lại lỗi trước |
| **F2** | `OneShotOptions.custom_rules` → `ZenCoreOptions.custom_rules` (`isolated_runner.rs:224`) và **không ai đọc** trong `zen_core` (grep: chỉ định nghĩa + `config_manager` key) | Tiêm context vào worker qua `custom_rules` = **không làm gì**, im lặng | Nối vào `system_prompt`/`guidance`; ghi bẫy |
| **F3** | Worker kanban một-lần: không jid, không recall, không retain | "Dùng facts của project trong kanban" cần đường mới, không phải bật cờ | Recall + retain gọi trong `KanbanDispatchProvider` (D8) |
| **F4** | "Facts chung" mơ hồ: kho agent-wide gần như rỗng dưới scope theo chat | Nếu "chung" = kho agent-wide thì project chat chẳng nhận gì | Câu hỏi mở #1; thiết kế đọc cả hai mức |
| **F5** | `WorkItem` thuộc SDK dùng chung | Đổi SDK = đổi hợp đồng Space App | Chỉ ghép `guidance`/`prompt` trong provider |
| **F6** | `GroupConfigEntry` phản chiếu ra `config.json` | Thêm cột mà quên trường này → `projectId` mất khi round-trip | Thêm `#[serde(default)]` trường |
| **F7** | Kanban DB tách file | Xoá project để lại `project_id` mồ côi | Daemon điều phối: gỡ liên kết + chốt `workspace_dir` trước khi xoá |
| **F8** | Agent tự ghi context sẽ là kênh tiêm lệnh bền vững (nội dung web → `CONTEXT.md` → mỗi lượt) | Rủi ro bảo mật | Không có tool agent ghi context ở v1 |

## 9. Chưa kiểm chứng

- Báo cáo này là đọc code. F1, F2 và các câu về `destroy_agent_full`/cascade xoá bank **đã được chạy** ở P0 (daemon cô lập, LLM giả): xem [test-261018-1724-project-p0-spike.md](test-261018-1724-project-p0-spike.md). F1 đúng; F2 đúng; `destroy_agent_full` chạy được nhưng rò MCP con.
- Không đo độ tin cậy của extractor (gemma-4-e4b) khi phân biệt fact "của project" với fact cá nhân — design tránh phụ thuộc vào đó (D5), nhưng chưa có số.
- Không đọc phần UI sâu (SessionList web/desktop, kanban dialogs): chỉ biết điểm vào.
- ~~`destroy_agent_full` có dùng được…~~ Đã chạy ở P0 (d): dùng được về mặt dữ liệu, nhưng không có đường gọi ngoài lúc tắt daemon và không đóng MCP con.
- ~~Cascade xoá bank~~ Đã chạy ở P0 (e): đủ khi `foreign_keys=ON` (daemon bật); xoá một lệnh là O(n²).

## Câu hỏi chưa giải quyết

Xem cuối [design.md](../261018-1618-project-feature/design.md).
