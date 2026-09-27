# `senclaw-app-sdk-go`

Viết Space App bằng Go. Không phụ thuộc thư viện ngoài — chỉ standard library,
nên app không cần tải module trước lần build đầu, và `go build` chạy được trên
máy không nối mạng.

> Bản Node/TypeScript: [`../senclaw-app-sdk`](../senclaw-app-sdk) ·
> Python: [`../senclaw-app-sdk-python`](../senclaw-app-sdk-python) ·
> Rust: [`../../app-space-sdk`](../../app-space-sdk).
> Vòng đời app (background/session), `requires`, `sandbox`:
> [`docs/space-app-lifecycle.md`](../../docs/space-app-lifecycle.md).
> English: [README.md](README.md).

## Cài

```bash
go get github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go
```

```go
import senclaw "github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go"
```

## App mẫu

[`examples/space-app-go-demo/`](examples/space-app-go-demo) — một Space App
hoàn chỉnh trong một file: 2 MCP tool, một trang UI, health endpoint, xử lý
SIGTERM. Manifest của nó khai đủ những thứ quyết định daemon chạy app thế nào
(`runtime.mode`, `runtime.runner`, `requires`, `sandbox`).

```bash
cd examples/space-app-go-demo
SENCLAW_SPACE_APP_ID=go-demo PORT=4830 go run .

# hoặc cài vào daemon đang chạy
curl -X POST http://127.0.0.1:18788/api/space/apps/register-local \
  -H 'Content-Type: application/json' -d "{\"path\": \"$(pwd)\"}"
```

## Đọc trước: app Go KHÔNG có bước cài đặt

Daemon chỉ chạy `runtime.install` cho runner **node** và **python**
([`src/apps/prepare.rs`](../../src/apps/prepare.rs) trả về ngay với `binary` và
`shell`). App Go khai `"install": "go build -o app ."` thì lệnh đó bị bỏ qua im
lặng, và `start` trỏ tới một binary chưa ai build. `manifest.Validate` bắt lỗi
này; daemon thì không.

Hai dạng chạy được:

| | `start` | `runner` | Đánh đổi |
|---|---|---|---|
| **Build sẵn** (nên dùng) | `./my-app` | `binary` (suy ra từ `./`) | Khởi động vài mili-giây. Phải build và ship binary cho từng nền tảng. |
| **`go run`** (bản demo) | `go run .` | `shell` | Không cần build, nhưng phải khai `requires.bin: ["go"]`. Lần chạy đầu phải biên dịch, mà daemon chỉ cho app **30 giây** để trả lời health endpoint. |

Với app build sẵn thì build trước khi đăng ký, và cross-compile đúng nền tảng
người dùng chạy:

```bash
GOOS=darwin GOARCH=arm64 go build -o my-app . # rồi register-local
```

## App tối thiểu

`main.go`:

```go
package main

import (
	"context"
	"net/http"

	senclaw "github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go"
)

func main() {
	space := senclaw.MustNew() // đọc SENCLAW_SPACE_APP_ID + SENCLAW_BASE_URL
	mcp := senclaw.NewMCPServer("demo-mcp", "1.0.0")

	mcp.Tool("demo_summarise", "Tóm tắt một đoạn văn bản", senclaw.Schema{
		"type":       "object",
		"properties": senclaw.Schema{"text": senclaw.Schema{"type": "string"}},
		"required":   []string{"text"},
	}, func(ctx context.Context, args map[string]any) (any, error) {
		// App KHÔNG BAO GIỜ giữ API key của provider — mọi lời gọi model đi
		// qua daemon, dùng provider người dùng đã cấu hình.
		return space.LLM(ctx, senclaw.LLMRequest{
			Prompt:    "Tóm tắt trong 3 câu:\n\n" + senclaw.String(args, "text"),
			MaxTokens: 800,
		})
	})

	senclaw.Serve(senclaw.Config{
		Routes: map[string]http.Handler{
			"GET /api/status": senclaw.JSONHandler(func(*http.Request) (any, error) {
				return map[string]any{"ok": true}, nil
			}),
		},
		HealthPath:  "/api/status",
		MCPPath:     "/api/mcp/sse",
		MCP:         mcp,
		StaticDir:   "web",
		DefaultPort: 4830,
	})
}
```

`senclaw-manifest.json`:

```json
{
  "id": "demo-go",
  "name": "Demo Go",
  "description": "Space App viết bằng Go",
  "icon": "🐹",
  "runtime": {
    "kind": "server",
    "mode": "session",
    "runner": "binary",
    "start": "./demo-go",
    "healthPath": "/api/status",
    "port": 4830
  },
  "integration": { "type": "iframe", "url": "/" },
  "mcp": {
    "name": "demo-go-mcp",
    "transport": "http",
    "path": "/api/mcp/sse",
    "autoRegister": true
  }
}
```

Cài vào daemon đang chạy:

```bash
curl -X POST http://127.0.0.1:18788/api/space/apps/register-local \
  -H 'Content-Type: application/json' -d '{"path": "/duong/dan/toi/app"}'
```

## Dịch vụ của daemon

```go
space := senclaw.MustNew(senclaw.WithAppID("my-app"))

space.Capabilities(ctx)                              // daemon này làm được gì

text, err := space.LLM(ctx, senclaw.LLMRequest{Prompt: p, System: s, MaxTokens: 4000})
reply, err := space.LLMDetailed(ctx, req)            // text, model, finish, usage
out, err := space.Agent(ctx, "làm việc này")          // agent đủ tool, nhiều bước

space.KnowledgeSave(ctx, senclaw.Memory{Text: "nhớ cái này", Space: "proj"})
space.KnowledgeSearch(ctx, "hỏi gì đó", "proj", 10)  // hit thô
space.KnowledgeRecall(ctx, senclaw.RecallQuery{Query: "hỏi gì đó", Space: "proj"})

space.GetConfig(ctx, "prefs", &prefs)                // đúng KV mà UI của app dùng
space.SQLiteScan(ctx, &rows, "SELECT * FROM t WHERE a = ?", 1)

active, models, err := space.ListModels(ctx)
space.UsageReport(ctx, senclaw.Usage{Model: m, Provider: p, InputTokens: 100})
```

Ba chỗ dễ sập:

- **`LLM` báo lỗi khi câu trả lời bị cắt.** `Finish == "length"` nghĩa là model
  chạm trần `MaxTokens` giữa chừng, mà một đoạn cụt thì không phân biệt được
  với một câu trả lời ngắn. Muốn tự xử thì dùng `LLMDetailed`.
- **Bridge lỗi vẫn về HTTP 200**, kèm `{"status":"error"}` trong thân. SDK trả
  về error; nếu tự gọi bridge thì phải kiểm trường `status`, không thì provider
  chết sẽ đọc thành chuỗi rỗng.
- **Dùng `LLMRequest.Profile` chứ đừng `SetActiveModel`.** Model đang hoạt động
  là **toàn cục** — agent và mọi app khác dùng chung.

Error mang theo thông điệp và status của daemon: `senclaw.StatusOf(err)`, hoặc
`errors.As(err, &senclawErr)`.

## Dispatch

Cho daemon (`MCPDispatcher`) điều khiển app:

```go
import "github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go/dispatch"

type Store struct{ dispatch.Unleased } // Heartbeat + Reclaim no-op

func (s *Store) ClaimReady(ctx context.Context, c dispatch.Capacity) ([]dispatch.WorkItem, error) {
	// phải nguyên tử — một item phát hai lần là chạy hai lần
}
func (s *Store) Finalize(ctx context.Context, id string, o dispatch.Outcome) error { … }

senclaw.Serve(senclaw.Config{
	Routes: senclaw.MergeRoutes(dispatch.Routes(&Store{}, ""), myRoutes),
})
```

Tên trường là snake_case (`depends_on`, `timeout_secs`, `item_id`) vì engine
đọc bằng serde — viết camelCase là bị bỏ im lặng, và nó hiện ra dưới dạng một
phụ thuộc không bao giờ có hiệu lực chứ không phải một lỗi. `WorkItem` xuất
mảng rỗng thành `[]` cũng vì lý do đó: `Vec` của serde không nhận `null`.

## Những gì SDK lo hộ

| | |
|---|---|
| `BindHost()` | `127.0.0.1` trừ khi `SENCLAW_BIND_HOST` nói khác. App **không có xác thực riêng** — bind `0.0.0.0` là mở toàn bộ REST + MCP ra LAN |
| `Port()` | Cổng daemon giao qua `PORT` |
| `Serve(...)` | Health + static + REST + MCP trên một cổng, và **bắt SIGTERM** |
| `Handler(...)` | Cùng bộ định tuyến nhưng không listen — đưa thẳng cho `httptest.NewServer` |
| `space.LLM(...)` | Gọi model qua bridge; báo lỗi khi bị cắt ở `MaxTokens` thay vì trả về đoạn cụt |
| `space.SQLite(...)` | DB riêng của app, luôn tham số hoá |
| `space.GetConfig` / `SetConfig` | Cùng KV mà UI của app đọc/ghi — không phải file trong thư mục app (update sẽ đè) |
| `MCPServer` | JSON-RPC `initialize` / `tools/list` / `tools/call`, không cần MCP SDK; tool panic thành một câu thông báo chứ không giết app |
| Phục vụ static | Chặn path-traversal, kèm fallback `index.html` để router phía client chạy được |

## SIGTERM — đọc trước khi viết app

App **session** bị dừng khi rảnh: daemon gửi `SIGTERM` cho cả process group,
hai giây sau là `SIGKILL`. `Serve` đã cài handler đóng listener và gọi
`OnShutdown`; đừng chặn quá khoảng một giây rưỡi trong đó.

```go
senclaw.Serve(senclaw.Config{
	OnShutdown: func(ctx context.Context) error { return db.Close() },
})
```

## Kiểm tra manifest

```bash
go run github.com/NortonBen/SenClaw/senclaw-sdk/senclaw-app-sdk-go/cmd/senclaw-manifest senclaw-manifest.json
```

Bắt đúng loại lỗi im lặng: `"mode": "backgroud"` (sai chính tả → thành
`session`, app đáng lẽ luôn chạy thì lặng lẽ dừng), `network: "hosts"` mà danh
sách host rỗng (= app mất mạng hoàn toàn), `autoRegister` mà không có `path`,
`idleTimeoutSecs` dưới sàn 15 giây, và `install` trên runner không bao giờ chạy nó.

Hoặc gọi từ Go:

```go
if problems := manifest.Validate(m); len(problems) > 0 { … }
```

## Test

```bash
go test ./...
```

Chốt hai hợp đồng hỏng vô hình: các method JSON-RPC mà SenClaw thật sự gửi, và
đúng những khoá mà bridge với dispatch engine đọc.
