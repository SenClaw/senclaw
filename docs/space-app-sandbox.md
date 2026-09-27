# Sandbox riêng cho từng Space App

> Plugins → Space Apps → nút **Sandbox** trên từng app (có cả ở Web UI và app
> desktop). Trả lời đúng ba câu: app này **có chạy trong sandbox không**, được
> **thư mục nào**, và ra **mạng tới đâu** — toàn bộ, không có, hay chỉ vài trang.

Trước tính năng này, một Space App chạy như một tiến trình bình thường của bạn.
Đo trên máy thật (app `apps/test-manager`, Node + Express):

| Phép đo | App không sandbox |
|---|---|
| Đọc `~/.ssh/id_ed25519` | **được** |
| Đọc `~/Documents`, `~/Projects` | **được** |
| Đọc `~/.senclaw` (DB daemon, token, dữ liệu app khác) | **được** |
| Ghi vào `$HOME` | **được** |
| Gọi mọi cổng loopback (API daemon, cổng WS, mọi app khác) | **được** |
| Ra internet | **không giới hạn** |

Đó không phải lỗi — Space App vốn là tiến trình do bạn cài. Tính năng này cho
bạn quyền thu hẹp lại, theo từng app.

## Ba công tắc

### 1. Bật/tắt sandbox (`enabled`)

Mặc định **tắt** cho mọi app. Bật lên = app chỉ còn **ghi** được vào thư mục của
chính nó và thư mục dữ liệu của nó (đọc và mạng vẫn nguyên) — bước đầu tiên rẻ
nhất, gần như không app nào hỏng vì nó.

> **App có thể tự khai mức giam của nó** trong `senclaw-manifest.json`, và
> `"force": true` khiến ô này không tắt được (`PUT .../sandbox` trả 409). Khai
> báo **không** `force` chỉ áp dụng khi người dùng chưa từng lưu cài đặt cho app
> đó — một bản update app không ghi đè được lựa chọn của bạn. Chi tiết + luật:
> [space-app-lifecycle.md § sandbox](space-app-lifecycle.md#4-sandbox--app-tự-khai-mức-giam-của-mình).

Được cấp tự động, đọc+ghi:

- thư mục cài của app (`<workspace>/space-apps/<id>`)
- mọi cách viết thư mục dữ liệu mà các app trong repo này từng dùng:
  `~/.senclaw/apps/<id>`, `~/.senclaw/space-apps/<id>`,
  `~/.senclaw/space-apps-data/<id>`, `~/.senclaw/space-app-data/<id>`,
  `~/.senclaw/<id>`
- một thư mục tạm riêng: `~/.senclaw/sandbox/app-tmp/<id>` (`TMPDIR` trỏ vào đây;
  `TMPDIR` mặc định của macOS nằm trong `/private/var/folders`, nơi profile
  **cấm ghi** vì đó là chỗ chứa dữ liệu của các ứng dụng khác)

### 2. Thư mục (`readMode` + `folders`)

- **`open`** (mặc định) — đọc được mọi thứ **trừ** các kho khoá (`~/.ssh`,
  `~/.aws`, `~/.gnupg`, Keychains…) và **trừ `~/.senclaw`**, tức là app không đọc
  được DB của daemon, token, hay dữ liệu của app khác.
- **`strict`** — chỉ còn thư mục của chính app + thư mục được cấp + system root.
  Toàn bộ `$HOME` còn lại biến mất khỏi tầm đọc.

Thêm thư mục bằng nút chọn thư mục (đọc+ghi hoặc chỉ đọc). Danh sách chặn giống
hệt của mount sandbox: không cấp được `/`, `$HOME`, hay kho khoá.

### 3. Mạng (`network`)

| Chế độ | Nghĩa |
|---|---|
| `all` (mặc định) | Ra internet tự do — như app ngoài sandbox. Loopback **vẫn đóng** trừ những cổng được khai. |
| `hosts` | Chỉ các tên miền trong danh sách. `*.example.com` phủ cả subdomain lẫn apex. |
| `off` | Không ra được đâu cả. App vẫn phục vụ cổng của nó bình thường. |

Cộng với, ở mục "Máy này":

- **Gọi API SenClaw** (`daemonApi`, mặc định bật) — cần cho **AI bridge**
  (`SENCLAW_BASE_URL/api/space/apps/<id>/bridge`), thứ gần như mọi app dùng để
  làm phần thông minh. Đây cũng là API local **không xác thực** của SenClaw: tắt
  đi cho app không cần AI.
- **Cổng local khác** (`loopback`) — DB, app khác… Ngoài danh sách này, loopback
  đóng hoàn toàn.

## "Chỉ vài trang" được cưỡng chế bằng cách nào

**Không OS sandbox nào ở đây lọc được theo tên miền.** Seatbelt chỉ nhận `*` hoặc
`localhost` làm remote host (đo thật: mọi thứ khác là lỗi cú pháp), bubblewrap
không có khái niệm host. Nên cơ chế đảo ngược lại:

1. Sandbox **không có đường ra trực tiếp nào** — không `connect` port, không cả
   resolver DNS.
2. Nó được đúng **một cổng loopback**: proxy allowlist của SenClaw
   ([src/sandbox/proxy.rs](../src/sandbox/proxy.rs)).
3. `HTTP_PROXY` / `HTTPS_PROXY` / `NODE_USE_ENV_PROXY` trỏ vào proxy đó.

Client tôn trọng biến môi trường proxy (curl, reqwest, axios, undici với
`NODE_USE_ENV_PROXY`) đi tới được các trang đã khai. Client **phớt lờ** proxy thì
không tới được **gì cả**, vì kết nối trực tiếp của nó bị chính sandbox chặn.
Hỏng theo kiểu **đóng**, không phải mở.

Proxy chỉ kiểm **đích đến**, không mở gói: `CONNECT` được tunnel sau khi kiểm tên
miền, nên TLS vẫn đầu-cuối và SenClaw không nhìn thấy nội dung.

Ba chốt chặn đáng nói:

- **Không bao giờ là cầu về máy này.** Sau khi phân giải tên, mọi địa chỉ
  loopback / link-local (gồm endpoint metadata `169.254.169.254`) đều bị từ chối,
  và proxy nối tới **đúng địa chỉ vừa kiểm** — nên DNS rebinding cũng không lách
  được. Danh sách trang cũng từ chối ngay khi lưu nếu bạn gõ `localhost`,
  `127.0.0.1`, `[::1]`, hay IP metadata.
- **Chỉ cổng web** (80/443/8080/8443): tunnel tới cổng tuỳ ý trên một host được
  phép sẽ chở được cả SSH hay giao thức DB.
- **Không phân biệt được virtual host trên HTTP thường**: hai trang chung một IP
  thì client có thể đổi header `Host`. Dùng HTTPS (mặc định) — SNI và chứng chỉ
  ràng tên miền lại.

Danh sách trang bị chặn hiện ngay trong dialog ("App đang cần: `+ x.com`"), bấm
một cái là thêm vào allowlist. Sửa danh sách có hiệu lực **ngay** với app đang
chạy; mọi thay đổi khác cần khởi động lại app (profile cố định lúc khởi chạy).

## Máy nào cưỡng chế được tới đâu

| | Thư mục | Mạng (off / chỉ vài trang) |
|---|---|---|
| macOS (Seatbelt) | có | **có** |
| Linux (bubblewrap) | có | **không** |
| Windows | không | không |

Trên Linux, app phục vụ một cổng thì **không thể** có network namespace riêng:
`--unshare-net` cắt luôn đường daemon gọi vào cổng app — đó không phải "cách ly",
đó là "hỏng". Nên app dùng chung namespace của máy và có thể lách proxy. Luật thư
mục thì thật. Windows dùng AppContainer qua pipe, không bọc được tiến trình
server dài hạn.

Dialog **nói thẳng điều này trước khi bạn bật**, không phải sau. Log runtime của
app cũng ghi một dòng mỗi lần khởi chạy:

```
sandbox: seatbelt — network via allowlist proxy on 127.0.0.1:59876, 6 folder(s) granted
```

## Đã đo được gì (app thật, `apps/test-manager`)

| Phép đo | Tắt | Bật, `open` + 1 trang | Bật, `strict` + 1 trang | Bật, mạng `off` |
|---|---|---|---|---|
| Đọc `~/.ssh/id_ed25519` | được | **không** | **không** | — |
| Đọc `~/Documents` | được | được | **không** | — |
| Đọc `~/.senclaw` (DB daemon) | được | **không** | **không** | — |
| Đọc/ghi thư mục dữ liệu của nó | được | được | được | — |
| Ghi `$HOME` | được | **không** | **không** | — |
| Ghi `/Users/<bạn>` (home thật) | được | **không** | **không** | — |
| `example.com` (đã khai) | 200 | 200 | 200 | **hỏng** |
| `wikipedia.org` (không khai) | 301 | **hỏng** | **hỏng** | **hỏng** |
| API daemon (AI bridge) | 200 | 200 | 200 | **hỏng** (khi bỏ tick) |
| Cổng WS 18991 của daemon | 400 | **hỏng** | **hỏng** | **hỏng** |
| **App vẫn phục vụ UI** | 200 | **200** | **200** | **200** |

## Bẫy đã gặp thật

1. **`strict` + runtime cài trong `$HOME` = app không khởi động.** Đo được:
   `EPERM … /Users/u/.nvm/versions/node/v24.13.1/lib/node_modules/npm/bin/npm-cli.js`.
   Node cài bằng nvm nằm dưới `$HOME`, mà `strict` bỏ đúng chỗ đó. Đã vá: ở chế
   độ đọc bị nhốt, mọi mục `PATH` **nằm ngoài system root** được cấp
   **chỉ đọc** ở mức *thư mục cài* (không chỉ `bin` — `npm-cli.js` nằm ở
   `../lib/node_modules`). Suy ra từ `PATH`, không phải danh sách tên
   nvm/volta/pyenv cứng — danh sách đó sẽ mục. Trần 16 mục, vượt thì ghi cảnh báo
   (một máy thật đã chạm trần 8 vì editor và model runner đứng trước nvm).
   Kho khoá bị loại kể cả khi nó nằm trên `PATH`, và xét theo `$HOME` được truyền
   vào chứ không phải `$HOME` của tiến trình.
2. **Cấp thư mục con thôi thì chưa đủ — phải qua được thư mục cha.** Đo trên app
   `ba`: SQLite chết `SQLITE_CANTOPEN: unable to open database file
   /Users/…/.senclaw/space-app-data/ba/app.sqlite` **dù đúng thư mục đó đã được
   cấp cả đọc lẫn ghi**, vì `~/.senclaw` phía trên bị cấm đọc và mở một file thì
   phải phân giải mọi thành phần đường dẫn. Lạ ở chỗ `ls` và `sqlite3` CLI trên
   cùng profile lại chạy được — nên lỗi rất dễ bị đổ nhầm cho app.
   Đã vá: mọi thư mục **tổ tiên nằm trong cây bị cấm** của một đường dẫn được cấp
   sẽ được trả lại **đúng quyền metadata** (`(allow file-read-metadata (literal
   …))`), không phải nội dung — DB của daemon, token, dữ liệu app khác vẫn tối.
   Nhánh `strict` không dính vì nó vốn đã `(allow file-read-metadata)`.
3. **`npm start` gọi mạng.** Với `hosts`, proxy chặn `registry.npmjs.org` và ghi
   lại — chính là thứ dialog hiển thị. App vẫn chạy; nếu app của bạn thật sự cần
   registry lúc khởi động thì phải khai.
4. **Danh sách cấm đọc kho khoá tính theo `$HOME` của daemon.** Chạy daemon với
   `HOME` khác (kiểm thử, sandbox lồng nhau) thì `~/.ssh` của user thật không nằm
   trong danh sách cấm của chế độ `open`. `strict` không bị ảnh hưởng.
5. **Profile của app không nằm trong vùng app ghi được.** Khác với đường `exec`
   (profile nằm trong sandbox và bị ghi đè trước mỗi lần chạy), app sống lâu nên
   profile để ở `~/.senclaw/sandbox/app-profiles/<id>.sb` — ngoài tầm ghi của app.
6. **Đường dẫn không được ánh xạ lại.** App tự tính thư mục dữ liệu từ `$HOME` lúc
   khởi động, nên mọi thứ được cấp đều giữ **đường dẫn thật** (macOS: rule theo
   path; Linux: bind `source == destination`). Đường dẫn cấp thêm luôn được
   `canonicalize` — rule Seatbelt trên `/var/x` không cấp gì cả vì đường thật là
   `/private/var/x`, và cái sai đó im lặng.

## Mã nguồn

| Việc | Chỗ |
|---|---|
| Cấu hình + kiểm tra hợp lệ | [src/sandbox/app_policy.rs](../src/sandbox/app_policy.rs) |
| Dựng lệnh khởi chạy (profile / bwrap / env) | [src/sandbox/app_launch.rs](../src/sandbox/app_launch.rs) |
| Proxy allowlist | [src/sandbox/proxy.rs](../src/sandbox/proxy.rs) |
| Bọc lúc spawn app | [src/gateway/ui_server/space_mcp.rs](../src/gateway/ui_server/space_mcp.rs) |
| REST `GET/PUT /api/space/apps/:id/sandbox` | [src/gateway/ui_server/space.rs](../src/gateway/ui_server/space.rs) |
| Web UI | [web/src/components/settings/SpaceAppSandboxModal.tsx](../web/src/components/settings/SpaceAppSandboxModal.tsx) |
| Desktop UI | [desktop_app/lib/features/plugins/space_app_sandbox_dialog.dart](../desktop_app/lib/features/plugins/space_app_sandbox_dialog.dart) |

Đọc thêm:

- Hướng dẫn dùng chung cả engine lẫn per-app: [docs/sandbox-guide.md](sandbox-guide.md)
- Theo dõi tiến trình một app: [docs/space-app-monitor.md](space-app-monitor.md)
- Thiết kế + vòng đời tiến trình: [docs/sandbox-app-design.md](sandbox-app-design.md)
- Vì sao loopback bị đóng kể cả khi bật mạng, và các phép đo gốc:
  [docs/sandbox-security-experiment.md](sandbox-security-experiment.md)
