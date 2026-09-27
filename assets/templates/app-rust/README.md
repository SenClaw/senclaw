# {{icon}} {{title_name}}

{{description}}

Space App cho SenClaw, viết bằng Rust (axum). Sinh ra bằng `senclaw create app`.

## Chạy thử

```bash
cargo run
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
from zip**. Daemon giải nén, đọc `senclaw-manifest.json` và chạy
`runtime.start` (`./{{crate_name}}`) từ thư mục app.

## Cấu trúc

| file | việc |
|---|---|
| `senclaw-manifest.json` | app này là gì, chạy thế nào, MCP ở đâu |
| `src/main.rs` | HTTP server, route, bind host |
| `src/mcp.rs` | các tool agent gọi được |
| `src/space.rs` | gọi ngược lên daemon: model, config KV |
| `web/index.html` | UI, phục vụ tĩnh — **một trang**, không cần build, không có SPA fallback |

`ServeDir` trả 404 cho đường dẫn không có file thật. Nếu bạn thêm router phía
client vào `web/`, deep-link hay F5 ở `/settings` sẽ 404 — thêm fallback về
`index.html` cho path không có phần mở rộng (template Node và Python đã có sẵn).

## Tool MCP

Server tên `{{mcp_name}}`, nên tên đầy đủ agent gọi là:

- `mcp__{{mcp_name}}__{{snake_name}}_status`
- `mcp__{{mcp_name}}__{{snake_name}}_summarise`

Thêm tool: khai báo trong `tools()` và xử lý trong `call()` ở
[`src/mcp.rs`](src/mcp.rs). **Mô tả tool là thứ duy nhất model nhìn thấy khi
quyết định gọi hay không** — viết rõ tool làm gì và khi nào nên dùng.

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
SENCLAW_SPACE_APP_ID={{id}} PORT={{port}} SENCLAW_BASE_URL=http://127.0.0.1:18788 cargo run
```

Thiếu `SENCLAW_TOKEN_ACCESS_APP` thì các lệnh gọi lên daemon bị từ chối ở chế độ
`strict` (mặc định) — lấy token thật bằng cách chạy app qua daemon, hoặc đổi
Settings → Space Apps → App token mode sang `warn` khi đang phát triển.
