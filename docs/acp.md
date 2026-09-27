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
| `tool:execution` | `session/update tool_call` — `kind` suy từ tên tool (Read→read, Edit/Write→edit, Bash→execute, Grep/find_symbol→search, WebFetch→fetch, Task/Plan→think), `status` completed/failed, diff của Edit làm content text, `locations` từ `path` |
| `permission:request` | `session/request_permission` — option `key/label` → `allow_once / allow_always / reject_once / reject_always`; trả lời → `permission:response`; editor huỷ → chọn option từ chối |
| `agent:reply` | kết thúc lượt: `stopReason: end_turn` (chỉ gửi text nếu chưa stream) |
| `session/cancel` | `agent:control stop` → `stopReason: cancelled` |
| `agent:state = error` | `stopReason: refusal` |

`initialize` trả `protocolVersion: 1`, `loadSession: false`,
`promptCapabilities.embeddedContext: true`, không `authMethods`.

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
- Đăng ký ACP registry: sau khi kiểm với Zed thật.

## Kiểm chứng

- `cargo test --lib acp`: khung JSON-RPC (3 dạng thông điệp, request→response
  theo id), ánh xạ tool kind / tool_call / permission option / prompt text /
  session id.
- Thật (chưa chạy trong lần cài này — cần daemon + Zed):
  `senclaw acp` trong Zed → prompt → thấy chunk, tool_call kèm diff, và hộp
  permission.
