# Thiết kế: Project cho session chat và Kanban

Status: **thiết kế đã chốt (2026-10-18), chưa code, sẵn sàng bắt đầu P1**; P0 spike đã chạy ([test-261018-1724-project-p0-spike.md](../reports/test-261018-1724-project-p0-spike.md)) và D6/D7 đã sửa theo kết quả · Nền: `main` @ ede1e16 (+ web-app f12b16b, desktop 3c70b29)
Bằng chứng và số dòng: [explore-261018-1618-project-feature-current-state.md](../reports/explore-261018-1618-project-feature-current-state.md) (F1–F8 dưới đây trỏ vào đó).
Kế hoạch giao việc: [plan.md](plan.md).

## 1. Yêu cầu

Người dùng (2026-10-18): thiết kế tính năng **project** cho session chat, dùng trong kanban.

1. Một project chọn **một folder root**; session và kanban dùng folder đó.
2. Project có **facts và context riêng** để mô tả dự án, vẫn dùng được **facts chung**.

Ràng buộc từ repo: không đổi hợp đồng `/api/code/projects` (app mobile), không đổi `app-space-sdk`, mọi cột/bảng thêm phải nâng cấp tại chỗ, không có project thì hành vi **y hệt hiện tại** (prompt, recall, cwd).

## 2. Mô hình

```
┌─ Project P ────────────────────────────────────┐
│ root            /abs/path (duy nhất, canonical)│
│ context         CONTEXT.md (ghim, sửa tay/UI)  │
│ facts           bank "project:P"               │
│ facts_write     project | chat | off           │
│ inherit_common  true | false                   │
└────────────────────────────────────────────────┘
    │ groups.project_id               │ boards.project_id
┌─ session chat ─────────────────┐    ┌─ kanban board ─────────────────┐
│ cwd = root                     │    │ thư mục = root                 │
│ system prompt + <project>      │    └────────────────────────────────┘
│ recall: fact P + fact chung    │
└────────────────────────────────┘
                                                    │ card Ready
                                      ┌─ worker một-lần ───────────────┐
                                      │ cwd = root                     │
                                      │ guidance + <project>           │
                                      │ prompt + recall fact P         │
                                      └────────────────────────────────┘
```

**Ai thấy gì** (cột = nơi chạy):

| Nguồn | Chat ngoài project | Chat trong P | Worker kanban của board ∈ P |
|---|---|---|---|
| Context của P | — | đọc (system prompt) | đọc (guidance) |
| Fact mức chat (`bank=profile`, `origin=chat`) | chat đó | chat đó | — |
| **Fact project** (`bank=project:P`) | — | đọc + ghi | đọc (+ ghi khi card xong) |
| **Fact chung** (`bank=profile`, `origin=''`) | đọc | đọc nếu `inherit_common` | — (worker không có profile) |
| `USER.md`, `MEMORY.md`, note curated | như hiện tại | như hiện tại | — (như hiện tại) |

Một project **không** thay agent profile: chat vẫn chọn profile (SOUL, `MEMORY.md`, model) như cũ.

## 3. Quyết định

| # | Quyết định | Vì sao | Bỏ phương án |
|---|---|---|---|
| D1 | **Thực thể mới** `projects` trong daemon | Cowork workspace có member/channel/board theo `docs/COWORK_DESIGN.md` (quá nặng); `boards.workspace_dir` chỉ là một chuỗi; `project-config.json` tối đa 20 mục, khoá theo đường dẫn, chứa history | Tái dùng ba thứ trên |
| D2 | Registry ở `senclaw.db`; context là **file** `~/senclaw/projects/<id>/CONTEXT.md` | Theo mẫu `profiles/<folder>/{SOUL,MEMORY}.md`: sửa tay được, nằm cùng data home, không làm bẩn repo của người dùng | Ghi vào `<root>/.senclaw/` (có thể bị commit; project-docs đã chọn "docs vào repo" vì là tài liệu, còn đây là metadata) |
| D3 | Một project = **một root** (canonical, unique); session và board N:1 | Đúng yêu cầu 1; root trùng gây hai bank cho một cây | Nhiều root (không ai yêu cầu) |
| D4 | Context vào **system prompt** (khối `<project>`), đọc lại **mỗi lượt**, cắt 8.000 ký tự (như `MEMORY.md` ghim) | Cùng chỗ và cùng luật với AGENTS.md: chữ người gõ nằm sau base prompt; sửa là có hiệu lực lượt sau; cacheable | Turn context (nằm cuối tin nhắn, không cache được như recall); first-turn (sửa không có hiệu lực tới phiên sau) |
| D5 | Fact project = **bank `project:<id>`**, `origin=''` (chung cho mọi chat của project). Nơi ghi theo `facts_write`, **không** nhờ extractor phân loại | Không đổi schema; banks/REST/Facts UI/`memory_forget_fact` chạy tiếp; xoá project = xoá một bank; `origin=''` cho hai chat cùng project **cùng thấy** và củng cố/thay thế fact của nhau. Extractor 2B–4B chưa đo được việc tách "fact của project" khỏi "fact cá nhân" | Cột `project_id` trong `mem_facts` (đụng unique index, mọi query phải thêm điều kiện); thêm trường `scope` vào JSON extractor |
| D6 | Session thuộc project có **file state workspace riêng theo chat** (`workspace-state-<folder>@<hash jid>.json`); DAG lấy workspace từ project **kể cả parent xếp hàng** | F1 đã dựng lại ở P0: `workspace_info`, `datagen` (đường tương đối), `doc_write` và DAG của session này thấy thư mục của session cùng profile. `Bash`/`Read`/`Write` vẫn đúng (cwd giữ theo jid) | Giữ file theo folder (hai project cùng profile sẽ giẫm nhau) |
| D7 | **v1: project gán lúc tạo session**; không đổi/gỡ project của session đang chạy | P0 (d): dựng lại core bằng `destroy_agent_full` giữ được lịch sử, cwd và env MCP, nhưng hàm này chỉ có lúc tắt daemon, **rò tiến trình `core-server` cũ**, và ghi đè file state của folder làm lệch session anh em (chỉ an toàn sau D6). Đổi project giữa chừng = làm đủ ba việc đó | Dựng lại core khi đổi project (làm sau; xem §11 #4) |
| D8 | Kanban: `boards.project_id`; thư mục worker = root; worker nhận **context + recall fact project** nối vào `guidance`/`prompt`; card hoàn thành → retain tóm tắt vào bank project | F2, F3, F5: không có đường sẵn, không sửa SDK | Sửa `WorkItem`; dùng `custom_rules` (không ai đọc) |
| D9 | **Agent không có tool ghi context.** Fact thêm tay qua REST/UI (`source='manual'`, không gọi LLM) | F8: nội dung web → `CONTEXT.md` → mỗi lượt là kênh tiêm lệnh bền vững | Tool `project_context_update` |
| D10 | v1: chỉ session **web-only** (`channel=""`), kind `chat` và `code`. Không cowork, không `schedule_*`, không chat kênh Telegram/Feishu | Chat nhóm có nhiều người; fact và context của project là dữ liệu của chủ máy | Gán cho mọi `GroupBinding` |

## 4. Dữ liệu

```sql
-- senclaw.db
CREATE TABLE IF NOT EXISTS projects (
  id             TEXT PRIMARY KEY,                 -- "prj_" + 8 hex, ổn định, dùng làm tên bank và thư mục
  name           TEXT NOT NULL,
  root           TEXT NOT NULL UNIQUE,             -- tuyệt đối, đã canonicalize
  description    TEXT NOT NULL DEFAULT '',         -- 1–2 dòng cho danh sách
  facts_write    TEXT NOT NULL DEFAULT 'project'
                 CHECK (facts_write IN ('project','chat','off')),
  inherit_common INTEGER NOT NULL DEFAULT 1,
  created_at     INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL,
  archived_at    INTEGER
);
ALTER TABLE groups ADD COLUMN project_id TEXT;     -- guard như schema.rs:609-621; + INDEX groups(project_id)
-- kanban.db (mảng MIGRATIONS, db.rs:106)
ALTER TABLE boards ADD COLUMN project_id TEXT;
```

- `GroupBinding.project_id: Option<String>`; `GroupConfigEntry.project_id` với `#[serde(default, skip_serializing_if = "Option::is_none")]` (F6).
- Fact: `mem_facts.bank = 'project:<id>'`, `origin = ''`, `source ∈ {reflection, compaction, kanban, manual}`. Không đổi schema.
- File: `~/senclaw/projects/<id>/CONTEXT.md`. `senclaw_data_home()/projects` — không đụng `~/senclaw/project/` có sẵn trên máy.
- Cross-DB (F7): không FK. Daemon điều phối xoá/lưu trữ.

**`facts_write`**: `project` (mặc định) — fact trích từ chat/worker của project vào bank project; `chat` — như hiện tại (project chỉ cho đọc fact project do người nhập tay); `off` — không trích gì từ các chat này.

## 5. Luồng

### 5.1 Tạo project
`POST /api/projects {name, root, …}` → kiểm root: tồn tại, là thư mục, canonicalize; **không phải** `/` hay `$HOME`; **không nằm trong** `senclaw_home()` (`~/.senclaw`: token, DB, mọi chat — cùng luật `datagen::doc_qa` đã dùng) hay thư mục `profiles/` và `projects/` của data home; chưa có project nào cùng root. Phần còn lại của data home (vd `~/senclaw/workspace/…`) được phép. Root lồng trong root khác được phép (monorepo). Trả thêm `hasClaudeMd`, `isGit` để UI nhắc "root đã có CLAUDE.md, context này bổ sung".

### 5.2 Session ↔ project
- WS `register:group` nhận `projectId`; `update:group` **từ chối** đổi hoặc gỡ `projectId` (D7). Từ chối khi `channel != ""`, `groupType ∉ {chat, code}` hoặc project đã lưu trữ.
- Lúc spawn (`get_or_create`): nếu có project thì `workspace_dir = project.root` cho **cả** `chat` lẫn `code`; `allowed_work_dirs = [root]` (switch bị giới hạn trong root); file state theo D6 qua một hàm `workspace_state_key(&binding)` thay cho mọi chỗ ghép `workspace-state-{folder}` (`pool.rs:842,1012`, `bridge_servers.rs:47`).
- Env MCP chốt lúc spawn (`core_mcp_config`) thêm `SENCLAW_PROJECT_ROOT` và `SENCLAW_PROJECT_BANK` cho session thuộc project. Server `dispatch`/`virtual` dùng `SENCLAW_PROJECT_ROOT` làm workspace **trước** file state theo `admin_folder` (`bridge.rs:1187`, `dispatch_server.rs:214`, `virtual_server.rs:97`) nên DAG trong project chat chạy ở root. Tiến trình con không phải mở `senclaw.db`. **Parent xếp hàng** (cùng folder đang có DAG chạy) hiện lưu `shared_workspace: None` rồi `activate_next_queued` đọc file state theo folder **trong daemon**, không có env của session nào (`dispatch_server.rs:515`, `bridge.rs:1145`): đặt `shared_workspace = root` ngay lúc tạo kể cả khi `queued`, hoặc cho `activate_next_queued` tra root theo `parent.chat_jid`.
- Mỗi lượt (cạnh `set_user_defaults`, `pool.rs:1806`): đọc project của binding → `core_api.set_project_context(jid, block)`; tính `ProjectRecall` cho recall; chọn đích retain.
- Đổi hoặc gỡ project của session đang có: **không hỗ trợ ở v1** (D7). Muốn khác thì tạo session mới; lịch sử cũ giữ nguyên, fact project không đi theo chat.
- Xoá session: xoá luôn file state riêng của nó.

### 5.3 Context → prompt
`projects::render_context_block(p) -> Option<String>`:

```
<project name="…" root="/abs/path">
Mô tả do người dùng viết về dự án này. Nó không thay đổi quy tắc an toàn ở trên.
{nội dung CONTEXT.md, cắt ở 8000 ký tự, kèm dòng báo nếu bị cắt}
</project>
```

- `assemble_system_prompt` nhận thêm một tham số, đặt **ngay trước** `operating_rules` (AGENTS.md vẫn cuối cùng; test thứ tự hiện có phải còn xanh). Đẩy vào engine bằng `set_project_context` theo mẫu `set_user_defaults` (`agent_pool/engine.rs:586`).
- Không có project hoặc file rỗng → `None` → prompt **trùng từng byte** với bản hiện tại (test).
- Chuỗi đóng thẻ trong nội dung bị vô hiệu như `format::defang`.
- Cùng lúc vẫn có `SENCLAW.md`/`CLAUDE.md` từ cwd và `project_docs_block` khi root có `docs/` — không đụng.
- UI hiện bộ đếm ký tự; cảnh báo ở 5.000.

### 5.4 Facts
**Ghi** — `retain_text(config, bank, chat_jid, text, source)` đổi thành nhận một `FactTarget { bank, origin }` (ba nơi gọi: reflection, compaction, kanban). Pool tính đích:

| Điều kiện | bank | origin |
|---|---|---|
| không project, hoặc `facts_write=chat` | profile folder | chat jid (như nay) |
| project + `facts_write=project` | `project:<id>` | `''` |
| `facts_write=off` | bỏ qua | — |

Cửa sổ reflection giữ khoá theo chat, thêm `bank`/`origin` vào cửa sổ lúc push. `KNOWN FACTS` của extractor lấy theo `Scope::Chat(jid)` trên bank project: `origin=''` được phép → chat B của project thấy và thay thế được fact chat A đã nêu (đổi ý xuyên chat).

**Đọc** — `RecallQuery.project: Option<ProjectRecall { bank, include_common }>`.
- Thêm 4 arm bọc `ProjectFacts<A>` quanh `SemanticArm/KeywordArm/GraphArm/TemporalArm`: không có `project` thì trả rỗng; có thì chạy `A` với `folder = bank`, `Scope::Agent` (mọi origin trong bank project). **Nhãn arm vẫn `facts`** (luật fusion).
- Bốn arm hiện có trả rỗng khi `include_common=false`.
- Tất cả vào cùng RRF + ngân sách 600 từ + một khối `<memory_context>`. Chưa nhân hệ số ưu tiên cho fact project (đặt 1.0); để eval quyết định.
- Tool agent: `memory_search_facts` đọc thêm bank project từ **env** (`SENCLAW_PROJECT_BANK`, đặt lúc spawn), không từ tham số — cùng luật "sở hữu lấy từ env chat" của watch.

**Nhập tay** — `POST /api/projects/:id/facts {text, kind?, when?}` → `FactStore::insert` (embed nếu có model, **không gọi LLM**), `source='manual'`. Retain không thay thế (`replaces`/slot) fact `manual`. Sửa = xoá + thêm.

**Chung → project, project → chung** (đưa fact đi giữa bank): để sau, cần tính lại `content_hash` và xử lý đụng unique `(bank, content_hash)`. Không nằm trong v1.

### 5.5 Kanban
- `POST /api/kanban/boards` nhận `project_id`; `GET` trả `project_id` và `effective_workspace_dir`. Quy tắc thư mục: `board.workspace_dir` (nếu đặt, phải nằm trong root) > `project.root` > scratch. Board có project thì UI ẩn ô "Workspace directory".
- Kanban nhận một `ProjectDirectory` (trait, gắn lúc `run_daemon`) thay vì mở `senclaw.db`; module vẫn tự đứng được khi không có project.
- `KanbanDispatchProvider::claim_ready` (không đổi `WorkItem`):
  - `workspace` = `Dir(effective)` / `Worktree` (label `worktree`) như nay;
  - `guidance` = chỉ dẫn card + khối `<project>`; nối vào, không thay (F3);
  - `prompt` = khối `<memory_context>` từ `pre_retrieval` (`ArmSelection{facts:true}` only, `folder=""`, `include_common=false`, truy vấn = title + description) + title + description.
- `finalize` khi `Completed` và `facts_write=project`: `retain_text(FactTarget{project:<id>, ''}, "Task \"<title>\" xong: <summary>", "kanban")`. Chỉ khi `complete`, không retain `block`/`gave_up`.
- `kanban/llm.rs` (AI board, breakdown): thêm context project (cắt 2.000 ký tự) vào prompt để card sinh ra hợp dự án.
- Chat trong project muốn tạo card: gắn `senclaw-kanban` vào MCP của session với env `SENCLAW_KANBAN_PROJECT=<id>` để `kanban_list_boards`/`kanban_create` mặc định theo board của project (P3, tuỳ chọn).
- Dispatcher vẫn **bật/tắt toàn cục**. Công tắc theo project là hướng sau.

### 5.6 Lưu trữ / xoá
- **Lưu trữ** (mặc định): `archived_at`; ẩn khỏi danh sách; session và board giữ nguyên; không nhận session mới.
- **Xoá**: hộp xác nhận nêu số session, board, fact. Session: gỡ `project_id` (giữ lịch sử); board: gỡ `project_id` và **chốt `workspace_dir = root cũ`** để vẫn chạy; fact: xoá bank **theo lô ≤500 hàng/lệnh, nhả khoá `Db` giữa các lô**, qua `Db` (FK bật) chứ không qua kết nối tự mở — cascade sang `mem_fact_entities`/`mem_links`/`mem_facts_fts` đã kiểm ở P0 (e), nhưng một lệnh `DELETE` là O(n²) do trigger FTS (20.000 fact ≈ 18 s chặn mọi truy vấn); xoá `CONTEXT.md`. **Không bao giờ xoá hay sửa file trong root.**

## 6. API

REST (cổng UI, sau token như các route khác):

```
GET    /api/projects?archived=            POST   /api/projects
GET    /api/projects/:id                  PATCH  /api/projects/:id          DELETE /api/projects/:id?purge=true
GET|PUT /api/projects/:id/context         # {content, updatedAt}; PUT từ chối nếu updatedAt cũ (409)
GET|POST /api/projects/:id/facts          DELETE /api/projects/:id/facts/:factId
GET    /api/projects/:id/chats            GET    /api/projects/:id/boards
POST   /api/projects/validate-root        # {path} → {ok, canonical, reasons[], hasClaudeMd, isGit}
```

- Route khai báo trong `ROUTER_SOURCES` (`ui_server/openapi.rs`) và sinh lại `docs/openapi-daemon.yaml` (`SENCLAW_WRITE_OPENAPI=1 cargo test --lib committed_spec_matches_the_routers`). Tham số route dạng `:id` (axum 0.7), không `{id}`.
- `GET /api/memory/facts/banks` giữ nguyên; UI ghép nhãn tên project cho bank `project:*`.
- WS: `groups`/`group:updated` thêm `projectId`. **Không có sự kiện `project:updated` ở v1** (đã chốt 2026-10-19): client làm mới sau mỗi thao tác của chính nó và khi cửa sổ lấy lại focus; hai cửa sổ cùng mở sẽ lệch tới lần làm mới kế.
- MCP: v1 **không thêm tool** (khối `<project>` đã nêu tên và root). Không có tool ghi context (D9). Gắn `senclaw-kanban` cho chat là tuỳ chọn ở P3.

## 7. Giao diện (web-app + desktop)

| Chỗ | Việc |
|---|---|
| Trang **Projects** (route mới `/projects`; web thêm nút ở `Sidebar.tsx`, desktop thêm vào `router.dart`/shell) | Danh sách + chi tiết, tab: Tổng quan (root, mô tả, `facts_write`, `inherit_common`), **Context** (soạn markdown, đếm ký tự), **Facts** (danh sách, thêm tay, xoá, bật "đã bị thay"), **Boards**, **Chats** |
| New chat (`NewChatScreen.tsx`, `new_chat_dialog.dart`) | Chọn Project (mặc định không); chọn thì khoá thư mục = root, kind `chat`/`code` |
| Danh sách session (`SessionList` web, `session_list.dart`) | Nhóm theo project (thu gọn được) + nhóm "Không project". Không có menu "Chuyển vào project" ở v1 (D7) |
| Kanban tạo/sửa board (`KanbanPage.tsx:417,446`, `kanban_dialogs.dart`) | Chọn Project; chọn thì ô Workspace thành chỉ-đọc "theo project"; danh sách board lọc theo project |
| Tab Facts của trang Knowledge | Bank `project:*` hiện tên project |
| i18n | Mọi chuỗi có Vietnamese (`desktop/lib/core/i18n/vi/`, `web-app` `vi.web.json`); grep trước khi thêm khoá (khoá toàn cục, spread sau thắng) |

## 8. Rủi ro và bẫy

| # | Rủi ro | Giảm |
|---|---|---|
| R1 | F1 (đã dựng lại ở P0): giẫm file state giữa session cùng profile. **Session ngoài project vẫn mang F1** — tiêu chí 2 chỉ hứa cho session trong project | D6 + test hai session/hai project/một profile; câu hỏi #10 |
| R2 | DAG trong project chat chạy sai thư mục (P0 b: chạy ở thư mục của session tạo core sau cùng của profile; parent xếp hàng đọc lại trong daemon) | `SENCLAW_PROJECT_ROOT` lúc spawn + `shared_workspace` đặt lúc tạo parent (5.2); test cả đường xếp hàng |
| R3 | F2: tiêm context qua `custom_rules` thành no-op | Dùng `system_prompt`/`guidance`; test kiểm khối **có mặt** trong prompt worker |
| R4 | Fact cá nhân trong chat project bị nhốt ở bank project (không thấy ở chat ngoài) | Đã chốt chấp nhận ở v1 (§11 #2): danh tính nằm ở `USER.md` qua `profile_update`; UI Facts của project nói rõ; đo ở P5 |
| R5 | Chat nhiều người ghi vào bank project | D10 |
| R6 | Tiêm lệnh qua context/fact | Context: chữ người gõ, đặt sau base + câu "không thay quy tắc an toàn"; fact: defang như hiện tại; không tool ghi context (D9) |
| R7 | Root nguy hiểm (`/`, `$HOME`, trong `~/.senclaw`) | Kiểm ở 5.1, kiểm lại lúc dùng (thư mục có thể bị xoá/đổi) |
| R8 | Hai chat/worker sửa cùng cây | Đã chốt không đổi hành vi (§11 #7); card nhãn `worktree` đã cô lập; nêu trong tài liệu |
| R9 | Token: context + recall chồng lên mỗi lượt | Cắt 8.000 ký tự (cùng cỡ `PINNED_MEMORY_MAX_CHARS`); ngân sách recall 600 từ dùng chung; đo ở P5 |
| R10 | Bề mặt đổi lớn: ≈24 vị trí khớp `GroupBinding {` (15 file, gồm cả khai báo), ≈8 `OneShotOptions {`, ≈8 `RecallQuery {`, `retain_text` 3 nơi gọi | Thêm trường có `Default`/builder; làm từng phase, `cargo check --all-targets` mỗi bước |
| R11 | Root bị xoá/đổi tên sau khi tạo | Session báo lỗi rõ "root không tồn tại" (không rơi về HOME); UI cho sửa root |
| R12 | Nhánh sai: checkout hiện tại không có memory engine | Cắt worktree từ `main` (xem plan) |
| R13 | Hàng đợi và Stop của DAG khoá theo `admin_folder` (P0 E1/E2): hai chat của hai project khác nhau dùng chung profile xếp hàng DAG với nhau, và Stop ở chat này huỷ DAG của chat kia (`bridge_api.rs:77`) | **Chấp nhận ở v1 (đã chốt)**; nêu trong tài liệu người dùng |

## 9. Kế hoạch triển khai (tóm tắt; chi tiết ở plan.md)

| # | Nội dung | Cỡ | Xong khi |
|---|---|---|---|
| P0 | Spike: dựng lại F1/F2, thử `destroy_agent_full`, thử cascade xoá bank | S | **Xong 2026-10-18**: 5/5 chạy, F1/F2 đúng, D6/D7 đã sửa; báo cáo `test-261018-1724-project-p0-spike.md` |
| P1 | Registry + REST + session binding + context + cwd/state file/DAG | L | Tiêu chí 1–3, 6 |
| P2 | Fact project: ghi, đọc, nhập tay, tool agent, eval | M | Tiêu chí 4 |
| P3 | Kanban | M | Tiêu chí 5 |
| P4 | Client web + desktop + i18n | L | Thao tác end-to-end trên hai client |
| P5 | E2E daemon cô lập, đo extractor, tài liệu `docs/projects.md`, CLAUDE.md | M | Báo cáo kiểm chứng |

P2 và P3 chạy song song sau P1 (khác file). P4 bắt đầu khi API ở P1 đã đứng.

## 10. Kiểm thử và eval

- **Unit**: validate root (symlink, `..`, đang nằm trong SenClaw home, trùng root); migration cột `groups.project_id` trên DB cũ; `GroupConfigEntry` round-trip file cũ; `workspace_state_key`; khối `<project>` (cắt, defang, không project ⇒ prompt trùng byte, thứ tự so với AGENTS.md); `FactTarget` theo ba giá trị `facts_write`.
- **REST** qua HTTP như `memory_facts.rs`: CRUD, 409 khi `updatedAt` cũ, xoá project gỡ liên kết.
- **Eval** (mở rộng `evals/memory/cases.json`, chạy `cargo test --lib memory::engine::eval`): (a) fact project P không bao giờ lọt sang chat ngoài hay project Q (`must_exclude`); (b) fact chung vẫn hiện trong chat P, mất khi `inherit_common=false`; (c) chat B của P trả lời được điều nói ở chat A; (d) đổi ý ở chat B thay fact của chat A; (e) worker kanban nhận fact P. Cổng: 22 câu hiện có không tụt; thêm arm project ≤ 150 ms p95.
- **Kanban**: `claim_ready` cho card board-có-project trả `workspace=Dir(root)`, `guidance` chứa khối project, `prompt` chứa recall; board không project y hệt cũ.
- **E2E** (P5) trên daemon cô lập: `SENCLAW_HOME`, `HOME`, **cwd** và cổng riêng (CLAUDE.md "Never develop against a live install"); lái chat qua WS như `scripts/evals/memory-changed-mind.mjs`. Ghi rõ model dùng (gemma-4-e4b qua LM Studio) và đừng suy ra kết quả cho model khác.

## 11. Quyết định đã chốt và việc để sau

1. ~~**"Facts chung" nghĩa là gì?**~~ **Đã chọn (2026-10-18): kho agent-wide (`origin=''`) của profile mà chat đang dùng**, như thiết kế. Lưu ý đã biết (F4): dưới scope theo chat mặc định kho này gần như rỗng, nên "dùng facts chung" trong project hiện chủ yếu là các fact legacy/upload; bank toàn cục xuyên agent **không** làm trong v1.
2. ~~Fact cá nhân trong chat project?~~ **Đã chốt (2026-10-18, giao cho thiết kế quyết): ở lại bank project ở v1** (D5, R4). Lý do: extractor nhỏ chưa đo được việc tách "của project" khỏi "của tôi"; danh tính đã có `USER.md` qua `profile_update`; xoá project là xoá sạch. UI Facts của project ghi rõ điều này. Làm sau nếu đo thấy mất fact cá nhân: nâng một fact lên mức chung bằng tay.
3. ~~Card xong tự retain?~~ **Đã chốt (2026-10-18, giao cho thiết kế quyết): bật, chỉ khi `complete` và `facts_write=project`** (5.5). Chỉ tóm tắt do worker tự viết, không retain `block`/`gave_up`. Công tắc là `facts_write` của project (`chat`/`off` tắt). P5 phải đo độ nhiễu; nếu rác thì đổi mặc định sang tắt. **Đã đo 2026-10-19 trên gemma-4-e2b (không phải e4b): rác ⇒ mặc định đổi sang TẮT** (`SENCLAW_DISPATCH_CARD_FACTS=true` để bật); số liệu: `plans/reports/test-261019-0112-project-p5-measure.md`.
4. ~~Cho phép chuyển project cho session đang chạy?~~ **Đã chốt (2026-10-18): không ở v1** — project chỉ gán lúc tạo session (D7). Làm sau nếu cần: đường gọi dựng lại core + đóng MCP con + D6.
5. ~~Cowork, `schedule_*`~~ **Đã chốt: ngoài v1** (D10); xét lại sau khi v1 chạy.
6. ~~Root lồng nhau~~ **Đã chốt: cho phép** (5.1); không tự gợi ý project theo thư mục.
7. ~~Mặc định `worktree` cho worker~~ **Đã chốt: không đổi hành vi hiện tại** (R8). Hai chat/worker vẫn có thể sửa cùng cây; muốn cô lập thì gắn nhãn `worktree` lên card. Nêu trong `docs/projects.md`.
8. ~~Công tắc dispatcher theo project~~ **Đã chốt: ngoài phạm vi**; dispatcher vẫn bật/tắt toàn cục.
9. ~~Đồng bộ context ra `<root>/.senclaw/PROJECT.md`~~ **Đã chốt: không** (D2).
10. ~~Sửa F1 cho mọi code chat?~~ **Đã chốt (2026-10-18): chỉ session trong project.** Hai code chat ngoài project cùng profile vẫn mang F1.
11. ~~Hàng đợi/Stop DAG theo profile~~ **Đã chốt (2026-10-18): ghi là hạn chế đã biết (R13)**, không khoá theo (folder + project) ở v1. Phải nêu trong `docs/projects.md` ở P5.
12. ~~E3, E4~~ **Đã chốt: ngoài phạm vi.** Ghi nhận là lỗi có sẵn, xử lý riêng: `doc_write` không nằm trong cấu hình MCP gộp mặc định (E3); registry persona nạp trước khi `virtual-agents/` tồn tại trên home mới (E4, chưa kiểm trên cài đặt thật).
