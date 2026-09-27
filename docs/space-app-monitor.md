# Theo dõi & debug một Space App

> Plugins → Space Apps → **Details & logs** trên app đó. Phần **Theo dõi tiến
> trình** hiện ngay đầu dialog, tự làm mới mỗi 3 giây. Có ở cả Web UI và app
> desktop, cùng một endpoint và cùng những con số.

Space App là một tiến trình do daemon khởi chạy rồi gần như quên đi. Khi nó dở
chứng thì câu hỏi luôn giống nhau: **nó có đang chạy không, chạy từ bao giờ, đã
bị khởi động lại mấy lần, ăn bao nhiêu CPU/RAM, đang nói chuyện với ai, và log
nói gì.** Mỗi câu trả lời trước đây nằm một chỗ khác nhau — nay gom vào một chỗ.

## Đọc gì ở dòng trạng thái

```
đang chạy   pid 67399   cổng 4740   đã chạy 1m 45s   1 lần khởi chạy   sandbox: seatbelt
health 200 · 0ms        [Khởi động lại] [Mở] [Mở thư mục]
```

- **đang chạy / chạy nhưng không trả lời / không chạy** — ba trạng thái khác
  nhau. "Được daemon theo dõi" không đồng nghĩa "làm việc được": app trả 500 hay
  treo vẫn là một tiến trình sống. Kết luận lấy từ health check thật (đường dẫn
  `runtime.healthPath`, mặc định `/`), kèm mã trả về và độ trễ.
- **số lần khởi chạy** — đếm từ lúc daemon bật. **Con số tự tăng đều là dấu hiệu
  crash loop**, thứ mà mọi màn hình khác đều hiển thị y hệt một app khoẻ mạnh:
  supervisor bật lại sau mỗi lần chết nên lúc nào bạn nhìn cũng thấy "đang chạy".
  Quá 3 lần thì panel nói thẳng ra và chỉ xuống log.
- **sandbox** — cơ chế đang thực sự áp dụng cho *tiến trình đang chạy*
  (`seatbelt` / `bubblewrap` / không hiện gì nếu chạy tự do), ghi lại lúc khởi
  chạy chứ không đọc lại từ cấu hình: cấu hình có thể đã bị sửa sau đó, còn cái
  đang chạy mới là cái cần báo cáo. Xem [docs/space-app-sandbox.md](space-app-sandbox.md).

## "Không chạy" thường là bình thường, không phải sự cố

Từ khi có [hai chế độ vòng đời](space-app-lifecycle.md), phần lớn app là
**`session`**: daemon khởi động chúng khi có người mở màn hình hoặc khi agent
gọi một tool MCP, rồi dừng lại sau `idleTimeoutSecs` giây không ai dùng (mặc
định 60s). Với những app đó, "không chạy" là trạng thái **nghỉ**, không phải
outage — và supervisor cố tình không hồi sinh chúng.

Payload `GET /api/space/apps/:id/runtime` vì thế có thêm khối `lifecycle`:

```jsonc
"lifecycle": {
  "mode": "session",          // "background" = thường trực, có supervisor
  "runner": "python",
  "idleTimeoutSecs": 60,      // 0 với app background
  "idleSecs": 12,             // đếm tới idleTimeoutSecs thì app bị dừng
  "stoppedByUser": false      // người dùng bấm Stop — supervisor sẽ không đụng vào
}
```

và `"requirements"` (null nếu app không khai `requires`) — cùng bài kiểm mà
launcher chạy trước khi spawn, nên một app **không chạy được vì máy thiếu
`ffmpeg`** hiện ra ở đây kèm câu phải làm gì, thay vì chỉ là một app "không chạy".

Bảng đội hình ở Plugins → Sandbox (`GET /api/space/apps/sandbox-overview`) cũng
có cột `mode` vì lý do đó: không có nó thì một nửa danh sách đọc như đang chết.

## "Đang chạy" và "do daemon khởi chạy" là hai chuyện khác nhau

`ensure_server_running` **không** khởi chạy lại một app đã khoẻ: nếu cổng cố định
của app đang trả lời thì daemon dùng luôn tiến trình đó (`[space-mcp] '<id>'
already serving on :<port>`). Đó là chuyện thường xuyên — mọi app sống sót qua
một lần daemon khởi động lại đều rơi vào diện này.

Hệ quả: launcher **không có bản ghi con** cho app đó. Bản đầu của màn theo dõi vì
thế báo `not running` cho một app đang phục vụ bình thường (gặp thật với
`deepwiki`, Activity Monitor thấy pid 18274 mà UI ghi không chạy).

Cách nhận ra bây giờ: nếu không có bản ghi con, daemon dò **cổng** mà manifest
biết (`runtime.port`, hoặc `runtime.url` daemon ghi lại sau lần chạy thành công),
tra một lượt `lsof` toàn máy để lấy pid đang giữ cổng đó, rồi đo CPU/RAM và uptime
từ `ps` như mọi app khác. Hiện ra là:

```
chạy (ngoài daemon)   pid 18274 · 1h 12m        không rõ   cần khởi động lại
```

- **`không rõ`** chứ không phải `seatbelt`: daemon này không dựng profile cho tiến
  trình đó nên **không thể** khẳng định nó đang bị nhốt. Cấu hình bật sandbox mà
  tiến trình là loại nhận nuôi thì gần như chắc chắn nó *không* bị nhốt — nên
  hàng hiện luôn `cần khởi động lại`.
- **Không có số lần khởi chạy**: bịa một con số cho tiến trình mình không khởi
  chạy thì vô nghĩa.

## Vì sao trước đây app nào cũng "ngoài daemon"

Ba lỗi nối nhau, đều đã vá:

1. **Daemon chỉ bắt SIGINT.** `run_daemon` chờ mỗi `ctrl_c()`, trong khi app
   desktop tắt daemon bằng `kill -TERM` rồi SIGKILL sau **800 ms**
   ([port_tools.dart](../desktop_app/lib/core/daemon/port_tools.dart)). Nghĩa là
   khối shutdown — nơi gọi `space_mcp_launcher.shutdown()` — **chưa từng chạy**
   theo cách người ta thật sự thoát app. Mọi Space App sống sót qua mỗi lần tắt.
2. **Shutdown quá chậm để kịp.** Nó giết từng app một, mỗi app chờ tối đa 2 giây:
   với vài chục app thì cần cả phút, trong khi chỉ có 800 ms. Giờ gửi SIGTERM cho
   **tất cả** rồi chờ **một lần** 300 ms, ai chưa chết thì SIGKILL — đo được
   ~300 ms cho toàn bộ.
3. **Daemon mới nhận nuôi xác cũ.** Thấy cổng cố định còn trả lời là dùng luôn,
   nên app chạy hàng tuần bằng mã cũ, từ thư mục cũ, **không có** sandbox nào.
   Giờ daemon **đòi lại cổng**: giết tiến trình đó rồi khởi chạy lại tử tế.

Đòi lại có kiểm chứng — chỉ giết khi **thư mục làm việc của tiến trình nằm trong
thư mục cài của app** (`lsof -d cwd`). Dev server của bạn tình cờ trùng cổng thì
daemon để yên, ghi log và nhận nuôi như cũ; nó không được phép trở thành thứ giết
tiến trình lạ mỗi lần khởi động.

Supervisor cũng học được điều tương tự: trước đây "cổng có trả lời" = khoẻ, nên
tiến trình lạ xuất hiện giữa chừng không bao giờ bị phát hiện. Giờ *cổng trả lời
mà không có bản ghi con* được coi là việc phải xử lý — thử đòi lại **một lần** cho
mỗi lần daemon chạy (đòi không được thì thôi, không thử lại mỗi nhịp).

Trạng thái `ngoài daemon` / `không rõ` vì vậy giờ là ngoại lệ hiếm, không còn là
mặc định sau mỗi lần khởi động lại.

## CPU / RAM

Đo theo **nhóm tiến trình** (`pgid`) chứ không theo pid: `npm start` sinh ra
`sh → npm → node`, và bộ nhớ đáng quan tâm nằm ở tiến trình con. Bảng liệt kê
từng tiến trình (pid, CPU %, RAM MB, thời gian chạy, lệnh) — đủ để thấy app nào
đẻ ra một tá tiến trình con hay tiến trình nào đang ngốn.

Số liệu lấy từ `ps` của máy, dùng chung bộ phân tích với trang Sandbox
([src/sandbox/monitor.rs](../src/sandbox/monitor.rs)).

## Mạng

Hai lớp, vì chúng trả lời hai câu khác nhau:

1. **Socket đang mở** (`lsof` theo pid): socket `LISTEN` chứng minh app thật sự
   đang phục vụ cổng của nó; các socket `ESTABLISHED` cho thấy nó đang nói
   chuyện với ai — kể cả loopback (daemon, app khác).
2. **Proxy allowlist**, khi app chạy sandbox ở chế độ "chỉ vài trang": số lượt
   cho qua / bị chặn và **những tên miền vừa bị chặn**. Đây thường là câu trả
   lời cho "app hỏng mà không hiểu vì sao" — nó đang cần một trang chưa khai.
   Thêm trang ở nút **Sandbox**.

Nếu máy không có `lsof`, phần socket trống kèm ghi chú, chứ không làm hỏng cả
panel. Chỉ đọc: hàm này **không bao giờ** được phép biến thành `lsof -t … | kill`
— đó chính là cách một sự cố trước đây giết luôn daemon.

## Truy cập để tự kiểm tra

Phần cuối panel là mọi thứ cần để **chạy lại app bằng tay trong terminal**:

| | |
|---|---|
| Thư mục | thư mục cài, bấm copy được |
| Lệnh chạy | đúng `runtime.start` daemon dùng |
| Biến môi trường | `PORT`, `SENCLAW_BASE_URL`, và `HTTPS_PROXY` nếu đang bị proxy |
| File log | đường dẫn `runtime.log` + kích thước |

Kèm nút **Mở** (mở URL app bằng trình duyệt hệ thống) và **Mở thư mục** (mở
Finder/Explorer ngay tại thư mục app — bản desktop).

Nội dung log vẫn ở mục **Logs** ngay dưới, tự tải lại mỗi 2 giây và có nút xoá.


## Xem cả đàn cùng lúc

Plugins → **Sandbox** có thêm card **Space Apps — sandbox từng app**: mỗi app
chạy server một dòng, tự làm mới 5 giây một lần —

| Cột | Nói gì |
|---|---|
| Sandbox | **cơ chế mà tiến trình đang chạy thật sự nhận được**, không phải cấu hình đã lưu |
| Đọc đĩa / Mạng | chế độ đang cấu hình, kèm số trang đã khai và số lượt proxy chặn |
| Tiến trình | đang chạy / không chạy, pid, thời gian chạy, số lần khởi chạy |
| CPU / RAM | đo theo nhóm tiến trình của app |

Cột đầu đáng giá nhất vì nó bắt được thứ không màn hình nào khác thấy: app **cấu
hình là có sandbox nhưng đang chạy không sandbox**, do profile chỉ cố định lúc
khởi chạy còn cấu hình thì sửa được sau. Gặp tình huống đó thì dòng hiện
`cần khởi động lại`, và nút khởi động lại nằm ngay cạnh.

Ba nút ở cuối mỗi dòng, theo thứ tự "xem → sửa → tác động":

- **nhịp tim** — mở hộp thoại theo dõi chi tiết của đúng app đó (đầy đủ bảng tiến
  trình, socket, đường dẫn để debug), không phải rời màn hình đi tìm ở Space Apps;
- **bình thí nghiệm** — mở hộp thoại Sandbox của app;
- **mũi tên vòng** — khởi động lại app.

Danh sách phân trang **10 app/trang** (47 app cài sẵn là con số bình thường, để
một mạch thì nó chôn hết các card khác trên màn hình).

**Sắp xếp** theo: trạng thái (mặc định), tên, sandbox bật/tắt, mạng, CPU/RAM, số
lần khởi chạy. Web bấm tiêu đề cột; desktop chọn ở ô "Sắp xếp" kèm nút đảo chiều.
Mặc định là **trạng thái chứ không phải tên**: với 47 app mà chỉ vài cái đang
chạy, danh sách A→Z mở ra toàn app đứng yên còn thứ đáng xem nằm ở trang ba. Đổi
kiểu sắp xếp thì quay về trang 1, vì số trang của thứ tự cũ không còn nghĩa gì.

API: `GET /api/space/apps/sandbox-overview` — một lần `ps` cho cả danh sách chứ
không phải mỗi app một lần.

## API

`GET /api/space/apps/:id/runtime` trả một ảnh chụp:

```jsonc
{
  "running": true,
  "launches": 1,
  "process": { "pid": 67399, "pgid": 67399, "port": 4740,
               "url": "http://127.0.0.1:4740", "uptimeMs": 105000,
               "isolation": "seatbelt" },
  "health":  { "url": "…/api/status", "ok": true, "status": 200, "ms": 0 },
  "resources": { "cpu": 0.0, "rssMb": 10.9, "processes": [ … ] },
  "network": { "connections": [ { "proto": "TCP", "local": "127.0.0.1:4740",
                                 "remote": null, "state": "LISTEN" } ],
               "proxy": { "port": 59876, "stats": { "allowed": 1, "denied": 2,
                          "recentDenied": ["wikipedia.org"] } } },
  "sandbox": { "enabled": true, "readMode": "open", "network": "hosts", "hosts": [] },
  "log": { "path": "…/.senclaw/runtime.log", "bytes": 14342 },
  "launch": { "cwd": "…", "command": "./ba", "env": [["PORT","4740"], …] }
}
```

Toàn bộ đều best-effort: thiếu `lsof`, tiến trình chết giữa lúc đo, health check
timeout — mỗi thứ thành một ghi chú trong payload chứ không làm request lỗi. Một
màn hình theo dõi mà trả 500 đúng lúc thứ nó theo dõi hỏng là lúc nó vô dụng nhất.

## Mã nguồn

| Việc | Chỗ |
|---|---|
| Endpoint + đọc socket | [src/gateway/ui_server/space_runtime.rs](../src/gateway/ui_server/space_runtime.rs) |
| pid/uptime/số lần khởi chạy | [src/gateway/ui_server/space_mcp.rs](../src/gateway/ui_server/space_mcp.rs) (`runtime_info`) |
| CPU/RAM theo nhóm tiến trình | [src/sandbox/monitor.rs](../src/sandbox/monitor.rs) (`stats_for_groups`) |
| Web UI (một app) | [web/src/components/space/AppRuntimePanel.tsx](../web/src/components/space/AppRuntimePanel.tsx) |
| Desktop UI (một app) | [desktop_app/lib/features/plugins/space_app_runtime_panel.dart](../desktop_app/lib/features/plugins/space_app_runtime_panel.dart) |
| Card cả đàn ở màn Sandbox | [web/src/components/plugins/SandboxAppsCard.tsx](../web/src/components/plugins/SandboxAppsCard.tsx) · [desktop_app/lib/features/plugins/sandbox_panel.dart](../desktop_app/lib/features/plugins/sandbox_panel.dart) (`_appsCard`) |

## Đọc thêm

- Cấu hình sandbox cho app: [docs/space-app-sandbox.md](space-app-sandbox.md)
- Hướng dẫn sandbox nói chung: [docs/sandbox-guide.md](sandbox-guide.md)
- Vòng đời tiến trình (SIGTERM, thu hồi cổng, supervisor):
  [docs/sandbox-app-design.md](sandbox-app-design.md)
