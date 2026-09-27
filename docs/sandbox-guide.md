# Hướng dẫn sử dụng Sandbox

**Sandbox** cho phép chạy lệnh shell và mã nguồn **tách khỏi máy thật**: mã chạy
được, nhưng không đọc được tài liệu của bạn, không ghi ra ngoài thư mục của nó,
và không ra internet trừ khi bạn cho phép.

Quản lý tại **Plugins → Sandbox**. Agent trong chat dùng qua bộ tool
`mcp__senclaw-sandbox__sbx_*`.

Có **hai thứ** dùng chung cơ chế này, đừng lẫn:

| | Dùng cho | Đặt ở đâu |
|---|---|---|
| **Sandbox engine** (§1–§10) | mã do agent chạy: lệnh Bash, Python/Node, script hẹn giờ | Plugins → Sandbox |
| **Sandbox từng Space App** (§7) | tiến trình app cài sẵn, sống lâu, phục vụ một cổng | Plugins → Space Apps → nút **Sandbox** |

## 1. Máy của bạn cách ly bằng gì

Card **Available isolation** trên đầu trang cho biết:

| Nền tảng | Cách ly | Ghi chú |
|---|---|---|
| macOS | **Seatbelt** (`sandbox-exec`) | Chính xác nhất — lọc được cả file lẫn từng cổng mạng |
| Linux | **bubblewrap** | Tốt cho file; **không lọc được cổng ra** |
| Windows | **AppContainer + Job Object** | Có code nhưng **chưa kiểm chứng trên máy thật** |
| Mọi nền tảng | **Docker** | Cần Docker daemon đang chạy; thêm giới hạn CPU/RAM |

Nếu máy thiếu công cụ cách ly, trạng thái là **Degraded** — mã vẫn chạy nhưng
**không có rào hệ điều hành nào**. Trang này nói thẳng điều đó chứ không giả vờ.

## 2. Công tắc cưỡng chế (Security enforcement) — phần quan trọng nhất

Mặc định, sandbox chỉ là *công cụ có sẵn*. Các công tắc ở card **Security
enforcement** biến nó thành *bắt buộc* cho những đường chạy vốn đã tồn tại:

| Công tắc | Mặc định | Bật thì sao | Tắt thì sao |
|---|---|---|---|
| **Agent shell (Bash)** | TẮT | Mọi lệnh Bash của agent chạy trong sandbox, chỉ ghi được vào thư mục làm việc của cuộc chat | Chạy shell thẳng trên máy như trước |
| ↳ Network / Disk read / Local ports | bật / `open` / trống | Điều chỉnh riêng cho shell bị cưỡng chế | |
| **Run Python** | BẬT | Python chạy trong sandbox | **Từ chối chạy Python** — agent báo "switched off (Plugins → Sandbox)" |
| **Run Node.js** | BẬT | Node chạy trong sandbox | **Từ chối chạy Node.js** |
| ↳ Code network | TẮT | Cho REPL Python/Node ra mạng | |
| **Scheduler scripts** | TẮT | Task lịch loại `script` / `script-agent` chạy trong sandbox dùng-một-lần | Chạy script thẳng như trước |

Ba điều cần nhớ:

1. **Hai kiểu "tắt" khác nhau.** Tắt *Agent shell* hay *Scheduler scripts* =
   quay về hành vi cũ (chạy thẳng). Tắt *Run Python/Node* = **từ chối hẳn**,
   không có đường chạy thay thế.
2. **Bật mà máy không cách ly được thì lệnh báo lỗi**, không lặng lẽ chạy thẳng.
   Đó là chủ ý: một công tắc an ninh tự nhả khi gặp khó thì vô nghĩa.
3. **Bật "Agent shell" là thay đổi lớn nhất bạn có thể làm ở đây** — nó chặn
   agent ghi ra ngoài thư mục dự án đang mở. Nếu workflow của bạn cần agent
   chạm file ngoài đó, nó sẽ hỏng; hãy bật khi bạn thực sự muốn giới hạn ấy.

## 3. Sandbox đọc được gì trên đĩa

Ba mức, đặt mặc định ở card **Defaults for new sandboxes**, đổi riêng từng
sandbox bằng tool `sbx_fs_mode`:

| Mức | Đọc được |
|---|---|
| **strict** (mặc định) | thư mục của sandbox + thư mục bạn gắn vào + thư viện hệ thống |
| **allowlist** | như trên, cộng các thư mục bạn khai ở card **Allowlist** |
| **open** | cả đĩa, trừ `~/.ssh`, `~/.aws`, Keychain, `~/.senclaw` |

**Ghi thì luôn bị nhốt** trong thư mục sandbox, ở cả ba mức.

Thư viện hệ thống (`/usr`, `/System`, `/opt/homebrew`…) luôn đọc được kể cả ở
`strict` — bỏ chúng ra thì Python không khởi động nổi, chứ không phải "cách ly
chặt hơn".

⚠️ Riêng shell của agent (khi bật cưỡng chế) mặc định dùng mức **`open`**, không
phải `strict` — đổi ở ô *Disk read* ngay cạnh công tắc đó.

## 4. Mạng và cổng

Công tắc mạng là thô: bật hoặc tắt. Muốn chính xác hơn, dùng `sbx_ports` với ba
danh sách:

- **`listen: [8000]`** — sandbox được phục vụ trên cổng 8000, bạn xem được ở
  `http://127.0.0.1:8000`. Đây là cách chạy app của người khác trong sandbox rồi
  mở bằng trình duyệt. Cổng phải ≥ 1024.
- **`connect: [443]`** — cổng **từ xa** duy nhất nó được gọi ra.
- **`loopback: [8899]`** — dịch vụ **trên chính máy này** nó được gọi. Mặc định
  trống = không dịch vụ nào.

Không cần bật `network` — luật cổng chính là toàn bộ quyền: app phục vụ trên
8000 không đồng thời được quyền gọi về nhà.

**Ba điều dễ hiểu sai:**

- **`connect` tính theo cổng, không theo website.** `connect: [443]` nghĩa là
  *mọi* trang trên 443, không phải một trang. Sandbox của macOS không diễn đạt
  được "chỉ trang này" — xem §5.
- **Dịch vụ trên máy bạn luôn bị chặn, kể cả khi bật mạng.** Cố ý: API của
  SenClaw trên loopback không có mật khẩu, nên sandbox gọi được nó thì chỉ cần
  nhờ daemon đọc hộ file mà nó bị cấm đọc — và tự tạo cho mình một sandbox mới
  không giới hạn. Điều này **đã được chứng minh trên daemon thật** trước khi có
  luật chặn. Cần một dịch vụ local thì khai đúng cổng đó vào `loopback`.
- **Trên Linux và Docker, mở một cổng lắng nghe là sandbox có mạng** — hai nền
  đó không lọc được chiều ra. Câu trả lời của tool nói rõ điều này ở trường
  `note`; đừng bỏ qua.

## 5. Giới hạn chỉ vào MỘT website

Luật cổng không làm được (xem trên).

**Với Space App: đã có sẵn.** Chọn *Chỉ các trang này* trong hộp thoại Sandbox
của app rồi khai tên miền — SenClaw tự dựng proxy allowlist, xem §7.

**Với sandbox của agent (`sbx_*`)**: tự dựng theo công thức đã kiểm chứng dưới
đây.

1. Chạy một **HTTP proxy có allowlist** bên ngoài sandbox — nó quyết định tên
   miền nào được đi.
2. Đặt sandbox `connect: []` (không có đường ra trực tiếp) và
   `loopback: [<cổng proxy>]`.
3. Cho sandbox biến môi trường `HTTPS_PROXY=http://127.0.0.1:<cổng proxy>`.

App nào lờ proxy thì đâm tường — hỏng theo chiều đóng. Và vì không có `connect`
nên sandbox cũng không có bộ phân giải tên miền, khoá luôn đường tuồn dữ liệu
qua DNS.

## 6. Chạy một app thật trong sandbox (thủ công)

> Nếu là **Space App đã cài**, đừng làm thủ công — dùng §7, nó lo hết vòng đời.
> Phần này dành cho khi bạn muốn nhét một app *bất kỳ* vào một phiên sandbox.

Hai điều sẽ làm mất thời gian nếu không biết trước (cả hai đều đo được):

- **Khởi động server nền bằng `( cmd < /dev/null > log 2>&1 & )`**, đừng dùng
  `cmd &` trần. `&` trần giữ lệnh chạy tới hết hạn rồi bị giết cả nhóm tiến
  trình — server chết theo.
- **Đừng mount thư mục app ở chế độ chỉ-đọc rồi mong app chạy.** Thứ gì ghi cạnh
  mã của nó (SQLite, lock, cache) sẽ chết. Copy app vào workspace ghi được của
  sandbox, chỉ mount *dữ liệu* ở chế độ chỉ-đọc.

## 7. Sandbox cho từng Space App

Space App là tiến trình cài sẵn, chạy lâu dài, phục vụ một cổng — nên nó có hộp
thoại riêng: **Plugins → Space Apps → nút Sandbox** (hoặc bấm *Cấu hình* ở card
Space Apps ngay trong trang Sandbox). Ba câu hỏi:

1. **Có chạy trong sandbox không** — bật lên là app chỉ còn *ghi* được vào thư
   mục của chính nó và thư mục dữ liệu của nó. Bước rẻ nhất, gần như không app
   nào hỏng vì nó.
2. **Thư mục** — `open` (mọi thứ trừ kho khoá và trừ `~/.senclaw`) hoặc `strict`
   (chỉ thư mục của nó + thư mục bạn cấp), cộng danh sách thư mục cấp thêm.
3. **Mạng** — toàn bộ / chỉ vài trang / không có gì, cộng ô cho phép gọi API
   SenClaw (cần cho AI bridge) và các cổng local khác.

Khác với sandbox của agent ở ba điểm đáng nhớ: **đường dẫn giữ nguyên** (app tự
tính thư mục dữ liệu từ `$HOME`), **cấu hình chỉ áp dụng lúc khởi chạy** (đổi
xong phải khởi động lại app), và **"chỉ vài trang" đi kèm proxy** chứ không phải
tự dựng.

Chi tiết + bảng đo trước/sau: [docs/space-app-sandbox.md](space-app-sandbox.md).

## 8. Theo dõi hoạt động (trace) — và giới hạn của nó

Bật bằng `sbx_trace`, xem bằng `sbx_events`: ghi lại mã đã đọc/ghi file nào,
chạy tiến trình gì, kết nối tới đâu. Rất hợp để kiểm thử "đoạn mã này thực sự
đụng vào những gì".

> ⚠️ **Đây KHÔNG phải bằng chứng an ninh.** Hook chạy *bên trong* sandbox và
> nhật ký là một file *trong chính sandbox*. Mã cố tình che giấu thì gỡ hook,
> sửa log, hoặc ghi thẳng xuống fd mà không chạm API nào bị theo dõi được. Log
> sạch **không** có nghĩa là an toàn. Ranh giới thật sự chống được mã độc là bản
> thân sandbox, do nhân hệ điều hành cưỡng chế.

## 9. Quản lý sandbox trên UI

Card **Managed sandboxes** liệt kê sandbox đang có: backend, mức đọc, mạng, giới
hạn, trạng thái, lần dùng cuối. Thao tác được: **Stop all** (dừng mọi tiến
trình), **Stop container**, **Delete** (giữ file), **Delete with files**. Mở
rộng một dòng để xem tiến trình đang chạy (PID, %CPU, RAM) và dừng từng cái.

Card **Recent runs** hiển thị 30 lần chạy gần nhất kèm cột **Isolation** — cách
ly *thực sự* đã áp cho lần chạy đó. Đây là chỗ kiểm tra xem công tắc cưỡng chế
có đang hoạt động thật không.

Card **Space Apps — sandbox từng app** liệt kê mọi app có tiến trình server: cơ
chế mà **tiến trình đang chạy thật sự nhận được** (không phải cấu hình đã lưu),
chế độ đọc, chế độ mạng kèm số lượt proxy chặn, pid/thời gian chạy/số lần khởi
chạy, và CPU/RAM. 10 app một trang, sắp xếp được, ba nút mỗi dòng: theo dõi chi
tiết, cấu hình sandbox, khởi động lại.

Cột đầu là chỗ bắt được thứ không màn hình nào khác thấy: app **cấu hình có
sandbox nhưng đang chạy không sandbox** (profile chỉ cố định lúc khởi chạy) —
hàng đó hiện `cần khởi động lại`.

Trang này **không** làm: tạo sandbox, gắn thư mục, chạy lệnh, đổi cổng, terminal,
duyệt file. Những việc đó đi qua agent (MCP tool) hoặc REST.

## 10. Nhờ agent làm — 22 tool

Nói với agent bằng tiếng Việt là đủ ("chạy đoạn Python này cách ly", "chạy app
này trong sandbox và mở cổng 8000"). Skill `sandbox-runner` sẽ dẫn nó dùng đúng
tool:

- **Mặc định `sbx_run`** — sandbox dùng-một-lần, chạy xong tự xoá. Đừng tạo
  sandbox lâu dài cho một phép tính.
- Việc nhiều bước cần giữ file/gói đã cài: `sbx_create` → `sbx_file_write` →
  `sbx_run_in` / `sbx_exec` → `sbx_install` → `sbx_delete`.
- Còn lại: `sbx_capabilities`, `sbx_list`, `sbx_update`, `sbx_files`,
  `sbx_file_read`, `sbx_stats`, `sbx_kill`, `sbx_mount`, `sbx_unmount`,
  `sbx_fs_mode`, `sbx_settings`, `sbx_ports`, `sbx_trace`, `sbx_events`,
  `sbx_runs`.

## 11. Xử lý sự cố

| Triệu chứng | Nguyên nhân / cách xử lý |
|---|---|
| "Python execution is switched off (Plugins → Sandbox)" | Công tắc **Run Python** đang tắt. Bật lại ở Plugins → Sandbox; không có đường chạy thay thế. |
| Lệnh Bash của agent bỗng báo lỗi sau khi bật cưỡng chế | Máy không cách ly được (xem card Available isolation) → lệnh fail thay vì chạy thẳng. Hoặc lệnh đang ghi ra ngoài thư mục dự án. |
| App trong sandbox không phân giải được tên miền | Sandbox chưa có quyền ra ngoài. Bộ phân giải chỉ được cấp khi bật `network` hoặc có `connect` — mở `connect: [53]` **không** phải cách làm đúng trên macOS. |
| App trong sandbox không gọi được dịch vụ trên máy | Đúng như thiết kế. Khai cổng đó vào `loopback`. |
| Server chạy trong sandbox chết ngay sau khi khởi động | Dùng `cmd &` trần — xem §6. |
| Docker báo không dùng được dù `docker --version` chạy | Engine hỏi **daemon**, không hỏi CLI. Mở Docker Desktop rồi bấm kiểm tra lại. |
| Chạy đoạn mã nhỏ mà mất mấy giây | Bình thường là ~0,03 s. Nếu chậm hàng giây, kiểm tra Docker Desktop có đang treo không. |

Riêng cho **Space App chạy trong sandbox** (§7):

| Triệu chứng | Nguyên nhân / cách xử lý |
|---|---|
| App hiện `ngoài daemon` · `không rõ` · `cần khởi động lại` | Tiến trình đang chạy không do daemon hiện tại khởi chạy (thường là còn sót sau lần khởi động lại trước), nên **không biết** nó có bị nhốt hay không. Bấm khởi động lại. Từ v0.4.5 daemon tự thu hồi cổng lúc khởi động nên trạng thái này thành hiếm. |
| Thoát SenClaw mà app vẫn chạy | Lỗi cũ: daemon chỉ bắt SIGINT nên khối shutdown không chạy. Đã vá ở v0.4.5 — cần daemon mới. Dọn tồn đọng: `pgrep -f "$HOME/senclaw/workspace/space-apps/" \| xargs -r kill -TERM`. |
| App có SQLite chết `unable to open database file` dù thư mục dữ liệu đã được cấp | Thư mục **cha** (`~/.senclaw`) bị cấm đọc nên không phân giải được đường dẫn. Đã vá ở v0.4.5. Dấu hiệu nhận biết: `ls` và `sqlite3` CLI trên cùng profile vẫn chạy được. |
| Bật `strict` là app không khởi động, log có `EPERM .../npm-cli.js` | Runtime cài bằng nvm nằm trong `$HOME`, đúng chỗ `strict` cắt. Đã vá (cấp chỉ-đọc cho mục `PATH` ngoài system root). Nếu vẫn dính: `PATH` có quá 16 mục ngoài system root, log ghi cảnh báo. |
| Chọn "chỉ vài trang" xong app hỏng lúc khởi động | Nhiều app gọi mạng khi start (`npm start` hỏi `registry.npmjs.org`). Xem danh sách bị chặn ngay trong hộp thoại Sandbox hoặc màn theo dõi rồi khai thêm. |
| Đã bật sandbox mà cột vẫn hiện `tắt`/`none` | Cấu hình chỉ áp dụng **lúc khởi chạy**. Bấm *Lưu & khởi động lại app*. |

## 12. Đọc thêm

- Sandbox từng Space App (thư mục, mạng, bảng đo trước/sau):
  [docs/space-app-sandbox.md](space-app-sandbox.md)
- Theo dõi & debug một Space App (pid, CPU/RAM, socket, log):
  [docs/space-app-monitor.md](space-app-monitor.md)
- Thiết kế và lý do kỹ thuật: [docs/sandbox-app-design.md](sandbox-app-design.md)
- Đo thực nghiệm chạy app thật + các lỗ đã vá:
  [docs/sandbox-security-experiment.md](sandbox-security-experiment.md)
- Windows: [docs/sandbox-windows-research.md](sandbox-windows-research.md)
- Skill cho agent: [skills/sandbox-runner/SKILL.md](../skills/sandbox-runner/SKILL.md)
