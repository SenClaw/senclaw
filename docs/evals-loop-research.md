# Evals loop cho SenClaw — Nghiên cứu

> **Trạng thái: LÀM** · chốt 2026-09-12 · kế hoạch riêng sẽ mở khi bắt đầu.
> Đã có phần nền: `src/trajectory/` ghi JSONL từng lượt, `scripts/evals/run.py`
> và `evals/cases/` — xem [evals.md](evals.md). Việc còn lại là đưa trajectory
> ra khỏi `run_one_shot` và cô lập state giữa các case, như tài liệu này mô tả.


> *(Dòng dưới đây viết trước khi `src/trajectory/` và `scripts/evals/` tồn tại —
> giữ lại làm mốc thời gian, xem ô Trạng thái ở trên cho hiện trạng.)*
> Tài liệu này khảo sát thực hành 2026, rồi soi xem SenClaw **đã có sẵn** những
> mảnh nào của một eval harness và còn thiếu chính xác cái gì.

## 0. Tóm tắt

"Evals loop" không phải một thứ, mà là **ba vòng lặp khác nhau về nhịp và mục
đích**, dùng chung một bộ case và một bộ grader:

| Vòng | Nhịp | Trả lời câu hỏi | Nơi chạy |
|---|---|---|---|
| **Inner** (dev loop) | mỗi lần sửa prompt/persona/skill | "Thay đổi này tốt hơn hay tệ hơn?" | máy dev, 20–50 case |
| **Regression** | mỗi commit / mỗi đêm | "Có làm hỏng cái đang chạy không?" | CI hoặc scheduler |
| **Production** | liên tục | "Ngoài thực tế nó đang hỏng ở đâu?" | daemon đang chạy, sampling |

Kết luận quan trọng nhất từ khảo sát: **hai lỗi chí mạng khi tự làm eval** là
(a) chấm **đường đi** thay vì **kết quả** → test giòn, agent tìm được cách đúng
khác là trượt; và (b) tin điểm của LLM-judge mà **không calibrate** với người →
đo nhầm thứ suốt nhiều tháng. Anthropic nói thẳng: với model tiên tiến, **pass
rate 0% qua nhiều lần chạy hầu như luôn là case hỏng, không phải agent kém**.

Với SenClaw, phần tốn công nhất — **chạy một agent cô lập, có timeout, có hạn
lượt, đổi được model, đếm token** — **đã có sẵn** trong
[`isolated_runner::run_one_shot`](../src/agent/isolated_runner.rs). Cái thiếu là
**định dạng case, grader, kho điểm, và quan trọng nhất: trajectory không được
trả ra** (§4.2).

## 1. Thực hành 2026 — chắt lại

### 1.1 Ba tầng chấm điểm

- **Output layer** — kết luận cuối có đúng không. Black box.
- **Trajectory layer** — các bước "chịu lực" có xảy ra không: agent **có** gọi
  đúng tool tra cứu với đúng tham số không, chứ **không** phải "có gọi đúng thứ
  tự này không". Đây là chỗ bắt được "đoán mò mà trúng".
- **Component layer** — retriever, sub-agent, một tool cụ thể. Dùng để định vị
  lỗi, không dùng để gate deploy.

Nguyên tắc cân bằng: **grader tất định ở đâu được thì dùng tất định** (tên tool,
tham số bắt buộc, output khớp regex/schema), **LLM-judge chỉ cho phần chủ quan**
(chất lượng lập luận, mức hoàn thành, giọng văn), **người chấm để calibrate
judge** — không phải để chấm đại trà.

### 1.2 Bộ case

- **Bắt đầu 20–50 case lấy từ lỗi thật**, không phải case tổng hợp. "Năm lỗi
  thật đáng giá hơn năm mươi happy path bịa ra."
- Mỗi case cần: đặc tả **không mơ hồ** (hai người trong nghề chấm độc lập phải
  ra cùng đạt/trượt), một **lời giải tham chiếu** chứng minh case giải được, và
  tiêu chí thành công chấm được khách quan.
- **Cân bằng lớp**: phải có cả case "hành vi X *nên* xảy ra" và "hành vi X
  *không được* xảy ra". Test một chiều → tối ưu một chiều.
- **Điểm từng phần**: agent xác định đúng vấn đề + xác minh khách hàng nhưng
  trượt bước hoàn tiền **tốt hơn hẳn** agent trượt ngay từ đầu. Grader nhị phân
  vứt mất tín hiệu này.

### 1.3 Đo tính ngẫu nhiên

Một lần chạy không nói lên gì. Hai chỉ số chuẩn:

- **pass@k** — xác suất đúng **ít nhất một** trong k lần. Đo *năng lực*.
- **pass^k** — xác suất đúng **cả** k lần. Đo *độ ổn định*. Với tỉ lệ 75%/lần,
  pass^3 = 0.75³ ≈ **42%**.

SenClaw đặc biệt cần pass^k, vì sampling mặc định lấy từ `generation_config.json`
của chính checkpoint (xem CLAUDE.md, mục Gemma 4) — tức là **mỗi checkpoint local
có độ ngẫu nhiên riêng**, không phải một hằng số toàn hệ.

### 1.4 Judge phải được calibrate

LLM-judge là **một component cần hiệu chuẩn, không phải một oracle**. Giữ một
tập calibration có nhãn người; theo dõi độ đồng thuận judge↔người (ngưỡng hay
dùng: **~75%**, các nguồn nghiêm ngặt hơn đòi vài trăm case trước khi tin số
tổng hợp). Khi lệch có hệ thống → judge trôi, phải hiệu chuẩn lại chứ không phải
kết luận agent kém đi.

### 1.5 Bão hoà và bảo trì

Khi capability eval chạm ~100%, nó **hết tín hiệu** → tốt nghiệp sang bộ
regression, và viết case khó hơn. Bộ eval là **artifact sống**, cần chủ sở hữu,
bảo trì như unit test.

## 2. Hiện trạng SenClaw

Không có gì. Không module, không test eval, không bảng DB, không endpoint.
Những thứ gần nhất là **benchmark hiệu năng** (`scripts/mlx_resource_bench.py`,
`examples/mlx_bench.rs`) — đo tok/s và RAM, **không đo chất lượng đầu ra**.

Hệ quả thực tế đang thấy được trong chính repo: mỗi lần sửa system prompt,
persona, skill hay đổi model, **không có cách nào biết mình vừa làm tốt lên hay
tệ đi** ngoài cảm giác khi chat thử. Mọi ghi chép "đã verify" trong `MEMORY.md`
đều là verify **thủ công, một lần**.

## 3. Nguyên liệu đã có sẵn

Đây là phần đáng giá của khảo sát: eval harness cho SenClaw **không phải viết từ
đầu**, mà là nối bốn mảnh đã chạy.

| Cần cho eval | SenClaw đã có | Ở đâu |
|---|---|---|
| Chạy 1 trial cô lập | `run_one_shot` — session riêng, dispose sau khi xong | [`src/agent/isolated_runner.rs`](../src/agent/isolated_runner.rs) |
| Timeout + huỷ | `timeout` (mặc định 5 phút) + `cancel: CancellationToken` | `OneShotOptions` |
| Chặn trial chạy loạn | `max_agent_turns` (mặc định engine = 30) | `OneShotOptions` |
| So model A/B | `model_config_id` — pin từng trial vào một LLM config | `OneShotOptions` |
| Cô lập tool | `use_tools` whitelist + `skip_mcp_init: true` | `OneShotOptions` |
| Đối tượng test | persona (`system_prompt`), `custom_rules`, `skills_extra_dirs` | `OneShotOptions` |
| Chi phí mỗi trial | `tokens_in` / `tokens_out` trả thẳng trong kết quả | `OneShotResult` |
| Chi phí mỗi lời gọi LLM | `LlmUsageData { profile, provider, model, usage, latency_ms, ok }` | [`src/zen_core/mod.rs:360`](../src/zen_core/mod.rs) |
| Kho chi phí lịch sử | bảng `llm_usage_log` | [`src/db/usage.rs`](../src/db/usage.rs) |
| Phân biệt trượt vs lỗi hạ tầng | `timed_out` / `aborted` / `errored` + `error_message` | `OneShotResult` |
| Fan-out nhiều case | workflow DAG (`StepKind::Agent` \| `Script`, `depends_on`) | [`src/workflow/`](../src/workflow/) |
| Bước judge | chính là một `StepKind::Agent` phụ thuộc bước chạy | `src/workflow/step_runners.rs:91` |
| Nhịp chạy định kỳ | scheduler, mode `isolated` / `script` | `src/scheduler/` |
| Guardrail production | hook `PreToolUse` chặn được tool trước khi chạy | [`src/zen_core/hooks/`](../src/zen_core/hooks/) |

`OneShotResult` hiện trả: `text`, `all_texts`, `duration`, `turn_count`,
`timed_out`, `aborted`, `errored`, `error_message`, `tokens_in`, `tokens_out`.
Tức là **năm trong sáu chỉ số hiệu quả mà Anthropic khuyến nghị đã có sẵn**
(số lượt, token, thời gian, chi phí suy ra được, tỉ lệ lỗi) — thiếu đúng **số
lần gọi tool**.

## 4. Khoảng trống — cụ thể

### 4.1 Không có định dạng case, grader, kho điểm

Ba thứ phải viết mới hoàn toàn. Chi tiết đề xuất ở §5.

### 4.2 Trajectory không ra khỏi `run_one_shot` — **đây là chặn lớn nhất**

`run_one_shot` tự tạo `ZenEngine` bên trong và `subscribe()` event bus **cho
riêng nó**; caller không cầm được bus. Đường duy nhất ra ngoài là callback
`OnActivity(kind, text)` với `kind ∈ think | text | tool | tool_error | message`
— **hai chuỗi string**. Với `ToolExecutionComplete`, callback nhận
`summary` (hoặc `title` nếu summary rỗng), tức là **tên tool bị mất, kết quả bị
mất, tham số chưa bao giờ có**.

Trên bus thì dữ liệu **có**: `ToolExecutionCompleteData { agent_id, tool_name,
title, summary, content }` ([`src/zen_core/mod.rs:404`](../src/zen_core/mod.rs)).
Nên fix đúng là **thêm `Vec<ToolCallRecord>` vào `OneShotResult`**, không phải
parse chuỗi từ `OnActivity`.

### 4.3 Bus không mang **tham số** tool

`ToolExecutionCompleteData.content` là **kết quả**, không phải input. Chỗ duy
nhất có `tool_input` là hook:
`PreToolUseInput { tool_name, tool_input: serde_json::Value }`
([`src/zen_core/hooks/types.rs:238`](../src/zen_core/hooks/types.rs)).

⇒ Muốn có **argument correctness** (chỉ số quan trọng thứ hai sau task
completion) thì hoặc thêm field `tool_input` vào event, hoặc cắm một hook
capture trong lúc eval. **Thêm vào event là đúng hơn** — hook là cơ chế người
dùng cấu hình được, eval không nên phụ thuộc vào cấu hình của người dùng.

### 4.4 Cô lập trial chỉ **một nửa**

`run_one_shot` cô lập tốt phần **session**: `instance_id` sinh riêng
(`oneshot-{millis}-{rand}`), `working_dir`/`agent_data_dir` do caller đưa,
`skip_mcp_init: true` nên không dính MCP nào trừ khi inject tay.

Nhưng **state toàn cục thì không cô lập**: memory FTS, đồ thị cognitive
(`senclaw_cognitive.db`), kanban, wiki — tất cả nằm trong `~/.senclaw/` dùng
chung. Một trial gọi `memory_write` hay `cog_*` sẽ **làm bẩn trial sau**, đúng
kiểu "shared state thổi phồng kết quả" mà tài liệu Anthropic cảnh báo.

⇒ Eval runner phải ép `SENCLAW_DATA_DIR` (hoặc tương đương) sang thư mục tạm
cho từng trial, hoặc whitelist `use_tools` loại hẳn nhóm tool ghi state.
**Không có cái này thì con số đo được không tin được**, và đây là loại lỗi
không tự lộ ra — nó chỉ làm điểm đẹp lên.

### 4.5 Chưa có tập calibration

Chưa có nhãn người cho bất kỳ output nào. Không có nhãn thì judge không kiểm
chứng được, mà judge không kiểm chứng được thì cả bộ eval là **niềm tin**, không
phải phép đo.

## 5. Thiết kế đề xuất

### 5.1 Định dạng case — `.md` + YAML frontmatter

Đi theo đúng quy ước đã có của workflow (`<workflows_dir>/<name>.md` frontmatter)
và của skill/persona, thay vì đẻ ra format thứ tư:

```yaml
---
id: memory-recall-vi-diacritics
suite: memory
tags: [memory, vietnamese, regression]
subject:                      # đối tượng test
  persona: research-assistant
  model_config_id: null       # null = model đang active; đặt tên để pin
  use_tools: [memory_search, memory_write]
setup:
  script: ./setup/seed-memory.sh   # chạy trước, trong workspace của trial
grade:
  - kind: contains_all         # tất định
    values: ["Đà Nẵng"]
  - kind: tool_called          # tất định, theo tập hợp — KHÔNG theo thứ tự
    tool: memory_search
  - kind: rubric               # LLM judge
    weight: 0.5
    criteria: |
      Trả lời có nêu đúng thành phố và không bịa thêm chi tiết nào
      không có trong bộ nhớ.
trials: 3                      # → pass@3 và pass^3
timeout_secs: 120
---

Tôi đã kể cho bạn về chuyến đi hồi tháng trước. Tôi đã đi đâu?
```

Ràng buộc thiết kế:
- `grade` là **danh sách**, cộng điểm từng phần — không phải một boolean.
- `tool_called` chấm **tập hợp**, có mặt/không có mặt, **không chấm thứ tự**
  (§1.1). Muốn chấm thứ tự phải là một kind riêng, dùng hạn chế.
- `trials` mặc định > 1. Một trial là vô nghĩa với agent ngẫu nhiên.

### 5.2 Grader — trait, hai họ

```rust
pub enum GradeOutcome { Pass, Fail, Partial(f32) }

#[async_trait]
pub trait Grader {
    async fn grade(&self, case: &EvalCase, trial: &TrialResult) -> Result<GradeOutcome>;
}
```

- **Tất định** (`contains_all`, `regex`, `json_schema`, `tool_called`,
  `no_tool_called`, `max_turns`, `script`): thuần Rust, rẻ, tái lập được.
  `no_tool_called` là vế "hành vi không được xảy ra" của §1.2 — thiếu nó là
  test một chiều.
- **Judge**: một `run_one_shot` thứ hai, persona `evaluator` chuyên dụng,
  **pin model riêng** (`model_config_id`) để đổi model của subject không kéo
  theo đổi judge. Trả JSON có điểm + lý do; lý do được lưu, vì §7 cần đọc.

### 5.3 Runner

`src/evals/` — `case.rs` (parse), `grader.rs`, `runner.rs`, `store.rs`,
`report.rs`. Runner làm đúng bốn việc:

1. dựng workspace tạm **mới cho mỗi trial** (§4.4) + chạy `setup.script`;
2. `run_one_shot` với options dựng từ `subject`;
3. chạy hết grader, cộng điểm;
4. ghi `eval_runs` / `eval_trials` vào SQLite, kèm `tokens_in/out`, `duration`,
   `turn_count`, và **toàn bộ transcript** — vì §1.5/§7 bắt buộc phải đọc lại.

CLI: `senclaw eval run <suite> [--filter tag] [--model <id>] [--trials k]`,
`senclaw eval report [--vs <run_id>]`.

### 5.4 Báo cáo — so sánh mới là mục đích

Một con số tuyệt đối gần như vô dụng. Báo cáo phải là **diff giữa hai run**:
case nào chuyển pass→fail (regression, chặn deploy), fail→pass (cải thiện),
token/case tăng bao nhiêu %, độ lệch pass^k. Đây chính là thứ biến eval từ
"bài kiểm tra" thành "vòng lặp".

## 6. Chỉ số theo dõi

| Nhóm | Chỉ số | Nguồn dữ liệu |
|---|---|---|
| Chất lượng | pass@k, pass^k, điểm từng phần trung bình | grader |
| Quỹ đạo | tool correctness, argument correctness, step efficiency | cần §4.2 + §4.3 |
| Chi phí | token/case, chi phí/case, `latency_ms` mỗi lời gọi | `OneShotResult`, `llm_usage_log` |
| Hiệu quả | số lượt (`turn_count`), số lời gọi tool | `OneShotResult` (+ §4.2) |
| Sức khoẻ bộ eval | đồng thuận judge↔người, mức bão hoà, tỉ lệ `errored` | store |

Ba dòng cuối của bảng là thứ hay bị bỏ. Tỉ lệ `errored` cao mà không tách ra
khỏi `fail` sẽ đọc thành "agent kém" trong khi thực tế là provider lỗi.

## 7. Lộ trình

| Phase | Nội dung | Ghi chú |
|---|---|---|
| **P0** | `Vec<ToolCallRecord>` trong `OneShotResult` + `tool_input` trên event | Chặn mọi thứ về trajectory (§4.2, §4.3) |
| **P1** | Parse case, grader tất định, runner, store, CLI `eval run` | Đã đủ dùng cho inner loop |
| **P2** | Cô lập data dir cho từng trial | **Không có P2 thì số của P1 không tin được** (§4.4) |
| **P3** | Judge persona + `rubric` grader | Sau P1, không trước |
| **P4** | 20–50 case đầu tiên **lấy từ lỗi thật** | Nguồn: `MEMORY.md` — mỗi mục "FIXED/bẫy" là một case ứng viên |
| **P5** | Tập calibration có nhãn người, đo đồng thuận | Trước khi tin bất kỳ số tổng hợp nào từ judge |
| **P6** | Diff report + chạy định kỳ qua scheduler | Vòng regression |
| **P7** | Sampling production qua hook | Vòng production |

P4 đáng chú ý: repo này **đã có sẵn kho case**. Mỗi dòng "FIXED ngày X" hoặc
"bẫy Y" trong `MEMORY.md` là một lỗi thật đã xảy ra — đúng loại nguyên liệu mà
tài liệu khuyên dùng, và tốt hơn hẳn case bịa.

## 8. Bẫy — ghi trước khi vấp

- **Đừng chấm đường đi.** `tool_called` theo tập hợp, không theo thứ tự. Agent
  tìm đường khác mà ra kết quả đúng thì đó là **đạt**, không phải trượt.
- **0% pass = case hỏng**, không phải agent kém. Đọc transcript trước khi kết
  luận.
- **Điểm judge chưa calibrate là ý kiến, không phải phép đo.** Không được dùng
  để gate deploy khi chưa có §5.2 + P5.
- **Một trial không kết luận được gì** — sampling mặc định lấy từ
  `generation_config.json` của checkpoint, mỗi model local ngẫu nhiên một kiểu.
- **State dùng chung làm điểm đẹp lên**, và không tự lộ ra (§4.4).
- **Grader phải chống lách.** Nếu `contains_all` là tiêu chí duy nhất, agent
  học được cách nhét từ khoá vào là qua.
- **`errored`/`timed_out` phải tách khỏi `fail`.** Trộn chung là đo nhầm hạ
  tầng thành chất lượng.
- **Eval bão hoà là eval chết.** Chạm ~100% thì chuyển sang regression và viết
  case khó hơn.

## Nguồn

- [Demystifying evals for AI agents — Anthropic](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents)
- [Evals for Agentic Loop Applications — TechEmpower](https://www.techempower.com/blog/2026/07/14/evals-for-agentic-loop-applications/)
- [LLM Agent Evaluation Metrics in 2026 — Confident AI](https://www.confident-ai.com/blog/llm-agent-evaluation-complete-guide)
- [LLM-as-a-Judge in 2026 — DeepEval](https://deepeval.com/blog/llm-as-a-judge)
- [LLM-as-Judge Patterns for Agent Evaluation — Zylos Research](https://zylos.ai/research/2026-05-26-llm-as-judge-agent-evaluation-patterns/)
- [Evaluating AI Agents: Trajectory & Tool-Use Evals — AppScale](https://appscale.blog/en/blog/evaluating-ai-agents-trajectory-tool-use-evaluation-2026)
- [building_evals.ipynb — anthropic cookbook](https://github.com/anthropics/anthropic-cookbook/blob/main/misc/building_evals.ipynb)
- [claude-evals — TribeAI](https://github.com/TribeAI/claude-evals)
