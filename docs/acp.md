# Cổng ACP — SenClaw trong Zed / JetBrains / Kiro

> Trạng thái: **đã cài** (2026-09-12, code-v2 phase 7). Nguồn:
> [`src/acp/`](../src/acp/) (`jsonrpc.rs` khung JSON-RPC stdio, `ws.rs`
> client tới gateway, `mod.rs` ánh xạ), lệnh
> [`src/cli/commands/acp.rs`](../src/cli/commands/acp.rs).

## Mô hình

[Agent Client Protocol](https://agentclientprotocol.com) (Zed + JetBrains,
registry 03/2026) là JSON-RPC 2.0, mỗi thông điệp một dòng trên stdio:
editor spawn agent, gọi `initialize` → `session/new` → `session/prompt`, nhận
`session/update`, trả lời `session/request_permission`.

`senclaw acp` là **bộ dịch**, không phải engine: phía kia nó nối vào
WebSocket gateway của daemon đang chạy (`ws://127.0.0.1:18789`, token qua
`SENCLAW_WS_TOKEN` nếu daemon đặt). Không có daemon → lỗi rõ "is the daemon
running? (`senclaw web`)".

| ACP | SenClaw |
|---|---|
| `session/new {cwd}` | `register:group` chat `group_type=code`, `allowedWorkDirs=[cwd]`, jid `acp:<thư mục>:<8 ký tự>` + `subscribe` |
| `session/prompt {prompt[]}` | `message {groupJid, text}` — text block nối nhau, `resource` nhúng thành `<file uri=…>…</file>` |
| `agent:delta` | `session/update agent_message_chunk` (stream) |
| `tool:execution` | `session/update tool_call` — `kind` suy từ tên tool (Read→read, Edit/Write→edit, Bash→execute, Grep/find_symbol→search, WebFetch→fetch, Task/Plan→think), `status` completed/failed, diff của Edit làm content text, `locations` từ `path`. Tool vừa được duyệt qua permission thì **không** tạo entry mới: thành `tool_call_update` của chính call đó (ghép theo tên tool, cũ nhất trước) |
| `permission:request` | trước tiên `session/update tool_call` `status: pending` (id `<sid>-N`), rồi `session/request_permission` cùng `toolCallId` — option của daemon `agree` "Confirm" → `allow_once`, `allow` "…never ask … again" → `allow_always`, `refuse` "Reject" → `reject_once` (key/label khác: suy theo chữ), xếp allow trước vì daemon gửi theo thứ tự HashMap; trả lời → `permission:response`; editor huỷ → option `reject_once` (không bao giờ `reject_always`). Sau đó: duyệt → `tool_call_update in_progress`, từ chối → `tool_call_update failed` (call bị từ chối không có `tool:execution` nào đóng nó) |
| `permission:resolved` cùng `requestId` khi editor chưa trả lời | đã trả lời ở Web UI: thôi chờ editor, không gửi `permission:response` nữa, cập nhật call như trên |
| `agent:reply` | mỗi tin nhắn assistant, **không** kết thúc lượt: khối `<think>` đầu → `agent_thought_chunk`, phần còn lại → `agent_message_chunk` (chỉ khi tin đó chưa stream qua `agent:delta`) |
| `agent:state = idle` | kết thúc lượt: `stopReason: end_turn` — chỉ khi lượt đã chạy (`processing`, delta, tool, permission); idle lúc engine của chat vừa được tạo bị bỏ qua. Call đã duyệt mà chưa có `tool:execution` (bị dừng giữa chừng) → `tool_call_update failed` |
| `agent:reply` khi chưa có lượt nào chạy | lệnh gateway tự trả lời (`/reset`, `/plugin …`) → `end_turn` ngay |
| `session/cancel` | `agent:control stop` → `stopReason: cancelled` |
| `agent:state = error` | `stopReason: refusal` |
| frame giữa hai prompt (lượt editor không mở: `schedule_watch` đánh thức chat, tin gửi từ Web UI) | dịch y như trong lượt — `session/update`, và permission hỏi qua `session/request_permission` — chỉ không có response `session/prompt` nào. ACP v1: "Session updates are not limited to active prompt turns"; Zed áp update vào thread bất kể đang có lượt hay không |

`initialize` trả `protocolVersion: 1`, `loadSession: false`,
`promptCapabilities.embeddedContext: true`, không `authMethods`.

Mỗi session ACP có một task dịch sống suốt session (`session_loop`): nó giữ
bộ đếm `toolCallId` (đánh số theo session — id lặp lại ở prompt sau sẽ ghi đè
call cũ trong editor) và các call đã duyệt đang chờ kết quả.

Ghép permission với kết quả bằng **tên tool**, không bằng id: cả
`permission:request` lẫn `tool:execution` đều không mang tool-use id của
model. Đủ dùng vì daemon chỉ hỏi permission cho tool không read-only, chạy
chúng tuần tự, và chính nó trả lời permission cho call đang chờ theo tên tool
(`ResponseRegistry::register_tool_permission`). Giới hạn: subagent chạy song
song cùng một tool cần duyệt có thể bị ghép nhầm kết quả giữa hai call.

## Cấu hình editor

Zed (`settings.json`):

```json
{ "agent_servers": { "SenClaw": { "command": "senclaw", "args": ["acp"] } } }
```

JetBrains: Settings → Tools → AI Agents (ACP) → Add → command `senclaw`,
args `acp`. Cần binary `senclaw` trên PATH (cài `install.sh` hoặc
`~/.senclaw/bin`).

## Chưa làm ở v1

- `fs/read_text_file`, `fs/write_text_file`, `terminal/*`: daemon tự đọc
  ghi trên đĩa; editor thấy thay đổi qua watcher của nó.
- `session/load`: chat vẫn tồn tại trong SenClaw, gõ tiếp là được; ACP
  session mới mỗi lần mở.
- AskUserQuestion / FormUI: hiện dạng text; trả lời trong Web UI.
- Prompt gửi khi chat đang chạy một lượt editor không mở (watch vừa đánh
  thức): daemon xếp hàng tin nhắn, frame không mang id lượt, nên prompt kết
  thúc ở `idle` của lượt đang chạy; câu trả lời thật đến sau dưới dạng update
  ngoài lượt (Zed vẫn hiện). Lời nhắc của watch không hiện trong editor:
  không frame nào mang nó (`dispatch_into_chat` không broadcast tin nhắn).
- Đăng ký ACP registry: sau khi kiểm với Zed thật.

## Kiểm chứng

- `cargo test --lib acp`: khung JSON-RPC (3 dạng thông điệp, request→response
  theo id), ánh xạ tool kind / tool_call / permission option / prompt text /
  session id; `prompt_ends_at_idle_not_at_the_first_reply` dựng lại lượt
  gemma-4-e4b ngày 2026-10-16 (tin đầu chỉ có suy nghĩ + tool call → permission
  → tool → trả lời → idle): một response duy nhất, sau idle, và permission
  được hỏi qua ACP. Trước đó translator trả `end_turn` ở `agent:reply` đầu
  tiên, mọi frame sau đó bị bỏ, tool chờ duyệt mãi.
- Daemon scratch 2026-10-16 (gemma-4-e4b, `senclaw acp --gateway
  ws://127.0.0.1:38789`, prompt nhập gsm8k qua Datagen): Skill →
  `datagen_start` → `schedule_watch`, mỗi tool một `session/request_permission`
  (duyệt qua ACP, không cần REST) và một `tool_call`; `end_turn` 1 ms sau
  `state=idle` của daemon, sau câu trả lời cuối.
- `cargo test --lib acp` (2026-10-16, ghép permission ↔ tool và frame ngoài
  lượt): `a_refused_call_fails_and_call_ids_stay_unique_across_prompts`,
  `a_cancelled_turn_closes_the_call_it_approved`,
  `a_turn_the_editor_did_not_start_is_forwarded`,
  `a_permission_answered_in_the_web_ui_stops_waiting_on_the_editor`; hai test
  kết thúc lượt ở trên giữ nguyên ý, nay chạy qua `session_loop`.
- Daemon scratch 2026-10-16 (gemma-4-e4b, cổng 38798/38799, driver ACP +
  một client WS đóng vai Web UI): (1) Bash bị từ chối trong editor → một entry
  `pending` → `failed`, `end_turn`; (2) `schedule_watch` duyệt trong editor →
  một entry `pending` → `in_progress` → `completed`; (3) không prompt nào mở,
  watch đánh thức chat sau ~30 s → suy nghĩ, permission Bash hỏi qua
  `session/request_permission`, duyệt trong editor, một entry tới `completed`,
  câu trả lời tới editor; (4) không prompt nào mở, tin nhắn từ client WS, Web UI
  trả lời permission → `permission:resolved` → entry `in_progress` →
  `completed`; editor trả lời muộn 5 phút sau không gửi gì tới daemon (log:
  request đó resolve đúng một lần). Bốn call, bốn id khác nhau; chỉ hai
  response `session/prompt`.
- Thật với Zed (chưa chạy — cần daemon + Zed):
  `senclaw acp` trong Zed → prompt → thấy chunk, tool_call kèm diff, và hộp
  permission.
