# {{icon}} {{title_name}}

{{description}}

Space App cho SenClaw, viết bằng Python — một file, chỉ thư viện chuẩn. Sinh ra
bằng `senclaw create app --lang python`.

## Chạy thử

```bash
python3 main.py
```

Mở http://127.0.0.1:{{port}}. Kiểm tra nhanh:

```bash
curl -s http://127.0.0.1:{{port}}/api/status
```

## Đóng gói và cài

```bash
zip -r {{id}}-app.zip . -x '*/.venv/*' '*/__pycache__/*' '*.DS_Store'
```

Cài trong SenClaw: **Plugins → Space Apps → Install from zip**.

## Thêm thư viện ngoài

Daemon tạo **virtualenv tại `.venv`** trong thư mục app cho *mọi* app runner
`python`, và đặt nó lên đầu `PATH` — nên `python` ở đây không bao giờ là Python
hệ thống. Gói bạn `pip install` toàn cục **không** import được; phải khai trong
`requirements.txt`:

```bash
echo "httpx>=0.27" >> requirements.txt
```

Daemon cài vào đúng venv đó. Stamp băm theo *nội dung* file, nên update mà không
đổi gì thì không cài lại. Tool `{{snake_name}}_status` trả về đường dẫn venv để
kiểm chứng việc cô lập thật sự xảy ra.

## Cấu trúc

| file | việc |
|---|---|
| `senclaw-manifest.json` | app này là gì, chạy thế nào, MCP ở đâu |
| `main.py` | HTTP server, tool MCP, gọi ngược lên daemon |
| `web/index.html` | UI, phục vụ tĩnh |

## Tool MCP

Server tên `{{mcp_name}}`, nên tên đầy đủ agent gọi là:

- `mcp__{{mcp_name}}__{{snake_name}}_status`
- `mcp__{{mcp_name}}__{{snake_name}}_summarise`

Thêm tool: thêm một khoá vào `TOOLS` trong [`main.py`](main.py). **Mô tả tool là
thứ duy nhất model nhìn thấy khi quyết định gọi hay không** — viết rõ tool làm gì
và khi nào nên dùng.

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
SENCLAW_SPACE_APP_ID={{id}} PORT={{port}} SENCLAW_BASE_URL=http://127.0.0.1:18788 python3 main.py
```

Thiếu `SENCLAW_TOKEN_ACCESS_APP` thì các lệnh gọi lên daemon bị từ chối ở chế độ
`strict` (mặc định) — chạy app qua daemon để có token thật, hoặc tạm đổi
Settings → Space Apps → App token mode sang `warn` khi đang phát triển.
