# Nghiên cứu & thiết kế: Luồng theo dõi token in/out cho AI usage

> Tài liệu nghiên cứu — tháng 7/2026. Bổ sung cho [analytics-feature-design.md](analytics-feature-design.md) (5/2026): tài liệu cũ là umbrella analytics (tools/latency/heatmap); tài liệu này là lát cắt **token accounting** cập nhật theo hiện trạng code sau khi zen_core + Space Apps + cognitive đã thành hình.

> **TRẠNG THÁI: ĐÃ TRIỂN KHAI (31/07/2026)** — toàn bộ P0/P1/P2 dưới đây đã vào code:
> - `src/usage/` (UsageRecorder + aggregator/retention/pricing-seed), bảng `llm_usage_log` / `llm_usage_daily` / `model_pricing` trong `src/db/schema.rs`, query tại `src/db/usage.rs` (kèm round-trip test).
> - Capture: `EngineEvent::LlmUsage` funnel (agent/subagent/compact/hook) → AgentPool bridge ghi kèm jid; virtual pool + isolated runner tự ghi (kèm accumulate cho `VirtualRunResult`/`OneShotResult`); `chat_completion` trả `ChatCompletionResult` có usage (vision cũng đã trả finish); bridge `llm.request` trả `usage` + ghi source=bridge; action mới `usage.report`; cognitive (4 impl) + embeddings (OpenAI/OpenRouter) + local models (MLX/Candle `last_usage()`) đã nối.
> - `background_runs.tokens_in/out` sống lại qua `OneShotResult.tokens_*`.
> - REST `/api/usage/*` + trang web `/usage` (nút sidebar) + MCP `senclaw-usage` (`usage_overview`/`usage_breakdown`/`usage_query`, đã vào registry CLAUDE.md).
> - SDK `llm_request_usage` + `usage_report`; ai-office/ai-chat dùng số thật (fallback chars/4 cho daemon cũ).
> - Fix kèm: `agent:usage` chuyển sang broadcast theo subscription; `llm_log.rs` ghi usage vào audit log.
> - **Cần daemon build mới để endpoint/dữ liệu hoạt động** (daemon đang chạy là binary cũ → `/api/usage/*` rơi về SPA fallback, UI hiển thị trạng thái rỗng đúng thiết kế).
> - **UI quản trị (31/07/2026, đợt 2):** web `/usage` thêm **PricingEditor** (CRUD bảng giá inline: Add/Edit/Delete → PUT/DELETE `/api/usage/pricing`, E2E-verified qua browser trên instance scratch); desktop_app Flutter thêm màn **`/usage`** (`lib/features/usage/usage_screen.dart` — 4 stat card, LineChart fl_chart 30 ngày 2 series, breakdown model/app, bảng giá + dialog thêm/sửa/xoá; nav rail item "Usage" giữa Background và Settings; `flutter build macos` PASS). Bẫy đã sửa kèm: seed pricing `input*0.1` sinh float artifact `0.30000000000000004` → làm tròn 6 số lẻ ở seed (`r6` trong aggregate.rs) + trim ở cả hai UI.

---

## 1. Hiện trạng (đã xác minh từng file:line)

### 1.1 Hạ tầng đã có — RawUsage

`src/zen_core/mod.rs:125-222` đã có struct chuẩn, **dùng lại được nguyên vẹn**:

```rust
pub struct RawUsage {
    input_tokens, output_tokens,                        // Anthropic style
    cache_creation_input_tokens, cache_read_input_tokens,
    prompt_tokens, completion_tokens,                   // OpenAI style
}
// from_json()  — parse cả 2 kiểu API từ json["usage"]
// merge()      — gộp usage streaming (Anthropic tách message_start/message_delta)
// input()/output()/is_empty() — chuẩn hoá
```

Được populate đầy đủ trên **đường agent chính** (`src/zen_core/query_llm.rs`):

| Điểm | Path |
|---|---|
| `query_llm.rs:835` | OpenAI stream (`stream_options.include_usage` đã bật ở `:757`) |
| `query_llm.rs:911` | OpenAI non-stream |
| `query_llm.rs:1112`, `:1170` | Anthropic stream (merge message_start + message_delta) |
| `query_llm.rs:1216` | Anthropic non-stream |

Sau đó stamp lên `Message.usage` (`mod.rs:235`) tại `query_llm.rs:1267+`.

### 1.2 Usage hiện chảy đi đâu — và chết ở đâu

```
query_llm ──► Message.usage (RawUsage đầy đủ, per-call)
                 │
                 ├─► MessageCompleteData.output_tokens (u32, CHỈ output)     conversation.rs:1264
                 │      └─► agent:reply WS {tokens} ──► badge "N tok" UI      notify.rs:52
                 │            └─► group_messages KHÔNG có cột token → mất khi reload
                 │
                 ├─► count_tokens() = SNAPSHOT message cuối (KHÔNG cộng dồn)  conversation.rs:1785
                 │      └─► agent:usage WS {useTokens,maxTokens} = gauge context window
                 │            └─► chỉ localStorage phía client
                 │
                 └─► KHÔNG ghi DB ở bất kỳ đâu
```

**Cảnh báo ngữ nghĩa**: `agent:usage` là đồng hồ **chiếm dụng context** (last-message snapshot), không phải đồng hồ **chi tiêu**. Thiết kế mới phải là số liệu riêng, không trộn vào event này.

### 1.3 Bản đồ điểm rơi (nơi token bị vứt)

| # | Điểm rơi | File:line | Mất gì |
|---|---|---|---|
| D1 | **Bridge `llm.request`** của ~30 Space Apps → `chat_completion` parse full JSON nhưng `json["usage"]` không bao giờ được đọc | `src/gateway/ui_server/llm_config.rs:321-351` | Toàn bộ in/out/cache tokens của mọi app call |
| D2 | `chat_completion_vision` — mất cả usage lẫn `finish_reason` (luôn trả `String::new()`) | `llm_config.rs:453-469` | Usage + khả năng phát hiện truncation |
| D3 | Bridge `agent.run` — `VirtualRunResult{result, duration_ms}` không có token field | `src/agent/virtual_worker_pool.rs:24-28`, arm tại `space.rs:1560-1648` | Toàn bộ usage của agent chạy hộ app |
| D4 | **Subagent bị chặn 2 lớp**: gate `!is_subagent` + drop `agent_id != MAIN_AGENT_ID` | `conversation.rs:1285`, `pool.rs:2445`, `virtual_worker_pool.rs:756`, `isolated_runner.rs:293` | Task subagent đốt 200k tokens = vô hình |
| D5 | **Compaction nguỵ tạo usage**: call tóm tắt full-history (có thể 100k+ input) bị thay bằng `RawUsage{input: 30+summary}` giả để gauge tụt xuống | `conversation.rs:214-224` (call thật tại `:193`) | Call đắt nhất session bị xoá dấu vết |
| D6 | Local models trả `usage: None` dù MLX đã đếm thật (`prompt_token_count`, `decode_tokens` — chỉ vào `tracing::info!`) | `query_llm.rs:719` (candle), `~:718` (mlx); số thật ở `src/local_model/mlx_native.rs:826-847, :1760` | Token local (0 đồng nhưng cần đếm) |
| D7 | Cognitive stack riêng — `LlmClient::complete()` trả `String` trần, response struct không khai báo `usage` | `src/memory/cognitive/llm.rs:14`, `llm_anthropic.rs`, `llm_openai.rs:226`, caller `cognify.rs:422` | Extraction 2000+ tokens/lần chat busy (config.rs:202-256) |
| D8 | Embedding providers — OpenAI/OpenRouter trả `usage.prompt_tokens/total_tokens`, bị bỏ | `src/memory/embedding_providers.rs:95, 197, 283` | Chi phí embedding |
| D9 | `llm_log.rs` audit JSONL ghi cả prompt nhưng **cố tình bỏ usage** khi nhận full `&Message` | `src/util/llm_log.rs:156-182` | Audit trail lịch sử miễn phí |
| D10 | Callers khác của `chat_completion` cũng vứt usage | `background.rs:185`, `space.rs:1441` | Draft/background calls |
| D11 | 2 app gọi API trực tiếp (rule-engine đa provider, video-cloner Gemini) — ngoài tầm daemon | `apps/rule-engine/src/rules/ai_agent.rs:219`, `apps/video-cloner/src/gemini.rs:16` | Blind spot cần cơ chế report |

### 1.4 Hai phát hiện "ăn sẵn"

1. **`background_runs.tokens_in/tokens_out` là pipeline kế toán chết hoàn chỉnh**: cột DB (`schema.rs:302-317`) + struct (`types.rs:782`) + INSERT + `SUM` aggregation (`db/background.rs:648`) + HTTP (`ui_server/background.rs:694`) + MCP (`mcp/background_server.rs:505`) — tất cả tồn tại, nhưng **cả 2 write site đều truyền `None`** (`background/runner.rs:76`, `background/scheduler.rs:420`). Nối số thật vào đây là thắng lợi rẻ nhất.
2. **2 app đã tự chế workaround `chars/4`** vì thiếu tính năng này: ai-office (`apps/ai-office/src/engine.rs:688-691` — comment phàn nàn đích danh bridge không trả usage; lưu vào cột `tasks.tokens_in/out`) và ai-chat (`engine.rs:523-525` — lưu k/v `settings`). Khi bridge trả usage thật, 2 app này chuyển sang số thật ngay.

### 1.5 Dimension có sẵn tại các điểm ghi

- Trong `query_llm`: `ModelProfile{name, provider, model_name}` (`mod.rs:1084`) — **không ghi `api_key`**.
- Tại điểm emit `conversation.rs`: `QueryConfig{agent_id (main|task_id), session_id (RỖNG với subagent — task.rs:293), is_subagent, agent_mode}`.
- `jid` (prefix mã hoá channel: `web:`, `app:`, `bg:`, `cowork:`, telegram…) **không có trong zen_core** — ranh giới có đồng thời cả jid + model là `agent_pool/pool.rs`/`notify.rs`. → Recorder phải nhận jid từ tầng agent_pool, hoặc zen_core nhận thêm "usage context" opaque khi khởi tạo session.
- Join được từ DB: `groups.llm_config_id` (override model per-group), `agents.model_id`, `bindings`.

---

## 2. Thiết kế bổ sung

### 2.1 Nguyên tắc

1. **Đơn vị ghi = 1 LLM call** (không phải 1 turn/message) — vì input + cache tokens là per-call, và compaction/hook/subagent là những call độc lập cần thấy riêng.
2. **`RawUsage` là định dạng trung chuyển duy nhất** — mọi capture point đều quy về nó rồi mới ghi.
3. **Non-blocking tuyệt đối** — recorder dùng MPSC `try_send` + batch flush (giữ nguyên thiết kế collector trong analytics-feature-design.md §2.3); analytics không bao giờ được làm chậm agent loop.
4. **Không lưu nội dung** — chỉ metadata (privacy, nhất quán doc cũ §7.2).
5. **Phân biệt số thật vs ước lượng** — cột `estimated` (chars/4 fallback) để không trộn lẫn.
6. **Gauge ≠ spend**: `agent:usage` giữ nguyên ngữ nghĩa context-gauge; spend meter là API mới đọc từ bảng mới.

### 2.2 Schema

```sql
-- Raw log, 1 row / 1 LLM call. Retention 90 ngày (job dọn hàng ngày).
CREATE TABLE IF NOT EXISTS llm_usage_log (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  ts          INTEGER NOT NULL,            -- unix ms
  source      TEXT NOT NULL,               -- 'agent'|'subagent'|'compact'|'hook'|'bridge'
                                           -- |'agent_run'|'cognitive'|'embedding'
                                           -- |'background'|'app_direct'
  jid         TEXT NOT NULL DEFAULT '',    -- group jid (prefix = channel)
  agent_id    TEXT NOT NULL DEFAULT '',    -- 'main' | task_id | persona
  session_id  TEXT NOT NULL DEFAULT '',
  app_id      TEXT NOT NULL DEFAULT '',    -- Space App id (bridge/app_direct)
  profile     TEXT NOT NULL DEFAULT '',    -- ModelProfile.name
  provider    TEXT NOT NULL DEFAULT '',
  model       TEXT NOT NULL DEFAULT '',
  input_tokens          INTEGER NOT NULL DEFAULT 0,
  output_tokens         INTEGER NOT NULL DEFAULT 0,
  cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
  latency_ms  INTEGER NOT NULL DEFAULT 0,
  ok          INTEGER NOT NULL DEFAULT 1,
  estimated   INTEGER NOT NULL DEFAULT 0   -- 1 = chars/4, không phải số API
);
CREATE INDEX idx_ulog_ts    ON llm_usage_log(ts);
CREATE INDEX idx_ulog_jid   ON llm_usage_log(jid, ts);
CREATE INDEX idx_ulog_model ON llm_usage_log(model, ts);
CREATE INDEX idx_ulog_src   ON llm_usage_log(source, ts);
CREATE INDEX idx_ulog_app   ON llm_usage_log(app_id, ts);

-- Aggregate ngày (upsert mỗi giờ + on-demand; giữ vĩnh viễn)
CREATE TABLE IF NOT EXISTS llm_usage_daily (
  date TEXT NOT NULL, source TEXT NOT NULL, jid TEXT NOT NULL,
  app_id TEXT NOT NULL, model TEXT NOT NULL,
  calls INTEGER DEFAULT 0,
  input_tokens INTEGER DEFAULT 0,  output_tokens INTEGER DEFAULT 0,
  cache_creation_tokens INTEGER DEFAULT 0, cache_read_tokens INTEGER DEFAULT 0,
  est_cost_usd REAL,                        -- NULL nếu model không có pricing
  PRIMARY KEY (date, source, jid, app_id, model)
);

-- Pricing: thêm 2 cột cache so với doc cũ (chênh ~10x của cache-read)
CREATE TABLE IF NOT EXISTS model_pricing (
  model            TEXT PRIMARY KEY,        -- match theo model_name, hỗ trợ suffix-match
  input_per_1m     REAL NOT NULL,
  output_per_1m    REAL NOT NULL,
  cache_read_per_1m  REAL,                  -- NULL → = input
  cache_write_per_1m REAL                   -- NULL → = input
);
```

Lý do cột `TEXT NOT NULL DEFAULT ''` thay vì NULL: khớp convention các bảng hiện có (`tool_executions`, `schema.rs:181`) và tránh NULL-handling trong GROUP BY.

### 2.3 UsageRecorder (module mới `src/usage/`)

```rust
// src/usage/mod.rs
pub struct UsageEvent {
    pub source: UsageSource,           // enum như cột source
    pub jid: String, pub agent_id: String, pub session_id: String,
    pub app_id: String,
    pub profile: String, pub provider: String, pub model: String,
    pub usage: RawUsage,               // tái dùng zen_core::RawUsage
    pub latency_ms: u64, pub ok: bool, pub estimated: bool,
    pub ts: i64,
}

pub struct UsageRecorder { tx: mpsc::Sender<UsageEvent> }  // try_send, buffer 10k
// task nền: flush mỗi 5s hoặc 100 events, 1 transaction / batch (Db::with_conn)
```

Wiring trong `src/lib.rs::run_daemon()`: tạo `Arc<UsageRecorder>` ngay sau Db init, truyền vào AgentPool, UIServer (cho bridge), MemoryManager (cognitive + embedding), background runner. Zen_core **không** nhận recorder trực tiếp — nó emit đủ dữ liệu qua event/return, tầng agent_pool mới có `jid` để ghi (giữ zen_core sạch, đúng ranh giới ở §1.5).

### 2.4 Kế hoạch nối từng điểm (theo thứ tự lợi ích/công sức)

**P0 — trả usage về bridge + ghi log trung tâm (mở khoá nhiều nhất):**

1. `llm_config.rs::chat_completion` — đổi return `(String, String, String)` → struct:
   ```rust
   pub struct ChatCompletionResult { pub text: String, pub model: String,
       pub finish: String, pub usage: Option<RawUsage>, pub latency_ms: u64 }
   ```
   Parse `RawUsage::from_json(&json["usage"])` ngay tại `:321`. Sửa 3 caller (`space.rs:1651` arm llm.request, `space.rs:1441`, `background.rs:185`). Sửa luôn `chat_completion_vision` (D2 — thêm cả finish_reason).
2. Bridge envelope (`space.rs:1690`) thêm:
   ```json
   {"appId":..., "status":"ok", "text":..., "model":..., "finish":...,
    "usage": {"inputTokens":N, "outputTokens":N, "cacheReadTokens":N, "cacheCreationTokens":N}}
   ```
   Daemon ghi `UsageEvent{source: Bridge, app_id, jid: "app:<id>"}` tại arm này — **app không cần tự ghi**, mọi app hưởng ngay.
3. SDK `app-space-sdk/src/bridge.rs` — parse `usage`, thêm vào return của `llm_request_full` (giữ `llm_request` 3-tuple để không gãy 30 app; thêm `llm_request_usage` hoặc mở rộng `_full`). ai-office/ai-chat bỏ `chars/4`, dùng số thật.
4. Tầng agent chính: tại `agent_pool` nơi nhận `MessageComplete` (`pool.rs:2445`) — ghi `UsageEvent{source: Agent, jid, ...}`. Cần zen_core chuyển đủ số liệu: mở rộng `MessageCompleteData` (`mod.rs:317`) thêm `input_tokens: u64`, `cache_read_tokens: u64`, `cache_creation_tokens: u64`, `model: String`, `profile: String` (hiện chỉ có `output_tokens: u32`). **Recorder ghi trước filter `MAIN_AGENT_ID`** để subagent không bị nuốt (D4) — chỉ UI event mới bị filter.
5. Compaction (D5): tại `conversation.rs:214-224`, ghi `UsageEvent{source: Compact}` với usage **thật** trước khi thay bằng usage nguỵ tạo (giữ nguyên hành vi nguỵ tạo cho gauge — nó đúng cho mục đích context meter).
6. Nối `background_runs.tokens_in/out` (§1.4): runner tổng hợp từ `llm_usage_log` theo `session_id = "bg:<run_id>"` khi kết thúc run (hoặc accumulate in-process). Pipeline HTTP/MCP có sẵn sống dậy, UI background stats tự có số.

**P1 — vá nốt các stack song song:**

7. Local models (D6): `query_local_mlx/candle` trả `RawUsage{input_tokens: prompt_token_count, output_tokens: decode_tokens}` thật thay `None` (số đã có sẵn trong `mlx_native.rs`). Cost = 0 nhưng token vẫn đếm.
8. Cognitive (D7): đổi trait `LlmClient::complete` → `Result<LlmReply{text, usage: Option<RawUsage>}>`; 4 impl + StubLlm test. `cognify.rs:422` ghi `source: Cognitive`.
9. `agent.run` bridge (D3): `VirtualRunResult` thêm `tokens_in/tokens_out`; `virtual_worker_pool.rs:756` accumulate mọi `MessageComplete` (bỏ filter cho phần đếm); arm bridge trả usage về app.
10. Embeddings (D8): parse `usage` từ response OpenAI/OpenRouter, ghi `source: Embedding` (output=0). Local/Ollama: đếm ước lượng hoặc bỏ (estimated=1 nếu ghi).
11. Hooks (`prompt_executor.rs:135`) → `source: Hook`; subagent (`task.rs:310`) → `source: Subagent, agent_id: task_id`.

**P2 — hoàn thiện:**

12. Audit log (D9): serialize `msg.usage` trong `llm_log.rs::log_response` — 1 field, có ngay historical log 30 ngày.
13. Direct-API apps (D11): thêm bridge action `usage.report` (payload = UsageEvent rút gọn, source `app_direct`) để rule-engine/video-cloner đẩy usage về daemon. Không bắt buộc — app không report thì là blind spot có chủ đích.
14. Sửa leak: `agent:usage` đang `broadcast_to_all` (`notify.rs:126`) — đổi sang subscription-filtered `broadcast()` như `agent:reply`.
15. Aggregator + retention: task nền mỗi giờ upsert `llm_usage_daily` + tính `est_cost_usd`; task hàng ngày xoá `llm_usage_log` > 90 ngày (mirror FIFO-trim convention hiện có).

### 2.5 REST API + UI

Routes (đặt trong `ui_server`, style khớp `/api/background/stats`):

```
GET /api/usage/overview            → {today: {calls,in,out,cacheRead,costUsd}, week, month}
GET /api/usage/daily?days=30       → rows llm_usage_daily (cho chart)
GET /api/usage/breakdown?by=model|source|jid|app&days=7
GET /api/usage/log?limit=100&before=<id>   → raw log gần nhất (debug)
GET/PUT /api/usage/pricing         → CRUD model_pricing
```

UI: thêm section **Token Usage** vào `web/src/pages/DashboardPage.tsx` (đã tồn tại): 4 cards (in hôm nay / out hôm nay / cost ước tính / % cache-read) + line chart 30 ngày + bảng breakdown theo model và theo app. AnalyticsPage đầy đủ của doc cũ vẫn là hướng mở rộng sau.

Realtime không cần cho P0 — UI poll REST 30s; event WS `usage:tick` chỉ cân nhắc nếu muốn đồng hồ chạy live.

MCP server `senclaw-usage` (tools `usage_overview`, `usage_breakdown`, `usage_query`) để agent tự trả lời "hôm nay tốn bao nhiêu" — làm ở P2, và **phải thêm dòng mới vào bảng registry trong CLAUDE.md** theo naming convention.

### 2.6 Cost model

- Match pricing theo `model_name` chính xác trước, sau đó suffix-match (model id có date suffix), fallback NULL → UI hiển thị "n/a" thay vì 0 (không nguỵ tạo số 0 đồng).
- Seed vài dòng phổ biến (Claude/GPT hiện hành + `cache_read ≈ 0.1×input`, `cache_write ≈ 1.25×input`); editable qua `/api/usage/pricing` — **không hardcode giá trong code** (doc cũ §7.2 đúng, giữ nguyên).
- Local model: hiện diện trong breakdown với cost 0, để thấy tỷ trọng offload.

---

## 3. Rủi ro & gotchas

| Rủi ro | Đối sách |
|---|---|
| Đổi return type `chat_completion` gãy 3 caller | Compile-driven fix, 1 commit riêng |
| Đổi trait `LlmClient` (cognitive) gãy 4 impl + StubLlm | Trait nội bộ, ít impl; sửa cùng lúc |
| Nhầm gauge (snapshot) với spend (cộng dồn) | Không đụng `agent:usage`; spend chỉ từ bảng mới; đặt tên API `usage/overview` ≠ `conversation:usage` |
| `MessageCompleteData.output_tokens` là `u32` | Field mới dùng `u64` nhất quán RawUsage |
| SQLite 1 Mutex conn — recorder chèn nhiều | Batch 1 transaction / flush (≤100 rows/5s là nhẹ); `try_send` drop khi đầy, không block |
| `session_id` rỗng với subagent (`task.rs:293`) | Ghi `agent_id = task_id` làm khoá truy vết; cân nhắc điền session_id cha |
| `ModelProfile.api_key` trong scope điểm ghi | Tuyệt đối không đưa vào UsageEvent |
| Bridge thêm field `usage` — app cũ parse struct chặt | JSON thêm field là backward-compatible (serde bỏ qua field lạ); SDK giữ hàm cũ nguyên chữ ký |
| Double-count khi vừa ghi ở agent_pool vừa ở bridge (app dùng `agent.run` → chạy agent thật) | Quy ước: điểm ghi là **nơi call LLM xảy ra** (agent path ghi source=agent/subagent); bridge `agent.run` KHÔNG ghi thêm row mới, chỉ trả tổng về app |

---

## 4. Thứ tự triển khai đề xuất

- **Phase 1 (P0, ~3-4 ngày)**: `src/usage/` recorder + schema + capture agent path/compact/bridge + `ChatCompletionResult` + bridge envelope + SDK + nối `background_runs` + `/api/usage/overview` + cards Dashboard. → Đã có luồng token in/out end-to-end cho daemon + toàn bộ Space Apps.
- **Phase 2 (P1, ~2-3 ngày)**: local models, cognitive, embeddings, agent.run, aggregator + pricing + breakdown UI.
- **Phase 3 (P2, ~2 ngày)**: MCP `senclaw-usage`, `usage.report` cho direct apps, audit log field, fix broadcast leak, retention, ai-office/ai-chat chuyển sang số thật.

## 5. Ngoài phạm vi (ghi nhận, không làm đợt này)

- Budget/quota enforcement (chặn khi vượt ngưỡng) — cần thiết kế UX riêng; schema này đã đủ dữ liệu đầu vào.
- Latency percentiles, tool analytics, heatmap — vẫn thuộc analytics-feature-design.md.
- TTS/OCR/ASR không có khái niệm token tương đương — không nhét vào bảng này.
