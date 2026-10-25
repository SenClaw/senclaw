# Kiểm chứng P3a: Kanban gắn project — thư mục, khối project cho worker, context cho board AI

Ngày 2026-10-18 · Nền `59caf13` (P1, `feat/project`) · Nhánh `feat/project-kanban`, worktree `.worktrees/senclaw-project-kanban` · **Chưa commit** (chỉ commit khi người dùng yêu cầu) · 11 file sửa + 2 file mới trong `src/` (+1.271/−110 dòng, gồm test)
Phục vụ: [plan.md](../261018-1618-project-feature/plan.md) P3a, tiêu chí 5 · [design.md](../261018-1618-project-feature/design.md) D8, §5.5, §5.6, R3, R8 · [P1](test-261018-2159-project-p1.md) · [P0 mục c′](test-261018-1724-project-p0-spike.md)
Harness + bằng chứng: [project-p3a-e2e/](project-p3a-e2e/) (dùng lại `project-p0-spike/{lib.mjs,env-up.sh,mock-llm.mjs}`).

## 1. Kết quả

| Việc giao | Kết quả | Test tự động | Chạy thật (daemon cô lập + LLM giả, worker thật) |
|---|---|---|---|
| `boards.project_id`, `POST /api/kanban/boards {project_id}`, `GET` trả `project_id` + `effective_workspace_dir` | **đạt** | `kanban::db` 3 (DB cũ nâng cấp, lọc theo project, `release_project`), `kanban::api` 6 | tạo board trong project / thường; list + get đúng; 3 từ chối (thư mục ngoài root, có `..`, project lạ) không tạo gì |
| Thư mục: `workspace_dir` (trong root) > `root` > scratch | **đạt** | `kanban::projects` 3, `kanban::dispatch` (root / trong root / ngoài root / nhãn `worktree`) | worker A `pwd` = root A, file ghi vào root A; worker B `pwd` = `B/sub` (board đặt thư mục trong root B); worker board thường vẫn ở `…/mcp-dispatch/<id>` |
| `ProjectDirectory` gắn lúc `run_daemon`; kanban không mở `senclaw.db` | **đạt** | `projects::directory` 3 (cài đặt trên DB), mọi test kanban dùng `StaticDirectory` | chạy qua `lib.rs` thật |
| `claim_ready`: `guidance` = chỉ dẫn card + khối `<project>` **nối vào** | **đạt** (R3) | `kanban::dispatch` 7: `workspace=Dir(root)`, `guidance == guidance(card) + "\n\n" + block`; board không project **trùng từng trường** với trước (so JSON từng `WorkItem`, có/không directory, 4 kiểu thư mục/nhãn) | system prompt của worker bắt đầu đúng bằng chỉ dẫn card, **ngay sau đó** là `<project name="Alpha" root="…">…ALPHA-CONTEXT-MARKER…</project>`, rồi env/skill như cũ; không có "You are a helpful AI assistant" (không bị thay); không lẫn context project khác; board thường: không có `<project` |
| `llm.rs` (board AI, breakdown) nhận context cắt 2.000 ký tự | **đạt** | `kanban::llm` 2 (cắt đúng, không project ⇒ prompt trùng byte), `kanban::api` 1 (context theo project của board) | `/generate {project_id}` và `/breakdown` của card trên board project: prompt gửi LLM mở đầu bằng "About the project this board belongs to…" + marker; board thường: không có đoạn đó |
| `GET /api/projects/:id/boards` | **đạt** | `ui_server::projects::boards_tests` 4 | chỉ trả board của project, kèm thư mục chạy; project lạ 404 |
| Xoá (purge): board gỡ `project_id`, chốt `workspace_dir` = root cũ | **đạt** | `boards_tests` (purge, archive, không có kho board), `kanban::db` | purge Alpha → board `project_id=null`, `workspace_dir`=root cũ; card mới trên board đó chạy **trong root cũ**, hoàn thành, thư mục dự án nguyên vẹn |
| **Tiêu chí 5: board gắn project chạy card trong root; xoá project không làm hỏng board** | **đạt** | các test trên | 5 card chạy xong (A, B, B sau lưu trữ, X, A sau purge) |

Phần worker của tiêu chí 3 (khối project có trong system prompt của worker kanban) cũng đạt, xem dòng `claim_ready`. P3b (recall + retain) không làm.

## 2. Số liệu

- `cargo check --all-targets`: sạch, 2 cảnh báo có sẵn (`local_models/settings.rs:141`, `tools/write.rs:181`).
- `cargo test --lib`: **3207 passed, 0 failed, 11 ignored** (nền P1: 3179 ⇒ +28 test mới: dispatch 7, api 6, boards route 4, db 3, directory 3, kanban::projects 3, llm 2). HOME, `SENCLAW_HOME`, cwd riêng; chạy bản sao của binary test (xem §6.3).
- e2e: **31/31** check ([result-run3-final.json](project-p3a-e2e/evidence/result-run3-final.json), [run3-final-31of31.log](project-p3a-e2e/evidence/run3-final-31of31.log)). Binary daemon build từ cây mã cuối (sha256 `d5620afc54ab…13fef8`, [binary-sha256.txt](project-p3a-e2e/evidence/binary-sha256.txt)); sau đó chỉ chạy `rustfmt` trên file của mình và đổi chữ một comment + một tên test (không đổi logic), test + check chạy lại xanh trên cây cuối. System prompt từng worker lưu ở `evidence/system-prompt-K*.txt`.
- `docs/openapi-daemon.yaml` sinh lại: +1 route (`/api/projects/{id}/boards`).

## 3. Đã làm

1. **Dữ liệu**: `ALTER TABLE boards ADD COLUMN project_id TEXT` trong `MIGRATIONS` (kanban.db cũ nâng cấp tại chỗ, mở lại nhiều lần không lỗi); `BoardMeta.project_id`; `create_board_in` / `create_board_from_template_in` (`create_board` cũ giữ nguyên chữ ký, gọi vào với `None`); `boards_of_project`; `release_project` (một câu `UPDATE`, đặt `updated_at` để watcher WS làm mới).
2. **`src/kanban/projects.rs` (mới)**: trait `ProjectDirectory` + `ProjectInfo {id, name, root, root_problem, archived, context_block}`; `effective_workspace_dir`; `path_within`; `board_view(s)`; `ProjectBoards` (mặt kanban cho route project). Module vẫn tự đứng được: `AppState.projects: Option<_>`, stdio `kanban-server` để `None`.
3. **`src/projects/directory.rs` (mới)**: `DbProjectDirectory` trên `senclaw.db` — đọc project, **kiểm lại root khi dùng** (`recheck_root`, như P1 làm cho session), `render_block` từ `CONTEXT.md` hiện tại.
4. **Dispatch**: `KanbanDispatchProvider::with_projects`; `claim_ready` như §1. Không đổi `WorkItem`, `app-space-sdk`; không dùng `custom_rules`.
5. **REST**: `project_id` (+ kiểm) cho `POST /boards` và `POST /generate`; `GET /boards`, `/board` trả `effective_workspace_dir`; `GET /api/projects/:id/boards` và hook purge nằm **cuối** `ui_server/projects.rs`.
6. **Wiring `lib.rs`** (chỉ phần này): tạo `DbProjectDirectory`, `make_state_with`, `.with_projects(...)`, và một `Extension<ProjectBoards>` trên router (route project lấy kho board qua extension ⇒ không đổi `UiState` — 8+ file test dựng literal — cũng không đổi `ProjectsCtx`).

## 4. Quyết định tôi tự chọn

1. **Lưu trữ KHÔNG gỡ board**; chỉ purge gỡ. Brief ghi "xoá (purge) hoặc lưu trữ: board gỡ project_id…", nhưng design §5.6 nói lưu trữ "board giữ nguyên" và lưu trữ hoàn tác được (PATCH `archived:false`); gỡ khi lưu trữ sẽ làm mất liên kết không khôi phục được. Board của project đã lưu trữ vẫn chạy trong root, vẫn nhận khối project (e2e `KB2`). Người dùng xác nhận 2026-10-18: giữ như vậy. (Đổi sang "gỡ cả khi lưu trữ" chỉ là gọi `release` ở nhánh archive.)
2. Board có `workspace_dir` riêng **giữ nguyên** khi purge (đã nằm trong root cũ, cụ thể hơn root); chỉ board không đặt (hoặc đặt rỗng) mới bị chốt = root cũ.
3. Board của project: `workspace_dir` ngoài root (root bị `PATCH` sau khi tạo board) bị **bỏ qua, dùng root** — cả khi chạy lẫn khi hiển thị `effective_workspace_dir`, cùng một hàm. Tạo mới: `workspace_dir` phải tuyệt đối, không chứa `..` (một `..` sau symlink thoát khỏi chỗ nó có vẻ ở lại), nằm trong root sau khi resolve symlink; vi phạm ⇒ 400, không tạo gì.
4. Project **đã lưu trữ không nhận board mới** (400), cùng luật với session; project lạ ⇒ 404; process không có directory (stdio) ⇒ 503.
5. **Root không dùng được lúc claim** (thư mục mất / không còn hợp lệ): card **vào Blocked** với `gave_up: project "<tên>" cannot be used: …`, không có worker nào chạy ở thư mục khác (cùng luật P1: không rơi về thư mục khác). Chỉ áp cho board có project; board thường y như cũ. e2e: đổi tên thư mục root ⇒ card Blocked, 0 request tới mock.
6. Project không tìm thấy khi claim (đã xoá ngoài đường purge, hoặc không có directory) ⇒ board chạy như board thường theo `workspace_dir` của nó, có `warn` trong log.
7. Context cho `llm.rs` là **khối `<project>` đã render** cắt 2.000 ký tự (kể cả thẻ mở; thẻ đóng thường rơi mất khi cắt — chỉ là văn bản prompt). Áp cho `/generate` (cả nhánh template), `/breakdown`, và các tool MCP tương ứng đi qua daemon (`/api/kanban/mcp/*`). **Không** áp cho `/chat` (khung trò chuyện): brief chỉ nêu board AI và breakdown.
8. `kanban_create_board` (MCP) **không** nhận `project_id` (ngoài phạm vi: "tool kanban cho chat trong project"); `kanban_list_boards`/`kanban_get_board` tự có `project_id` vì serialize `BoardMeta`.

## 5. Chưa chạy / chưa kiểm chứng

- Không có model thật: LLM giả phát lại tool call; `/generate` và `/breakdown` chỉ chứng minh **prompt có/không có** đoạn project (mock trả `[]` nên cả 4 lời gọi kết thúc 502, không tạo board/card). Chưa đo chất lượng board sinh ra khi có context.
- Card nhãn `worktree` trong project: chỉ test đơn vị (`Worktree{repo: root, branch: senclaw/card-N}`); chưa chạy thật tạo worktree git trong root.
- Worker qua `kanban-server` stdio **không có directory** (tiến trình riêng): `kanban_breakdown_card` do worker tự gọi qua stdio không kèm context project; chỉ đường qua daemon (`/mcp/*`, REST) có. `kanban_get_board` qua stdio có `project_id` nhưng không có `effective_workspace_dir`.
- Client web / desktop (P4): chưa hiển thị `project_id` / `effective_workspace_dir`, chưa chọn project khi tạo board. Mới kiểm payload bằng REST.
- Không tạo sự kiện WS `project:updated` (P4). Core chat đang chạy giữ root cũ sau `PATCH root` (hạn chế đã biết của P1) — với kanban thì **không**: root đọc lại mỗi lần claim.
- Chỉ macOS. `path_within` đã thử symlink; Windows (đường `\\?\`) chưa.
- Hai project nhiều board chạy đồng thời / dispatcher với `assignee` persona: chưa thử (persona + guidance nối bằng code có sẵn: `format!("{persona}\n\n{guidance}")`, khối project nằm trong guidance nên theo sau).
- Daemon thật `18788/18789`, `~/.senclaw`, `senclaw/target/debug/senclaw`: **không bị đụng ngoại trừ sự cố ở §6.1**.

## 6. Sự cố và phát hiện (ngoài phạm vi, chưa sửa)

1. **SỰ CỐ — lượt e2e đầu đã gửi 4 lời gọi LLM sang daemon thật.** Endpoint AI của kanban đi qua `app-space-sdk` `SpaceClient`, base URL mặc định `http://127.0.0.1:18788` (`SENCLAW_BASE_URL`, `app-space-sdk/src/bridge.rs:166`). Daemon cô lập của tôi không đặt biến này nên `/generate` ×2 và `/breakdown` ×2 gọi `POST /api/space/apps/kanban/bridge` ở **daemon thật** (pid 14613). Nội dung toàn chuỗi thử nghiệm (mục tiêu "Ship invoicing"/"Ship plain things", tiêu đề card, khối project mẫu có đường dẫn scratch và `ALPHA-CONTEXT-MARKER`) — không có dữ liệu thật hay bí mật. Kết quả: 502, 200, 200, 200 (daemon thật trả lời được, nhiều khả năng vì chế độ app-token của nó không phải `strict`; tôi không kiểm tra). Board/card sinh ra được ghi vào kanban.db **scratch của tôi** (do daemon cô lập xử lý kết quả), nên kanban.db thật không đổi; phía daemon thật chỉ có thể có bản ghi dùng LLM (usage log) và chi phí 3–4 lời gọi ngắn. Tôi không kiểm tra phía daemon thật (sẽ là chạm vào nó). Đã dừng, dựng lại daemon với `SENCLAW_BASE_URL=http://127.0.0.1:58788`, và **thêm chốt vào driver**: `exp-p3a.mjs` từ chối chạy nếu môi trường của daemon (qua `ps eww`) không có `SENCLAW_BASE_URL=<UI cổng thử>`. Log hai lượt đầu giữ ở `evidence/run1-*.log`, `run2-*.log`.
2. **Lỗi có sẵn**: dưới `SENCLAW_APP_TOKEN_MODE=strict` (mặc định), `/api/kanban/generate` và `/breakdown` **bị 401** (`/chat`, `/models` đi cùng đường `SpaceClient` nên rất có thể cũng vậy — chưa thử) `app_token_required` vì `SpaceClient` trong kanban không gửi `x-senclaw-app-token` (lượt 2: 4/4 lời gọi 401, log `app-auth`). Lượt 3 chạy với `SENCLAW_APP_TOKEN_MODE=off` — chỉ trên daemon cô lập. Daemon thật chạy được là do chế độ của người dùng. Cần xử lý riêng (gửi token nội bộ qua `util::internal_auth`, và đặt `SENCLAW_BASE_URL` = chính cổng UI trong `run_daemon`) — cũng là chỗ khiến mọi daemon đổi cổng gọi nhầm nơi.
3. **Hash binary test `--lib` trùng giữa các worktree** (`senclaw-4dcb4595fe0d1c80`, cùng `CARGO_TARGET_DIR`): build sau ghi đè binary của build trước. Tôi sao binary vào scratch ngay sau khi build rồi mới chạy. Lúc dọn `*.rcgu.o` có một lần chạy `find` khi session P2 đang `cargo test --no-run` (không chờ rustc xong); build của họ vẫn xong bình thường (binary mới lúc 22:40) — nhưng lần sau chỉ xoá khi `ps` không còn `rustc`.

## 7. Chạy lại

```bash
# test (HOME, SENCLAW_HOME, cwd riêng; dùng chung CARGO_TARGET_DIR; sao binary ngay sau khi build)
cd .worktrees/senclaw-project-kanban && export CARGO_TARGET_DIR=/Users/benji/Projects/SenClaw/senclaw/target CARGO_INCREMENTAL=0
cargo check --all-targets && cargo test --lib --no-run     # in đường dẫn binary test
S=<scratch>; mkdir -p $S/home $S/cwd; cp <binary test> $S/lib-test; cd $S/cwd && HOME=$S/home SENCLAW_HOME=$S/home/.senclaw $S/lib-test

# e2e: build vào bản COW của target/debug (không ghi đè target/debug/senclaw đang dùng)
cp -c -R $CARGO_TARGET_DIR/debug $S/p3a-target/debug && CARGO_TARGET_DIR=$S/p3a-target cargo build --bin senclaw
export P0_RUNS=$S/runs P0_UI=58788 P0_WS=58789 P0_MOCK=58931       # cổng rảnh, không bao giờ 18788/18789
cd plans/reports/project-p0-spike && ./env-up.sh p3a $S/p3a-target/debug/senclaw \
  SENCLAW_DISPATCH_INTERVAL_SECS=3 SENCLAW_BASE_URL=http://127.0.0.1:58788 SENCLAW_APP_TOKEN_MODE=off
node ../project-p3a-e2e/exp-p3a.mjs p3a && ./env-down.sh p3a
```

## Câu hỏi chưa giải quyết

1. ~~Lưu trữ project có nên gỡ board như purge không?~~ Đã chốt 2026-10-18: không, chỉ purge gỡ.
2. `/chat` (khung trò chuyện của board) có nhận context project không? Hiện không.
3. Sửa hai lỗi ở §6.2 (token nội bộ cho `SpaceClient` của kanban; `SENCLAW_BASE_URL` = cổng của chính daemon) ở nhánh riêng? Nó làm board AI hỏng trên cài đặt mặc định, độc lập với project.
