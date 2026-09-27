# `senclaw-space-sdk` (Python)

Viết Space App bằng Python. Không phụ thuộc thư viện ngoài — chỉ standard
library, nên app **không cần bước cài đặt** trước lần chạy đầu.

> Bản Node/TypeScript: [`../senclaw-app-sdk`](../senclaw-app-sdk).
> Bản Go: [`../senclaw-app-sdk-go`](../senclaw-app-sdk-go).
> Bản Rust: [`../../app-space-sdk`](../../app-space-sdk).
> Vòng đời app (background/session), `requires`, `sandbox`:
> [`docs/space-app-lifecycle.md`](../../docs/space-app-lifecycle.md).

## Cài

```bash
pip install senclaw-space-sdk
# hoặc, làm việc thẳng trong repo này:
pip install -e senclaw-sdk/senclaw-app-sdk-python
```

Trong app thật, khai `requirements.txt` trỏ tới package đã publish, hoặc copy
thư mục `senclaw_space/` vào app — nó chỉ là 5 file stdlib.

## App mẫu

[`examples/space-app-python-demo/`](examples/space-app-python-demo) — một Space
App hoàn chỉnh trong một file: 2 MCP tool, một trang UI, health endpoint, xử lý
SIGTERM. Manifest của nó khai đủ bốn thứ quyết định daemon chạy app thế nào
(`runtime.mode`, `runtime.runner`, `requires`, `sandbox`).

```bash
cd examples/space-app-python-demo
PYTHONPATH=../.. SENCLAW_SPACE_APP_ID=python-demo PORT=4810 python main.py

# hoặc cài vào daemon đang chạy — daemon tự tạo .venv và cài requirements.txt
curl -X POST http://127.0.0.1:18788/api/space/apps/register-local \
  -H 'Content-Type: application/json' -d "{\"path\": \"$(pwd)\"}"
```

## App tối thiểu

`main.py`:

```python
from senclaw_space import McpServer, SenclawSpace, serve

space = SenclawSpace()          # đọc SENCLAW_SPACE_APP_ID + SENCLAW_BASE_URL
mcp = McpServer("demo-mcp")

@mcp.tool("demo_summarise", "Tóm tắt một đoạn văn bản", {
    "type": "object",
    "properties": {"text": {"type": "string"}},
    "required": ["text"],
})
def summarise(args):
    # App KHÔNG BAO GIỜ giữ API key của provider — mọi lời gọi model đi qua
    # daemon, dùng provider người dùng đã cấu hình.
    return space.llm(f"Tóm tắt trong 3 câu:\n\n{args['text']}", max_tokens=800)

serve(
    {("GET", "/api/status"): lambda req: {"ok": True}},
    health_path="/api/status",
    mcp_path="/api/mcp/sse",
    mcp_handler=mcp.handle,
    static_dir="web",
)
```

`senclaw-manifest.json`:

```json
{
  "id": "demo-py",
  "name": "Demo Python",
  "description": "Space App viết bằng Python",
  "icon": "🐍",
  "runtime": {
    "kind": "server",
    "mode": "session",
    "runner": "python",
    "start": "python main.py",
    "healthPath": "/api/status",
    "port": 4810
  },
  "requires": { "python": ">=3.10" },
  "integration": { "type": "iframe", "url": "/" },
  "mcp": {
    "name": "demo-py-mcp",
    "transport": "http",
    "path": "/api/mcp/sse",
    "autoRegister": true
  }
}
```

Cài vào daemon đang chạy:

```bash
curl -X POST http://127.0.0.1:18788/api/space/apps/register-local \
  -H 'Content-Type: application/json' \
  -d '{"path": "/duong/dan/toi/app"}'
```

## Dịch vụ của daemon

```python
sc = SenclawSpace(app_id="my-app")

sc.capabilities()                                    # daemon này làm được gì

sc.llm(prompt, system=..., max_tokens=..., profile=...)   # -> str
sc.llm_detailed(prompt)                              # -> text, model, finish, usage

sc.knowledge_save("nhớ cái này", space="proj", tags=["x"])
sc.knowledge_search("hỏi gì đó", space="proj")       # hit thô
sc.knowledge_recall("hỏi gì đó", space="proj")       # câu trả lời đã tổng hợp

active, models = sc.list_models()
sc.usage_report(model, provider, input_tokens, output_tokens)
sc.register_mcp({"transport": "sse", "url": "..."})
```

Ba chỗ dễ sập:

- **`llm()` báo lỗi khi câu trả lời bị cắt.** `finish == "length"` nghĩa là model
  chạm trần `max_tokens` giữa chừng, mà một đoạn cụt thì không phân biệt được
  với một câu trả lời ngắn. Muốn tự xử thì dùng `llm_detailed()`.
- **Bridge lỗi vẫn về HTTP 200**, kèm `{"status": "error"}` trong thân. SDK ném
  `SenclawError`; nếu tự gọi bridge thì phải kiểm trường `status`, không thì
  provider chết sẽ đọc thành chuỗi rỗng.
- **`profile` chứ đừng `set_active_model`.** Model đang hoạt động là **toàn
  cục** — agent và mọi app khác dùng chung.

## Dispatch

Cho daemon (`MCPDispatcher`) điều khiển app:

```python
from senclaw_space.dispatch import DispatchProvider, dispatch_routes

class Store(DispatchProvider):
    def claim_ready(self, capacity): ...   # phải nguyên tử
    def finalize(self, item_id, outcome): ...

serve(routes={**dispatch_routes(Store()), ("GET", "/api/status"): status})
```

`heartbeat` / `reclaim` có mặc định no-op. Tên trường là snake_case
(`depends_on`, `timeout_secs`, `item_id`) vì engine đọc bằng serde — viết
camelCase là bị bỏ im lặng, và nó hiện ra dưới dạng một phụ thuộc không bao giờ
có hiệu lực chứ không phải một lỗi.

## App làm model (`senclaw_space.llm`)

Chiều ngược của `space.llm()`: thay vì app hỏi daemon một câu trả lời, **app
chính là model**. App khai một khối `llm` trong manifest, daemon đăng ký mọi
model app quảng cáo vào **cùng picker với OpenAI/Anthropic**, và lượt của agent
gửi tới qua HTTP dưới dạng request OpenAI `chat/completions`.

```json
"llm": { "autoRegister": true, "path": "/v1", "adapt": "openai", "displayName": "MLX" }
```

App nói **OpenAI** (`GET /v1/models`, `POST /v1/chat/completions`), nên daemon
dùng lại `adapt: "openai"` — không cần adapter mới. Provider chỉ phát **sự kiện
ngữ nghĩa** (`Text` / `Reasoning` / `ToolCall` / `Usage`); SDK lo phần render ra
wire.

```python
from senclaw_space import serve
from senclaw_space.llm import (
    ChatRequest, ChunkSink, LlmProvider, ModelCard, llm_routes, publish_models,
)

class Mlx(LlmProvider):
    def models(self):
        # `vision` là BẮT BUỘC — daemon dựa vào nó để chọn gửi ảnh thật hay
        # rơi về OCR. Đừng đoán từ tên model.
        return [ModelCard("gemma-4-e2b-it-4bit", 128_000, 8192, vision=True)]

    def chat(self, req: ChatRequest, sink: ChunkSink) -> None:
        # Nạp weights ở đây (lười), KHÔNG nạp lúc khởi động — daemon
        # health-check trong 30s, nạp GB trước khi bind cổng = "app chết".
        sink.text("xin chào")

provider = Mlx()
publish_models(".", provider.models())   # để app ĐANG DỪNG vẫn hiện trong picker
serve(
    routes={**llm_routes(provider), ("GET", "/health"): lambda r: {"ok": True}},
    health_path="/health",
)
```

Bốn chỗ dễ sập:

- **`vision` bắt buộc, không suy diễn.** Endpoint text-only trả **400 cứng** cho
  một khối ảnh và hỏng cả lượt; OCR chỉ làm giảm chất lượng — hậu quả bất đối
  xứng nên đoán sai là đắt. App đang mở `config.json` của model, app biết.
- **Nhiều tool call phải ra ở các `index` KHÁC nhau.** Adapter OpenAI cộng dồn
  `function.name`/`arguments` theo `index`, nên dùng lại `index 0` sẽ hàn
  `get_weatherget_time` dính vào nhau. Cứ phát `ToolCall` **nguyên khối** —
  `llm_routes` tự cấp index mới tăng dần, không tự tay viết JSON delta.
- **`ChunkSink` ở đây *gom* chứ không *stream*.** Harness `serve()` của Python
  là đồng bộ + đệm: route chạy `chat()` xong mới render. `is_closed()` luôn
  `False` (không có tín hiệu client ngắt), stream luôn kết thúc bằng
  `data: [DONE]` — cả khi lỗi (một error chunk rồi `[DONE]`, không cắt im lặng).
- **`publish_models` từ chối danh sách rỗng** và ghi kiểu write-then-rename: một
  lần khởi động hỏng không được xoá cache tốt đang có trong picker.

## Những gì SDK lo hộ

| | |
|---|---|
| `bind_host()` | `127.0.0.1` trừ khi `SENCLAW_BIND_HOST` nói khác. App **không có xác thực riêng** — bind `0.0.0.0` là mở toàn bộ REST + MCP ra LAN |
| `port()` | Cổng daemon giao qua `PORT` |
| `serve(...)` | Health + static + REST + MCP trên một cổng, và **bắt SIGTERM** |
| `space.llm(...)` | Gọi model qua bridge; báo lỗi khi bị cắt ở `maxTokens` thay vì trả về đoạn cụt |
| `space.sqlite(...)` | DB riêng của app, luôn tham số hoá |
| `space.get_config` / `set_config` | Cùng KV mà UI của app đọc/ghi — không phải file trong thư mục app (update sẽ đè) |
| `McpServer` | JSON-RPC `initialize` / `tools/list` / `tools/call`, không cần MCP SDK |
| `llm_routes(...)` | Biến app thành model: `/v1/models` + `/v1/chat/completions` OpenAI, render `ToolCall`/`Usage` đúng shape wire |

## Token truy cập của app

Daemon phát cho mỗi app đã cài một token, đặt vào môi trường tiến trình qua
`SENCLAW_TOKEN_ACCESS_APP`. Đó là **định danh** của app: token gắn với đúng một
app id, dùng cho id khác sẽ bị từ chối. Không có nó, bất kỳ tiến trình nào biết
id của app — mà id là công khai — cũng đọc được settings, truy vấn được database
và điều khiển được AI bridge của app đó.

**Chiều ra: tự động.** `SenclawSpace()` đọc token từ môi trường và gửi kèm
(cùng `X-SenClaw-Api-Version`) trên mọi lời gọi daemon. Chạy tay thì truyền
thẳng:

```python
space = SenclawSpace(app_id="demo", app_token="sca_…")
```

**Chiều vào: bật thủ công.** REST + MCP của chính app không có xác thực nào —
cổng mở cho mọi tiến trình trên máy. Bật guard thì chỉ còn daemon gọi được, vì
proxy của daemon đóng dấu token lên mọi request nó chuyển tiếp (iframe UI, fetch
của app, mọi lời gọi MCP):

```python
serve(
    routes,
    health_path="/api/status",     # luôn được miễn
    require_app_token=True,
    auth_skip_paths=["/public/*"], # extension gọi thẳng
)
```

Hai trường hợp không bao giờ bị từ chối: **không có token trong env** (đó là
`python app.py` chạy tay ngoài SenClaw — trả 401 cho cả health check sẽ biến
"chưa phát token" thành "app chết"), và các đường dẫn miễn trừ ở trên.

`SENCLAW_API_VERSION` mang phiên bản hợp đồng (hiện là 2). Daemon phục vụ hợp
đồng cũ hơn bình thường; hợp đồng mới hơn nó chưa hỗ trợ thì trả **426** thay vì
trả lời nửa vời.

Hướng dẫn đầy đủ, gồm cả `SENCLAW_APP_TOKEN_MODE=strict`:
[docs/space-app-api-token.md](https://github.com/NortonBen/SenClaw/blob/main/docs/space-app-api-token.md).

## SIGTERM — đọc trước khi viết app

Một app **session** bị dừng khi rảnh: daemon gửi `SIGTERM` cho cả process
group, hai giây sau là `SIGKILL`. `serve()` đã cài handler đóng listener và gọi
`on_shutdown` của bạn; đừng chặn quá 2 giây trong đó.

```python
serve(..., on_shutdown=lambda: db.close())
```

## `runner: "python"` — daemon làm gì

Khi manifest khai `runner: "python"` (hoặc `start` bắt đầu bằng `python`):

1. Kiểm tra `requires.python` — thiếu thì **không chạy**, và lý do nằm trong
   thông báo lỗi chứ không phải trong log.
2. Nếu app có `requirements.txt` / `pyproject.toml` / khai `runtime.install`:
   tạo `.venv` **trong thư mục app** và cài vào đó. Không bao giờ cài vào
   Python hệ thống của người dùng.
3. Chạy `runtime.start` với `.venv/bin` đứng đầu `PATH`.

Bước 2 chỉ chạy lại khi nội dung `requirements.txt` / lệnh install đổi — dấu
vân tay theo **nội dung** file, nên giải nén bản update (làm mới mtime) không
kích hoạt cài lại.

Tắt venv bằng `"venv": false` nếu app tự lo môi trường.

## Kiểm tra manifest

```bash
python -m senclaw_space.manifest senclaw-manifest.json
```

Bắt đúng loại lỗi im lặng: `"mode": "backgroud"` (sai chính tả → thành
`session`), `network: "hosts"` mà danh sách host rỗng (= app mất mạng hoàn
toàn), `autoRegister` mà không có `path`.
