# `senclaw-app-sdk-swift`

Viết Space App bằng Swift. **Chỉ dùng Foundation** — không có package nào phải
resolve trước lần build đầu, và không cần `pip`/`npm install` trước khi chạy.

> Bản Node/TypeScript: [`../senclaw-app-sdk`](../senclaw-app-sdk) ·
> Python: [`../senclaw-app-sdk-python`](../senclaw-app-sdk-python) ·
> Go: [`../senclaw-app-sdk-go`](../senclaw-app-sdk-go) ·
> Rust: [`../../app-space-sdk`](../../app-space-sdk).
> Vòng đời app (background/session), `requires`, `sandbox`:
> [`docs/space-app-lifecycle.md`](../../docs/space-app-lifecycle.md).
> English: [README.md](README.md).

## Cài

`Package.swift`:

```swift
.package(url: "https://github.com/NortonBen/SenClaw.git", from: "0.1.0"),
// …rồi ở target:
.product(name: "SenclawSpace", package: "SenClaw"),
```

```swift
import SenclawSpace
```

Cần macOS 12+ (server là một HTTP server nhỏ trên POSIX socket; phần client,
manifest, MCP, dispatch và render LLM thì thuần Foundation).

## App mẫu

[`Examples/space-app-swift-demo/`](Examples/space-app-swift-demo) — một Space App
hoàn chỉnh trong một file: 2 MCP tool, một model app tự phục vụ (`swift-echo`),
một trang UI, health endpoint, xử lý SIGTERM.

```bash
cd Examples/space-app-swift-demo
SENCLAW_SPACE_APP_ID=swift-demo PORT=4831 swift run
```

## Đọc trước: app Swift KHÔNG có bước cài đặt

Daemon chỉ chạy `runtime.install` cho runner **node** và **python**
([`src/apps/prepare.rs`](../../src/apps/prepare.rs) trả về ngay với `binary` và
`shell`). Hai dạng chạy được:

| | `start` | `runner` | Đánh đổi |
|---|---|---|---|
| **Build sẵn** (nên dùng) | `./my-app` | `binary` | Khởi động vài mili-giây; phải build binary cho từng nền tảng. |
| **`swift run`** (bản demo) | `swift run -c release` | `shell` | Không cần build tay, nhưng phải khai `requires.bin: ["swift"]`. **Lần đầu phải biên dịch**, mà daemon chỉ cho **30 giây** để trả lời health — nên build trước một lần bằng tay. |

## App tối thiểu

```swift
import Foundation
import SenclawSpace

let space = try SpaceClient(appId: "demo-swift")
let mcp = McpServer("demo-swift-mcp")

mcp.tool("demo_summarise", "Tóm tắt văn bản",
         ["type": "object", "properties": ["text": ["type": "string"]], "required": ["text"]]) { args in
    // App KHÔNG bao giờ giữ API key của provider — mọi lượt gọi model đi qua
    // daemon, dùng provider mà người dùng đã cấu hình.
    try space.llm(prompt: "Tóm tắt trong ba câu:\n\n\((args["text"] as? String) ?? "")", maxTokens: 800)
}

try Serve(Config(
    routes: [RouteKey("GET", "/api/status"): { _ in Response(json: ["ok": true]) }],
    healthPath: "/api/status", mcpPath: "/api/mcp/sse", mcp: mcp, staticDir: "web", defaultPort: 4831
))
```

## Daemon cho app những gì

```swift
try space.llm(prompt: p, system: s, maxTokens: 4000)     // gọi model qua bridge
try space.llmDetailed(prompt: p)                         // text, model, finish, usage
try space.agent("làm việc này")                          // một lượt agent đầy đủ, có tool
try space.knowledgeSave("nhớ cái này", space: "proj")
try space.knowledgeRecall("một câu hỏi", space: "proj")
try space.getConfig("prefs")                             // cùng KV mà UI của app đọc/ghi
try space.sqlite("SELECT * FROM t WHERE a = ?", [1])
```

Ba chỗ dễ sai:

- **`llm` ném lỗi khi câu trả lời bị cắt.** `finish == "length"` nghĩa là model
  chạm trần `maxTokens` giữa chừng — nửa câu trả lời không phân biệt được với
  câu trả lời ngắn. Dùng `llmDetailed` để tự xử lý.
- **Bridge lỗi vẫn trả HTTP 200**, kèm `{"status":"error"}`. SDK biến nó thành
  `SenclawError`; nếu tự gọi `bridge`, phải kiểm tra field `status`.
- **Dùng `profile:` trên `llm`, đừng dùng `setActiveModel`.** Active model là
  **toàn cục** — agent và mọi app khác dùng chung.

## App tự làm model (LLM provider)

Cho app **trở thành một model**: model của nó xuất hiện trong cùng bộ chọn với
OpenAI/Anthropic, và các lượt agent định tuyến tới nó qua HTTP. Conform
`LlmProvider` rồi ghép hai route của nó vào routes của bạn.

```swift
struct Mlx: LlmProvider {
    func models() -> [ModelCard] {
        // vision là BẮT BUỘC — daemon dựa vào đó để gửi image block hay quay về
        // OCR; endpoint text-only sẽ 400 khi gặp ảnh. Đừng đoán.
        [ModelCard("gemma-4-e2b-it-4bit", contextLength: 128_000, maxOutputTokens: 8192, vision: true)]
    }
    func chat(_ req: ChatRequest, _ sink: ChunkSink) throws {
        sink.text("hello")
        sink.send(.reasoning("đang nghĩ…"))
        sink.send(.toolCall(id: "id", name: "get_time", arguments: "{}"))
        sink.send(.usage(promptTokens: 12, completionTokens: 3))   // nhiều nhất một lần, ở cuối
    }
}

let provider = Mlx()
try? publishModels(FileManager.default.currentDirectoryPath, provider.models())
try Serve(Config(routes: llmRoutes(provider).merging(myRoutes) { _, b in b }))
```

Manifest biến app thành provider — daemon nói **OpenAI** với nó, nên `adapt` là
`"openai"` và không cần adapter mới:

```json
"llm": { "autoRegister": true, "path": "/v1", "adapt": "openai", "displayName": "MLX" }
```

`llmRoutes` render đúng wire `chat.completion.chunk` mà parser OpenAI của daemon
mong đợi: mỗi `.toolCall` là một delta ở **index tăng dần** (index trùng sẽ dán
`get_weatherget_time` lại với nhau), `.usage` đi trên chunk riêng với `choices`
rỗng, stream **luôn** kết thúc bằng `data: [DONE]` kể cả khi lỗi, và
`publishModels` từ chối danh sách rỗng (ghi-rồi-đổi-tên) để một lần khởi động
hỏng không xoá cache tốt khỏi bộ chọn. Nạp trọng số **lười** trong `chat`, không
phải lúc khởi động — daemon chỉ cho 30 giây trước khi coi là khởi động hỏng.

## Access token của app

Daemon đúc một access token cho mỗi app đã cài, đặt vào env
`SENCLAW_TOKEN_ACCESS_APP`. Đó là **danh tính** của app: token buộc vào đúng một
app id, dùng cho id khác sẽ bị từ chối. **Chiều ra tự động** — `SpaceClient` đọc
token và gửi kèm mọi lượt gọi daemon. **Chiều vào tuỳ chọn** — bật
`requireAppToken` để chỉ daemon gọi được vào port của app:

```swift
try Serve(Config(requireAppToken: true, healthPath: "/api/status", authSkipPaths: ["/ws/*"]))
```

Hai thứ không bao giờ bị từ chối: thiếu token trong env (chạy tay `swift run`),
và các đường dẫn miễn trừ. Toàn bộ (kể cả `SENCLAW_APP_TOKEN_MODE=strict`):
[docs/space-app-api-token.md](https://github.com/NortonBen/SenClaw/blob/main/docs/space-app-api-token.md).

## SIGTERM — đọc trước khi viết app

App **session** bị dừng khi rảnh: daemon gửi `SIGTERM` cho process group rồi
`SIGKILL` sau hai giây. `Serve` cài handler, chạy `onShutdown` rồi ngừng nhận;
đừng chặn quá khoảng một giây rưỡi.

```swift
try Serve(Config(onShutdown: { db.close() }))
```

## Kiểm tra manifest

```bash
swift run senclaw-manifest senclaw-manifest.json
```

Bắt đúng lớp lỗi im lặng: `"mode": "backgroud"` (gõ sai → bị coi là `session`),
`network: "hosts"` mà `hosts` rỗng (= không có mạng), `autoRegister` mà thiếu
`path`, `idleTimeoutSecs` dưới sàn 15s, và `llm.adapt` mà daemon không định
tuyến. Hoặc từ Swift: `validateManifest(dict)`.

## Test

```bash
swift test
```
