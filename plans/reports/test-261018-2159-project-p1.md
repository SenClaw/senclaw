# Kiểm chứng P1: Project cho session chat — registry, REST, session binding, context, cwd/DAG

Ngày 2026-10-18 · Nền `main` @ ede1e16 · Nhánh `feat/project`, worktree `.worktrees/senclaw-project` · **Chưa commit** (theo ràng buộc: chỉ commit khi người dùng yêu cầu) · 38 file sửa + 9 file mới
Phục vụ: [plan.md](../261018-1618-project-feature/plan.md) P1 · [design.md](../261018-1618-project-feature/design.md) D1–D10, §4–§6 · [P0](test-261018-1724-project-p0-spike.md)
Harness e2e + bằng chứng: [project-p1-e2e/](project-p1-e2e/) (dùng lại `project-p0-spike/lib.mjs`, `env-up.sh`, `mock-llm.mjs`).

## 1. Kết quả theo tiêu chí

| Tiêu chí (plan.md) | Kết quả | Test tự động | Chạy thật (daemon cô lập + LLM giả) |
|---|---|---|---|
| 1. Tạo project với folder thật; root không an toàn hoặc trùng bị từ chối kèm lý do | **đạt** | `projects::root` 7, `projects::store` 6, REST `ui_server::projects` 10 | `/`, `$HOME`, `~/.senclaw`, `profiles/`, không tồn tại → `validate-root` nêu mã lý do; `POST` → 201; trùng root qua `A/`, `A/../A` → 409 `root_in_use`; `/` → 400 `invalid_root`; từ chối không tạo gì |
| 2. Session trong project: đường dẫn tương đối rơi vào root; hai session cùng profile ở hai project không đổi thư mục của nhau (docs, datagen, DAG) | **đạt** (chỉ session trong project; session ngoài project vẫn mang F1, đúng §11 #10) | `agent_pool::project_tests` 9 (hai project + một session ngoài project, một profile: cwd, env MCP, file state riêng, không đụng file state của profile); `dispatch_server` 2; `dispatch_bridge` 2 (đường parent xếp hàng) | 3 session cùng profile `shared` (A, B trong project; C ngoài project), xen kẽ A1 B1 C1 A2 B2 C2: `Bash pwd`, `workspace_info`, `Write` tương đối, `datagen_start` file tương đối đều đúng thư mục của chính nó. 3 file state: `workspace-state-shared.json` = C, hai file `shared@<hash>` = root A / root B. DAG chặn từ A, B, C: worker `pwd` đúng root, ghi file đúng chỗ. **DAG xếp hàng**: parent của B `queued` mà đã có `sharedWorkspace` = root B; sau khi A xong, daemon promote, worker chạy ở root B. DAG tool built-in (`DispatchCreateParentAndRun`) cũng ở root A |
| 3. Context hiện trong system prompt, sửa có hiệu lực lượt sau; không project ⇒ prompt trùng từng byte | **đạt cho session**; worker kanban thuộc P3 | `projects::context` 10, `zen_core::engine` 3 (thứ tự, byte-identical, set/clear), `project_tests` (push mỗi lượt, xoá project ⇒ `None`) | Khối `<project name="Alpha" root="…">` có trong system prompt của A; nằm trước `<user_operating_rules>` và AGENTS.md vẫn cuối cùng; B (project không context) và C (không project) không có khối; PUT v2 và sửa tay file v3 đều hiện ở lượt kế tiếp; client giữ bản cũ → 409 kèm nội dung hiện tại |
| 6. DB, `config.json`, WS cũ nâng cấp tại chỗ; không project ⇒ recall và cwd như cũ | **đạt** | `db::tests` 5 (DB cũ có/không `UNIQUE(folder)`, idempotent, upsert không gỡ project, rename jid), `group_manager::config` 2, `websocket_gateway::tests::projects` 6 | Khởi động lại daemon sau khi **xoá `projectId` khỏi config.json** (giả config cũ): session vẫn thuộc project, lượt sau vẫn ở đúng root, không sinh thêm file state. `register:group` / `update:group` không có `projectId` chạy như cũ; refusal: kênh Telegram, project lạ, `groupType` cowork, đổi/gỡ/gắn muộn project (6 lỗi, không tạo/đổi gì) |

Tiêu chí 4, 5, 7 là P2/P3, không thuộc P1.

## 2. Số liệu

- `cargo check --all-targets`: sạch; 2 cảnh báo có sẵn (`local_models/settings.rs:141` duplicated attribute, `tools/write.rs:181` unused `text`), y hệt bản trước khi sửa.
- `cargo test --lib`: **3179 passed, 0 failed, 11 ignored** (HOME, `SENCLAW_HOME`, cwd riêng; ~8 s). 70 test mới (49 trong file mới, 21 thêm vào file cũ).
- e2e: **96/96** check (`project-p1-e2e/evidence/result-p1.json`, `run.log`). Binary `senclaw` build từ đúng cây mã cuối, sha256 `b04feab2…e9c0` ([binary-sha256.txt](project-p1-e2e/evidence/binary-sha256.txt)). Hai lượt chạy trước (89/93 do check của tôi giả định sai dạng `allowedWorkDirs` là mảng — thực tế là chuỗi; 93/93) đã sửa driver, không sửa code.
- OpenAPI: `docs/openapi-daemon.yaml` sinh lại (`SENCLAW_WRITE_OPENAPI=1`), +6 route `/api/projects*`, `committed_spec_matches_the_routers` xanh.

## 3. Cái đã làm (theo phạm vi giao)

1. **Registry**: `src/projects/{types,store,root,context,session}.rs` (không phụ thuộc `gateway`); bảng `projects` + `groups.project_id` + index trong `db/schema.rs`, **đặt sau** khối rebuild bỏ `UNIQUE(folder)` (khối đó copy danh sách cột cố định). Root: từ chối `/`, `$HOME`, trong `~/.senclaw`, `profiles/`, `projects/`; unique theo đường canonical; kiểm lại mỗi lần dựng core (R7/R11).
2. **REST** `src/gateway/ui_server/projects.rs`, khai báo trong `ROUTER_SOURCES`: `GET|POST /api/projects`, `POST /validate-root`, `GET|PATCH|DELETE /:id`, `GET|PUT /:id/context` (409 theo `updatedAt`), `GET /:id/chats`.
3. **Session**: `GroupBinding.project_id`, `GroupConfigEntry.projectId` (serde default), `GroupInfo.projectId`; WS `register:group` nhận `projectId` (chỉ `channel=""`, `chat`/`code`, project chưa lưu trữ), `update:group` từ chối đổi/gỡ/gắn muộn, cho phép lặp lại đúng giá trị.
4. **D6**: `projects::workspace_state_key(&binding)` (hash 12 hex của jid) thay mọi chỗ ghép tên file ở `pool.rs` (3 chỗ) và `bridge_servers.rs`; `workspace_dir = root`, `allowed_work_dirs = [root]` (cả `chat` và `code`); env `SENCLAW_PROJECT_ROOT`, `SENCLAW_PROJECT_BANK` qua `CoreMcpParams`/`with_project_env`; `DispatchServer`, `McpDispatchServer`, `DispatchToolsConfig` (tool built-in), `VirtualServer` dùng root trước file state; `shared_workspace` đặt ngay lúc tạo parent kể cả `queued`, và `activate_next_queued` không ghi đè workspace đã biết.
5. **Prompt**: `set_project_context` (trait `CoreApi`, `ZenCoreApi`, `ZenEngine`, `ZenCoreOptions.project_context`); `assemble_system_prompt` thêm tham số ngay trước `operating_rules`; `~/senclaw/projects/<id>/CONTEXT.md` cắt 8.000 ký tự, đọc lại mỗi lượt, `</project` bị vô hiệu mọi kiểu chữ hoa/thường, thuộc tính tên/root được escape.

## 4. Quyết định tôi tự chọn (design im lặng hoặc mơ hồ)

1. `DELETE /api/projects/:id` = **lưu trữ**; `?purge=true` = xoá thật (design chỉ ghi dạng `?purge=true`). Xoá thật: gỡ `project_id` của session (một transaction), xoá `CONTEXT.md` + file state riêng của từng session, không đụng root.
2. `PATCH` cho sửa `root` (R11), kiểm đầy đủ; root giữ nguyên không bị kiểm lại (project có folder bị xoá vẫn đổi tên được). Body REST dùng `deny_unknown_fields` (gõ sai tên trường là lỗi, không bị lờ).
3. Từ chối thêm **tổ tiên** của `$HOME` và của `~/.senclaw` (`contains_home`, `senclaw_state`), không chỉ chính `/` và `$HOME`: `/Users` chứa token còn nguy hiểm hơn `$HOME`.
4. `upsert_group` dùng `COALESCE(excluded.project_id, groups.project_id)`: ít nhất 8 nơi dựng binding từ đầu rồi upsert (scheduler tự chữa, đối soát config lúc boot, kênh, cowork…) không được lặng lẽ tách session khỏi project. Chỉ `delete_project` gỡ.
5. `project_id` trỏ project đã mất ⇒ chạy như session thường + cảnh báo log. Project còn mà root hỏng/không còn an toàn ⇒ **từ chối dựng core**, báo lỗi nêu tên project và mã (`not_found`, `senclaw_profiles`…), không rơi về thư mục khác.
6. Session trong project luôn khởi động ở root (ghi đè file state của chính nó mỗi lần dựng core, như code session).
7. `allowed_work_dirs = [root]` được ghi vào DB lúc đăng ký; pool vẫn lấy từ project (đổi root sau đó có hiệu lực ở lần dựng core kế).
8. Thêm `PathsConfig.projects_dir` (`PROJECTS_DIR`, mặc định `<data home>/projects`).
9. Hai thay đổi nhỏ phục vụ test, không đổi hành vi thật: `DispatchServer::with_state_dir` (`#[cfg(test)]`); `get_or_create_internal` dùng `try_get_instance()` thay `get_instance()` cho `init_agent` (daemon luôn khởi tạo, nên không khác; pool test dựng được core không cần memory singleton).
10. Wire project thêm `chatCount`, `rootExists`, `hasClaudeMd`, `isGit`; context trả `chars`, `maxChars`; PUT ép mtime tăng nghiêm ngặt (hai lần ghi cùng mili-giây không làm bản cũ trông còn mới); file tối đa 100 KB (413).

## 5. Chưa chạy / chưa kiểm chứng

- Không có model thật: LLM giả phát lại tool call. Không đo gì về chất lượng trả lời, và không thử UI web/desktop (P4). Chưa kiểm payload `groups` / `group:registered` / `group:updated` qua WS thật (chỉ test `to_group_info`).
- `mcp.bundled=false` (mỗi server một tiến trình): chỉ test `with_project_env` và hàm dựng config, không chạy thật. `doc_write` không nằm trong core gộp mặc định (E3 của P0) nên server docs không có trong e2e; nó dùng chung biến `SENCLAW_WORKSPACE_STATE_FILE` với workspace/datagen (đã chạy thật).
- Chỉ macOS. Test dùng symlink gắn `#[cfg(unix)]`. Windows: `canonicalize` trả đường `\\?\…`, chưa thử.
- Daemon thật 18788/18789, `~/.senclaw`, `senclaw/target/debug/senclaw` không bị đụng (mtime nguyên). Build binary e2e vào bản COW của `target/debug` (xoá sau khi xong).
- Kanban worker nhận khối project (P3). Fact project (P2).

## 6. Hạn chế đã biết / cần theo dõi

- Core đang chạy giữ root cũ sau khi `PATCH root` hoặc `purge`; chỉ lần dựng core sau (khởi động lại daemon) mới đổi. Khối `<project>` thì biến mất ngay lượt sau purge (đọc theo binding mỗi lượt). Phản hồi purge trả `detachedChats` để client biết.
- R13 giữ nguyên: hàng đợi và Stop của DAG vẫn khoá theo profile; session ngoài project vẫn mang F1.
- Session `code` trong project xuất hiện trong `GET /api/code/sessions` (hợp đồng mobile liệt kê mọi `group_type=code`).
- Xoá project **chưa** xoá bank fact `project:<id>` (chưa có fact nào cho đến P2; P2 phải thêm xoá theo lô ≤500 hàng/lệnh qua `Db`). Chưa có `GET /:id/boards`, `/facts` (P3, P2) và sự kiện WS `project:updated` (cần cho P4).

## 7. Chạy lại

```bash
# test (HOME, SENCLAW_HOME, cwd riêng; dùng chung CARGO_TARGET_DIR của checkout chính)
cd .worktrees/senclaw-project && export CARGO_TARGET_DIR=/Users/benji/Projects/SenClaw/senclaw/target CARGO_INCREMENTAL=0
cargo check --all-targets && cargo test --lib --no-run      # in đường dẫn binary test
find $CARGO_TARGET_DIR/debug/deps -maxdepth 1 -name 'senclaw-<hash>.*.rcgu.o' -delete
S=<scratch>; mkdir -p $S/home $S/cwd; cd $S/cwd && HOME=$S/home SENCLAW_HOME=$S/home/.senclaw <binary test>

# e2e: build vào bản COW để không ghi đè target/debug/senclaw đang dùng
cp -c -R $CARGO_TARGET_DIR/debug $S/p1-target/debug && CARGO_TARGET_DIR=$S/p1-target cargo build --bin senclaw
export P0_RUNS=$S/runs P0_UI=48788 P0_WS=48789 P0_MOCK=48931     # cổng rảnh, không bao giờ 18788/18789
plans/reports/project-p0-spike/env-up.sh p1 $S/p1-target/debug/senclaw
node plans/reports/project-p1-e2e/exp-p1.mjs p1 $S/p1-target/debug/senclaw
plans/reports/project-p0-spike/env-down.sh p1
```

## Câu hỏi chưa giải quyết

1. Khi purge/đổi root, có cần dựng lại core của session đang chạy không? Cần D7 đầy đủ (đường gọi + đóng MCP con); hiện chỉ báo `detachedChats`.
2. `project:updated` thuộc P1 hay P4? Tôi để P4, vì không client nào dùng trước đó.
3. Có ẩn session `code` của project khỏi `/api/code/sessions` không (hợp đồng mobile)?
4. Từ chối tổ tiên (mục 4.3) vượt design một chút. Root như `$HOME/Projects` (chứa nhiều repo) vẫn được; chỉ `$HOME`, `/Users`, `/` bị cấm. Nếu người dùng muốn root = `/Users`, phải đổi luật.
