# Luồng spec cho phiên Code: brainstorm → draft → review → approve

*   **Author:** Claude (phiên 2026-10-27), theo yêu cầu của benji
*   **Status:** Approved 2026-10-27 20:38 (người dùng: "tiếp tục" sau câu hỏi duyệt; Q1, Q2 theo đề xuất)
*   **Date:** 2026-10-27
*   **Nghiên cứu:** [EGM (google/elephant-goldfish-model)](../reports/research-261027-1505-egm-elephant-goldfish.md) ·
    [Kiro / Spec Kit / OpenSpec / superpowers](../reports/researcher-261027-1500-spec-driven-coding-tools.md)

--------------------------------------------------------------------------------

## Overview

Thêm vào SenClaw một luồng "thiết kế trước, code sau" theo mô hình Elephant-Goldfish (EGM) của Google: agent của chat
(Elephant — có toàn bộ ngữ cảnh) viết spec vào repo; bốn reviewer **không có ngữ cảnh hội thoại** (Goldfish) soi spec
theo bốn hướng **Cảnh, Kiểm, Hiểu, Phản biện**; **chỉ người** duyệt. Khác EGM, SenClaw ép luật bằng code: khi một thư
mục có spec đang mở, mọi lần ghi vào thư mục đó bị từ chối cho tới khi spec được duyệt, và sau khi duyệt, mỗi chat phải
đọc trọn bản spec hiện hành trước lần ghi đầu tiên.

**Thuật ngữ** (dùng trong cả tài liệu):

| Từ | Nghĩa |
|---|---|
| Phiên Code | chat có `group_type = "code"` (tạo ở màn "Code" / `POST /api/code/sessions`), gắn với một thư mục |
| Chat dự án | chat có `project_id` (thuộc một Project, có root) |
| Coding session | cờ của engine (`ConversationConfig.coding_session`): phiên Code, chat dự án, **hoặc** chat có thư mục làm việc là git repo |
| Main agent / subagent | vòng lặp chính của một chat / vòng lặp con do tool `Task` tạo (`is_subagent = true`, `chat_jid = ""`) |
| jid | id của một chat (`code:<uuid>`, `web:…`, `tg:…`) |
| Đường tự động | lịch hẹn (scheduler), DAG/cowork, kanban worker, one-shot — đi thẳng vào pool, không qua bộ xử lý lệnh của gateway |
| `codeAutonomy` | `ask \| auto \| full`: chat code chạy tới đâu thì hỏi người; `full` không hỏi gì. Trực giao với luồng spec |
| Repo map | outline (tên file + chữ ký hàm/kiểu) do `src/repo_map/` dựng bằng tree-sitter |
| `<project_commands>` | lệnh build/test/lint đọc từ manifest (`repo_map::commands::render`) |
| Turn context | khối chữ pool gắn vào **bản sao** tin nhắn user cuối của mỗi lệnh gọi model trong lượt (`set_turn_context`), không lưu vào lịch sử |
| Root | thư mục mà spec thuộc về (nơi `/brainstorm` được gõ) |
| Spec active | spec đang mở của một root (tối đa một) |
| Receipt | dấu "chat X đã đọc trọn spec P ở hash H" |
| Zero-Decision Rule | (EGM) kế hoạch không để lại quyết định thiết kế nào cho người code |
| gemma-4-e4b | model local nhỏ người dùng chạy hằng ngày; đã thấy nó bỏ qua lời dặn trong prompt |

## The Problem

1. **Agent nhảy vào code.** Coding session không còn `EnterPlanMode` (model nhỏ kẹt 100-500 s trong Plan mode —
   CLAUDE.md "Code autonomy"). Không còn chỗ nào bắt thiết kế trước.
2. **Thiết kế không sống qua phiên.** Plan của `ExitPlanMode` nằm trong DB của daemon theo từng chat, không ở repo; chat
   khác cùng thư mục không biết nó. Chỉ dẫn dự án (AGENTS.md…) chỉ gửi ở lượt đầu.
3. **Prompt không đủ.** EGM, Spec Kit, superpowers chỉ dặn bằng lời. gemma-4-e4b đã bỏ qua lời dặn nhiều lần (bộ nhớ dự
   án `senclaw-code-v3.md`: reminder kiểm thử, dừng sau câu "Đây là nội dung cho file X:"). Không công cụ nào trong 6
   công cụ khảo sát gắn việc duyệt với nội dung (hash) hay với một người.
4. **Tự review thì tự xác nhận.** Ngữ cảnh vừa viết vừa soát không thấy giả định ngầm của chính nó.

## Goals

*   **G1 — Thiết kế trước khi code, có cổng thật.** Root có spec active chưa duyệt (hoặc sửa sau khi duyệt) → mọi `Edit`,
    `Write`, `NotebookEdit` vào file trong root (trừ `docs/designs/*.md`) và mọi `Bash` không chỉ-đọc chạy trong root
    đều bị từ chối, kèm lý do và lệnh để đi tiếp.
*   **G2 — Mọi chat xem spec trước khi làm tiếp.** Mỗi lượt của mọi chat có thư mục làm việc nằm trong một root có spec
    active mang khối `<spec>` (đường dẫn, giai đoạn, mục tiêu, việc chưa xong, luật). Sau khi duyệt, main agent của một
    chat không ghi được gì cho tới khi chính chat đó đã `Read` trọn bản spec **đúng hash hiện tại**.
*   **G3 — Lệnh:** `/brainstorm <ý tưởng>`, `/draft`, `/review`, `/approve`, `/spec`.
*   **G4 — Review 4 hướng kiểu Goldfish:** Cảnh, Kiểm, Hiểu, Phản biện; mỗi hướng một lệnh gọi model ngữ cảnh trống;
    "Kiểm" có thêm kiểm tra bằng máy. Mức Blocker / Major / Nit. Kết quả gắn với hash.
*   **G5 — Chỉ người duyệt, trong phạm vi tool của agent.** `/approve` chỉ nhận từ tin nhắn người gửi qua gateway (WS,
    kênh, REST chat của phiên Code). Mọi `Edit`/`Write` vào kho state của luồng spec bị từ chối ở mọi mức autonomy.
    Ngoài phạm vi: `Bash` ghi đè state khi `codeAutonomy = full` (người đã chọn không bị hỏi) — xem Risks.
*   **G6 — Chạy được với model local nhỏ.** Bốn lệnh gọi review tuần tự, tắt thinking; khối `<spec>` ≤ 1.200 ký tự; kiểm
    tra máy không tốn token.

## Non-Goals

*   Drift report; swarm 6 lăng kính cho brainstorm; reviewer `structural`/`style`; bước "intermediate zoom out".
*   Giao diện riêng cho spec (panel, nút duyệt, trình xem báo cáo). Đợt này chỉ: gợi ý 5 lệnh trong ô soạn tin (web +
    desktop) và công tắc `specFlow` trong Settings → Agent behavior (web + desktop).
*   Chặn: tool MCP ghi file (`doc_write`, wiki), lịch `script`/`script-agent` (chạy shell ngoài vòng agent), subagent
    chạy trong git worktree riêng (`~/.senclaw/worktrees/…`, sửa nằm trên nhánh, người merge). `sbx_run` và `js_eval`
    không cần chặn: chạy trong sandbox tạm / không có filesystem.
*   Ép agent chỉ sửa file liệt kê trong Target Files. Cổng chỉ ép "đã duyệt" + "đã đọc"; "làm đúng spec" là lời dặn.
*   Thay Plan mode / `ExitPlanMode` cho chat thường.

## Open Questions

*   **Q1 — Mặc định của `specFlow`:** `on` (cổng chỉ bật khi root đã có spec; sửa nhỏ chạy như cũ) hay `required`
    (phiên Code và chat dự án phải có spec trước lần ghi đầu tiên; sửa nhỏ: `/spec skip`)? → **Đã chốt: `required`.**
*   **Q2 — `/review` trên brief:** chạy 4 hướng ở chế độ BRAINSTORM (đề xuất), hay từ chối "chạy /draft trước"?
    → **Đã chốt: chạy 4 hướng ở chế độ BRAINSTORM.**

## Critical User Journeys

### CUJ 1: Tính năng mới, thiết kế rồi mới code
*   **Given**: phiên Code trên `~/p/shop`, `specFlow = on`, chưa có spec.
*   **When**: `/brainstorm Thêm webhook khi đơn hàng đổi trạng thái` → trả lời từng câu hỏi → `/draft` → `/review` →
    agent xử lý finding → `/review` (0 Blocker) → `/approve` → "làm đi".
*   **Then**: file `docs/designs/261027-them-webhook-khi-don-hang-doi.md`; trước `/approve` mọi lần ghi ngoài
    `docs/designs/*.md` bị từ chối; sau `/approve` agent `Read` trọn spec, sửa code, tick `- [x]`; `/spec` báo
    `approved · 3/7 tasks`.

### CUJ 2: Chat thứ hai làm tiếp
*   **Given**: spec CUJ 1 đã duyệt; mở phiên Code mới cùng repo.
*   **When**: "làm tiếp task T4".
*   **Then**: lượt đầu có `<spec>` nêu đường dẫn và T4; `Edit` trước khi đọc → "Read docs/designs/… first"; `Read` trọn
    file → sửa được.

### CUJ 3: Spec bị sửa sau khi duyệt
*   **Given**: spec đã duyệt; agent sửa mục Technical Plan.
*   **When**: agent định sửa `src/hook.rs`.
*   **Then**: từ chối "changed after approval"; `/spec` báo `changed`; tick checkbox, đổi `**Status:**`/`**Date:**`, ghi
    thêm vào `## Review Log` cuối file, đổi NFC/NFD, CRLF, khoảng trắng cuối dòng **không** gây `changed`.

### CUJ 4: Sửa nhỏ khi `specFlow = required`
*   **Given**: `required`, phiên Code, repo chưa có spec.
*   **When**: nhờ sửa lỗi chính tả; agent gọi `Edit`.
*   **Then**: từ chối, chữ nói "/brainstorm … hoặc /spec skip"; người gõ `/spec skip`; sửa được tới khi lịch sử chat bị
    xoá (`/reset`, "clear context", compaction) hoặc daemon khởi động lại.

## Technical Plan

```text
 person ─▶ gateway: WS handler · message_router (kênh) · POST /api/code/sessions/:id/chat
             └─ specs::commands::dispatch(text, binding, via) ─▶ reply, no LLM   (/approve, /spec*, lỗi của 3 lệnh kia)
             └─ else ─▶ enqueue_and_process (3 lệnh agent KHÔNG đi đường "mid-turn": chờ GroupQueue)
 AgentPool lượt mới:
   set_spec_flow(jid, effective flow) · /review → specs::review::run (4 goldfish) · prompt viết lại · <spec> vào turn context
 run_single_tool:  sau PreToolUse hook, trước nhánh permission ─▶ specs::gate::check ─▶ "Error: [spec] …" | đi tiếp
                   sau Read thành công ─▶ specs::gate::record_read
 kho:  repo  <root>/docs/designs/<slug>.md                    agent ghi; người đọc; commit cùng code
       daemon <SENCLAW_HOME>/specs/<key>/state.json           chỉ daemon ghi
              <SENCLAW_HOME>/specs/<key>/reviews/<slug>-<YYYYMMDDTHHMM>.md
```

### Root, tra cứu, kho state

*   **Thư mục của chat** — `specs::session_dir(binding)`: `current_dir` trong file workspace-state của chat (tên từ
    `projects::workspace_state_file_name(&projects::workspace_state_key(binding))`, nằm ở `SENCLAW_HOME`; pool ghi nó khi
    tạo engine), nếu thiếu thì `allowed_work_dirs[0]`. Hợp lệ khi `checkpoints::checkpointable_dir(dir, binding.group_type
    == "code" || binding.project_id.is_some())` trả `Some` (không `/`, không `$HOME`, phải là phiên Code / chat dự án / git
    repo). Chỉ dùng cho lệnh (`/brainstorm` lấy nó làm root mới).
*   **`key`** = 16 ký tự hex đầu của `sha256(đường dẫn canonical của root)`.
*   **Tra cứu** — `SpecStore::find_root(start) -> Option<(PathBuf, State)>`: chuẩn hoá `start` (bỏ `.`/`..` theo chữ, rồi
    canonicalize phần sâu nhất đang tồn tại và nối phần còn lại), đi lên từng thư mục cha, **dừng trước** `$HOME` và `/`;
    thư mục đầu tiên có `specs/<key>/state.json` với `active` khác rỗng là root. Cổng và khối `<spec>` đều dùng hàm này;
    cổng bắt đầu từ **thư mục của file đích** (Edit/Write/NotebookEdit) hoặc `working_dir` (Bash), nên `cd`,
    `workspace_switch` hay đường dẫn tuyệt đối đều không né được.
*   **State** (`state.json`):

    ```json
    { "root": "/Users/x/p/shop", "active": "docs/designs/261027-them-webhook.md",
      "specs": { "docs/designs/261027-them-webhook.md": {
          "created_at": "2026-10-27T15:00:00+07:00", "title": "Thêm webhook khi đơn hàng đổi trạng thái",
          "review":   { "hash": "…64 hex", "at": "…", "mode": "design", "complete": true,
                        "blocker": 0, "major": 2, "nit": 3, "lenses": {"canh":"ok","kiem":"ok","hieu":"ok","phan_bien":"ok"},
                        "report": "reviews/261027-them-webhook-20261027T1530.md" },
          "approval": { "hash": "…64 hex", "at": "…", "via": "web", "forced": false } } } }
    ```

    Mọi lần ghi qua `SpecStore::update(root, |state| …)`: một `Mutex` toàn tiến trình, đọc lại file, áp closure, ghi bằng
    `tools::file_io::write_atomic`. Review và approval là hai trường riêng, nên một `/approve` trong lúc review chạy không
    bị ghi đè; review chỉ mở khoá khi `review.hash` bằng hash hiện tại.

### Hash và giai đoạn

*   **`design_hash(text)`** = sha256 hex của văn bản sau: chuẩn hoá Unicode NFC; CRLF→LF; bỏ khoảng trắng cuối dòng;
    `- [x]`/`- [X]`/`* [x]` → `- [ ]`/`* [ ]`; bỏ các dòng chứa `**Status:**` hoặc `**Date:**`; bỏ mục `## Review Log`
    **chỉ khi** nó là heading `## ` cuối cùng của file (từ heading đó tới hết file). "Version" hiển thị = 8 ký tự đầu.
*   **Giai đoạn** (tính mỗi lần, không lưu), theo thứ tự kiểm:
    `missing` (file chưa có) → `brief` (chưa có dòng `## Detailed Implementation Plan`) → `changed` (có approval, hash
    khác) → `approved` (approval.hash = hash) → `reviewed` (review.hash = hash, mode `design`) → `draft`.

### Effective flow của một chat

Pool tính mỗi lượt và đẩy vào engine bằng `CoreApi::set_spec_flow(jid, flow)` (như `set_autonomy`) →
`ZenCoreOptions.spec_flow` → `ConversationConfig.spec_flow` → `RunContext.spec_flow`. Tool `Task` nhận flow của chat cha
qua closure `spec_flow` (như `profile_resolver`) và đặt vào `QueryConfig` của subagent.

| `specFlow` (config) | Effective |
|---|---|
| `off` | `Off` |
| `on` | `On` |
| `required` | `Required` nếu (`group_type == "code"` hoặc có `project_id`) và jid không nằm trong tập `skip`; còn lại `On` |

Mặc định khi không ai đặt (ACP, one-shot, test): `On`. Tập `skip` (jid đã `/spec skip`) ở trong bộ nhớ, xoá cùng receipt.

### Cổng — `specs::gate::check(&GateInput, &SpecStore) -> Option<String>`

`GateInput { tool: permission_name(), input (đã resolve đường dẫn), working_dir, chat_jid, is_subagent, flow }`. Gọi
trong `run_single_tool` sau khối PreToolUse, trước `if !tool.is_read_only()` — nên autonomy `full`, cờ skip-permission
và hook PrePermission đều không vượt qua. Từ chối = `ToolResult { is_error: true, content: "Error: [spec] …" }` và một
`ToolExecutionError` event như nhánh hook chặn.

1. `flow == Off` → qua.
2. Phân loại theo `permission_name()` (alias không né được): `Edit`/`Write` → ghi file `file_path`; `NotebookEdit` → ghi
   file `notebook_path`; `Bash` mà `!shell_safety::is_readonly_safe_command(command)` → shell; còn lại → qua.
3. Ghi file mà đích (đã chuẩn hoá như find_root) nằm trong `SpecStore.base` → từ chối `STORE`.
4. `find_root(start)`; không thấy:
   *   `flow == Required` và đích nằm trong `working_dir` (đã chuẩn hoá) → từ chối `NO_SPEC` (main agent) hoặc
       `NO_SPEC_SUBAGENT` (subagent);
   *   còn lại → qua.
5. Thấy root, spec active `P`. Ghi file mà đích nằm trong `<root>/docs/designs/`, đuôi `.md`, và `symlink_metadata` của
   đích (nếu đã có) không phải symlink → qua.
6. Giai đoạn khác `approved` → từ chối `STAGE_<giai đoạn>`.
7. `approved`, main agent (`!is_subagent && chat_jid != ""`): receipt của jid phải đúng `(canonical P, hash hiện tại)`;
   thiếu receipt, receipt của file khác hoặc hash khác → từ chối `READ_FIRST`.
8. Qua.

**Receipt** — `record_read(jid, input, store)` sau một `Read` (theo `permission_name()`) trả `Ok`: nếu `file_path` (đã
canonical) là spec active của root tìm được từ nó, **và** `offset` không có hoặc ≤ 1, **và** `limit` không có hoặc ≥ số
dòng của file → ghi `jid → (P, design_hash lúc đó)`. `forget(jid)` (xoá receipt + skip) ở: `clearContextAndStart`
(`conversation.rs` ~2495), xoá session trong `agent_pool/engine.rs` (~729), `reset` (`command_dispatcher.rs` ~151), và
`on_compact_exec` trong `pool.rs` (~3240). Daemon khởi động lại → mất hết (hướng an toàn).

**Lời từ chối lặp lại.** `EditTally` bỏ qua kết quả bắt đầu bằng `Error: [spec]` (không sinh nhắc "No change was applied"
để agent khỏi thử lại một lần ghi chắc chắn bị chặn). Bộ phát hiện kẹt vẫn đếm: 4 lời từ chối giống nhau → một lần nhắc
đổi cách → lần nữa thì dừng lượt với câu trả lời cuối — đúng điều muốn: agent dừng và nói cho người biết cần lệnh gì.

**Sửa kèm:** `shell_safety` coi `tree -o <file>` / `tree --output…` là ghi (hiện lọt là "chỉ đọc", ảnh hưởng cả
checkpoint và autonomy).

### Lệnh

`specs::commands::dispatch(text, &binding, via: &str) -> Option<String>` gọi ở **ba** chỗ ngay sau `dispatch_command`
(hoặc trước `submit_user_message`): WS handler (`handlers.rs` ~1170), `message_router.rs` (~212), REST chat của phiên
Code (`code_sessions.rs` ~521, trả `{ "ok": true, "reply": "…" }`). Nó trả lời ngay cho `/approve`, `/spec*`, và cho
`/brainstorm|/draft|/review` khi có lỗi; trả `None` để tin đi tiếp. `enqueue_and_process` (`lib.rs` ~160) bỏ đường
"mid-turn" khi `specs::commands::is_agent_command(text)` để lệnh chờ trong GroupQueue và được pool viết lại. Pool
(`process_and_wait_inner_with_attachments`, trước "composer directives"): `parse_agent_command` → dựng prompt → **bỏ
qua** bước composer directives của lượt đó. Lệnh nhận dạng khi tin bắt đầu bằng `/brainstorm`, `/draft`, `/review`,
`/approve`, `/spec` (chữ thường, theo sau là khoảng trắng hoặc hết tin); chúng che skill trùng tên.

| Lệnh | Điều kiện | Kết quả |
|---|---|---|
| `/brainstorm <ý tưởng>` | flow ≠ off; có ý tưởng; thư mục chat hợp lệ; root chưa có spec active (hoặc file của nó đã bị xoá) | tạo spec active `docs/designs/<slug>.md`; agent nhận lời dặn BRAINSTORM |
| `/draft [path]` | flow ≠ off; có spec active (hoặc `path` là `docs/designs/*.md` có thật → thành active); file có thật | lời dặn DRAFT |
| `/review [path]` | như `/draft` | chạy review; lời dặn REVIEW_RESULTS |
| `/approve` | spec active; file có; giai đoạn `reviewed` (đã có Detailed, review của hash hiện tại, mode `design`); review `complete`; `blocker == 0` | ghi approval |
| `/approve force` | spec active; file có; có `## Detailed Implementation Plan` | ghi approval `forced: true` |
| `/spec` | — | trạng thái |
| `/spec use <path>` | `path` = `docs/designs/*.md` có thật trong root của chat | đặt active |
| `/spec done` | có spec active (file có hay không) | bỏ active; file ở lại repo |
| `/spec skip` | effective flow của chat là `Required` | thêm jid vào `skip` |

**Slug:** `YYMMDD-` + `docs::slugify(6 từ đầu của ý tưởng)`, cắt ≤ 40 ký tự ở dấu `-` gần nhất; trùng file thì thêm
`-2`, `-3`… Code đặt tên, agent không chọn. Ví dụ: "Thêm webhook khi đơn hàng đổi trạng thái" →
`261027-them-webhook-khi-don-hang-doi.md`.

**`via`:** `web` (WS), tên kênh (`telegram`, `feishu`, `wechat`), `rest`. Đường tự động không đi qua `dispatch` nên
không duyệt được; pool coi `/approve` như văn bản thường.

### Văn bản cố định (tiếng Anh; agent tự trả lời theo ngôn ngữ của người)

| Mã | Chữ |
|---|---|
| `OFF` | `Spec flow is off (Settings → Agent behavior → Spec flow).` |
| `NO_FOLDER` | `This chat has no project folder (a Code session, a project chat or a git repository is needed).` |
| `BUSY` | `<path> is still open. Continue it with /draft or /review, or close it with /spec done.` |
| `NO_SPEC_CMD` | `No open spec in <root>. Start one with /brainstorm <idea>.` |
| `APPROVED` | `Approved <path> (version <v8>). Code changes in <root> are unlocked; every chat reads this spec before its first change. Editing the design text locks them again until /review and /approve.` |
| `APPROVE_NEEDS` | `Cannot approve <path>: <lý do>. /approve force approves it anyway.` — lý do: `it has no "## Detailed Implementation Plan" yet (run /draft)` · `it has not been reviewed in its current version (run /review)` · `the last review did not finish (<lens>: <status>)` · `the last review has <n> blocker(s)` |
| `SKIPPED` | `Spec check skipped for this chat until its history is cleared.` |
| gate `STORE` | `Error: [spec] The spec flow's own records cannot be changed by a tool.` |
| gate `NO_SPEC` | `Error: [spec] No design spec for this folder yet. Changes here need one: ask the person to run /brainstorm <idea>, or /spec skip for a small fix.` |
| gate `NO_SPEC_SUBAGENT` | `Error: [spec] No design spec for this folder yet; a subagent cannot change files here. Report back to the main agent.` |
| gate `STAGE_*` | `Error: [spec] <path> is <stage>, not approved. Only files in docs/designs/ may change until the person runs /approve.` (`changed`: `… changed after it was approved. Code is locked until the person runs /review and /approve again.`) |
| gate `READ_FIRST` | `Error: [spec] Read <path> in full first (no offset/limit): it is the approved design for this folder.` |

`/spec` trả lời: dòng 1 `<path> — <title>`; dòng 2 `stage: <stage> · version <v8>`; dòng 3 `review: <at> · <b> blocker,
<m> major, <n> nit · <complete|incomplete>` hoặc `review: none`; dòng 4 `approval: <at> via <via>[ (forced)]` hoặc
`approval: none`; dòng 5 `tasks: <done>/<total> · open questions: <n>`. Không có spec active: `NO_SPEC_CMD`.

### Lời dặn viết lại lượt (prompt)

Mở đầu `[Spec flow — <BRAINSTORM|DRAFT|REVIEW RESULTS>] The person ran /<lệnh>.` + lời người gõ nguyên văn trong
`<request>…</request>` + đường dẫn spec + luật dưới + `Reply in the person's language.`

*   **BRAINSTORM:** Source files are locked until the person approves a spec; you may read code. Read the code the idea
    touches before asking. Ask ONE question per reply (root problem, who uses it, what must not break, what is out of
    scope). When you can state the idea in 2-3 sentences, ask the person to confirm them. After they confirm, Write
    `<path>` with exactly these headings: `# <title>`, `*   **Status:** Brief`, `*   **Date:** <today>`, `## Overview`,
    `## The Problem`, `## Goals`, `## Non-Goals`, `## Open Questions`, `## Anchored Hypothesis` (the confirmed 2-3
    sentences), `## Options` (2-3 approaches, pros/cons). Then give the path and the next steps: /review to stress-test
    the idea, /draft to write the full spec.
*   **DRAFT:** Read `<path>`. Turn it into the full spec with the template below, keeping what was agreed. First write
    Overview → Risks, then stop and ask the person ONE question about them; only after they agree, write
    `## Detailed Implementation Plan`. Every target file is `#### N. \`path\` (Create|Modify|Delete)`; commands in the
    Verification Plan come from `<project_commands>`; no real code, only signatures and schemas; unknowns go to Open
    Questions. Template: `# <title>`, `**Status:** Draft`, `**Date:**`, `## Overview`, `## The Problem`, `## Goals`,
    `## Non-Goals`, `## Open Questions`, `## Critical User Journeys` (Given/When/Then), `## Technical Plan`,
    `## Alternatives Considered`, `## Risks`, `## Detailed Implementation Plan` → `### Target Files`, `### Tasks`
    (`- [x] T1 …`), `### Verification Plan`, `## Review Log` (last).
*   **REVIEW RESULTS:** Four reviewers who never saw this conversation read `<path>` (version `<v8>`). Findings (≤ 40,
    Blocker first, each `[SEVERITY] lens · where :: problem :: fix`). Report saved at `<report path>`. Do not change
    source files. For each Blocker and Major choose FIX (edit the spec), CONTEST (only with evidence a reviewer could
    read in the spec or the code — never for a Hiểu finding), or ESCALATE (a choice only the person can make: ask, one
    question per reply). Nits: fix if trivial, else DECLINE. Add one line per decision under `## Review Log` (keep it the
    last section). Then tell the person the counts, what you changed, what needs their decision, and that /review checks
    the new version and /approve unlocks the code.

### Review — `specs::review::run(ReviewInput) -> Review`

`ReviewInput { root, path, config_path, profile: group.llm_config_id.as_deref(), jid, usage: Option<Arc<UsageRecorder>> }`.
Đọc file **một lần** ở đầu (text + hash dùng suốt). Mode `design` nếu có `## Detailed Implementation Plan`, không thì
`brainstorm`. Bốn hướng chạy **tuần tự**; mỗi hướng `llm_config::chat_completion_with(config_path, profile, system, user,
4096, None, CompletionOptions { thinking: Some(false) })` trong `tokio::time::timeout(600 s)`; usage ghi bằng
`record_completion(&usage, jid, "spec:review", &r)`.

| Hướng | Persona | DESIGN: đọc + bằng chứng | BRAINSTORM: đọc + bằng chứng | Hỏi |
|---|---|---|---|---|
| **Cảnh** (`canh`) | Skeptical Architect | cả file + chỉ dẫn dự án + repo map | cả file + chỉ dẫn + repo map | DESIGN: gốc vấn đề hay triệu chứng, YAGNI, tối ưu sớm, phát minh lại thứ repo có, sa vào chi tiết, sai tầng. BRAINSTORM: có cần làm không, thứ gì có sẵn giải được |
| **Kiểm** (`kiem`) | Tech Lead + Codebase Auditor | từ `## Detailed…` tới hết + kết quả kiểm máy + outline file `Modify` + `<project_commands>` | cả file + repo map | DESIGN: Zero-Decision Rule, thiếu file/bước/test, chữ ký không khớp outline. BRAINSTORM: migration, tương thích, rollout/rollback |
| **Hiểu** (`hieu`) | Naive Reader | cả file, không gì khác | cả file | thuật ngữ chưa định nghĩa, điều kiện ngầm, "tự động lo", hai cách hiểu |
| **Phản biện** (`phan_bien`) | Design Critic | phần trên `## Detailed…` | cả file | phương án thay thế bị loại có lý do, giả định chưa chứng minh, điểm hỏng, bảo mật, giới hạn / chi phí, nút thắt |

*   **Chỉ dẫn dự án:** file đầu tiên có trong `SENCLAW.md`, `AGENTS.md`, `CLAUDE.md` ở root, ≤ 3.000 ký tự đầu.
*   **Repo map:** `repo_map::index_ready(root)` trong `timeout(60 s)`, rồi `repo_map::map_for_prompt(root, text)`; `None`
    → dòng `(no repo map for this folder)`.
*   **Outline file `Modify`:** từ index ở trên, `files[rel].defs[].sig` của từng file `Modify`, ≤ 40 dòng mỗi file;
    không có index → bỏ qua, ghi `(no outline)`.
*   **System prompt mỗi hướng** = 3 luật Goldfish của EGM (chỉ đọc; không có lịch sử; không nói chuyện phiếm) + vai + danh
    sách câu hỏi của hướng + định dạng ra:
    `One finding per line: [BLOCKER] where :: problem :: fix (also [MAJOR], [NIT]). BLOCKER = the code cannot be written
    correctly from this text; MAJOR = it can, but likely wrong or costly; NIT = wording. If you find nothing, reply
    exactly NONE.`

**Kiểm tra máy** (mode `design`, 0 token; thành finding của hướng `kiem`, ghi `machine`):
*   thiếu hoặc rỗng `## Goals` hay `## Detailed Implementation Plan` → BLOCKER;
*   thiếu heading khác của template DRAFT → MAJOR (mỗi heading một finding);
*   `#### N. \`path\` (Modify|Delete)` mà `path` không tồn tại trong root → BLOCKER; `(Create)` mà đã tồn tại → MAJOR;
    mục trong `### Target Files` không có nhãn → một MAJOR chung;
*   `### Verification Plan` không có lệnh nào trong backtick → MAJOR;
*   còn `TBD`, `TODO`, `[File Path`, `[Feature`, `<title>` → MAJOR;
*   `## Open Questions` còn gạch đầu dòng → MAJOR (người quyết có duyệt không);
*   file > 24.000 ký tự → MAJOR "too long to review in one pass — split the spec", các hướng đọc 24.000 ký tự đầu.

**Đọc kết quả** (`parse_findings`): bỏ `<think>…</think>`; với mỗi dòng: bỏ đầu dòng `-`, `*`, `•`, số thứ tự, `**`;
nhận mức từ `[BLOCKER]`/`BLOCKER:`/`Severity: Blocker`/`🔴` (tương tự MAJOR/`🟡`, NIT/MINOR/`🟢`, không phân biệt hoa
thường); phần còn lại tách theo `::` thành where / problem / fix (thiếu phần nào thì để trống). Trạng thái hướng: `ok`
nếu đọc được ≥ 1 finding hoặc cả câu trả lời (sau khi bỏ think, trim, bỏ dấu chấm cuối) là `NONE`; `cut` nếu
`finish == "length"`; `failed: <lỗi>` nếu lỗi gọi/timeout; còn lại `unparsed`. `complete` = cả 4 hướng `ok`.

**Ý nghĩa mức:** BLOCKER chặn `/approve` (trừ `force`); MAJOR và NIT không chặn. Một BLOCKER bị CONTEST vẫn chặn nếu lần
review sau còn nêu; người đọc `## Review Log` rồi dùng `/approve force` nếu đồng ý với agent.

**Báo cáo:** markdown: tiêu đề + path + version + mode + thời gian; bảng trạng thái 4 hướng; finding Blocker → Major → Nit
(mỗi cái ghi hướng); cuối là nguyên văn trả lời của từng hướng. Lưu `reviews/<slug>-<YYYYMMDDTHHMM>.md`; tóm tắt vào state
qua `update` dưới key `path` (không phụ thuộc spec còn active hay không).

### Khối `<spec>` mỗi lượt

Pool dựng khi effective flow ≠ `Off` và `find_root(thư mục làm việc của chat)` thấy root; nối vào `context` truyền cho
`set_turn_context`. `</spec` trong nội dung được thay bằng `<\/spec`. Cắt ở 1.200 ký tự.

```text
<spec>
Design spec for this folder: docs/designs/261027-them-webhook.md — "Thêm webhook khi đơn hàng đổi trạng thái"
Stage: approved · version 3f2a9c1d · review: 0 blocker, 2 major, 3 nit
Goals: <≤ 3 gạch đầu dòng đầu tiên của ## Goals, mỗi cái ≤ 120 ký tự>
Next tasks: <≤ 3 dòng "- [ ]" đầu tiên>
Open questions: <n>
Rule: <theo giai đoạn>
</spec>
```

Rule: `brief`/`draft`/`reviewed`/`missing` → `Design phase: only files in docs/designs/ may change; code unlocks when the
person runs /approve.` · `approved` → `Before your first change in this chat, Read the whole spec. Build what it says,
tick "- [x]" as tasks finish; if the code must differ from the design, stop and tell the person.` · `changed` → `The spec
changed after approval; code is locked until the person runs /review and /approve.`

### Setting `specFlow`

`off | on | required` trong `config.json` (`specFlow`), `get_spec_flow` (mặc định `required`) / `save_spec_flow` trong
`group_manager/llm.rs`; GET/POST `/api/agent-behavior` (sai giá trị → 400 `specFlow must be off, on or required`). Pool đọc
mỗi lượt như `codeAutonomy`.

## Alternatives Considered

### A: Chỉ prompt (đóng gói EGM thành skill)
*   **Pros**: không đụng engine. **Cons**: gemma-4-e4b bỏ qua lời dặn; không gì bảo đảm chat sau đọc spec.
*   **Reason for Rejection**: hai yêu cầu đều là "đảm bảo".

### B: Dùng lại Plan mode / `ExitPlanMode`
*   **Pros**: có cổng và thẻ duyệt. **Cons**: plan nằm trong DB theo chat; coding session đã bỏ `EnterPlanMode`; "clear
    context" từng làm mất yêu cầu (`plan-exit-clear-context-wipes-request.md`); duyệt không gắn nội dung.
*   **Reason for Rejection**: không đáp ứng G2, G5.

### C: Trạng thái duyệt trong file spec
*   **Pros**: một nguồn, đi cùng repo. **Cons**: agent sửa được → tự duyệt.
*   **Reason for Rejection**: vi phạm G5.

### D: Review bằng tool agent tự gọi hoặc `Task` subagent
*   **Pros**: thấy tiến độ dạng thẻ tool; subagent có tool đọc code. **Cons**: model nhỏ có thể không gọi; subagent kế
    thừa thiên lệch của lời giao việc; vòng tool × 4 rất chậm trên e4b.
*   **Reason for Rejection**: daemon gọi thẳng, ngữ cảnh trống thật, bằng chứng do daemon gom — tất định, rẻ.

### E: Lưu state trong SQLite (`Db::with_conn`)
*   **Pros**: quy ước chung. **Cons**: cổng ở `zen_core::run_tools` không có handle Db (engine không giữ Db).
*   **Reason for Rejection**: JSON nhỏ theo root như `checkpoints/<chat>/`; ghi bằng `write_atomic` có sẵn.

### F: Đặt spec ở `.senclaw/specs/` hoặc `specs/`
*   **Cons**: `.senclaw/` là chỗ của `mcp.json`; `specs/` va thư mục test của Ruby.
*   **Reason for Rejection**: theo EGM, `docs/designs/`.

### G: Biến `specFlow` toàn tiến trình
*   **Pros**: ít luồn dây. **Cons**: test chạy song song giẫm nhau; không mang được "chat này đã skip".
*   **Reason for Rejection**: luồn theo chat như `codeAutonomy`.

## Risks

*   **`Bash` ghi `state.json` khi `codeAutonomy = full`.** `~/.senclaw` là đường dẫn nhạy cảm nên `auto`/`ask` hỏi; ở
    `full` người đã chọn không bị hỏi. `Edit`/`Write` bị chặn cứng (bước 3). Lời từ chối không nói duyệt lưu ở đâu.
*   **Bash ghi file qua lệnh bị xếp nhầm "chỉ đọc".** Phụ thuộc `is_readonly_safe_command`; sửa `tree -o` trong đợt này.
*   **Review chậm trên model local:** ≤ 4 × (≤ 24.000 ký tự vào, ≤ 4.096 token ra), timeout 600 s mỗi hướng → tệ nhất
    40 phút. Chỉ chạy khi người gõ `/review`; UI hiện "đang gõ". Thời gian thật trên e4b đo trong e2e nếu LAN model trả lời.
*   **Model nhỏ viết sai định dạng** → hướng `unparsed` → `/approve` từ chối, `/approve force` vẫn dùng được; báo cáo giữ
    nguyên văn.
*   **Spec bị xoá / root bị đổi tên:** `find_root` không thấy → cổng thôi chặn (`on`) hoặc báo `NO_SPEC` (`required`, khi
    đó `/spec skip` vẫn chạy vì không cần root). `/spec done` đóng được spec có file đã mất.
*   **Lối thoát luôn có:** `/spec done`, `/approve force` (khi đã có Detailed), `/spec skip` (`required`),
    `specFlow = off` (tắt khối `<spec>`, cổng, lệnh; không đụng file trong repo).

## Detailed Implementation Plan

### Target Files

#### 1. `src/specs/mod.rs` (Create)
*   `SpecFlow { Off, On, Required }` (`parse`, `as_str`, `Default = On`); `SpecStore { base }` (`default()` =
    `util::paths::senclaw_home().join("specs")`, `at(base)` cho test); `State`, `SpecEntry`, `ReviewSummary`, `Approval`
    (serde, tên trường như JSON ở trên); `load`, `update`, `find_root`, `normalize_path`, `design_hash`, `Stage` +
    `stage(root, rel, entry)`, `slug_for(idea, today, root)`, `session_dir(binding)`, `capsule(root, state)`.

#### 2. `src/specs/gate.rs` (Create)
*   `GateInput`, `check`, `record_read`, `forget`, tập `skip` (`skip(jid)`, `is_skipped(jid)`), map receipt; hằng chữ
    `STORE`, `NO_SPEC`, `NO_SPEC_SUBAGENT`, `READ_FIRST`, hàm `stage_refusal(path, stage)`; `REFUSAL_PREFIX = "Error: [spec]"`.

#### 3. `src/specs/review.rs` (Create)
*   `Lens { Canh, Kiem, Hieu, PhanBien }`, `Mode`, `Severity`, `Finding`, `LensStatus`, `Review`; `machine_checks`,
    `lens_system`, `lens_user`, `parse_findings`, `run`, `render_report`, `findings_for_prompt`.

#### 4. `src/specs/commands.rs` (Create)
*   `is_agent_command`, `dispatch(text, binding, via)`, `AgentCommand`, `parse_agent_command`, `brainstorm_prompt`,
    `draft_prompt`, `review_prompt`, `status_text`; các hằng chữ của bảng "Văn bản cố định".

#### 5. `src/lib.rs` (Modify)
*   `pub mod specs;`; `enqueue_and_process`: điều kiện mid-turn thêm `&& !specs::commands::is_agent_command(text)`.

#### 6. `src/zen_core/run_tools.rs` (Modify)
*   `RunContext` thêm `is_subagent: bool`, `spec_flow: SpecFlow`; literal ở `run_serially` (~358) và `test_ctx` (~1212);
    gọi `gate::check` sau khối PreToolUse; `gate::record_read` sau `tool.call` trả `Ok` cho `Read`.

#### 7. `src/zen_core/conversation.rs` (Modify)
*   `ConversationConfig.spec_flow`; literal `RunContext` (~2413) truyền `is_subagent`, `spec_flow`; `gate::forget` cạnh
    `session_store::clear` (~2495); `EditTally` bỏ qua kết quả có `REFUSAL_PREFIX`; `may_change_workspace` không đổi.

#### 8. `src/zen_core/engine.rs`, `src/zen_core/mod.rs` (Modify)
*   `ZenCoreOptions.spec_flow` + `set_spec_flow`; chép vào `ConversationConfig` ở `start_query` (~2017) và chỗ tạo config
    khác (~2726); closure `spec_flow` cho tool `Task`.

#### 9. `src/tools/task.rs` (Modify)
*   Trường closure `spec_flow`; `QueryConfig.spec_flow` của subagent lấy từ nó.

#### 10. `src/agent/agent_pool/{traits.rs,engine.rs,pool.rs}` (Modify)
*   `CoreApi::set_spec_flow` (traits mặc định no-op; engine.rs gọi `engine.set_spec_flow`); `gate::forget` cạnh
    `session_store::clear` (engine.rs ~729); pool: tính effective flow mỗi lượt, `/review` + viết lại prompt trước
    composer directives (và bỏ qua bước đó), nối `<spec>` vào `context`, `gate::forget` trong `on_compact_exec`.

#### 11. `src/gateway/websocket_gateway/handlers.rs`, `src/gateway/message_router.rs`, `src/gateway/ui_server/code_sessions.rs` (Modify)
*   Gọi `specs::commands::dispatch` (via `web` / tên kênh / `rest`).

#### 12. `src/gateway/command_dispatcher.rs` (Modify)
*   5 dòng trong `COMMANDS_HELP`; `gate::forget(jid)` ở nhánh reset (~151).

#### 13. `src/gateway/group_manager/{types.rs,llm.rs,mod.rs}` (Modify) — `spec_flow`, `get_spec_flow`, `save_spec_flow`.

#### 14. `src/gateway/ui_server/agent_behavior_config.rs` (Modify) — GET/POST `specFlow`.

#### 15. `src/util/shell_safety.rs` (Modify) — `tree` với `-o`/`--output` không còn "chỉ đọc".

#### 16. `skills/code/SKILL.md` (Modify), `docs/spec-flow.md` (Create), `CLAUDE.md` (Modify)

#### 17. `../web-app/src/components/ChatView.tsx` (Modify) — `commands` = 5 lệnh (tên + mô tả)

#### 18. `../web-app/src/components/settings/AgentBehaviorSettings.tsx` (Modify), `SpecFlowControl.tsx` (Create, theo `CodeAutonomyControl.tsx`), `../web-app/src/i18n/vi.web.json` (Modify)

#### 19. `../desktop/lib/features/chat/widgets/slash_mention_input.dart` (Modify) — 5 lệnh cố định đứng trước skill trong danh sách `/`

#### 20. `../desktop/lib/features/settings/spec_flow_field.dart` (Create, theo `code_autonomy_field.dart`), `../desktop/lib/features/settings/settings_screen.dart` (Modify), `../desktop/lib/core/i18n/vi/settings_screen.dart` (Modify)

### Tasks

- [ ] T1 `specs/mod.rs` + test.
- [x] T2 `specs/gate.rs` + nối `run_tools`/`conversation`/`engine`/`task`/pool (flow, forget) + `shell_safety` + test.
- [x] T3 setting `specFlow` + test.
- [x] T4 `specs/review.rs` + test.
- [x] T5 `specs/commands.rs` + nối gateway (3 chỗ), `enqueue_and_process`, pool + test.
- [x] T6 khối `<spec>` + test.
- [x] T7 tài liệu.
- [x] T8 web-app + desktop.
- [x] T9 e2e CUJ 1-4 trên daemon nháp.

### Verification Plan

*   **Unit (tên test):** `specs::tests::{hash_ignores_checkboxes_status_date_crlf_nfc_and_trailing_review_log,
    hash_sees_text_under_a_review_log_heading_in_the_middle, find_root_walks_up_and_stops_before_home,
    slug_folds_vietnamese_and_numbers_duplicates, stage_order}`;
    `specs::gate::tests::{off_lets_everything_through, design_phase_refuses_code_allows_designs_md,
    dotdot_and_symlink_into_designs_are_refused, non_md_in_designs_is_refused, store_writes_are_always_refused,
    readonly_bash_passes_writing_bash_is_refused, approved_needs_full_read_receipt, partial_read_gives_no_receipt,
    changed_after_approval_is_refused, required_without_spec_refuses_main_and_subagent, skip_lets_main_agent_through,
    alias_is_gated_by_permission_name, target_in_other_spec_root_is_gated_after_workspace_switch}`;
    `specs::review::tests::{machine_checks_*, parse_accepts_marker_variants, none_is_ok_empty_is_unparsed,
    length_finish_is_cut, think_block_is_stripped}`;
    `specs::commands::tests::{brainstorm_creates_active_spec_and_prompt, busy_folder_is_refused, approve_conditions,
    approve_force_needs_detailed_section, spec_status_text, off_replies_off}`;
    `shell_safety::tests::tree_output_flag_is_not_readonly`; `agent_behavior` test cho `specFlow`;
    `run_tools` test: lời từ chối đi trước permission checker (`full`).
*   **Lệnh:** `cargo test --lib specs::`, `cargo test --lib zen_core`, `cargo test --lib shell_safety`,
    `cargo test --lib agent_behavior`, `cargo test --lib command_dispatcher`, rồi `cargo test --lib`;
    `cargo test --lib committed_spec_matches_the_routers` (không thêm route).
*   **Client:** `npm run build` (web-app); `flutter analyze` + test widget liên quan (desktop) nếu đĩa đủ.
*   **E2E:** daemon nháp (`SENCLAW_HOME`, `HOME`, cwd nháp, cổng khác 18788/18789, guard sandbox-exec) + mock LLM của
    `plans/reports/code-v3-e2e/` → CUJ 1-4; kiểm `state.json`, lời từ chối, receipt.
*   **Đĩa:** ~3,7 GB trống → build `--lib` trong target chung, không worktree.

## Review Log

Triển khai 2026-10-27 21:05: xem [báo cáo](../reports/cook-261027-2105-code-spec-flow-impl.md).

Vòng 1 (2026-10-27, 4 Goldfish Explore, ngữ cảnh trống): Cảnh 8 · Kiểm 12 · Hiểu 10 · Phản biện 12 finding.

*   Hiểu-B receipt thiếu không định nghĩa → FIX (bước 7: thiếu = từ chối).
*   Hiểu-B / Kiểm-B / Phản biện duyệt được brief → FIX (`/approve` cần `reviewed`, mode `design`; `force` cần Detailed).
*   Cảnh-B / Phản biện-B / Hiểu-M `Write` vào `state.json` → FIX (bước 3 chặn cứng; G5 nói rõ phạm vi).
*   Kiểm-B / Phản biện-B so đường dẫn theo chữ, symlink, file không phải `.md` → FIX (chuẩn hoá, symlink, `.md`).
*   Phản biện-B / Cảnh-M root đổi theo `workspace_switch` → FIX (tra theo file đích).
*   Phản biện-B / Kiểm-M / Cảnh-M parse rỗng = sạch, cắt `length`, thinking → FIX (`ok` cần finding hoặc `NONE`; `cut`;
    `thinking: Some(false)`, 4.096 token, timeout).
*   Kiểm-B lời gọi `chat_completion` không biên dịch, thiếu usage recorder → FIX (`as_deref`, `record_completion`).
*   Phản biện-B đường thực thi khác (`script` schedule, `sbx_run`, `js_eval`, alias) → FIX alias (`permission_name`);
    CONTEST `sbx_run`/`js_eval` (sandbox tạm / "no filesystem" — `mcp/sandbox_server.rs` ~364, `mcp/js_server.rs` ~331);
    `script` schedule → Non-Goals.
*   Phản biện-M subagent lách `required`, worktree → FIX (flow truyền vào subagent, `NO_SPEC_SUBAGENT`); worktree → Non-Goals.
*   Phản biện-M / Cảnh-M receipt sau compaction, đọc một phần → FIX (`forget` ở compaction; offset/limit).
*   Phản biện-M `tree -o` → FIX (`shell_safety`).
*   Phản biện-M hash churn, lối thoát → FIX (NFC, bỏ Date; mục Risks liệt kê lối thoát).
*   Phản biện-M race review/approve → FIX (`update` theo trường, hash chụp ở đầu).
*   Phản biện-M / Cảnh-M thành viên nhóm Telegram gõ `/approve` → CONTEST: mọi thành viên của chat đã ghép cặp vốn ra lệnh
    cho agent sửa code được (CLAUDE.md "Telegram access", "every chat has full (admin) privileges"); duyệt spec không thêm
    quyền nào.
*   Cảnh-M / Kiểm-M REST chat của phiên Code bỏ qua lệnh; lệnh gõ giữa lượt → FIX (3 chỗ gọi; bỏ đường mid-turn).
*   Cảnh-M biến toàn tiến trình → FIX (luồn theo chat; Alternative G).
*   Cảnh-M YAGNI `required`/`force`/review brief → CONTEST `required` (người dùng yêu cầu "đảm bảo thiết kế trước khi
    code"), CONTEST `force` (lối thoát duy nhất khi review local hỏng); review brief → ESCALATE (Q2).
*   Cảnh-N dùng lại helper → FIX (`write_atomic`, `docs::slugify`, literal thứ ba); Db → CONTEST (Alternative E).
*   Kiểm-M che giấu chữ dưới `## Review Log` giữa file → FIX (chỉ bỏ khi là mục cuối).
*   Kiểm-M lời từ chối kích hoạt stuck/EditTally → FIX (EditTally bỏ qua; stuck dừng lượt là chủ ý — ghi ở mục Cổng).
*   Kiểm-M outline không có hàm sẵn → FIX (`index_ready` + `defs[].sig`).
*   Kiểm-M quyết định bị bỏ ngỏ (chữ, parser, test) → FIX (bảng văn bản, luật parse, tên test). Q1, Q2 → ESCALATE.
*   Kiểm-M nhãn file client, nguồn lệnh desktop → FIX. Kiểm-N đường xoá receipt/skip, cờ của `checkpointable_dir` → FIX.
*   Hiểu-M thuật ngữ, "phiên Code", brief template, slug, severity, skip → FIX (bảng thuật ngữ, template BRAINSTORM, slug,
    ý nghĩa mức, skip trong bộ nhớ).
*   Phản biện-M Target Files không bị ép; chi phí review → Non-Goals (chỉ ép duyệt + đọc); thời gian đo trong e2e.

Code review sau triển khai (2026-10-27 21:17, reviewer code ngữ cảnh trống): 9 finding.

*   M1 lịch hẹn `/draft <path>` mở lại spec cũ đã duyệt → FIX (`resolve_open`: chỉ gateway đổi spec đang mở).
*   M2 phiên Code dùng chung `workspace-state-main.json` → FIX (`session_dir_in`: phiên Code dùng `allowed_work_dirs[0]`).
*   M3 retry vì lỗi tạm chạy lại `/review` → FIX (`retry_prompt` = lượt đã viết lại).
*   M4 subagent worktree bị `required` chặn → FIX (miễn dưới `SENCLAW_HOME`).
*   m5 `Bash` xét theo cwd → CONTEST/giới hạn: chặn mọi lệnh ghi dưới thư mục cha giữ cả thư mục không liên quan; ghi vào CLAUDE.md + docs.
*   m6 parser: "BLOCKER: None", gạch dài, heading `### Blockers`, `finish: ""` → FIX ba cái đầu; cái cuối ghi giới hạn.
*   m7 `..` sau symlink → FIX (`normalize_path` resolve từng tiền tố).
*   m8 `**Status:**` ở dòng task → FIX (chỉ bỏ ở phần đầu file).
*   m9 kênh: cursor qua cả lô → FIX (cursor dừng ở tin lệnh).
*   n: refusal phát `ToolExecutionError` → FIX (không phát). `forget` ở mọi compaction → giữ (an toàn hơn). Stop không huỷ review → giới hạn.

