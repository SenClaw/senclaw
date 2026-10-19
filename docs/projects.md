# Project — một folder, một ngữ cảnh, một kho fact cho chat và kanban

**Project** gắn một **folder root** với: các session chat/code làm việc trong folder đó, một đoạn **context** (mô tả dự án)
được đưa vào system prompt mỗi lượt, một **kho fact riêng** mà mọi chat của project cùng đọc và ghi, và các **board kanban**
chạy card trong folder đó. Project không thay agent profile: chat vẫn chọn profile (SOUL, `MEMORY.md`, model) như cũ.

Trạng thái: phần daemon (registry, REST, WS, prompt, fact, kanban) đã xong và đã kiểm bằng test + chạy thật trên daemon cô lập
với **LLM và extractor là kịch bản**; trích xuất fact và worker đã đo thêm trên một model cục bộ nhỏ (e2b, **không phải** e4b của người dùng — mục 9).
Giao diện web/desktop đang làm riêng; tài liệu này nói về REST/WS. Thiết kế và lý do: [`plans/261018-1618-project-feature/`](../plans/261018-1618-project-feature/design.md).

## 1. Project là gì, tạo thế nào

| Trường | Ý nghĩa |
|---|---|
| `id` | `prj_` + 8 hex, đặt một lần. Dùng làm tên thư mục context và tên bank fact (`project:<id>`) |
| `name`, `description` | Tên, 1–2 dòng cho danh sách |
| `root` | Đường dẫn tuyệt đối đã canonical hoá, **duy nhất** giữa các project |
| `factsWrite` | `project` (mặc định) \| `chat` \| `off` — fact trích từ chat/worker của project đi đâu (mục 5) |
| `inheritCommon` | `true` (mặc định): chat trong project vẫn đọc fact chung của agent |
| `archivedAt` | Đã lưu trữ: ẩn khỏi danh sách, không nhận session/board mới, nhưng cái đã có vẫn chạy |

Tạo: `POST /api/projects {name, root, description?, factsWrite?, inheritCommon?}` → 201. Nên gọi trước
`POST /api/projects/validate-root {path}` để hiện lý do cho người dùng; câu trả lời có `hasClaudeMd` / `isGit` (root đã có
`CLAUDE.md`/`SENCLAW.md` thì context của project **bổ sung** cho file đó, không thay).

**Luật root** (`projects/root.rs`; từ chối kèm `reasons[].code`):

| Bị từ chối khi | `code` |
|---|---|
| rỗng; không phải đường tuyệt đối (`~` được mở rộng); không tồn tại; không phải thư mục; không đọc được | `empty`, `not_absolute`, `not_found`, `not_a_directory`, `unreadable` |
| là `/`, là `$HOME`, hoặc **chứa** `$HOME` (như `/Users`) | `filesystem_root`, `home_dir`, `contains_home` |
| nằm trong hoặc chứa `~/.senclaw` (token, DB, mọi chat) | `senclaw_state` |
| nằm trong `profiles/` hoặc `projects/` của data home | `senclaw_profiles`, `senclaw_projects` |
| đã là root của project khác (so theo đường canonical: `A/`, `A/../A` đều trùng) | `root_in_use` (409 khi tạo; 400 `invalid_root` cho các lý do còn lại) |

Root **nằm lồng** trong root của project khác thì được (monorepo). `PATCH /api/projects/:id` sửa được `name`, `root`
(kiểm đầy đủ lại), `description`, `factsWrite`, `inheritCommon`, `archived`. Root được kiểm lại **mỗi lần dựng core của một
session và mỗi lần card được nhặt** (folder có thể đã bị đổi tên hay xoá): hỏng thì session báo lỗi nêu tên project và mã lý do,
card vào Blocked — không bao giờ rơi về thư mục khác.

## 2. Session trong project

- **Gán lúc tạo**, qua WS `register:group` có `projectId`. Chỉ nhận session web-only (`channel` rỗng), `groupType` `chat` hoặc `code`,
  project chưa lưu trữ. **Không đổi, không gỡ, không gắn muộn**: `update:group` đổi `projectId` bị từ chối
  (`update:group failed: projectId cannot be changed…`); gửi lại đúng giá trị hiện có thì được. Muốn session ở project khác thì tạo session mới.
- **Thư mục**: `workspace_dir = root` cho cả `chat` lẫn `code`; `allowed_work_dirs = [root]` nên đổi thư mục chỉ trong root.
  Đường dẫn tương đối của Read/Write/Bash rơi vào root.
- **State file riêng theo chat**: session trong project có `workspace-state-<folder>@<12 hex của hash jid>.json`; session ngoài project vẫn dùng
  `workspace-state-<folder>.json` chung theo profile. Nhờ vậy `workspace_info`, `datagen` (đường tương đối), `doc_write` và DAG của hai
  session cùng profile ở hai project khác nhau không đổi thư mục của nhau (đã dựng lại lỗi và kiểm: P0 → P1).
- **DAG** (`dispatch_*`, `create_parent_and_run`) trong project chat chạy ở root, kể cả parent phải xếp hàng chờ DAG khác.
- Tiến trình MCP con biết project qua env lúc spawn (`SENCLAW_PROJECT_ROOT`, `SENCLAW_PROJECT_BANK`), không tra DB.
- Project bị xoá khi session còn trỏ vào: session chạy như chat thường, có cảnh báo trong log.
- `GET /api/projects/:id/chats` liệt kê session của project.

## 3. Context

Context là **file** `<data home>/projects/<id>/CONTEXT.md` (mặc định `~/senclaw/projects/<id>/CONTEXT.md`), do **người** viết — sửa bằng editor
bất kỳ hoặc `PUT /api/projects/:id/context {content, updatedAt}`. `updatedAt` là mtime (ms) mà client đã đọc; file đã đổi sau đó thì 409
`context_changed` kèm bản hiện tại để gộp. Tối đa 100 KB (413 nếu hơn).

- Đọc lại từ đĩa **mỗi lượt**: sửa xong, lượt kế tiếp đã có hiệu lực.
- Vào system prompt thành khối `<project name="…" root="…">`: câu mở đầu nói nội dung này "không thay đổi quy tắc an toàn", rồi nội dung **cắt ở 8.000 ký tự**
  (cắt thì có dòng báo). Khối nằm **ngay trước** quy tắc người dùng; `AGENTS.md` vẫn là phần cuối. Chuỗi `</project` trong nội dung bị vô hiệu.
- Không có project, hoặc file rỗng/trắng ⇒ prompt **trùng từng byte** với bản không có project.
- **Agent không có tool ghi context.** Một file agent sửa được sẽ mang nguyên văn trang web nó vừa đọc vào mọi lượt sau. Cùng lý do, fact nhập tay đi qua REST, không qua tool.
- Worker kanban của board gắn project cũng nhận khối này (mục 6). Client nên cảnh báo từ ~5.000 ký tự (`chars`/`maxChars` có trong câu trả lời `GET`).

## 4. Facts — ba mức

| Mức | Nằm ở | Ai thấy |
|---|---|---|
| **Chat** | bank của profile, gắn jid chat (`origin`) | chỉ chat đó |
| **Project** | bank `project:<id>`, không gắn chat | mọi chat của project + worker kanban của board gắn project; **không** chat ngoài project, không project khác |
| **Chung** | bank của profile, `origin` rỗng | chat ngoài project; chat trong project nếu `inheritCommon` |

Mỗi lượt, recall của chat trong project chạy cùng bốn chiến lược trên **cả hai** bank rồi gộp vào một khối `<memory_context>` (ngân sách 600 từ chung). Chi tiết
kỹ thuật: [`memory-engine.md`](memory-engine.md). Đích **ghi** do `factsWrite`:

| `factsWrite` | Fact trích từ hội thoại / card xong đi đâu |
|---|---|
| `project` | bank project, không gắn chat ⇒ chat A nói thì chat B của project nhớ; chat B đổi ý thì fact của chat A bị thay |
| `chat` | như chat ngoài project (bank profile, gắn chat). Project chỉ **đọc** fact project do người gõ. Card xong không lưu gì |
| `off` | không trích gì từ các chat này; vẫn đọc fact project có sẵn |

`factsWrite` đọc **mỗi lần** nên đổi có hiệu lực ở cửa sổ/tóm tắt kế tiếp. `inheritCommon=false` làm bốn arm của bank profile trả rỗng: chat chỉ còn fact project
(kể cả mất fact mức chat của chính nó — xem mục 8).

**Nhập tay và xoá** (không gọi LLM):

```
GET    /api/projects/:id/facts?q=&limit=&history=     # bank, total, facts[] (source: reflection|compaction|kanban|manual)
POST   /api/projects/:id/facts {text, kind?, when?}    # 201 → {fact, existing:false}; cùng chữ đã có → 200 existing:true
DELETE /api/projects/:id/facts/:factId
```

`text` 8–600 ký tự; `kind` `world` (mặc định) \| `experience`; `when` dạng `2026-03` / `2026-03-02` / `a/b`. Chứa secret ⇒ 400 `secret_in_fact`.
Fact `manual` **không bao giờ bị hội thoại thay thế** (kể cả khi model nêu `replaces` hay trùng slot); sửa = xoá + thêm. Chat trong project còn có `memory_search_facts`
(đọc bank project, từ env lúc spawn) và `memory_forget_fact` (xoá được cả fact project). `GET /api/memory/facts/banks` liệt kê cả bank `project:*`.

**Fact cá nhân nói trong chat project ở lại bank project** (quyết định #2): extractor không phân loại "của dự án" với "của tôi". Danh tính người dùng nằm ở `USER.md` qua `profile_update`
và không đi qua đây. Muốn một fact có mặt ở mọi nơi, ghi nó ở chat ngoài project hoặc vào `USER.md`.

## 5. Lưu trữ và xoá

`DELETE /api/projects/:id` = **lưu trữ** (hoàn tác bằng `PATCH {archived:false}`): session, board, fact, context nguyên vẹn; chỉ không nhận session/board mới.
`DELETE /api/projects/:id?purge=true` xoá thật:

1. xoá bank fact theo lô 500 hàng (xoá bank **trước**, hàng project **sau** cùng — lỗi giữa chừng còn project để purge lại);
2. session: gỡ `projectId` (giữ lịch sử), xoá state file riêng của từng session; chúng tiếp tục như chat thường;
3. board: gỡ `project_id` và **chốt `workspace_dir` = root cũ** (card vẫn chạy ở đó);
4. xoá `CONTEXT.md`. **Không bao giờ xoá hay sửa file trong root.**

Trả `{deleted, deletedFacts, detachedChats}`.

## 6. Kanban

- Board gắn project lúc tạo: `POST /api/kanban/boards {title, project_id, workspace_dir?}`. `GET /boards` và `/board` trả `project_id` và `effective_workspace_dir`.
  `GET /api/projects/:id/boards` liệt kê board của project.
- **Thư mục chạy** = `workspace_dir` của board (nếu đặt: tuyệt đối, không có `..`, nằm trong root sau khi resolve symlink; sai ⇒ 400) > `root` > thư mục scratch
  (board thường). Root bị `PATCH` sau khi tạo board mà `workspace_dir` rơi ra ngoài thì bị bỏ qua, dùng root. Project đã lưu trữ không nhận board mới (400).
- **Worker nhận gì** (`claim_ready`): system prompt = chỉ dẫn card **rồi** khối `<project>` (nối vào, không thay; `custom_rules` không có tác dụng — xem mục 8);
  prompt = khối `<memory_context>` của **bank project và không gì khác** (không fact chung, không project khác; đọc cả khi project đã lưu trữ hay `factsWrite` là `chat`/`off`) + tiêu đề + mô tả. Recall chờ tối đa 3 giây cho
  cả một tick; lỗi/quá hạn thì card chạy không có khối. Board thường: `WorkItem` trùng từng trường với trước.
- **Card xong lưu fact — mặc định TẮT** (`SENCLAW_DISPATCH_CARD_FACTS=true`, toàn daemon, đọc lúc khởi động). Bật thì: khi `factsWrite=project` và **worker tự** gọi `kanban_complete` với summary không rỗng, daemon retain nền
  `Kanban task "<tiêu đề>" completed: <summary>` (cắt 4.000 byte) vào bank project, `source=kanban`, không gắn chat; card kế tiếp của project được nghe nó.
  **Không** retain dù đã bật: `kanban_block`, card để dispatcher tự đóng ("auto-closed"), summary rỗng, `gave_up`/hết giờ, `factsWrite` `chat`/`off`, board thường. Retain hỏng không làm hỏng card.
  Tắt vì số đo trên e2b (mục 9): summary rỗng như "Done." vẫn sinh fact "Kanban task X đã hoàn thành" (8/8 summary rỗng ở 2 lần, 7/8 ở lần còn lại) và 31–44% fact lưu được là loại đó hoặc bản chép lại fact khác.
- Board AI (`/generate`, `/breakdown`) nhận context project cắt ở 2.000 ký tự; khung chat của board thì không.
- Lưu trữ project **không** gỡ board; chỉ purge gỡ (mục 5). Root mất ⇒ card Blocked với `gave_up: project "<tên>" cannot be used: …`.
- Muốn cô lập hai card cùng sửa một cây: gắn nhãn `worktree` lên card (xem [`worktree-isolation.md`](worktree-isolation.md)). Mặc định không đổi.

## 7. REST / WS

| Việc | Gọi |
|---|---|
| Danh sách (`archived=false\|true\|all`), tạo | `GET`, `POST /api/projects` |
| Kiểm root | `POST /api/projects/validate-root {path, projectId?}` → `{ok, canonical, reasons[], hasClaudeMd, isGit, inUseBy}` |
| Đọc, sửa, lưu trữ / xoá | `GET`, `PATCH`, `DELETE /api/projects/:id[?purge=true]` |
| Context | `GET`, `PUT /api/projects/:id/context` |
| Fact | `GET`, `POST /api/projects/:id/facts`; `DELETE /api/projects/:id/facts/:factId` |
| Session, board của project | `GET /api/projects/:id/chats`, `/boards` |
| Board kanban gắn project | `project_id` trong `POST /api/kanban/boards`, `/generate`; `project_id`, `effective_workspace_dir` trong `GET` |
| Gán session | WS `register:group {…, projectId}`; `groups`, `group:registered`, `group:updated` mang `projectId` |

Lỗi dạng `{"error": <mã>, …}`: `project_not_found` (404), `invalid_root` (400, `reasons`), `root_in_use` (409, `projectId`, `projectName`), `invalid_project`, `context_changed` (409), `fact_not_found`.
Body REST từ chối trường lạ (gõ sai tên là lỗi). **Không có** sự kiện WS `project:updated`: client làm mới sau thao tác của chính nó và khi lấy lại focus; hai cửa sổ cùng mở sẽ lệch tới lần làm mới kế.
Route nằm trong `ROUTER_SOURCES`; spec: [`openapi-daemon.yaml`](openapi-daemon.yaml).

## 8. Hạn chế & bẫy

1. **Session ngoài project vẫn mang lỗi workspace dùng chung theo profile**: hai code chat cùng profile vẫn thấy thư mục của nhau qua `workspace_info`/`datagen`/`doc_write`. Chỉ session trong project được sửa.
2. **Hàng đợi và Stop của DAG khoá theo profile**, không theo project: hai chat của hai project khác nhau, cùng profile, xếp hàng DAG với nhau, và Stop ở chat này huỷ DAG của chat kia.
3. **Không đổi project giữa chừng** (mục 2). Dựng lại core đang chạy cần đường gọi mới + đóng MCP con (hiện rò tiến trình) — chưa làm.
4. **Đổi root hoặc purge không dựng lại core đang chạy**: session đang mở giữ root cũ và `SENCLAW_PROJECT_BANK` cũ tới khi core dựng lại (khởi động lại daemon). Khối `<project>` thì biến mất ngay lượt sau purge. Card kanban đọc root mỗi lần nhặt nên không bị.
5. **Hai chat hoặc hai worker có thể sửa cùng một cây** (không khoá). Chỉ card nhãn `worktree` được cô lập — đã kiểm bằng test đơn vị, chưa chạy thật tạo worktree trong root của project.
6. **Fact cá nhân ở lại bank project** (mục 4). Hệ quả: với `SENCLAW_MEMORY_FACTS_SCOPE=agent`, fact nói trong chat project không còn hiện ở chat ngoài project. Với scope mặc định (theo chat) điều này không đổi gì so với trước ở chat ngoài.
7. **`factsWrite=chat` + `inheritCommon=false`**: chat không còn thấy cả fact mức chat của chính nó.
8. **Purge bank lớn chậm**: 20.000 fact ≈ 23 giây (trigger FTS bậc hai); các yêu cầu khác đợi tối đa ~1,1 giây nhờ nghỉ giữa lô, nhưng bản thân phản hồi purge chậm — client cần timeout đủ dài. Bank vài trăm đến vài nghìn fact thì dưới một giây. Một cửa sổ reflection đang chờ flush lúc purge có thể tạo lại vài fact trong bank vừa xoá (không project nào sở hữu); chưa có test.
9. **Session `code` của project vẫn hiện ở `GET /api/code/sessions`** (hợp đồng app mobile liệt kê mọi `group_type=code`).
10. **`OneShotOptions.custom_rules` không có tác dụng** — không ai đọc nó; guidance của workflow và `continuity_context` của tác vụ nền đi cùng đường nên cũng không vào prompt. Đừng dùng để đưa context vào worker; worker kanban của project nhận context bằng cách nối vào `guidance`.
11. **`kanban-server` stdio không có context project**: `kanban_breakdown_card` do worker tự gọi qua stdio không kèm context; đường qua daemon (REST, `/api/kanban/mcp/*`) thì có.
12. **Fact gõ tay thắng fact do card nói sau**: fact `manual` không bị thay, và recall gộp hai câu gần giống làm một nên có thể chỉ hiện giá trị cũ (đã gặp khi "cổng 7421" gõ tay và "cổng 8123" từ card). Với model thật, card là nơi giá trị đổi nhiều nhất.
13. **Summary của worker thành fact tự động** (chỉ khi bật `SENCLAW_DISPATCH_CARD_FACTS`; có thể sau khi worker đọc nội dung web): cùng loại kênh với reflection của chat (fact là câu khai báo ngắn, đóng thẻ bị vô hiệu, secret bị quét), nhưng là đường ghi tự động duy nhất vào bank project không qua người gõ, và đã đo là nhiễu (mục 9). Bật rồi vẫn tắt riêng từng project được bằng `factsWrite=chat` hoặc `off`.
14. Chỉ macOS được kiểm; Windows (đường `\\?\`) chưa.

## 9. Đã đo trên model nhỏ cục bộ, và cái chưa đo

**Model đo: `mlx-community/gemma-4-e2b-it-OptiQ-4bit` qua `sen-mlx` (2026-10-19), KHÔNG phải `google/gemma-4-e4b` của người dùng** (máy LM Studio tắt lúc đo, máy này không có weights e4b).
e2b nhỏ hơn nên nhiễu nghiêng bi quan. Mẫu nhỏ, 3 lần chạy mỗi phép đo; chi tiết, ngưỡng đã chốt trước khi nhìn số và cách chạy lại cho e4b: báo cáo `test-261019-0112-project-p5-measure.md`.

- **Fact cá nhân trong bank project** (12 cửa sổ hỗn hợp, 3 lần): 41–50% fact lưu vào bank project là cá nhân hoặc lẫn (trung bình 45%). Hỏi lại câu cá nhân từ chính chat đó: 6–7/7 như khi chưa có project; từ chat khác trong project: 6–7/7 (có thêm); từ chat **ngoài** project: 0/7 trước và sau với scope mặc định (theo chat), nhưng với `SENCLAW_MEMORY_FACTS_SCOPE=agent` mất từ 6–7/7 xuống 0/7. Không fact dự án nào lọt ra chat ngoài (0 lần).
- **Đổi ý xuyên cửa sổ không hoạt động với e2b**: 3/3 lần, "cổng 8420 → 8431" lưu thành fact mới nhưng fact cũ **không** bị thay (`superseded 0`), nên cả hai cùng được recall. Đường thay thế có kiểm chứng bằng extractor kịch bản chỉ chứng minh cơ chế.
- **Card xong lưu fact** (28 summary, 3 lần): quyết định #3 đổi sang **TẮT** theo ngưỡng đã chốt trước khi nhìn số (báo cáo, mục 7). 8 summary vốn không có gì đáng lưu đều sinh fact (8/8, 8/8, 7/8); 31–44% fact lưu được là rác (câu "đã hoàn thành" trống rỗng, bản chép lại fact của card khác bằng tiếng khác hoặc gán nhầm card, câu hỏi của worker); secret không vào fact (0/3). Worker kế tiếp được nghe đúng 5–6/7 điều hỏi.
- **Worker thật** (8 card, e2b): 0 card đóng bằng `kanban_complete` kèm summary. Tool kanban nằm ngoài roster của worker (phải nạp bằng `ToolSearch` trước) và model **không gọi `ToolSearch` lần nào**: 2 card thử gọi thẳng `kanban_complete`/`kanban_show` và nhận "No such tool available", 6 card không gọi tool kanban nào. 7 card do dispatcher tự đóng (làm đúng việc 5/8), 1 card gọi `AskUser` rồi treo tới hết giờ (902 giây, đặt timeout 900) → Blocked. Hệ quả: với model này không fact nào từ card được lưu, bật hay tắt.

Chưa đo: tất cả các phép đo trên, **trên `google/gemma-4-e4b` thật** (ba lệnh chạy lại trong báo cáo); worker thật với model biết nạp tool bằng `ToolSearch`; embedding model thật cho nhánh semantic (eval dùng embedding giả lập, đo thật chạy không embedding như cài đặt mặc định).

## 10. Chạy lại và tham chiếu

Test: `cargo test --lib projects`, `cargo test --lib memory::engine::eval` (ca `projects` 10 + `worker` 4). Daemon cô lập (`SENCLAW_HOME`, `HOME`, cwd, cổng riêng; **đặt `SENCLAW_BASE_URL` = cổng UI của chính nó**
và `SENCLAW_APP_TOKEN_MODE=off`, nếu không board AI gọi nhầm cổng 18788): `plans/reports/project-p0-spike/`, `project-p1-e2e/`, `project-p2-e2e/`, `project-p3a-e2e/`, `project-p3b-e2e/`.

Code: [`src/projects/`](../src/projects/) (registry, root, context, session, facts, directory) · REST [`ui_server/projects.rs`](../src/gateway/ui_server/projects.rs) ·
WS [`handlers.rs`](../src/gateway/websocket_gateway/handlers.rs) (`decide_project`) · kanban [`src/kanban/projects.rs`](../src/kanban/projects.rs), [`dispatch.rs`](../src/kanban/dispatch.rs) ·
báo cáo: `plans/reports/test-261018-1724-project-p0-spike.md`, `…-p1.md`, `…-p2.md`, `…-p3a.md`, `…-p3b.md`.
