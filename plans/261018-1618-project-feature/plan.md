# Plan: Project cho session chat và Kanban

Status: **Xong toàn bộ trừ các việc đã ghi là chưa đo** (2026-10-19): daemon `feat/project` (code `9318e0a`, `cargo test --lib` 3256 xanh), web-app `feat/project` `cd1fb24`, desktop `feat/project` `47f1fd9`; chưa push. Chưa đo: model e4b thật (đo trên e2b; card xong lưu fact đã TẮT mặc định), cửa sổ app macOS thật (quyền bị từ chối), hai cửa sổ cùng mở · Nhánh: `feat/project` (daemon `.worktrees/senclaw-project`, web `.worktrees/web-app-project`, desktop `.worktrees/desktop-project`)
Thiết kế: [design.md](design.md) · Hiện trạng: [explore-261018-1618-project-feature-current-state.md](../reports/explore-261018-1618-project-feature-current-state.md)

## Outcome

Người dùng tạo **project** gắn một folder root. Session chat và board kanban thuộc project làm việc trong root đó, nhận **context** (mô tả dự án) mỗi lượt, và dùng **facts riêng của project** cộng với **facts chung** của agent.

## Non-goals

- Nhiều root cho một project; chia sẻ/phân quyền nhiều người; đồng bộ cloud.
- Cowork, `schedule_*`, chat kênh (Telegram/Feishu/WeChat) thuộc project.
- Agent tự ghi context. Đưa fact giữa bank (chung ↔ project).
- Model/agent mặc định theo project; công tắc dispatcher theo project.
- Đổi `/api/code/projects`, `app-space-sdk`, `~/.senclaw/project-config.json`, Cowork workspace.

## Acceptance (tổng)

1. Tạo project với folder có thật; root không an toàn (`/`, `$HOME`, trong `~/.senclaw`) hoặc trùng root bị từ chối với lý do.
2. Session trong project: đường dẫn tương đối của Read/Write/Bash rơi vào root; **hai session cùng profile ở hai project khác nhau không đổi cwd/thư mục của nhau** (docs, datagen, DAG). Chỉ hứa cho session trong project; session ngoài project vẫn mang F1 (design.md §11 #10).
3. Context hiện trong system prompt của session và worker kanban của project; sửa file có hiệu lực lượt sau; không project ⇒ prompt trùng từng byte với bản hiện tại.
4. Fact nói ở chat A của P được nhớ ở chat B của P và ở worker kanban của P; **không** hiện ở chat ngoài project hay ở project Q; fact chung vẫn hiện trong P trừ khi tắt `inherit_common`.
5. Board gắn project chạy card trong root; xoá project không làm hỏng board.
6. DB, `config.json`, WS cũ nâng cấp tại chỗ; không project ⇒ recall và cwd như cũ.
7. Lỗi retain/recall của project không làm hỏng lượt chat (luật memory engine).

## Quyết định (tất cả đã chốt 2026-10-18)

| # | Câu hỏi | Kết luận | Chặn |
|---|---|---|---|
| 1 | "Facts chung" là gì | Kho agent-wide (`origin=''`) của profile đang dùng; không thêm bank toàn cục (người dùng) | — |
| 2 | Fact cá nhân nói trong chat project | Ở lại bank project ở v1 (thiết kế quyết, người dùng giao) | — |
| 3 | Card xong tự retain vào fact project | ~~Bật~~ → **TẮT mặc định từ 2026-10-19** (`SENCLAW_DISPATCH_CARD_FACTS=true` để bật; khi bật vẫn chỉ `complete` + `facts_write=project`). Đo P5 trên gemma-4-e2b (không phải e4b): summary rỗng vẫn sinh fact (8/8, 8/8, 7/8), 31–44% fact là rác ⇒ trượt ngưỡng đã chốt trước. Bật lại sau khi đo trên e4b thật là việc rẻ | — |
| 4 | Đổi project cho session đang chạy | Không ở v1, gán lúc tạo (người dùng) | — |
| 10 | Sửa F1 cho mọi code chat | Chỉ session trong project (người dùng) | — |
| 11 | Hàng đợi/Stop DAG theo profile | Hạn chế đã biết (R13), ghi trong tài liệu (người dùng) | — |

Mặc định cho #5–#9, #12 (cowork/schedule ngoài v1; root lồng nhau được phép; không đổi mặc định `worktree`; dispatcher vẫn toàn cục; không đồng bộ context ra repo; E3/E4 là lỗi có sẵn ngoài phạm vi) nằm ở design.md §11. Không còn câu hỏi mở chặn việc nào.

## Phases

| # | Tên | Cỡ | Phụ thuộc | Trạng thái |
|---|---|---|---|---|
| P0 | Spike: dựng lại F1/F2, `destroy_agent_full`, cascade xoá bank | S | — | **done** → [test-261018-1724-project-p0-spike.md](../reports/test-261018-1724-project-p0-spike.md) |
| P1 | Registry + REST + session binding + context + cwd/DAG | L | P0 | **done**, commit `59caf13` trên `feat/project` (chưa push) → [test-261018-2159-project-p1.md](../reports/test-261018-2159-project-p1.md) |
| P2 | Fact project | M | P1 | **done**, commit `e00e5f7` trên `feat/project-facts` (chưa push), worktree `.worktrees/senclaw-project-facts`; tiêu chí 4 (phần chat) đạt, **extractor thật chưa đo** → [test-261018-2310-project-p2.md](../reports/test-261018-2310-project-p2.md) |
| P3a | Kanban: board↔project, thư mục, context vào guidance, AI board | M | P1 (song song P2) | **done** (chưa commit) trên `feat/project-kanban` @ `59caf13`, worktree `.worktrees/senclaw-project-kanban`; tiêu chí 5 đạt → [test-261018-2243-project-p3a.md](../reports/test-261018-2243-project-p3a.md) |
| P3b | Kanban: worker recall fact project + retain khi card xong | S | P2, P3a | **done**, commit `e8b5264` → [test-261019-0035-project-p3b.md](../reports/test-261019-0035-project-p3b.md) |
| P4 | Client web + desktop | L | API P1 đứng | **done**: web `cd1fb24` → [test-261019-0127-project-p4-web.md](../reports/test-261019-0127-project-p4-web.md); desktop `47f1fd9` → [test-261019-0145-project-p4-desktop.md](../reports/test-261019-0145-project-p4-desktop.md) |
| P5 | E2E, đo, tài liệu | M | P1–P4 | **done** trừ e4b thật: tài liệu `docs/projects.md`; đo trên gemma-4-e2b ⇒ #3 TẮT mặc định, #2 giữ → [test-261019-0112-project-p5-measure.md](../reports/test-261019-0112-project-p5-measure.md); smoke tích hợp daemon + web dist trên binary cuối đạt |

### P0 — Spike (xong)
Kết quả: (a) F1 đúng, (b) DAG chạy sai thư mục kể cả parent xếp hàng, (c) `custom_rules` không vào prompt, (d) `destroy_agent_full` chạy được nhưng chỉ có lúc tắt daemon và rò MCP con → bỏ "đổi project giữa chừng" khỏi v1 (D7), (e) cascade xoá bank đủ với FK bật nhưng O(n²) → xoá theo lô. Chi tiết và harness chạy lại: báo cáo trên. Mô tả ban đầu giữ dưới đây để đối chiếu.

### P0 — kế hoạch ban đầu
- Làm: scratch `SENCLAW_HOME` + `HOME` + **cwd** + cổng riêng (CLAUDE.md "Never develop against a live install": repo có `.senclaw/mcp.json` nên cwd phải là thư mục tạm).
  (a) Hai code session cùng profile ở hai thư mục: `doc_write`/`workspace_info`/`datagen` có thấy nhầm thư mục? (b) Một DAG trong session thứ hai chạy ở đâu? (c) Một worker kanban với `OneShotOptions.custom_rules`: prompt có chứa nó không? (d) `destroy_agent_full` rồi nhắn tiếp: lịch sử còn, MCP env mới? (e) Xoá cả bank `mem_facts` với `PRAGMA foreign_keys` như daemon dùng: `mem_fact_entities`/`mem_links` đi theo?
- Xong khi: một báo cáo trong `plans/reports/` ghi pass/fail từng mục; D6/D7 sửa nếu thực tế khác suy luận.
- Rủi ro: không có model chạy được ⇒ dùng runtime cục bộ như `scripts/evals/memory-changed-mind.mjs` (đầu file có cách dựng daemon cô lập).

### P1 — Registry, session, context
**Xong 2026-10-18** (nhánh `feat/project`, chưa commit): tiêu chí 1, 2, 3 (phần session), 6 đạt; `cargo check --all-targets` sạch, `cargo test --lib` 3179 passed / 0 failed; e2e 96/96 trên daemon cô lập. Chi tiết, quyết định tự chọn, phần chưa kiểm chứng: [test-261018-2159-project-p1.md](../reports/test-261018-2159-project-p1.md). Còn nợ cho phase sau: xoá bank fact khi xoá project (P2), `GET /:id/boards` (P3), `GET|POST /:id/facts` (P2), sự kiện WS `project:updated` (P4), khối project cho worker kanban (P3, tiêu chí 3 phần worker). Mô tả giao việc giữ dưới đây để đối chiếu.

- Làm:
  `src/projects/` (store, validate root, render context; không phụ thuộc `gateway`); bảng `projects`; `groups.project_id` + `GroupBinding.project_id` + `GroupConfigEntry`; `/api/projects*` (+ `ROUTER_SOURCES`, regen OpenAPI); WS `projectId`; `workspace_state_key`; `get_or_create` đặt cwd/`allowed_work_dirs`/env (`SENCLAW_PROJECT_ROOT`, `SENCLAW_PROJECT_BANK`); `set_project_context` + tham số mới của `assemble_system_prompt`; DAG dùng root (cả parent xếp hàng); **D6 trước mọi thứ khác, không làm đổi-project-giữa-chừng (D7)**.
  File chính: `src/types.rs`, `src/db/{schema,groups}.rs`, `src/gateway/group_manager/{types,config,manager}.rs`, `src/gateway/websocket_gateway/handlers.rs`, `src/agent/agent_pool/{pool,engine,traits}.rs`, `src/zen_core/engine.rs`, `src/mcp/{helper,bridge_servers,dispatch_server,virtual_server}.rs`.
- Xong khi: tiêu chí 1, 2, 3, 6; `cargo test --lib` xanh (nền ≈3.17k test theo báo cáo kiểm chứng 2026-10-17); test thứ tự prompt (AGENTS.md cuối) xanh.
- Rủi ro: R1, R2, R10, R13. Thêm trường theo từng bước, `cargo check --all-targets` sau mỗi bước.

### P2 — Fact project
- Làm: `FactTarget`; sửa 3 nơi gọi `retain_text`; cửa sổ reflection mang `bank/origin`; `RecallQuery.project`; 4 arm bọc `ProjectFacts`; `include_common`; `POST/DELETE /api/projects/:id/facts` (`source='manual'`, không LLM); retain không thay fact `manual`; `memory_search_facts` đọc `SENCLAW_PROJECT_BANK`; ca eval mới.
  File chính: `src/memory/engine/{types,mod}.rs`, `src/memory/engine/facts/{mod,arms,retain,store,tools}.rs`, `src/agent/agent_pool/{reflection,pool}.rs`, `src/mcp/memory_server.rs`, `evals/memory/cases.json`, `src/memory/engine/eval.rs`.
- Xong khi: tiêu chí 4; 22 câu eval cũ không tụt; ca (a)–(d) của design.md §10 xanh; p95 arm thêm ≤150 ms.
- Rủi ro: R4, R9; đổi chữ ký `retain_text` chạm test cũ.
- **P2 xong 2026-10-18** (commit `e00e5f7` trên `feat/project-facts`, chưa push): `cargo test --lib` 3207 passed / 0 failed (+28), `cargo check --all-targets` sạch, 22 câu eval cũ không tụt, 10 ca eval mới (a)–(d) xanh ở cả hai chế độ (có/không embedding), arm project p95 ≤52 ms trên 5.000 fact, e2e 53/53 và harness P1 96/96 trên daemon cô lập. **Extractor chạy bằng kịch bản; chất lượng trích xuất trên model thật chưa đo** (R4 còn mở, để P5). Phát hiện khi đo: nhả khoá `Mutex` giữa các lô chưa đủ, phải nghỉ giữa lô (purge bank 20.000 fact: yêu cầu khác đợi 21,3 s ⇒ 1,1 s; tổng thời gian vẫn ≈23 s vì trigger FTS bậc hai). Chi tiết, quyết định tự chọn, hạn chế, xung đột merge dự kiến với `feat/project-kanban`: [test-261018-2310-project-p2.md](../reports/test-261018-2310-project-p2.md). P3b làm được ngay sau khi P2 và P3a được gộp: dùng `projects::fact_target`, `projects::recall_scope`, `facts::fact_arms`.

### P3 — Kanban (tách hai nửa)
P3a không đụng kho fact nên chạy song song P2. P3b dùng `FactTarget`, `RecallQuery.project` và các arm `ProjectFacts` của P2 (`claim_ready` gọi recall, `finalize` gọi retain) nên chờ P2 xong.
- Làm: `boards.project_id` + API; `ProjectDirectory` gắn lúc `run_daemon`; `effective_workspace_dir`; `claim_ready` ghép `guidance` + `prompt`; `finalize` retain (theo câu #3); `llm.rs` nhận context; gỡ liên kết khi xoá project; (tuỳ chọn) gắn `senclaw-kanban` cho project chat.
  File chính: `src/kanban/{db,api,dispatch,llm,mcp,mod}.rs`, `src/lib.rs` (wiring).
- Xong khi: tiêu chí 5; test `claim_ready` cho cả board có và không có project; worker thật nhận khối project (xem P0 mục c).
- Rủi ro: R3, R8; không đụng `WorkItem` hay `app-space-sdk`.
- **P3a xong 2026-10-18** (chưa commit): `cargo test --lib` 3207 passed / 0 failed (+28), `cargo check --all-targets` sạch, e2e 31/31 với worker thật trên daemon cô lập (khối `<project>` nằm ngay sau chỉ dẫn card trong system prompt của worker; board thường không đổi; lưu trữ giữ board; purge chốt `workspace_dir` = root cũ và card vẫn chạy; root mất ⇒ card Blocked). Còn nợ P3b: recall + retain (`FactTarget`, `RecallQuery.project`). Đã chốt 2026-10-18: lưu trữ KHÔNG gỡ board (chỉ purge gỡ). Hai lỗi có sẵn phát hiện khi chạy (board AI 401 dưới app-token `strict`; `SpaceClient` mặc định gọi `127.0.0.1:18788`): xem báo cáo §6.

- **P3b xong 2026-10-19** (chưa commit, `feat/project-kanban-facts` @ `1079aea`): `cargo test --lib` 3255 passed / 0 failed (+20), `cargo check --all-targets` sạch, eval 4 ca mới (worker) xanh ở cả hai chế độ, e2e 49/49 với worker kanban thật trên daemon cô lập (LLM và extractor là kịch bản). Worker của card trong project nhận khối `<memory_context>` của bank project (chỉ bank đó, không fact chung, không project khác) ngay trước tiêu đề; card do worker tự đóng bằng `kanban_complete` + summary được retain thành fact `source='kanban'`, `origin=''`; `facts_write` chat/off, board thường, block, card để dispatcher tự đóng, summary rỗng, extractor hỏng ⇒ không retain và card vẫn xong. **Chưa đo**: extractor thật và độ nhiễu của fact sinh từ summary card (quyết định #3: P5 đo, rác thì đổi mặc định sang tắt). Phát hiện ngoài phạm vi, chưa sửa: `finalize` đẩy card mà worker đã `kanban_block` sang Done khi lượt chạy kết thúc bình thường. Chi tiết: [test-261019-0035-project-p3b.md](../reports/test-261019-0035-project-p3b.md).

### P4 — Client
- Làm: web-app (`/projects`, `NewChatScreen.tsx`, `Sidebar.tsx` `SessionList`, `KanbanPage.tsx`, `FactsView.tsx` nhãn bank, `vi.web.json`); desktop (`features/projects/`, `new_chat_dialog.dart`, `session_list.dart`, `kanban_dialogs.dart`, `facts_panel.dart`, `lib/core/i18n/vi/`). Mỗi client có widget/unit test với API giả.
- Không có sự kiện WS `project:updated` ở v1: làm mới sau thao tác và khi lấy lại focus (design.md §6).
- Xong khi: tạo project → chat trong project → thấy context/fact → tạo board → card chạy, trên cả hai client (chụp màn hình); `npx tsc`, `npm run build`, `flutter analyze`, `flutter test` sạch.
- Rủi ro: dịch tiếng Việt trùng khoá toàn cục; sidebar tiếng Việt tràn (chạy `vietnamese_layout_test` với font thật).
- **P4 web xong 2026-10-19** (chưa commit, nhánh `feat/project` của web-app @ `f12b16b`): `tsc -b` và `npm run build` sạch; sáu mục giao diện (trang `/projects` đủ năm tab, `NewChatScreen`, `SessionList`, Kanban, `FactsView`, `vi.web.json` 153 khoá) chạy thật trên daemon e8b5264 cô lập với LLM giả, 35 ảnh chụp (tiếng Anh, tiếng Việt, cửa sổ 640 px). Chứng minh: khối `<project>` và fact tay có trong request tới model; chat ngoài project không nhận; xoá thật để nguyên root và gỡ board/chat ra khỏi project; purge 20.000 fact từ giao diện 22,0 s. **Chưa**: model thật, card kanban chạy thật, hai cửa sổ thật, test runner (web-app không có). Quyết định cần desktop đồng bộ: chữ "dự án" thay "project" trong tiếng Việt, nhóm "Group by → Project" cũ của sidebar web là nhóm theo thư mục profile (đổi tên `folder`). Chi tiết, thiếu sót API đã đi vòng: [test-261019-0127-project-p4-web.md](../reports/test-261019-0127-project-p4-web.md).
- **P4 desktop xong 2026-10-19** (chưa commit, nhánh `feat/project` của desktop @ `3c70b29`, worktree `.worktrees/desktop-project`): `flutter analyze` sạch, `flutter test` **504 xanh** (nền 379, +125) gồm `vietnamese_layout_test` và `projects_layout_test` với font thật (en + vi, kể cả `AppShell` với mục Projects mới); sáu mục giao diện đều có test với API giả, 6 đột biến đều bị test bắt. Chạy thật: **client Dart + widget thật trên daemon `e8b5264` cô lập** (`tool/live/projects_live_test.dart`, 20 ảnh): tạo/sửa/lưu trữ/xoá project, 409 context bằng body thật, fact tay + secret bị từ chối, board tạo trong project (`effective_workspace_dir` = root), phiên tạo từ `NewChatScreen` qua WS thật mang `projectId` và nhận khối `<project>` với đúng context vừa sửa, `update:group` đổi project bị từ chối, đăng ký vào project đã lưu trữ bị từ chối và quay về form, purge 277 ms gỡ phiên/board nhưng giữ root. **Chưa**: cửa sổ app macOS (quyền computer-use bị từ chối; bản `flutter build macos --debug` build được, khởi chạy và nối WS vào daemon cô lập nhưng không có thao tác chuột/phím), native file picker, hai cửa sổ thật, model thật, purge bank lớn. Quyết định cần web đồng bộ: nhãn trục nhóm cũ (desktop "Agent", web "Thư mục profile"). Daemon: lý do từ chối `register:group` là tiếng Anh, không có `code`. Chi tiết: [test-261019-0145-project-p4-desktop.md](../reports/test-261019-0145-project-p4-desktop.md).

### P5 — Kiểm chứng và tài liệu
- Làm: E2E trên daemon cô lập (tạo project, hai chat, fact xuyên chat, đổi ý, kanban card, xoá project); đo extractor trên model người dùng thực dùng, ghi rõ model; `docs/projects.md` (tiếng Việt, theo `kanban-guide.md`); mục "Projects" trong `CLAUDE.md` của senclaw/web-app/desktop; cập nhật `docs/kanban-guide.md` và `docs/memory-engine.md` (bảng "ai đọc/ghi").
- Xong khi: báo cáo `test-*` liệt kê từng tiêu chí đạt/không và phần **chưa** kiểm chứng.

- **P5 phần daemon (2026-10-19, chưa commit, `feat/project` @ `e8b5264`)**: tài liệu xong — `docs/projects.md` mới, cập nhật `docs/kanban-guide.md`, `docs/memory-engine.md` (bảng "ai đọc/ghi" + mục Project), mục "Projects" trong `CLAUDE.md`. **Đo trên model thật**: máy LM Studio 192.168.0.93 (`google/gemma-4-e4b`) không trả lời và máy này không có weights e4b, nên đo trên **gemma-4-e2b-it-OptiQ-4bit qua sen-mlx** (được phép; số đo là của e2b, không suy ra cho e4b). Kết quả (3 lần mỗi phép đo): 41–50% fact trong bank project là cá nhân/lẫn, mất câu cá nhân ở chat ngoài project chỉ với scope `agent` (không với scope theo chat mặc định) ⇒ #2 giữ; đổi ý không thay fact cũ (0/3); 28 summary card → 36–48 fact, summary rỗng vẫn thành fact (8/8, 8/8, 7/8), rác 31–44% ⇒ ngưỡng của #3 bảo TẮT ⇒ **mặc định đã đổi sang TẮT** (`DispatchConfig::card_facts`, `worker_fact_target(project, enabled)`, test cập nhật); worker thật (8 card): 0 `kanban_complete` thành công vì model không gọi `ToolSearch` để nạp tool kanban, 7 card dispatcher tự đóng, 1 treo ở `AskUser`. `cargo check --all-targets` sạch, `cargo test --lib` 3256 passed / 0 failed / 13 ignored. **Chưa đo trên e4b thật**: ba lệnh chạy lại trong báo cáo. Báo cáo: [test-261019-0112-project-p5-measure.md](../reports/test-261019-0112-project-p5-measure.md). Còn lại của P5: E2E hai client sau P4.

## Ràng buộc vận hành

- Làm trên worktree cắt từ `main`, không từ `feat/datagen-import` (không có memory engine). Đĩa chật: dùng chung `CARGO_TARGET_DIR` của checkout chính, chỉ `cargo test --lib`, xoá `target/` thừa trong worktree.
- Không chạy thử trên daemon thật 18788/18789 hay `~/.senclaw`; không cài vào `/Applications` khi chưa được yêu cầu.
- Commit conventional, tập trung; không nhắc AI, plan hay phase trong message, comment, tên migration hay tên test.
- Không dùng `cargo fmt` toàn repo; chỉ format hunk của mình.
