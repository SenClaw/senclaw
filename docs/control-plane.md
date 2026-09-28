# Control plane JEV v2.2 — P0/P1

> **Trạng thái: ĐÃ CÀI VÀ NỐI DÂY (P0/P1).** 2026-09-27, cập nhật sau khi nối
> `<agent_status>` và L1 offload vào lượt thật theo quyết định của lead.
> Nguồn kiến trúc: `JEV Architecture v2.html` (v2.2) §3–§9, §12–§13, §15–§17.
> Code: [`src/control_plane/`](../src/control_plane/mod.rs), REST
> [`src/gateway/ui_server/control_plane.rs`](../src/gateway/ui_server/control_plane.rs),
> spec đóng gói [`assets/specs/`](../assets/specs/), harness
> [`scripts/evals/run.py`](../scripts/evals/run.py). Nguyên tắc bất biến:
> **hành vi mặc định của một lượt agent không đổi** trừ nơi §4 nói tầng đó
> không tắt được (Policy Gate, kiểm quyền mỗi lần gọi tool, budget, trace) —
> và ngay cả ở đó, test cũ vẫn phải xanh.

## 1. Bản đồ thành phần → code

| §6 thành phần | Code trong repo này | Trạng thái |
|---|---|---|
| Rule → Jev → LLM → người | [`control_plane::ladder`](../src/control_plane/ladder.rs) — `Decision`, `should_ask`, `ask`, `force_ask` | Mới, dùng cho spec mới; gate/router cũ giữ nguyên logic |
| Policy Gate | [`control_plane::policy_gate`](../src/control_plane/policy_gate.rs) — danh sách nguy hiểm chuyển từ `decision::gate::shell` sang đây, `may_auto_approve` | Luôn bật (§4) |
| Tool Registry (B7) | [`control_plane::tool_registry`](../src/control_plane/tool_registry.rs) — bảng `when_to_use/not_for/examples/risk_tier/reversible/idempotent/concurrency_safe/requires_preview/cancellable` cho mọi tool built-in | Mới, chỉ mô tả — không đổi hành vi tool |
| Context Assembler (hai vùng, status bar) | [`control_plane::context_assembler`](../src/control_plane/context_assembler.rs) — `prefix_hash`, `render` (agent_status); nối vào [`zen_core::conversation::outgoing_messages`](../src/zen_core/conversation.rs) | **Đã nối vào lượt thật** — xem §4 |
| Workspace | [`control_plane::workspace`](../src/control_plane/workspace.rs) — `progress.md`/`todo.json`/`artifacts/`/`handoff.md`, quota, path confinement | Mirror thụ động bật mặc định; L1 offload **đã nối** vào [`zen_core::run_tools`](../src/zen_core/run_tools.rs), tắt mặc định — xem §4 |
| Trace neutral-v1 | [`control_plane::trace`](../src/control_plane/trace.rs) | Bật mặc định, chỉ metadata |
| Registry (file, P1) | [`control_plane::registry`](../src/control_plane/registry.rs) — bundled `assets/specs/*.json` + `~/.senclaw/registry/specs/` | Đã cài |
| Loop Controller | [`control_plane::loop_controller`](../src/control_plane/loop_controller.rs) — taxonomy 4 lớp, `fingerprint`, `is_stuck`, `CircuitBreaker`, `Budgets` | Vocabulary + test đầy đủ; **chưa thay** cơ chế hiện có (§17 P2 mới "bật qua cổng") |
| Eval Harness | [`scripts/evals/run.py`](../scripts/evals/run.py) + [`evals/cases/`](../evals/cases/) | Mở rộng theo schema §13 |

Cơ chế đã có từ trước, không viết lại (theo đúng chỉ dẫn "reuse before adding"):
exact-duplicate tool call (`zen_core::conversation::tool_call_sig`,
`is_duplicate_exempt`), `TOOL_ERROR_NUDGE`/`TOOL_ERROR_FINAL_NUDGE`, stall
detection (`SENCLAW_STALL_TOOL_TURNS`), `auto_compact`, sổ sự cố
(`src/failures`), trajectory (`src/trajectory`), decision gate
(`src/decision/gate`) và pre-skill router (`src/decision/skill_route`).

## 2. Công tắc

Đọc qua `controlPlane` trong `config.json` (seam cấu hình sẵn có, giống
`decisionConfig`) và `SENCLAW_JEV_OFF=1` (env thắng):

```json
"controlPlane": {
  "jevOff": false,
  "shadow": false,
  "agentStatus": false,
  "recordDecisionInputs": false,
  "workspace": { "enabled": true, "substituteToolOutput": false }
}
```

| Công tắc | Mặc định | Ý nghĩa |
|---|---|---|
| `jevOff` (hoặc `SENCLAW_JEV_OFF=1`) | tắt | Đường cơ sở ablation (§13): bỏ qua **mọi** tầng Jev, kể cả gate/router đang chạy — chúng quay về hành vi không-engine cũ |
| `shadow` | tắt | Quyết định của lead: spec mới ở chế độ `shadow` (`input.guard.*`, `clarify.needed`, `task.done`, `loop.next_step`) chỉ thật sự gọi decision runtime khi bật cờ này (hoặc khi chính spec đó để `mode: active`). Lý do: một lần load Laya tốn 1.2–1.7 GB RAM và khởi động tiến trình `sen-sysone` — bản cài mặc định không được trả giá đó chỉ để thu nhãn |
| `agentStatus` | tắt | `<agent_status>` do code tính (§7) — không gọi Jev, không đổi quyết định. Mặc định **tắt** vì khối này nằm ở message user *cuối*, mỗi lần gọi một chỗ khác, nên prompt lần trước không bao giờ là tiền tố của lần sau: engine local (sen-mlx) chỉ dùng lại KV cache khi khớp tiền tố tuyệt đối, bật lên thì mỗi bước agent prefill lại toàn bộ (Gemma 4 E2B, 16k token: ~30s thay vì ~1,3s). Bật được khi cần (xem §4) |
| `recordDecisionInputs` | tắt | G1: ghi state/questions đầy đủ gửi cho một spec vào file riêng (không lẫn vào trace) để `--g1-replay` dùng lại |
| `workspace.enabled` | bật | Mirror `progress.md`/`todo.json` — chỉ ghi thêm, không đổi tool result |
| `workspace.substituteToolOutput` | tắt | L1 offload thật sự thay tool result bằng preview — đây là phần duy nhất của Workspace **có thể đổi quyết định**, nên giữ tắt |

`route.skill`/`tool.risk` trong registry chỉ **mô tả** router/gate hiện có —
đổi mode qua `/api/decision/skills` / `/api/decision/gate` như cũ, `GET
/api/control-plane/specs` đồng bộ hiển thị theo giá trị sống.

## 3. REST

- `GET /api/control-plane/specs` — toàn bộ spec, `route.skill`/`tool.risk`
  đồng bộ mode sống từ `decisionConfig`.
- `PUT /api/control-plane/specs/:id/mode` — `{mode}`; từ chối cho spec
  `wraps_existing`.
- `GET /api/traces` / `GET /api/traces/:id` — trace neutral-v1.
- `POST /api/control-plane/decisions/replay` — hỏi lại một spec hiện hành với
  một `state` đã ghi, bỏ qua mode (chỉ dùng cho G1).

## 4. Cách `<agent_status>` và L1 offload nối vào lượt thật

Quyết định của lead (2026-09-27): nối cả hai, theo thiết kế "không thể làm hỏng
lịch sử".

### 4.1 `<agent_status>` — mỗi lần gọi LLM, không đổi lịch sử đã lưu

Seam duy nhất `zen_core::conversation::query()` gọi `query_llm::query_llm` (vòng
lặp chính; **không phải** lời gọi nén `summarize_history`, seam đó cố tình
không đi qua đường này). Ngay trước lời gọi đó:

```rust
let outgoing = outgoing_messages(&messages, config.agent_status, &status_input);
let llm_call = query_llm::query_llm(&config.http_client, &outgoing, ...);
```

`outgoing_messages` trả về [`Cow<[Message]>`](https://doc.rust-lang.org/std/borrow/enum.Cow.html):
- tắt (`config.agent_status == false`) → `Cow::Borrowed(messages)`, **cùng một
  borrow**, không sao chép — request giống hệt trước khi tính năng này tồn tại;
- bật → sao chép toàn bộ `messages` một lần, tìm message role `"user"` cuối
  cùng (theo định dạng nội bộ, đây chính là nơi tool result cũng nằm — cả
  adapter Anthropic (`anthropic_content_blocks`, mọi loại block chung một mảng
  content) lẫn OpenAI (`openai_messages_for_api`, một `ContentBlock::Text` sau
  các `ToolResult` tự động trở thành message `role:"user"` **tách riêng, sau**
  các message `role:"tool"`) đã render đúng hình dạng — không cần code riêng
  cho từng provider, đã xác nhận bằng cách đọc `query_llm.rs`), rồi nối thêm
  đúng một `ContentBlock::Text` chứa `<agent_status>` vào message đó.

`messages` (biến sở hữu, được vòng lặp trả về ở cuối, dùng để nén và ghi
trajectory) **không bao giờ bị đổi** — chỉ có bản sao cục bộ `outgoing` bị đổi,
và nó chỉ sống trong phạm vi một lời gọi LLM.

`AgentStatusInput.todo_open/todo_total/pending_events` hiện để `0` — state của
todo (StateManager) và sự kiện chờ không tới được seam này nếu không thêm dây
dẫn mới; hàm `render` đã xử lý `0` một cách an toàn (in "0/0 open"), nên đây là
đơn giản hoá có ghi chú, không phải lỗi. `call_index`/`call_budget`/`elapsed_secs`
là thật (từ biến đếm vòng lặp `turn`/`max_turns` và một `Instant` chụp một lần ở
đầu `query()`). `original_goal` lấy từ message user "thật" cuối cùng
(`last_real_user_index`, tái dùng từ logic nén sẵn có) chụp một lần trước vòng
lặp, nên không đổi kể cả khi nén xảy ra giữa chừng.

5 test (`conversation.rs`) khoá đúng 5 yêu cầu của lead: lịch sử đã lưu giống
hệt bật/tắt (`agent_status_never_mutates_the_persisted_history`), request đi ra
có đúng một block cuối (`agent_status_on_appends_exactly_one_trailing_block…`),
hash tiền tố tĩnh không đổi (`a_static_prefix_hash_does_not_depend_on_agent_status`
— đúng theo cấu trúc, vì `outgoing_messages` không hề nhận `system_prompt`),
tắt thì y hệt trước đây, cùng một borrow
(`agent_status_off_returns_the_same_borrow_untouched`), và mọi test hình dạng
request cũ (`query_llm.rs`, ví dụ các test stream) vẫn xanh — `cargo test
--workspace` xác nhận (xem báo cáo).

### 4.2 L1 offload — tại seam kết quả tool

`zen_core::run_tools`, ngay trước khi `result_for_assistant` (chuỗi mô hình sẽ
thấy) được đóng gói vào `ContentBlock::ToolResult`: nếu độ dài vượt
`workspace::OFFLOAD_THRESHOLD_BYTES` (8000 byte, kiểm tra trong bộ nhớ trước —
kết quả nhỏ thường gặp không bao giờ phải đọc `config.json`) **và**
`controlPlane.workspace.substituteToolOutput` đang bật, `result_for_assistant`
được thay bằng preview đầu+cuối từ `Workspace::write_artifact`, ghi file đầy đủ
vào `artifacts/<tool_call_id>`. Không có trường `jid` sẵn có ở seam này —
`RunContext::agent_data_dir` (đã có, không cần thêm field mới vào struct có độ
rủi ro cao theo CLAUDE.md) đóng vai trò khoá không gian tên thay cho jid, vốn
đã là duy nhất theo từng chat.

Test `l1_offload_only_replaces_a_large_result_when_the_switch_is_on`
(`run_tools.rs`) chạy cả hai chiều: tắt → không đổi; bật → preview có "cut" +
"full output saved at", file gốc còn nguyên trên đĩa.

## 5. Chạy đường cơ sở (P0, JEV_OFF)

```bash
SENCLAW_JEV_OFF=1 python3 scripts/evals/run.py --dry-run   # kiểm schema, không chạy gì
SENCLAW_JEV_OFF=1 python3 scripts/evals/run.py --jev-off --k 3
```

`--k` chạy lại mỗi task k lần, in `pass@1` (tỉ lệ đỗ từng lần thử) và `pass^k`
(tỉ lệ task đỗ **cả k lần** — phép đo độ tin cậy, không phải độ giỏi, §13).
Khi có trace, mỗi dòng kết quả kèm `trace: {tokensIn, tokensOut,
cacheReadTokens, cacheHitRatio, billableTokens, decisions}` đọc thẳng từ
`~/.senclaw/control-plane/traces/`.

## 6. G1 — hồi quy tiền tố

1. Bật `controlPlane.recordDecisionInputs` và chạy một số lượt thật (hoặc
   `--jev-off` tắt rồi bật shadow để có input ghi lại).
2. `python3 scripts/evals/run.py --g1-replay` — đọc từng file đã ghi ở
   `~/.senclaw/control-plane/decision-inputs/`, gọi lại
   `POST /api/control-plane/decisions/replay` với `state` y hệt, so khớp với
   `criteria.decision_assertions` của mọi task khớp `spec_id`.

Cần daemon đang chạy và có `decision` runtime — đây là bước sống, không phải
dry-run.

## 7. Bộ eval tối thiểu

32 task thủ công trong [`evals/cases/`](../evals/cases/) theo schema §13
(`id, source, lang, difficulty, split, initial_state, user_scenario,
criteria{env_assertions, decision_assertions, veto}, versions`), 12/32
(37.5%) tiếng Việt. Chấm theo **trạng thái môi trường sau khi chạy**
(`env_assertions`) và câu trả lời cuối (`nl_assertions`) — không chấm theo
đường đi tool. Hai task (`clarify-ambiguous-request`,
`instruction-override-attempt`, và bản tiếng Việt của chúng) có
`decision_assertions` neo vào `clarify.needed`/`input.guard.override` để
dùng làm dữ liệu G1 mẫu.

## 8. Chưa làm / câu hỏi mở

- **Loop Controller (đã chốt, P1 hợp lệ theo lead):** taxonomy/fingerprint/
  breaker/budget đã đúng và có test, nhưng **chưa thay** cơ chế stall/dup/error
  hiện có — theo đúng lộ trình §17 (P2 mới "bật qua cổng, so với đường cơ sở
  P0"). `Budgets::defaults()` **cố ý tĩnh**, chỉ ghi lại giới hạn hiệu lực hôm
  nay (không đọc `SENCLAW_MAX_AGENT_TURNS` sống) — việc đọc cấu hình sống là
  của P2, khi Loop Controller thật sự kiểm soát vòng lặp.
- **Live WS/ACP smoke (đã chốt, không phải việc của phase 06):** lead sẽ tự
  chạy lượt chat sống qua WS/ACP với model GGUF cục bộ qua llama.cpp trong E2E
  cuối cùng, dùng chính cơ chế đã nối ở §4 để tạo trace có quyết định từ spec
  shadow. Phase 06 không cần dựng harness riêng cho việc này.
- `cost` trong `trace_stats()` là `billableTokens` (token thật trừ cache),
  không phải USD — không có bảng giá theo provider ở phía Python.
- `AgentStatusInput.todo_open/todo_total/pending_events` là `0` cho tới khi có
  seam đọc `StateManager`/hàng đợi sự kiện từ `zen_core::conversation` — xem
  §4.1.
