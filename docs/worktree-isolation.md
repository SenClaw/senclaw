# Worktree isolation & kanban → branch → PR

> Trạng thái: **đã cài** (2026-09-12, code-v2 phase 5). Nguồn:
> [`src/worktree/mod.rs`](../src/worktree/mod.rs), dispatch ở
> `src/agent/dispatch_bridge/bridge.rs::start_task`, Task tool ở
> `src/tools/task.rs`, kanban ở `src/kanban/dispatch.rs` +
> `src/agent/mcp_dispatch/mod.rs`, REST ở
> [`src/gateway/ui_server/worktrees.rs`](../src/gateway/ui_server/worktrees.rs).

## Mô hình (vibe-kanban / Claude Code)

Một tác vụ chạy `isolation: worktree` được **checkout riêng** của repo tại
`~/.senclaw/worktrees/<repo-hash>/<name>/` trên nhánh `senclaw/<name>`. Hai
agent có thể sửa cùng một file cùng lúc; checkout của người dùng **không bị
đụng** cho đến khi họ chọn merge. Kết quả tác vụ luôn kèm khối `<worktree>`
với nhánh, số file, insertions/deletions, đường dẫn.

Mọi thao tác là `git` CLI; `gh` chỉ cần khi mở PR (thiếu → lỗi có tên nhánh,
không im lặng).

## Ba đường vào

| Đường | Cách bật | Tên nhánh | Ghi chú |
|---|---|---|---|
| DAG `dispatch_task` | task `isolation: "worktree"` | `senclaw/<parent-id>-<label>` | write-set không cần rời nhau nữa; không phải git repo → chạy chung thư mục **và ghi rõ** trong result |
| `Task` tool (subagent) | tham số `isolation: "worktree"` | `senclaw/task-<8 ký tự id>` | working dir của chat là repo |
| Kanban | gắn nhãn `worktree` cho thẻ (board có `workspace_dir`) | `senclaw/card-<id>` | MCP dispatcher tạo worktree khi claim; khi xong, comment `worktree:` trên thẻ |

Cùng tên → cùng worktree (retry đáp xuống đúng nhánh cũ).

## REST

| Method | Path | Body / Query | Làm gì |
|---|---|---|---|
| GET | `/api/worktrees?repo=&owner=` | `owner` = `kanban:<id>` / `dispatch:<task>` / `task:<id>` | liệt kê worktree SenClaw tạo cho repo |
| GET | `/api/worktrees/diff?path=` | | `{worktree, diff:{base, branch, stat, files, diff, truncated, commits, dirty}}` — diff so với merge-base, gồm cả thay đổi chưa commit |
| POST | `/api/worktrees/merge` | `{path, message?}` | commit phần dở trong worktree, rồi `merge --no-ff` vào nhánh đang checkout của repo. **Từ chối** nếu checkout người dùng có thay đổi chưa commit |
| POST | `/api/worktrees/rebase` | `{path}` | rebase nhánh lên base; xung đột → abort, báo lỗi |
| POST | `/api/worktrees/pr` | `{path, title, body?}` | `git push -u origin` + `gh pr create` |
| POST | `/api/worktrees/remove` | `{path, delete_branch?=true}` | `git worktree remove --force` (+ xoá nhánh) |

Chỉ đường dẫn có sidecar metadata `.<name>.json` do SenClaw ghi mới được
nhận — không thể merge/xoá thư mục tuỳ ý qua API.

## Kiểm chứng

- `cargo test --lib worktree`: tạo → sửa → diff (file mới chưa track cũng
  tính) → merge vào repo → xoá cả nhánh; merge bị từ chối khi checkout bẩn;
  thư mục không phải repo → lỗi.
- `gh` không có trên máy dev → nhánh PR chỉ kiểm bằng lỗi có tên nhánh.

## Quyết định

- **Không auto-merge, không auto-PR.** Worktree tồn tại tới khi người dùng
  merge/xoá; dọn dẹp tự động chưa làm (ghi ở phase 7 kế hoạch: ≥ 7 ngày không
  commit).
- **Fallback nói to.** Thư mục không phải git → chạy chung và ghi
  `isolation=worktree ignored: …` vào result, không giả vờ cô lập.
