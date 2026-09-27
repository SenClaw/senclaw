# Vòng đời Space App: chạy ngầm hay chạy theo phiên

Trước bản này, **mọi** Space App đã cài đều được daemon khởi động lúc boot và
giữ sống đến khi tắt máy. Trên máy cài ~50 app, đó là ~50 tiến trình server
thường trực, gần hết đứng không.

Giờ mỗi app khai một trong hai **chế độ**, ở `runtime.mode`:

| | `background` | `session` (**mặc định**) |
|---|---|---|
| Khi daemon khởi động | Chạy ngay | Không chạy |
| Chạy khi nào | Luôn luôn | Khi mở app, hoặc khi agent gọi tool MCP của nó |
| Dừng khi nào | Chỉ khi daemon tắt / người dùng bấm Stop | Sau `idleTimeoutSecs` giây không ai dùng (mặc định 60s) |
| Supervisor hồi sinh khi chết | Có | Không — "không chạy" là trạng thái nghỉ bình thường |
| Dành cho | App **tự làm việc**: hứng tin nhắn, chạy lịch, giữ WebSocket cho extension | Còn lại: app là một màn hình người dùng mở ra, hoặc một bộ tool agent gọi |

Code: model ở [`src/apps/manifest.rs`](../src/apps/manifest.rs), vòng đời tiến
trình ở [`src/gateway/ui_server/space_mcp.rs`](../src/gateway/ui_server/space_mcp.rs).

> Sandbox từng app: [space-app-sandbox.md](space-app-sandbox.md) ·
> Màn theo dõi tiến trình: [space-app-monitor.md](space-app-monitor.md) ·
> SDK: [Python](../senclaw-sdk/senclaw-app-sdk-python) ·
> [Node](../senclaw-sdk/senclaw-app-sdk) · [Rust](../app-space-sdk)

---

## 1. Cơ chế: vì sao app đang tắt vẫn có tool trong roster

Đây là phần dễ sai nhất, và là lý do "chạy theo phiên" không phải chỉ là "đừng
khởi động app".

Nếu app đang tắt mà tool của nó **biến mất** khỏi roster của agent, sẽ không ai
gọi nó — và nó **không bao giờ** được khởi động. Vòng chết. Nên:

1. **URL của MCP trỏ vào daemon, không vào app.** App `session` được đăng ký MCP
   ở `http://127.0.0.1:18788/api/space/apps/<id>/proxy<mcp.path>`, chứ không phải
   `http://127.0.0.1:<cổng app><mcp.path>`. Proxy của daemon
   ([`space_apps_proxy`](../src/gateway/ui_server/space.rs)) **khởi động app rồi
   mới chuyển tiếp**. App `background` vẫn trỏ thẳng vào cổng của nó như cũ.
2. **Danh sách tool được cache trên đĩa.** Mỗi lần kết nối MCP thành công,
   daemon ghi `<app>/.senclaw/mcp-tools.json`. Lần boot sau, app `session` được
   đăng ký **offline** từ cache đó (`McpManager::add_or_update_offline`) — có
   tool trong roster, không có kết nối, không có tiến trình.
3. **`call_external_tool` tự kết nối khi cần.** Lần gọi tool đầu tiên thấy chưa
   kết nối thì nó connect → request đi qua proxy → proxy khởi động app →
   `tools/list` → gọi. Từ góc nhìn agent chỉ là một lời gọi hơi chậm.

Một cache rỗng **không bao giờ** ghi đè cache tốt: một lần connect hỏng mà xoá
cache sẽ làm app mất tool → không gọi được → không khởi động được.

App chưa từng chạy lần nào thì chưa có cache. Lần boot đó daemon khởi động nó
**một lần** để học danh sách tool, rồi reaper dừng lại sau một phút. Mỗi app chỉ
tốn đúng một lần như vậy trong đời.

### "Đang dùng" được đo ở đâu

Mọi đường dùng app đều đi qua đúng một handler — proxy. iframe UI, REST của app,
và (với app `session`) từng lời gọi tool MCP. Nên proxy là nơi gọi
`launcher.touch(app_id)`, và reaper đo từ dấu đó.

```
agent → mcp__<app>-mcp__<tool>
          → daemon proxy /api/space/apps/<id>/proxy/api/mcp/sse   ← touch()
              → (chưa chạy?) ensure_running: kiểm requires → prepare → spawn → chờ health
                  → app :<port>
```

---

## 2. Khai trong `senclaw-manifest.json`

```jsonc
{
  "runtime": {
    "kind": "server",
    "mode": "background",       // "background" | "session" (mặc định "session")
    "runner": "python",         // "binary" | "node" | "python" | "shell" — đoán được thì bỏ
    "start": "python main.py",
    "install": "pip install -r requirements.txt",  // chạy 1 lần sau cài/update
    "venv": true,               // mặc định true cho runner python
    "healthPath": "/api/status",
    "port": 4810,
    "idleTimeoutSecs": 60       // chỉ với session; tối thiểu 15
  }
}
```

**Bẫy:** giá trị `mode` viết sai (`"backgroud"`, `"always-on"`) **không báo lỗi
ở đâu cả** — nó rơi về `session`, và một app đáng lẽ hứng tin nhắn 24/7 sẽ lặng
lẽ tắt sau 60 giây. Chấp nhận thêm `always` / `daemon` / `resident` cho
`background`, và `on-demand` / `lazy` cho `session`.
[`tests/space_app_lifecycle_manifests.rs`](../tests/space_app_lifecycle_manifests.rs)
bắt lỗi này trên mọi app trong repo ở mỗi `cargo test`; app ngoài repo thì dùng
`python -m senclaw_space.manifest <file>` hoặc `validateManifest()` của SDK Node.

### App nào trong repo là `background`

21 app. Phần lớn có bằng chứng ngay trong source của chính nó:

| App | Vì sao |
|---|---|
| `ai-chat`, `crm` | Poll Telegram/Zalo/FB/TikTok lấy tin nhắn vào (`channels::spawn`) |
| `facebook-pro`, `moltbook`, `shopee` | Heartbeat luật / duyệt nháp (`engine::spawn_heartbeat`) |
| `lakehouse` | Bộ lập lịch ETL (`runner::spawn_poller`) |
| `news` | Tự lấy RSS + gom lại dòng sự kiện |
| `rule-engine` | Nối lại chain đang chạy + janitor |
| `sentinel` | Tick quét bảo mật định kỳ |
| `social`, `youtube`, `tiktok-activity`, `video-flow` | Giữ WebSocket cho extension Chrome dial vào (`extbridge::serve_ws`) |
| `tiktok-dl` | Worker tải, nhận lại job dở từ lần chạy trước |
| `autotest` | Chạy suite theo lịch mỗi 30s |
| `predict` | Lấy dữ liệu theo độ cũ + tự chấm sổ |
| `ai-office` | Nhận và chạy task hàng đợi của team |
| `discuss` | Đẩy các buổi thảo luận đang chạy, tick 700ms |
| `ssh-manager` | Quét dọn log mỗi 30s + giữ tunnel port-forward đang mở |
| `email`, `kaen` | **Người dùng yêu cầu** (07/08/2026) — không poll gì cả, để thường trú cho mở tức thì |

Còn lại (31 app) là `session`.

Test `an_app_that_works_on_its_own_at_startup_is_declared_background` quét
source từng app tìm dấu hiệu (`extbridge::serve_ws`, `spawn_heartbeat`,
`spawn_scheduler`, `spawn_poller`, `run_supervisor`, `spawn_janitor`) và bắt
buộc app đó phải khai `background` — nên một app mới thêm heartbeat mà quên đổi
manifest sẽ đỏ CI, không phải "phát hiện sau ba tuần vì lịch không chạy".

---

## 3. `requires` — máy phải có gì

```jsonc
"requires": {
  "node": ">=18",
  "python": ">=3.10",
  "bin": ["ffmpeg", "git"],
  "optionalBin": ["yt-dlp"],
  "env": ["SOME_TOKEN"],
  "os": ["macos", "linux"]
}
```

Kiểm **hai lần**: lúc cài (kết quả nằm trong response cài đặt, và trong
`GET /api/space/apps/:id/requirements`) và **trước mỗi lần khởi động**. Lý do
kiểm lại: kết quả lúc cài chỉ đúng cho cái máy của ngày hôm đó — người ta gỡ
Homebrew, `nvm` đổi node đang active. Thiếu thứ **bắt buộc** thì app **không
chạy**, và thông báo lỗi là một câu tiếng người:

```
'video-cloner' cannot start — missing: ffmpeg (missing).
`ffmpeg` is not on PATH. Install it (macOS: `brew install ffmpeg`, …).
```

thay vì `exit 127` trong file log.

Cú pháp version là loại thường gặp: `>=18`, `>=3.10 <4`, `^18`, `~3.10`, `18.x`,
`1.2.3`. So sánh **theo số**, không theo chuỗi — `3.9` **không** thoả `>=3.10`
(lỗi kinh điển khi so sánh text). Range không đọc được (`latest`) coi như thoả:
lỗi của chúng ta không được biến thành app không chạy được của người dùng.

`optionalBin` / `optionalEnv` chỉ báo cáo, không chặn.

---

## 4. `sandbox` — app tự khai mức giam của mình

Sandbox từng app vốn **mặc định tắt** và chỉ bật tay ở Plugins → Space Apps. Điều
đó đúng cho app tải về từ đâu đó, nhưng sai theo chiều ngược lại: app chỉ nói
chuyện với đúng một API và **biết** điều đó lại không có cách nào nói ra.

```jsonc
"sandbox": {
  "force": true,              // người dùng KHÔNG được tắt sandbox của app này
  "enabled": true,            // (force: true đã hàm ý enabled)
  "readMode": "strict",       // "open" | "strict" | "allowlist"
  "network": "hosts",         // "off" | "all" | "hosts"
  "hosts": ["api.openai.com"],
  "daemonApi": true,
  "loopback": [5432],
  "folders": [{ "path": "~/Movies", "readOnly": true }]
}
```

Hai luật giữ cho khai báo này không thành lỗ hổng
([`src/apps/sandbox_decl.rs`](../src/apps/sandbox_decl.rs)):

1. Khai báo **không** `force` chỉ áp dụng khi người dùng **chưa từng** lưu cài
   đặt cho app đó. Một bản update app không thể ghi đè lựa chọn của người dùng.
2. `force` được lưu lên config, và `PUT /api/space/apps/:id/sandbox` **từ chối**
   tắt sandbox của app bị force (409). Cờ `forced` không bao giờ đọc từ body
   request — chỉ từ manifest; nếu không, client nào cũng mở khoá được mọi app.
   Manifest bỏ `force` thì cờ cũng mất theo (giam vẫn giữ, chỉ khoá được gỡ).

`validate()` cũ vẫn chạy: cùng danh sách thư mục cấm (`/`, `$HOME`, kho
credential) và cùng danh sách host không bao giờ allowlist được (`localhost`,
`127.0.0.1`, `169.254.169.254`) áp cho khai báo y như cho ô người dùng gõ tay.

`~` được nở thành `$HOME` — manifest viết một lần, cài trên nhiều máy.

**Bẫy `network: "hosts"`:** không OS sandbox nào lọc được theo hostname
(Seatbelt chỉ nhận `*` hoặc `localhost`), nên nó là **proxy allowlist trên
loopback** và sandbox bị cắt sạch egress trực tiếp. Client nào bỏ qua
`HTTP_PROXY` sẽ với tới **không gì cả**, không phải tới tất cả — hỏng theo chiều
đóng. Thử app với nó bật trước khi ship khai báo.

---

## 5. Chạy Python và Node

`runtime.start` vẫn chạy qua shell như cũ, nên `npm start` / `python main.py`
xưa nay vẫn "chạy được". Cái thiếu là bước cài phụ thuộc — và không có nó thì
`Cannot find module` / `ModuleNotFoundError` đọc y như app hỏng.

[`src/apps/prepare.rs`](../src/apps/prepare.rs) chạy **một lần** sau mỗi lần
cài/update, trong thư mục app:

| `runner` | Làm gì |
|---|---|
| `binary`, `shell` | Không gì cả (đại đa số app trong repo) |
| `node` | `npm ci --omit=dev` nếu có `package-lock.json`, `pnpm`/`yarn` nếu có lockfile của chúng, ngược lại `npm install --omit=dev`. `runtime.install` ghi đè |
| `python` | Tạo `<app>/.venv`, rồi `pip install -r requirements.txt` (hoặc `runtime.install`) **vào venv đó**. Chạy app với `.venv/bin` đứng đầu `PATH` |

**Vì sao Python có venv mà Node không:** `npm install` ghi vào `node_modules`
trong thư mục app — cục bộ theo thiết kế. `pip install` ghi vào interpreter nào
nó tìm thấy, mà trên phần lớn máy là Python hệ thống của người dùng — cài pin
của một app vào đó không phải việc của chúng ta, và pin của một app sẽ lặng lẽ
thành pin của mọi app.

**Dấu vân tay theo nội dung, không theo mtime:** giải nén bản update viết lại
mtime của mọi file; stamp mà key theo mtime sẽ cài lại mỗi lần update. Stamp
(`<app>/.senclaw/prepare.stamp`) băm **nội dung** `package.json` / lockfile /
`requirements.txt` cộng với chính câu lệnh.

**Bước prepare chạy ngoài sandbox.** Cài phụ thuộc cần mạng và cần ghi vào thư
mục app; chạy nó bên trong sandbox của app (có thể khai `network: off`) sẽ hỏng
theo kiểu trông như sandbox bị lỗi. Cái được giam là **app**, lúc khởi động.
Câu lệnh install đến từ manifest, mà manifest đã đi qua scan bảo mật trước cài.

Log của bước này ghi vào đúng file log app (`<app>/.senclaw/runtime.log`) — là
file Web UI đang hiển thị sẵn.

---

## 6. API

| | |
|---|---|
| `POST /api/space/apps/:id/stop` | Dừng ngay. App `session`: làm sớm việc reaper sẽ làm. App `background`: là **override**, supervisor tôn trọng đến khi start lại |
| `POST /api/space/apps/:id/start` | Chạy + đăng ký lại MCP. Xoá cờ "người dùng đã dừng" |
| `POST /api/space/apps/:id/restart` | Như cũ: kill + đòi cổng + spawn lại |
| `GET /api/space/apps/status` | **Ảnh chụp cả đội, một request.** Mỗi app một dòng: `kind`, `mode`, `running`, `userStopped`, `port`, `launches`, `idleSecs`, `mcpName`. `?probe=1` hỏi thẳng cổng từng app (`ready`) — mặc định tắt vì tốn một round-trip mỗi app |
| `GET /api/space/apps/:id/requirements` | Chính bài kiểm launcher chạy, để xem trước khi app fail |
| `GET /api/space/apps/:id/runtime` | Thêm khối `lifecycle` (`mode`, `runner`, `idleSecs`, `idleTimeoutSecs`, `stoppedByUser`) và `requirements` |
| `GET /api/space/apps/sandbox-overview` | Thêm `mode` mỗi dòng và `config.forced` |
| `PUT /api/space/apps/:id/sandbox` | 409 nếu định tắt sandbox của app `force` |

`status` là **anh em ruột của `:id`** trong router, nên nó phải nằm trong danh
sách literal của `app_auth::split_app_path` — thiếu là nó bị hiểu thành một app
tên "status".

Web UI: Settings → Space Apps có nhãn **always on / on demand** và nút
**Start** / **Stop** cho từng app server.

### Quản lý app từ khung chat (MCP)

Cùng những việc đó, agent làm được qua `senclaw-space`:

| Tool | |
|---|---|
| `space_app_list` | App đã cài + trạng thái. `query` lọc theo id/tên, `status` = `all`/`running`/`stopped`, `probe` = hỏi cổng thật |
| `space_app_start` | Bật một app và chờ nó trả lời. Lỗi thì kèm đuôi log của app |
| `space_app_stop` | Tắt một app. Kết quả nói rõ "tắt" nghĩa là gì theo `mode` |
| `space_app_restart` | Kill + đòi cổng + spawn lại, chạy được cả khi app đang tắt |
| `space_app_mcp_list` | MCP server theo từng app: `mcpName`, trạng thái kết nối, số tool (kèm tên tool khi hỏi một app) |

Chúng là **client HTTP loopback gọi ngược về daemon**
([`src/mcp/space_apps.rs`](../src/mcp/space_apps.rs)), không phải đọc DB như
phần notes/calendar của cùng server đó — vì thứ chúng động vào (`SpaceMcpLauncher`)
là bản đồ tiến trình con nằm trong bộ nhớ của tiến trình daemon, không phải một
bảng trong SQLite. Daemon miễn token cho peer loopback nên bình thường không cần
credential; `SENCLAW_SPACE_API_URL` (do `space_mcp_config` đặt) trỏ tới daemon, và
`SENCLAW_API_TOKEN` được chuyển tiếp nếu có.

Ba điều đáng lưu ý khi agent dùng những tool này:

- **`running: false` của app `session` không phải lỗi** — mô tả tool nói thẳng
  điều đó, vì nếu không agent sẽ "sửa" một cái đang chạy đúng thiết kế.
- **Tắt một app `background` là dừng cả việc trực của nó** (poll kênh, chạy
  lịch). Mô tả `space_app_stop` yêu cầu hỏi người dùng trước.
- **`space_app_start` có thể mất hàng phút** ở lần đầu (`npm ci`, tạo venv).
  Quá 120 giây client trả về "daemon vẫn đang xử lý, kiểm tra lại bằng
  `space_app_list`" — **không** phải "thất bại", vì hết giờ ở phía client không
  huỷ việc phía daemon.

### Biến môi trường

| | Mặc định | |
|---|---|---|
| `SENCLAW_SPACE_SUPERVISE_SECS` | 20 | Nhịp supervisor. 0 = tắt. **Chỉ** giám sát app `background` |
| `SENCLAW_SPACE_IDLE_SWEEP_SECS` | 10 | Nhịp reaper. 0 = tắt → mọi app `session` thành thường trực sau lần dùng đầu |

---

## 7. Bẫy đã gặp

- **`mode` sai chính tả không báo lỗi.** Rơi về `session`. Chạy test hoặc
  validator của SDK.
- **App `session` bị dừng giữa chừng khi đang làm việc dài.** Reaper đo từ
  request cuối, không biết app đang bận. App có việc chạy nền dài (đang tải,
  đang chạy một buổi thảo luận) phải khai `background`, hoặc nâng
  `idleTimeoutSecs`.
- **Đừng đăng ký MCP của app `session` bằng URL cổng của chính nó.** Chạy được
  đúng đến lần idle đầu tiên rồi im. Daemon tự chọn URL proxy theo `mode` — chỉ
  cần đừng ghi đè bằng `mcp.url` tuyệt đối (với app `session`, `mcp.url` bị bỏ
  qua đúng vì lý do này).
- **`network: "hosts"` với `hosts` rỗng = app mất mạng hoàn toàn**, không phải
  "chưa giới hạn gì".
- **SIGTERM có 2 giây.** App bị dừng nhận SIGTERM cho cả process group, SIGKILL
  ~2s sau. App không bắt SIGTERM sẽ mất mọi thứ chưa flush. Cả hai SDK đều có
  helper (`serve(on_shutdown=...)` / `onShutdown()`).
- **`.venv` nằm trong thư mục app**, nên nó bị xoá khi gỡ app — đúng ý — và
  `readMode: "strict"` vẫn đọc được nó vì thư mục app luôn được cấp.
