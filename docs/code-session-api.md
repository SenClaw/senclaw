# Code sessions & checkpoints — REST contract

> Trạng thái: **đã cài** (2026-09-12, code-v2 phase 1–2). Nguồn:
> [`src/gateway/ui_server/code_sessions.rs`](../src/gateway/ui_server/code_sessions.rs),
> [`src/gateway/ui_server/checkpoints.rs`](../src/gateway/ui_server/checkpoints.rs),
> [`src/checkpoints/`](../src/checkpoints/). Test:
> [`tests/code_sessions_api.rs`](../tests/code_sessions_api.rs),
> [`tests/checkpoints_api.rs`](../tests/checkpoints_api.rs).

## Mô hình

Một **code session** là một chat thường có `group_type = "code"` và một
working directory duy nhất (`allowed_work_dirs[0]`). Không có engine riêng:
prompt gửi vào session đi đúng đường `GroupQueue → AgentPool` như mọi chat;
cây file là working directory; "git log" / "rollback" là **checkpoint** của
chat đó.

Một **checkpoint** là một commit trong *shadow git repo* tại
`~/.senclaw/checkpoints/<chat>/` (work tree = working directory; `.git` của
dự án **không bao giờ** bị đụng). Daemon tự tạo checkpoint sau mỗi tool ghi
file (`Edit`, `Write`, `NotebookEdit`, `Bash` không read-only). Điều kiện:
working directory là git repo, hoặc chat là code session; không bao giờ là
`$HOME` hay `/`. Baseline được chụp ở tool đầu tiên của chat (thường là
`Read`/`Grep`, trước lần sửa đầu). File > 5 MB, `node_modules/`, `target/`,
`.venv/`… bị loại; `.gitignore` của dự án được tôn trọng.

## Checkpoints — `/api/chats/:jid/checkpoints`

| Method | Path | Body / Query | Trả về |
|---|---|---|---|
| GET | `/api/chats/:jid/checkpoints` | — | `{enabled, workspace, items:[Checkpoint]}` mới nhất trước |
| PUT | `/api/chats/:jid/checkpoints/settings` | `{enabled}` | `{ok, enabled}` |
| GET | `/api/chats/:jid/checkpoints/:id/diff?from=<id>` | `from` mặc định = parent | `{checkpoint, fromSha, files:[{status,path}], diff, truncated}` (diff cap 200 KB) |
| POST | `/api/chats/:jid/checkpoints/:id/restore` | `{files?: []}` rỗng = cả cây | `{ok, restored:[], removed:[], checkpoint}` |
| POST | `/api/chats/:jid/checkpoints/:id/explain` | `{from?, profile?, language?}` | `{ok, text, model, files, truncated}` — một lượt LLM, không tool |

`Checkpoint` (camelCase): `id, chatJid, sha, parentSha|null, toolName
("Edit"|"Write"|"NotebookEdit"|"Bash"|"snapshot"|"restore"), summary,
workspace, filesChanged, createdAt`. `parentSha = null` chỉ ở baseline.

Restore luôn chụp `snapshot` trạng thái hiện tại trước, rồi mới đưa cây về
`sha`; bản thân restore cũng là một checkpoint → hoàn tác được. Restore cả
cây xoá những file được *thêm* giữa `sha` và checkpoint mới nhất; file chưa
từng vào checkpoint nào được giữ nguyên.

WS: `{type:"checkpoint:new", groupJid, checkpoint}` tới client đang xem chat.

UI: web — nút đồng hồ ở header chat mở drawer **Changes**; desktop — tab
**Changes** ở dock phải; mobile — màn *Code session* (git log / rollback).

## Code sessions — `/api/code/*` (hợp đồng mobile)

Tên route và key JSON (snake_case, epoch ms) giữ nguyên như
`channel_app/lib/services/code_api.dart` để app đã phát hành chạy được.

| Method | Path | Ghi chú |
|---|---|---|
| GET | `/api/code/sessions?status=active\|archived\|all` | `{sessions:[Session]}` |
| POST | `/api/code/sessions` | `{name, workspace, language?, init_git?, folder?}` → `Session`; tạo thư mục, `git init` nếu `init_git` |
| GET | `/api/code/sessions/:id` | `Session` |
| DELETE | `/api/code/sessions/:id` | **archive** (chat và lịch sử còn nguyên) |
| GET | `/api/code/sessions/:id/files` | `{workspace, tree:[{name,path,type:"dir"\|"file",children}]}` ≤ 5 000 mục, sâu ≤ 8, bỏ `node_modules/`… |
| GET | `/api/code/sessions/:id/file-content?path=` | `{path, content, size}` ≤ 2 MB; đường dẫn thoát workspace → 400 |
| GET | `/api/code/sessions/:id/git-log` | `{log:[{hash, message, date, checkpoint_id, files_changed}]}` = checkpoints |
| POST | `/api/code/sessions/:id/rollback` | `{steps?}` (mặc định 1) hoặc `{checkpoint_id}` |
| POST | `/api/code/sessions/:id/chat` | `{prompt, group_id?}` → queue vào agent; `{messages}` snapshot; 503 khi không có agent runtime |
| GET | `/api/code/projects/:id/groups` | một session = một group duy nhất (`id == project_id == jid`) |
| POST | `/api/code/projects/:id/groups` | trả về group đó (giữ cho client cũ) |
| GET | `/api/code/groups/:gid/messages` | `{messages:[{id, role:"user"\|"assistant", content, status:"done"\|"processing", created_at, processed_at}]}` |
| POST | `/api/code/groups/:gid/stop-current` | dừng agent |
| GET | `/api/fs/ls?path=` | `{current, parent, dirs:[{name,path}]}` — chỉ thư mục, bỏ ẩn; mặc định `$HOME` |

`Session`: `{id (= chat jid, "code:<uuid>" khi tạo qua API), name, workspace,
language, status, git_enabled, created_at, updated_at}`. Chat code tạo từ
web/desktop ("New chat" → kind *code*) cũng xuất hiện ở đây với jid `web:…`.

Trạng thái archived và ngôn ngữ lưu trong `router_state`
(`code:archived:<jid>`, `code:lang:<jid>`); không có bảng mới.

## Không có trong v2 (so với engine cũ đã gỡ)

- Không có nhiều "group" trong một project — mỗi session một hội thoại.
- Không có `.senclaw-code/context.json`, `get_skeleton`, `graph_*` — repo map
  và symbol tool là phase 3 của kế hoạch, dưới tên tool mới.
