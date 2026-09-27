# SDK: Space App đăng ký LLM provider — Thiết kế

> **Trạng thái: THIẾT KẾ ban đầu.** Bản khảo sát nền (`local-model-space-app-extraction.md`)
> đã chuyển sang repo `sen-mlx` cùng với chính engine đó — model cục bộ giờ là
> một *runtime* (`docs/runtime-protocol.md`), không còn là Space App
> `apps/mlx-lm`. Tài liệu này vẫn đúng cho **cơ chế chung**: bất kỳ Space App
> nào khai báo khối `llm` đều trở thành một provider theo cách mô tả bên dưới.

## 0. Phạm vi đã chốt

Chỉ phần **LLM** rời daemon. ASR (Whisper), TTS (ZipVoice + MMS-VITS) và
cognitive embedder ở lại — nên `mlx-rs` vẫn biên dịch vào daemon và **thời gian
build gần như không giảm** (chỉ mất `turboquant-rs`, xem §1).

Cái được thật của phạm vi này là **nhịp phát hành**, không phải build:
`mlx_lm/models/` là nơi mọi kiến trúc mới hạ cánh — Gemma-4 (2 319 LOC),
Qwen3.5 (1 105), Mamba2 (1 367), Gemma2 (1 381), DeepSeek-V2 (914),
Bonsai-Q1 (894), Falcon-Mamba (837), Ouro (748)… Sau khi tách, **thêm một
kiến trúc model = phát hành một bản app**, không phải một bản daemon (macOS
release ~37 phút CI, phải ký + notarize + đẩy updater).

---

## 1. Đường cắt chính xác cho phạm vi "chỉ LLM"

Đã kiểm tra từng import. Đường cắt sạch hơn dự đoán vì
`mlx_lm/models/whisper.rs` chỉ import **`error::Error`** từ `mlx_lm` — nó
không dùng `cache.rs` cũng không dùng `utils/`.

### Sang app `apps/mlx-llm` (~21 700 LOC)

| Đường dẫn | LOC |
|---|---|
| `mlx_lm/models/*` **trừ** `whisper.rs` | 12 660 |
| `mlx_lm/cache.rs` | 1 802 |
| `mlx_lm/utils/` (moe, rope, yarn, turboquant_attn) | 1 092 |
| `mlx_lm/prefix_cache.rs` | 451 |
| `mlx_lm/sampling.rs` | 219 |
| `mlx_native.rs` | 4 487 |
| `chat_template_openai.rs` | 475 |
| `mlx_prompt.rs` | 260 |
| `image_input.rs` | 294 |
| Nửa LLM của `gateway/ui_server/local_models.rs` (engine registry, downloader HF, `settings.json`, 14 route) | ~1 000 |

Dep `turboquant-rs` đi theo trọn vẹn — Whisper/TTS không dùng nó. Đây là phần
build win duy nhất của phạm vi này.

### Ở lại daemon

- `mlx_lm/models/whisper.rs` (718) → dời lên `src/local_model/whisper_model.rs`.
  Một lần `mv` + sửa `use`, không có gì khác.
- `whisper_transcribe.rs`, `audio.rs`, `tts/`, cognitive embedder — nguyên vẹn.
- `mlx_lm/error.rs` (19 LOC) + `mlx_serial_lock` (~25 LOC): hai bên đều cần.
  Cho vào crate workspace `senclaw-mlx-core`. Lưu ý `MLX_SERIAL` là `static` —
  **mỗi tiến trình có một cái riêng**, đúng ý: daemon serialize giữa
  ASR/TTS/embedder của nó, app serialize giữa các lượt LLM của nó. Còn chuyện
  *liên tiến trình* là rủi ro mở, §7.

### Pha 0 vẫn phải làm trước

`stream_parser.rs` (1 506) + `thinking_parse.rs` + `models.rs` + bảng arch của
`hf_validate.rs` → crate `senclaw-local-core` (không dep MLX/candle). Cả daemon
lẫn app dep vào. Không làm bước này thì 1 500 LOC parser bị chép đôi.

---

## 2. Wire: app nói OpenAI, daemon không cần adapter mới

App đã có sẵn `stream_openai_to_channel` và `chat_template_openai.rs`. Cho app
phơi ra đúng OpenAI thì daemon **dùng lại `adapt: "openai"` đang có** — không
một dòng adapter mới nào.

```
GET  /v1/models              → danh sách model + capability
POST /v1/chat/completions    → SSE (stream) và JSON (non-stream)
GET  /health                 → health gate của daemon
```

Đã xác minh proxy của daemon **stream được**:
[`space_apps_proxy`](../src/gateway/ui_server/space.rs:3404) trả
`Body::from_stream(res.bytes_stream())` và client forward là
`reqwest::Client::new()` **không đặt timeout tổng**. SSE đi qua nguyên vẹn.

---

## 3. Đăng ký: soi gương `mcp.autoRegister`

Mọi mảnh đã tồn tại; việc là lặp lại đúng khuôn.

### Manifest

```json
"llm": {
  "autoRegister": true,
  "path": "/v1",
  "adapt": "openai",
  "healthPath": "/health",
  "providers": [{
    "id": "mlx-local",
    "displayName": "MLX (Apple Silicon)"
  }]
}
```

### Bảng đối chiếu

| MCP hôm nay | LLM provider |
|---|---|
| registry MCP của `McpManager` | bảng `space_app_llm_providers` |
| session app → `/api/space/apps/<id>/proxy<mcp.path>`; background app → cổng thật | **y hệt** |
| tool cache `<app>/.senclaw/mcp-tools.json` | model cache `<app>/.senclaw/llm-models.json` |
| `read_tool_cache` — **rỗng không được ghi đè** (đã có test ở `space_mcp.rs:1355`) | giữ đúng luật đó |
| `stamp_app_identity` đóng dấu token | proxy đã tự đóng dấu, không cần |
| Stop app **không** gỡ đăng ký (nếu không "stop" = "tool biến mất") | Stop app **không** gỡ model khỏi picker |

Vì sao cache model list lại quan trọng: giống hệt lý do của tool cache — model
picker phải thấy model khi app **đang tắt**, nếu không sẽ không ai chọn nó,
không ai gọi nó, và app không bao giờ khởi động.

### Nối vào picker

`GET /api/llm-config` trộn thêm các dòng do app cấp, gắn `source: "app:<id>"`,
**read-only** trên UI. `LlmConfig`
([types.rs:261](../src/gateway/group_manager/types.rs:261)) đã có đủ trường:
`provider`, `base_url`, `api_key`, `model_name`, `adapt`, `max_tokens`,
`context_length`, `vision`. `api_key` để rỗng — loopback được miễn token
daemon, và proxy tự đóng dấu app token. (Nhắc: `/api/llm-config` là
world-readable, không được để gì bí mật lọt vào đó.)

Gỡ/tắt app phải dọn dòng tương ứng **và** xử lý `active_id` đang trỏ vào nó —
để treo một `active_id` mồ côi là hỏng cả phiên chat.

---

## 4. SDK phải viết

```rust
// app-space-sdk/src/llm.rs
pub struct ModelCard {
    pub id: String,
    pub context_length: u32,
    pub max_output_tokens: u32,
    pub vision: bool,          // BẮT BUỘC, xem §5
    pub tools: bool,
}

pub trait LlmProvider: Send + Sync + 'static {
    fn models(&self) -> Vec<ModelCard>;
    async fn chat(&self, req: ChatRequest, sink: ChunkSink) -> Result<()>;
}

/// Router axum dựng sẵn: /v1/models + /v1/chat/completions (SSE & JSON).
pub fn openai_router<P: LlmProvider>(p: Arc<P>) -> axum::Router;

/// Ghi <app>/.senclaw/llm-models.json lúc khởi động.
pub fn publish_models(cards: &[ModelCard]) -> Result<()>;
```

Tác giả app chỉ `impl LlmProvider`; wire format là việc của SDK. Rust trước
(app MLX là Rust), Node/Python/Go theo sau cùng validator.

`manifest::LlmDecl` + validator, giữ đúng kỷ luật "sai chính tả phải kêu" đã có
với `runtime.mode`:

- `adapt` không nằm trong tập adapter daemon thật sự route → **từ chối**, không
  im lặng fallback về `openai`. Đây chính là hình dạng của test
  `every_signin_provider_routes_to_a_real_adapter`
  ([query_llm.rs:1957](../src/zen_core/query_llm.rs:1957)) — viết lại một bản
  cho app-provided provider.
- Thiếu `vision` trên `ModelCard` → từ chối (§5).
- Thêm test kiểu `tests/space_app_lifecycle_manifests.rs` cho khối `llm`.

---

## 5. `vision` phải khai tường minh, không được suy diễn

CLAUDE.md đã chốt: kiểm tra khả năng nhìn ảnh **bắt buộc** đi qua
`ZenEngine::model_accepts_images` → `resolve_model_profile_at`, và *"Never send
image blocks on a maybe"* — endpoint text-only nhận image block sẽ trả 400 cứng,
hỏng cả lượt.

Model app cấp nằm chung danh sách `LlmConfig` nên `model_accepts_images` chạy
đúng **miễn là** `vision` được điền. Vấn đề: id model MLX có dạng
`mlx-community__Qwen3.5-2B-OptiQ-4bit` — không khớp pattern nào trong
[`vision.rs`](../src/zen_core/vision.rs), nên suy diễn sẽ ra `false` một cách
tình cờ, và một ngày nào đó ra `true` một cách tình cờ.

Nên: **`vision` là trường bắt buộc của `ModelCard`**, validator từ chối nếu
thiếu. App biết chính xác — nó vừa nạp `config.json` của checkpoint.

---

## 6. Bẫy vận hành — cái này là nghiêm trọng nhất

### 6a. `REQUEST_TIMEOUT = 120s` sẽ giết các lượt sinh dài đang chạy được hôm nay

[`query_llm.rs:20`](../src/zen_core/query_llm.rs:20) đặt
`REQUEST_TIMEOUT: Duration = from_secs(120)` — **timeout tổng** cho toàn bộ
request, áp lên đường `openai`. Trong khi đó đường `query_local_mlx` in-process
hôm nay **không có timeout nào cả**: nó là vòng lặp channel, chỉ dừng bởi
`cancel`.

Làm phép tính với mặc định hiện tại (`DEFAULT_MLX_MAX_NEW_TOKENS = 8192`,
`DEFAULT_MLX_MAX_PROMPT_TOKENS = 128_000`):

```
8192 token ÷ 60 tok/s   ≈ 136 s   ← chỉ riêng decode, đã quá 120 s
+ nạp weights lần đầu   ≈ vài đến vài chục giây
+ prefill prompt dài
```

Chuyển thẳng local MLX sang adapter `openai` là **làm hỏng thứ đang chạy tốt**.

Cách sửa đúng không phải nâng `REQUEST_TIMEOUT` (nó tồn tại để bắt provider
chết): dùng **read timeout** — reqwest 0.12 có `.read_timeout()`. Im lặng 120 s
= chết; chảy token đều trong 10 phút = bình thường. Đặt theo profile: provider
do app cấp thì bỏ timeout tổng, giữ `connect_timeout` + `read_timeout`.

### 6b. Không được nạp weights trong lúc health-gate

`wait_answering` có ngân sách **30 s**, mỗi lần probe dùng client
`.timeout(5s)` ([space_mcp.rs:150](../src/gateway/ui_server/space_mcp.rs:150)).
App **phải** bind cổng và trả `/health` **trước** khi chạm vào một byte weights
nào. Nạp 4 GB trong `main()` = health gate trượt = daemon báo *"App is not
running"*, mà nguyên nhân thật thì không hiện ở đâu cả.

Nạp lazy ở request `/v1/chat/completions` đầu tiên.

### 6c. `idleTimeoutSecs` phải là 300, không phải 60

Mặc định session app là 60 s. Daemon hôm nay đã trả lời đúng bài toán này bằng
`DEFAULT_IDLE_UNLOAD_SECS = 300`
([local_models.rs:704](../src/gateway/ui_server/local_models.rs:704)). Để 60 s
thì hai lượt chat cách nhau 2 phút = nạp lại 4 GB. Vẫn dùng `session` —
`background` là giữ 4 GB thường trực từ lúc boot, đúng thứ
`docs/space-app-lifecycle.md` sinh ra để giết.

### 6d. Vòng lặp vô hạn qua `SpaceClient::llm_request`

`app-space-sdk` cho app hỏi daemon một câu LLM
([bridge.rs](../app-space-sdk/src/bridge.rs)). Nếu model đang active **chính
là** provider của app đó → đệ quy vô hạn. Chặn: bridge resolve ra profile có
`source == "app:<id-người-gọi>"` thì từ chối.

### 6e. 25 GB weights nằm ngoài thư mục app

Daemon phải tiêm `SENCLAW_LOCAL_MODELS_DIR` trỏ về `~/.senclaw/local-models/`.
Dùng `space-app-data/mlx-llm/` là bắt mọi người tải lại 25 GB. Sandbox từng app
phải cấp thư mục đó — nhớ bẫy trong `docs/space-app-sandbox.md`: cấp thư mục
con không đủ nếu thư mục **cha** bị chặn.

### 6f. `mlx.metallib` phải nằm cạnh binary của app

CI hiện copy nó vào `Contents/Resources/` cạnh binary daemon
([desktop.yml:262](../.github/workflows/desktop.yml)). App có binary riêng →
bundle app phải mang bản của chính nó. Thiếu thì MLX hỏng **im lặng**.

### 6g. Proxy retry gửi lại body

`space_apps_proxy` thử `forward` một lần, lỗi kết nối thì `ensure_port()` rồi
thử lại — **gửi lại nguyên body**. Với `/v1/chat/completions` thì vô hại (chỉ
tốn token, và lỗi chỉ xảy ra trước khi có response header), nhưng nên biết.

---

## 7. Rủi ro mở

| Rủi ro | Mức | Ghi chú |
|---|---|---|
| `REQUEST_TIMEOUT` 120 s cắt lượt sinh dài | **Cao** | §6a — regression thật so với hôm nay, phải sửa cùng lúc |
| Hai tiến trình cùng dùng Metal | **Cao** | Daemon (ASR/TTS) và app (LLM) cùng đập Metal. Repo chưa từng chạy cấu hình này; crash AGX đã ghi nhận là *trong cùng tiến trình*. **Phải đo trước khi viết code app** |
| Seatbelt có chặn truy cập GPU/Metal không | **Chưa rõ** | Chưa ai chạy app sandbox nào cần GPU. Phải thử trước khi hứa `sandbox` cho app này |
| Health gate 30 s | Trung bình | §6b — sửa được bằng nạp lazy |
| Hop thêm qua HTTP server của chính daemon | Thấp | query_llm → axum daemon → proxy → app. Loopback, có stream, không deadlock |

---

## 8. Thứ tự làm

1. **Pha 0** — crate `senclaw-local-core` (parser + arch table). Độc lập, làm
   được ngay.
2. **Đo Metal đa tiến trình** + **thử Seatbelt với GPU**. Hai câu hỏi này quyết
   định app có khả thi không; trả lời trước khi viết app.
3. **Sửa timeout** (§6a) — `read_timeout` thay `timeout` tổng cho provider
   streaming. Việc này đứng riêng được và có lợi cho cả provider từ xa.
4. **SDK + registry** (§3, §4) kèm app demo `echo` ~200 LOC. Wire được test
   thật trước khi 21 700 LOC dọn nhà.
5. **`apps/mlx-llm`** — chỉ sau khi (2) xanh và (4) chạy được với app demo.
