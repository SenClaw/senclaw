# Chạy app thật trong sandbox — đo thực nghiệm, không suy đoán

Ngày đo: 2026-08-06 · macOS 25.5 (arm64) · backend `direct` / Seatbelt ·
daemon dựng riêng ở `HOME` tạm, cổng 18990 (không đụng daemon thật của máy).

Câu hỏi đặt ra: đưa vài **app thật** vào sandbox và kiểm chứng ba ràng buộc —
**chỉ vài thư mục**, **chỉ truy cập local**, **chỉ tới một web nhất định** — mà
app vẫn chạy đúng. Mọi dòng dưới đây là số đo, kịch bản nằm ở
`scripts/sandbox-experiment/` (chạy lại được).

> **Cập nhật 2026-08-06** — những gì đo ở đây đã thành tính năng: công thức
> "chỉ tới một web" nay là proxy allowlist đi kèm cho Space App
> ([docs/space-app-sandbox.md](space-app-sandbox.md)), và hai lỗ loopback/DNS
> trong báo cáo này đã vá trong engine. Bài này giữ nguyên như một bản ghi đo
> đạc — đọc để biết *vì sao* cơ chế có hình dạng như vậy.

## Tóm tắt

| # | Kết luận | Trạng thái |
|---|---|---|
| 1 | Giới hạn thư mục + cổng giữ đúng với app thật (Python `http.server`, Express+sqlite3) | ✅ 19/19 phép đo |
| 2 | `network: true` cho sandbox gọi **API không xác thực của chính daemon** trên loopback → đọc cấu hình và **tạo sandbox mới không giới hạn** | 🔴 lỗ hổng — **đã vá** |
| 3 | `connect:[443]` một mình **không** phân giải được tên miền trên macOS → app dùng hostname chết | 🟠 lỗi dùng được — **đã vá** |
| 4 | Seatbelt **không lọc được theo host** → "chỉ 1 web" không thể là luật OS | ⚪ giới hạn hệ điều hành — có cách vòng, đã chứng minh |
| 5 | Server nền khởi động bằng `cmd &` bị giết khi exec hết hạn; phải `( cmd & )` | ⚪ cách dùng — đã ghi vào skill |

## 1. App thật vẫn chạy đúng dưới ràng buộc

**Chủ thể A** — `python3 -m http.server` phục vụ một thư mục được mount.
**Chủ thể B** — `apps/test-manager` của chính repo: Node 24, Express 5, sqlite3
(native module), multer.

Cấu hình: `fsMode: strict`, mount **một** thư mục read-only, mở **một** cổng
`listen`, không mạng.

| Phép đo | Kỳ vọng | Đo được |
|---|---|---|
| App phục vụ được trên cổng đã mở (HTTP 200 từ máy thật) | cho phép | ✅ |
| Đọc được thư mục đã mount | cho phép | ✅ |
| Đọc thư mục **anh em** không mount | chặn | ✅ |
| Đọc `~/.ssh`, `~/.senclaw/oauth.json` | chặn | ✅ |
| Ghi vào project của người dùng | chặn | ✅ |
| Ghi trong workspace của chính nó | cho phép | ✅ |
| Ghi đè mount read-only | chặn | ✅ |
| Bind cổng **không** nằm trong `listen` | chặn | ✅ |
| Ra internet khi không có `connect` | chặn | ✅ |
| Node (runtime khác) cũng bị chặn y hệt | chặn | ✅ |

Kết luận: với Seatbelt, ba ràng buộc đường dẫn/ghi/cổng **đúng như mô tả**, và
một app Express + native module chạy bình thường bên trong.

### Bẫy 1 — mount app read-only thì app chết

`test-manager` mở sqlite tại `__dirname/test-manager.sqlite`. Mount thư mục app
read-only ⇒ không tạo được DB ⇒ app thoát ngay (đo: exit≠0).

**Cách đúng**: copy app vào **workspace** của sandbox (nơi được ghi), chỉ mount
*dữ liệu* read-only. Đo lại: app boot, tạo DB trong workspace, trả HTTP 200, và
vẫn không đọc/ghi được gì ngoài phạm vi.

### Bẫy 2 — server nền bị giết theo nhóm tiến trình

`exec` chờ **cả nhóm tiến trình**. Khởi động server bằng `node server.js &` giữ
exec chạy tới deadline rồi cả nhóm bị kill — server chết theo.

| Cách khởi động | exec trả về | Server sống |
|---|---|---|
| `cmd > log 2>&1 &` | ⏳ tới deadline | ❌ |
| `cmd < /dev/null > log 2>&1 &` | ⏳ tới deadline | ❌ |
| `( cmd < /dev/null > log 2>&1 & )` | ✅ 0.0s | ✅ |

## 2. 🔴 Lỗ hổng: loopback vô hiệu hoá toàn bộ cách ly (đã vá)

Với `network: true`, mã trong sandbox làm được — **đo thật trên daemon đang chạy**:

```
GET  /api/sandbox/status           → 200, đọc được cấu hình daemon
POST /api/sandbox/sandboxes        → 201, TẠO ĐƯỢC SANDBOX MỚI
```

Nghĩa là: chặn đọc `~/.senclaw/oauth.json` ở tầng file **không còn giá trị** khi
daemon sẵn sàng đọc hộ qua HTTP — API loopback không có xác thực vì ranh giới
tin cậy của nó là chính giao diện loopback. Tệ hơn, sandbox tự tạo được sandbox
thứ hai với `fsMode: open` và mount cả đĩa: thoát hoàn toàn.

**Vá**: `ports.rs` nay luôn phát luật cuối cùng
`(deny network-outbound (remote ip "localhost:*"))` — kể cả khi `network: true`
— và chỉ trả lại đúng các cổng liệt kê trong trường mới `loopback`. Đo lại: cả
4 phép leo thang đều `Operation not permitted`, bằng cả IP số lẫn tên
`localhost`. Có test e2e Rust dựng listener thật rồi kiểm chứng
(`this_machines_services_are_unreachable_even_with_the_network_on`).

## 3. 🟠 DNS trên macOS không đi qua cổng 53 (đã vá)

`connect:[53,443]` vẫn **không** fetch được `https://example.com` (đo: `000`).
Nguyên nhân: `getaddrinfo` không tự gửi UDP — nó hỏi **mDNSResponder** qua Unix
socket, thứ mà `(deny network*)` chặn mất.

**Vá**: khi sandbox có quyền ra ngoài (network bật, hoặc có `connect`), profile
thêm `(allow network-outbound (literal "/private/var/run/mDNSResponder"))`. Đo
lại: `connect:[443]` fetch `https://` thành công, mà `http://` cổng 80 vẫn bị
chặn — bộ lọc cổng còn nguyên.

Đánh đổi phải nói rõ: có resolver là có kênh rò rỉ (mã hoá dữ liệu vào tên
miền). Vì vậy chỉ cấp cho sandbox **đã** có quyền ra ngoài, không cấp cho
sandbox tắt mạng.

## 4. ⚪ "Chỉ tới một web" — không thể bằng luật OS, nhưng làm được

Seatbelt từ chối mọi rule theo host, đo trực tiếp:

```
(allow network-outbound (remote ip "93.184.216.34:443"))
  → sandbox-exec: host must be * or localhost in network address
(remote host "example.com")  → unbound variable: host
```

Nên `connect:[443]` nghĩa là **mọi host trên cổng 443**, không phải một web. Đo
xác nhận: cùng cấu hình vào được `example.com` **và** `www.wikipedia.org`.

**Cách làm được** (đã chứng minh end-to-end, 4/4): chạy một **proxy allowlist**
trên máy thật, sandbox **không có egress trực tiếp**, chỉ mở `loopback:[cổng
proxy]`:

| Phép đo | Kết quả |
|---|---|
| Qua proxy → `example.com` (trong allowlist) | ✅ 200 |
| Qua proxy → `www.wikipedia.org` | ✅ bị proxy từ chối |
| Bỏ qua proxy, gọi thẳng `example.com` | ✅ sandbox chặn |
| Tự phân giải tên miền | ✅ không phân giải được |

Điểm mấu chốt: app nào **lờ proxy đi** thì đâm vào tường sandbox — hỏng theo
kiểu **đóng**, không phải mở. Và vì không cấp `connect`, sandbox không có
resolver ⇒ không có kênh DNS tunnel.

## Công thức dùng lại

```jsonc
// Chạy app trong sandbox: phục vụ local, đọc 1 thư mục, gọi 1 web qua proxy
{
  "fsMode": "strict",           // + mount đúng thư mục dữ liệu, read-only
  "network": false,             // không egress trực tiếp
  "ports": {
    "listen":   [8080],         // app phục vụ, máy thật vào 127.0.0.1:8080
    "connect":  [],             // không ra thẳng internet
    "loopback": [8899]          // chỉ nói chuyện với proxy allowlist
  }
}
```

Kèm `HTTPS_PROXY=http://127.0.0.1:8899` trong `env` của sandbox.

## Còn hở

- **Docker / bubblewrap**: mở cổng là mất cách ly mạng; `loopback` không cưỡng
  chế được ở đó (docker còn có `host.docker.internal`). Trường `note` trong
  phản hồi nói thẳng điều này — đừng hứa với người dùng nhiều hơn thế.
- **Proxy allowlist chưa phải tính năng** của daemon; hiện là script trong thí
  nghiệm. Nếu muốn thành cơ chế chính thức thì đây là chỗ để làm tiếp.
- **API loopback của daemon vẫn không xác thực**. Sandbox hết đường tới nó,
  nhưng mọi tiến trình khác trên máy thì không — đó là vấn đề riêng, xem
  [[senclaw-self-exposure-findings]].
