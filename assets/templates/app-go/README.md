# {{icon}} {{title_name}}

{{description}}

Space App cho SenClaw, viết bằng Go — một file, chỉ thư viện chuẩn. Sinh ra bằng
`senclaw create app --lang go`.

## Chạy thử

```bash
go run .
```

Mở http://127.0.0.1:{{port}}. Kiểm tra nhanh:

```bash
curl -s http://127.0.0.1:{{port}}/api/status
```

## Đóng gói và cài

```bash
./scripts/pack.sh
```

Tạo ra `{{id}}-app.zip`. Cài trong SenClaw: **Plugins → Space Apps → Install
from zip**.

> **App Go không có bước cài.** `runtime.install` chỉ chạy cho runner `node` và
> `python`; với `binary` và `shell` daemon bỏ qua hoàn toàn. Nghĩa là khai báo
> `"install": "go build ."` trong manifest sẽ bị **lặng lẽ bỏ qua** — binary phải
> có sẵn trước khi app được khởi chạy lần đầu, và đó là việc của `pack.sh`.
>
> Cách còn lại: để `start` là `go run .` (runner suy ra `shell`) và khai báo
> `requires.bin: ["go"]`. Chạy được, nhưng lần khởi động đầu phải compile xong
> trong ngân sách health-check 30 giây của daemon.

## Cấu trúc

| file | việc |
|---|---|
| `senclaw-manifest.json` | app này là gì, chạy thế nào, MCP ở đâu |
| `main.go` | HTTP server, tool MCP, gọi ngược lên daemon |
| `web/index.html` | UI, phục vụ tĩnh — **một trang**, không có SPA fallback |

`http.FileServer` trả 404 cho đường dẫn không có file thật. Nếu bạn thêm router
phía client vào `web/`, deep-link hay F5 ở `/settings` sẽ 404 — thêm fallback về
`index.html` cho path không có phần mở rộng (template Node và Python đã có sẵn).

## Tool MCP

Server tên `{{mcp_name}}`, nên tên đầy đủ agent gọi là:

- `mcp__{{mcp_name}}__{{snake_name}}_status`
- `mcp__{{mcp_name}}__{{snake_name}}_summarise`

Thêm tool: thêm một mục vào map `tools` trong [`main.go`](main.go). **Mô tả tool
là thứ duy nhất model nhìn thấy khi quyết định gọi hay không** — viết rõ tool làm
gì và khi nào nên dùng.

## Ba điều dễ sai

1. **Đừng bind `0.0.0.0`.** Space App không có xác thực riêng: ranh giới tin cậy
   là loopback, không phải bản thân app. Host đọc từ `SENCLAW_BIND_HOST`, mặc
   định `127.0.0.1`.
2. **`runtime.mode` viết sai là im lặng.** Giá trị không nhận ra rơi về
   `session`, nên một app cần chạy nền sẽ lặng lẽ dừng. Chỉ có `background` và
   `session`.
3. **UI phải gọi URL tương đối** (`api/status`, không phải
   `http://127.0.0.1:{{port}}/api/status`). Daemon proxy trang này, nên URL
   tuyệt đối hỏng ngay khi mở app qua daemon.

## Chạy tay như daemon chạy

```bash
SENCLAW_SPACE_APP_ID={{id}} PORT={{port}} SENCLAW_BASE_URL=http://127.0.0.1:18788 go run .
```

Thiếu `SENCLAW_TOKEN_ACCESS_APP` thì các lệnh gọi lên daemon bị từ chối ở chế độ
`strict` (mặc định) — chạy app qua daemon để có token thật, hoặc tạm đổi
Settings → Space Apps → App token mode sang `warn` khi đang phát triển.
