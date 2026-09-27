# Trajectory log, replay & evals

> Trạng thái: **đã cài** (2026-09-12, code-v2 phase 8). Nguồn:
> [`src/trajectory/mod.rs`](../src/trajectory/mod.rs), REST
> [`src/gateway/ui_server/trajectory.rs`](../src/gateway/ui_server/trajectory.rs),
> runner [`scripts/evals/run.py`](../scripts/evals/run.py), case mẫu
> [`evals/cases/`](../evals/cases/). Nghiên cứu gốc:
> [`evals-loop-research.md`](evals-loop-research.md).

## Trajectory

Ý tưởng OpenHands: event log append-only là điểm tích hợp cho replay và
đánh giá. Ở SenClaw nó là **kênh phụ** trên `EngineEvent` — engine và
`run_one_shot` không đổi. Mỗi lượt chat = một file
`~/.senclaw/trajectories/<chat>/<turn-id>.jsonl`, mỗi dòng một object theo
đúng schema **agentevals** (OpenAI-style):

```json
{"role":"user","content":"sửa lỗi login","ts":1757700000000,"turnId":"…","chatJid":"web:…"}
{"role":"assistant","content":"Để tôi xem.","tool_calls":[{"id":"call_200","type":"function","function":{"name":"Read","arguments":"{\"file_path\":\"…\"}"}}]}
{"role":"tool","tool_call_id":"call_200","name":"Read","title":"a.rs","content":"…","ok":true}
{"role":"meta","kind":"usage","model":"…","usage":{…}}
```

- Sự kiện dùng: `InputReceived` (mở lượt), `MessageComplete` (assistant +
  `tool_calls`), `ToolExecutionComplete/Error` (kết quả tool, nối với call
  theo thứ tự), `LlmUsage`, `SessionInterrupted/Error` (meta). Tool result
  cắt ở 32 KB và đánh dấu `truncated`.
- **Tắt mặc định.** Bật theo chat: `PUT /api/chats/:jid/trajectory/settings
  {"enabled": true}` (lưu `~/.senclaw/trajectories/enabled.json`); bật toàn
  bộ: `SENCLAW_TRAJECTORY=1`.
- File 0600 trong thư mục 0700 — chúng chứa nội dung file agent đã đọc.
  Xoá: `DELETE /api/chats/:jid/trajectory`.

REST: `GET /api/chats/:jid/trajectory` → `{enabled, dir, turns:[{turnId,
startedAt, lines, toolCalls, firstUserLine, bytes}]}`;
`GET /api/chats/:jid/trajectory/:turn` → `{turnId, messages:[…]}`.

Web: drawer **Changes** ở header chat có tab **Replay** — chọn lượt, xem từng
bước (user → assistant → tool …), bật/tắt ghi.

## Evals

`scripts/evals/run.py` (Python, ngoài daemon — không thêm crate) chấm một
trajectory bằng [agentevals](https://github.com/langchain-ai/agentevals):

- `reference` + `match` (`strict | unordered | subset | superset`): so
  chuỗi tool call thực với chuỗi mong đợi, bỏ qua tham số.
- `rubric`: LLM-as-judge với `TRAJECTORY_ACCURACY_PROMPT` + rubric của case;
  model qua `EVALS_JUDGE` (mặc định `openai:gpt-4.1-mini`).

```bash
pip install agentevals
python3 scripts/evals/run.py --cases evals/cases --daemon http://127.0.0.1:18788
```

Case lấy lượt mới nhất của `chat_jid` qua REST, hoặc một file `trajectory`
cụ thể (để chạy trong CI với file đã lưu).

## Chạy case, không chỉ chấm (2026-09-21)

Trước đây harness chỉ **chấm** — nó đọc lượt mới nhất của một chat thật, trong
trạng thái workspace mà chat ấy tình cờ đang có. Nay một case có `prompt` được
**chạy** luôn:

- fixture dựng mới trong thư mục tạm (`files` nội tuyến, hoặc `copy` từ một thư
  mục), nên không case nào chấm phải rác của case khác;
- chạy bằng `senclaw agent-task --prompt … --working-dir <tmp> --trajectory <id>` —
  một lượt one-shot, không chat, không lịch sử;
- `SENCLAW_TRAJECTORIES_DIR` trỏ vào thư mục tạm của case. **`HOME` cố ý không
  bị cô lập**: lượt chạy cần config model và API key của máy.

`run_one_shot` trước đây **không ghi trajectory một dòng nào** — đó là lý do
harness phải mượn chat thật. Nay `OneShotOptions.trajectory_jid` bật ghi cho
đúng lượt đó, bằng một cờ **chỉ sống trong tiến trình** (`trajectory::force_enable`),
không ghi vào `enabled.json`: đặt tên một id trùng với chat thật cũng không bật
ghi cho chat ấy.

## Chấm kết quả, không chấm đường đi

`expect` kiểm tra **workspace sau khi chạy** và câu trả lời cuối:

```json
"expect": {
  "files": { "app.py": { "contains": ["TIMEOUT_SECONDS = 30"],
                          "notContains": ["TIMEOUT_SECONDS = 5"] } },
  "finalText": { "contains": ["timeout"] }
}
```

Case mẫu: [`evals/cases/targeted-edit.json`](../evals/cases/targeted-edit.json)
— sửa đúng một hằng số và **giữ nguyên phần còn lại của file**.

Nó thay case cũ `read-before-edit`, vốn ghim chuỗi tool `Read` → `Edit`: chấm
đường đi thì **trượt** một agent tìm được đường đúng khác, và **đỗ** một agent
gọi đủ mọi tool đúng rồi vẫn phá file. `reference`/`match` và `rubric` vẫn được
hỗ trợ cho case nào thật sự cần, nhưng không phải mặc định nữa.

Harness trả **exit code khác 0** khi có case trượt `expect` — một bộ eval luôn
exit 0 thì không chặn được build nào.

## Schema §13 và `pass^k` (2026-09-27)

Case giờ theo schema đầy đủ của kiến trúc JEV v2.2 §13 (`id, source, lang,
difficulty, split, user_scenario, criteria{env_assertions,
decision_assertions, veto}, versions`) — `--dry-run` kiểm mọi case theo schema
này mà không chạy gì. `--k N` chạy lại mỗi case N lần và in `pass@1` (tỉ lệ
đỗ từng lần thử) cùng `pass^k` (tỉ lệ case đỗ **cả N lần** — phép đo đúng cho
tính năng làm agent *ổn định hơn* thay vì *giỏi hơn*, xem
[failure-ledger.md](failure-ledger.md)). `--jev-off` đặt
`SENCLAW_JEV_OFF=1` cho lượt chạy — đường cơ sở ablation. Chi tiết, công tắc
và G1 (hồi quy tiền tố cho spec Jev): [control-plane.md](control-plane.md).

## Chưa làm

- Bộ case thật cho repo SenClaw (nhiều fixture hơn) và job CI
  `workflow_dispatch`.
- Replay ở desktop/mobile.
