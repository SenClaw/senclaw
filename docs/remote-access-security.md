# Mở SenClaw ra LAN / cloud an toàn (bind host + API token)

Mặc định daemon **chỉ bind loopback** — Web UI (18788) và WS gateway (18789)
đều nghe trên `127.0.0.1`, máy khác không gọi tới được. Muốn mở ra LAN phải
opt-in tường minh, và khi đó mọi peer không phải loopback bắt buộc kèm token.

## Chọn chế độ: `SENCLAW_AUTH_MODE`

Việc **có đòi token hay không** là một công tắc ba trạng thái, không còn suy ra
cứng từ bind host:

| Mode | Đòi token khi nào | Dùng cho |
|---|---|---|
| `auto` (mặc định) | chỉ khi bind host không phải loopback, và chỉ với peer không phải loopback | laptop, bản cài desktop |
| `always` | **mọi** peer, kể cả loopback | cloud, Docker, sau reverse proxy |
| `off` | không bao giờ | đã có lớp xác thực phía trước |

**`always` không phải bản "hoang tưởng" của `auto`.** Triển khai cloud chuẩn là
nginx/Caddy kết thúc TLS ngay trên máy chạy daemon rồi proxy về
`127.0.0.1:18788` — khi đó **mọi khách từ Internet đều đến từ loopback**, và
`auto` miễn trừ đúng nhóm đó. `always` không cần tin `X-Forwarded-For`, nên
không có bề mặt giả mạo header và đúng cả khi proxy nằm cùng pod.

Giá trị lạ (gõ sai) rơi về **`auto`**, không bao giờ về `off` — cùng nguyên tắc
với `SENCLAW_APP_TOKEN_MODE`.

Đổi được lúc đang chạy, **không cần restart**: Settings → General → Access token
(web + desktop), hoặc `GET`/`PUT /api/auth/mode`. Lựa chọn lưu ở `router_state`
(`auth:mode`) và **đè** biến môi trường. Route này cố ý **nằm sau cổng kiểm
soát** — nếu để mở thì một client ẩn danh từ xa có thể tự tắt gate.

> Bật `always` từ một phiên loopback sẽ tự khoá chính phiên đó ở request kế
> tiếp — đúng thiết kế. Desktop app trên cùng máy vẫn chạy tiếp (nó tự đọc
> `~/.senclaw/api_token`); trình duyệt sẽ hiện TokenGate và cần nhập token.

### Caller nội bộ

Ở `always`, mọi thứ gọi ngược vào `/api/*` qua loopback cũng phải kèm token:
MCP subprocess (`space`, `patterns`, `ocr`), `llm_info` của Kanban, và LLM của
Space App — endpoint OpenAI của nó **chính là** một route daemon
(`/api/space/apps/<id>/proxy/v1`). Daemon phát token vào môi trường của chính
nó lúc khởi động (`SENCLAW_API_TOKEN`), nên tiến trình con thừa kế; caller
trong process gắn header qua `util::internal_auth::header_for`.

## Bật truy cập từ xa

```bash
SENCLAW_UI_BIND_HOST=0.0.0.0 senclaw
```

Trên desktop app, cùng công tắc đó nằm ở **Settings → General → Network
access**: chọn *Private* (`127.0.0.1`) hay *Public* (`0.0.0.0`). Lựa chọn ghi
vào prefs (`senclaw:bind-public`) và được supervisor truyền thành
`SENCLAW_UI_BIND_HOST` **lúc spawn daemon** — socket đã bind rồi thì không đổi
được, nên panel hiện nút *Restart daemon* khi daemon đang chạy còn dùng thiết
lập cũ, và báo riêng trường hợp daemon được "nhận nuôi" (khởi động từ terminal
— nó lấy bind host từ môi trường của chính nó, không phải từ đây). Chọn Public
sẽ hiện luôn cảnh báo kèm địa chỉ LAN và nhắc token nằm ở
`~/.senclaw/api_token`.

Khi bind host không phải loopback, daemon tự bật chế độ token:

- Token đọc từ `SENCLAW_API_TOKEN` (env), nếu không có thì dùng/tự sinh
  `~/.senclaw/api_token` (32 byte ngẫu nhiên, hex, chmod `0600`).
- Log khởi động in đường dẫn file token (không in giá trị).
- **Peer loopback luôn được miễn token** — desktop app bundled, Space App gọi
  ngược về daemon, tooling cùng máy chạy y như cũ, không cần cấu hình gì.

`SENCLAW_UI_BIND_HOST` cố ý **tách khỏi** `SENCLAW_BIND_HOST` của Space App:
app không có auth riêng nên không được kéo theo daemon ra LAN (và ngược lại).
Truy cập app từ xa đi qua proxy `/api/space/...` của daemon — đã nằm sau token.

## Client gửi token thế nào

| Kênh | Cách gửi |
|---|---|
| REST | `Authorization: Bearer <token>` hoặc `X-SenClaw-Token: <token>` |
| WS upgrade (18789, `/api/ws/terminal`) | `?token=<token>` hoặc cookie |
| Trình duyệt (iframe Space App, WS) | cookie `senclaw_token` — mint bằng `POST /api/auth/login {token}` (HttpOnly, SameSite=Lax) |

Hai endpoint mở (không cần token, chỉ trả boolean/login):
`GET /api/auth/status` → `{authRequired, authorized, mode, modeSource}` và
`POST /api/auth/login`. `GET /api/config` có thêm trường `authRequired`.
`GET`/`PUT /api/auth/mode` **có** gác token.

- **Web UI**: tự hiện màn hình nhập token khi `authRequired && !authorized`
  (`web/src/components/TokenGate.tsx`); mọi `fetch` `/api/*` cùng origin được
  patch để kèm `X-SenClaw-Token` (`web/src/lib/auth.ts`), 401 → khoá lại gate.
- **Desktop app**: thứ tự nguồn token — Settings → General → Connection (prefs)
  → `--dart-define=SENCLAW_API_TOKEN` → `~/.senclaw/api_token` (cùng máy).
  Gắn header trong `ApiClient` + các call multipart, `?token=` cho WS.

## Vá kèm trong cùng thay đổi

- **CORS**: bỏ `CorsLayer::permissive()` (ACAO `*`) — trước đây *bất kỳ trang
  web nào* user đang mở cũng fetch được `http://127.0.0.1:18788/api/llm-config`
  và đọc API key cleartext. Giờ chỉ origin loopback (Vite dev...) được phép
  cross-origin; UI chính là same-origin nên không cần CORS.
- **WS gateway**: chặn ngay tại HTTP upgrade cho cả 3 route (`/`, `/browser`,
  `/browser-mcp`) — check `connect` in-band không đủ vì dispatcher vẫn chạy
  handler cho socket chưa auth.

## Chạy trong Docker

Repo có sẵn `Dockerfile` + `docker-compose.yml`:

```bash
docker compose up -d --build
```

Image đặt sẵn `SENCLAW_UI_BIND_HOST=0.0.0.0` (namespace mạng riêng của
container — `-p` mới là thứ quyết định ai tới được) và
`SENCLAW_AUTH_MODE=always`. Đọc token đã sinh:

```bash
docker compose exec senclaw cat /home/senclaw/.senclaw/api_token
```

hoặc tự đặt trước qua `SENCLAW_API_TOKEN` trong compose.

Những chỗ dễ sai:

- **Phải mount volume `~/.senclaw`.** Không mount thì token (và cả DB) sinh lại
  mỗi lần restart, mọi lần đăng nhập đã lưu thành vô hiệu — nhìn y hệt lỗi login.
- **Healthcheck phải trỏ `/api/auth/status`.** Ở `always` mọi route khác trả
  401, container sẽ unhealthy vĩnh viễn.
- **Tuyệt đối không set `SENCLAW_BIND_HOST=0.0.0.0`.** Đó là biến của Space App
  — app không có auth riêng, set nhầm là phơi toàn bộ REST + MCP của chúng.
- **Publish cả 18788 lẫn 18789**: Web UI dial WS theo `window.location.hostname`.
  Sau reverse proxy thì phải proxy cả hai.
- **Linux container không có MLX/Metal**: không model local, không Whisper ASR,
  OCR không tăng tốc. Chat qua provider hosted thì y như bản native.
- Node + Python có sẵn trong image cho Space App dùng runner đó; bỏ đi được nếu
  không chạy app nào như vậy.

## Sau reverse proxy TLS

- `SENCLAW_AUTH_MODE=always` (bắt buộc — xem trên).
- Cookie phiên tự thêm `Secure` khi thấy `X-Forwarded-Proto: https`; ép bằng
  `SENCLAW_AUTH_COOKIE_SECURE=1`/`0`. Sai chiều nào cũng im lặng: `Secure` trên
  HTTP thường làm trình duyệt vứt cookie vừa mint, thiếu nó thì token đi qua
  downgrade.
- Proxy cả `/` (18788) lẫn WS (18789).

## Giới hạn đã biết

- LAN là HTTP thường — token đi plaintext trên mạng nội bộ. Qua Internet thì
  đặt sau reverse-proxy TLS.
- So sánh token dùng constant-time; token 256-bit nên không cần rate-limit.
- Ảnh `NetworkImage` trong desktop app chưa gắn header — chỉ ảnh hưởng cấu hình
  desktop trỏ tới daemon từ xa, không ảnh hưởng mặc định loopback.
- `always` **không** phải rào chắn với malware cùng máy: thứ gì đọc được
  `~/.senclaw/api_token` thì có token.

Test: `cargo test ui_server::auth` (Rust — middleware/token/cookie/CORS/mode),
`cargo test --test daemon_auth_guard` (guard chống tái phát, gồm cả Dockerfile),
`cargo test internal_auth`, `flutter test test/api_token_test.dart` (desktop).
