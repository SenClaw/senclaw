# Sandbox trực tiếp trên Windows, không cần Docker — nghiên cứu

**Trạng thái: rà soát + sửa 2026-10-25; chạy thử trên Windows thật vẫn CHƯA có.**

Backend nằm ở `src/sandbox/backend/direct_windows.rs` (logic chuỗi/đường dẫn thuần ở
`winproc.rs`, test được trên mọi OS). Máy phát triển là macOS, nên:

* `winproc.rs` — **đã chạy test** (30 test, macOS).
* `direct_windows.rs` — **đã type-check** với crate `windows` 0.58 cho
  `x86_64-pc-windows-msvc` (`scripts/typecheck-windows-sandbox.sh`, ~1 phút, ~100 MB — dựng
  crate scratch `#[path]`-include file thật); chưa chạy.
* Hành vi thật (AppContainer có chặn ghi / đọc / mạng không, Python có khởi động nổi
  không) được kiểm bằng `cargo test --lib sandbox::backend::` trên một máy Windows —
  workflow `.github/workflows/sandbox-windows.yml` chạy đúng việc đó trên runner
  `windows-latest`. **Kết quả đầu tiên của job đó là việc cần làm tiếp theo.**

## Rà soát 2026-10-25: những gì sai và đã sửa

Đọc code đối chiếu với tài liệu API của Windows (không cần máy Windows để thấy các lỗi này).

| # | Lỗi | Hậu quả | Sửa |
|---|---|---|---|
| 1 | `SetEntriesInAclW(…, None, …)` rồi `SetNamedSecurityInfoW`: DACL cũ **bị thay bằng đúng một ACE** | Cấp quyền cho thư mục Python/dự án/mount làm mất mọi ACE tường minh khác của thư mục đó (chỉ nhờ ACE kế thừa mà user thường còn truy cập). Lỗi **phá hoại**, nặng nhất | Đọc DACL hiện có (`GetNamedSecurityInfoW`), gộp ACE mới vào, ghi lại. Có test: cấp cho container thứ hai không làm mất container thứ nhất |
| 2 | Môi trường child là môi trường Unix (`PATH`, `HOME`, `TMPDIR`) | Python 3 **không khởi động** nếu thiếu `SystemRoot`; `TEMP`/`TMP` không có nên tool ghi file tạm vào chỗ không được phép; thư mục `.tmp` không được tạo | `winproc::windows_env`: `SystemRoot`, `ComSpec`, `PATHEXT`… từ host; `TEMP`/`TMP`/`USERPROFILE`/`APPDATA` trỏ vào sandbox; `PYTHONUTF8=1`; tạo `.tmp` |
| 3 | Dòng lệnh: `"` + `replace('"', "\\\"")` | `C:\dir\` kết thúc bằng `\` nuốt dấu nháy đóng; đối số có `\"` sai | `winproc::quote_arg` theo quy tắc `CommandLineToArgvW`, test round-trip |
| 4 | `CreatePipe` đánh dấu **cả hai đầu** inheritable, không có handle list | Child giữ đầu của chính pipe của nó nên đọc không thấy EOF; mọi handle inheritable khác trong daemon (sandbox khác chạy cùng lúc) lọt vào child | Đầu của daemon không inheritable; `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` chỉ đúng 3 handle; stdin là pipe đã đóng đầu ghi |
| 5 | Không có `CREATE_NO_WINDOW` | Daemon không có console ⇒ mỗi lần chạy hiện một cửa sổ console | Thêm cờ |
| 6 | Sau khi process chính thoát, `join` đọc stdout không có hạn | Một tiến trình con giữ pipe (`start /b …`) làm run **treo mãi**, vì timeout chỉ áp lên `WaitForSingleObject` | Một deadline cho cả process lẫn pipe; quá hạn ⇒ `TerminateJobObject`. Output cắt ở `MAX_OUTPUT` ngay lúc đọc |
| 7 | Script shell ghi `.runs/x.cmd` rồi `cmd /c .runs/x.cmd` | `/` trong cmd là dấu bắt đầu switch ⇒ không tìm thấy lệnh; `\r\n` bị nhân đôi; output cmd là code page OEM, không phải UTF-8; file `.cmd` **không bao giờ bị xoá** (agent sandbox trỏ vào thư mục dự án của người dùng) | Đường dẫn dùng `\`; `chcp 65001`; CRLF chuẩn hoá; xoá file sau khi chạy |
| 8 | `AssignProcessToJobObject` lỗi ⇒ return, process đang `SUSPENDED` bị bỏ lại | Process mồ côi | `TerminateProcess` + đóng handle |
| 9 | Mỗi sandbox một AppContainer profile, **không bao giờ xoá** (REPL tạo một sandbox mỗi lần chạy) | Thư mục `%LOCALAPPDATA%\Packages\SenClawSbx…` + mục registry tích tụ vô hạn; ACE của container còn lại trên thư mục dự án | `forget()` khi xoá sandbox: `DeleteAppContainerProfile` + thu hồi quyền trên thư mục sống lâu hơn sandbox |
| 10 | `CreateAppContainerProfile` lỗi bất kỳ ⇒ coi như "đã tồn tại" rồi derive SID | `Derive…` luôn thành công kể cả khi profile chưa từng được tạo ⇒ lỗi thật (chính sách nhóm) bị che, launch lỗi mơ hồ | Chỉ `ERROR_ALREADY_EXISTS` mới chuyển sang derive; lỗi khác báo nguyên văn |
| 11 | `caps` luôn báo `available: true` | Máy chặn AppContainer vẫn hiện "sẵn sàng", mỗi lần chạy mới lỗi | Probe thật (`direct_windows::probe`); lỗi ⇒ `Unsupported` kèm lý do và gợi ý tắt sandbox / dùng Docker |
| 12 | Mount: danh sách cấm chỉ có đường dẫn Unix, `HOME` không có trên Windows | Mount `C:\`, `C:\Windows`, `%USERPROFILE%` hay `.ssh` được chấp nhận ⇒ ACE kế thừa ghi đè ACL hàng nghìn file | `winproc::forbidden_mount_reason`; bỏ tiền tố `\\?\` của `canonicalize` trước khi đưa cho API quyền |
| 13 | `which("python")` gặp alias Microsoft Store (`…\WindowsApps\python.exe`, 0 byte) | Chọn nhầm stub mở Store; AppContainer không chạy được nó | `winproc::is_store_stub` loại khỏi kết quả |
| 14 | `GENERIC_READ \| GENERIC_EXECUTE` trong ACE; cấp lại ACL mỗi lần chạy | Quyền generic không được mọi đường đọc ACL map; ghi lại ACL (lan xuống mọi file) mỗi lệnh là hàng giây trên dự án có `node_modules` | Quyền file cụ thể; nhớ các grant đã áp trong tiến trình |
| 15 | Quyền đọc thư mục interpreter lỗi ⇒ cả lần chạy lỗi | `C:\Program Files\…` người dùng thường không sửa được DACL — và không cần, vì đã đọc được | Lỗi cấp quyền đọc chỉ thành ghi chú, hiện ra nếu lần chạy sau đó lỗi. Venv: cấp cả thư mục `home` trong `pyvenv.cfg` |

### Giới hạn mới biết (không sửa được bằng code, đã ghi vào `caps`)

* **AppContainer chặn loopback** (`127.0.0.1`) kể cả khi có `internetClient`; mở lại cần
  `CheckNetIsolation LoopbackExempt` — **quyền admin**. Hệ quả: `execLoopback` (cổng
  local cho shell) và proxy allowlist theo site **không dùng được** trên Windows.
* `FsMode::Open` hành xử như `strict` (cố ý — xem dưới).
* Mount **không** hiện ở `<workdir>\<target>`: code thấy nó ở đường dẫn gốc của nó
  (Unix dùng symlink, Windows chưa tạo junction).
* Không terminal tương tác (ConPTY), `run_in_background` bị từ chối, monitor CPU/RAM
  chưa có — như trước.
* Sandbox từng Space App vẫn **không có** trên Windows (`app_launch.rs`).

### Công tắc tắt sandbox

Máy dựng không nổi AppContainer (hoặc Python không khởi động trong đó) trước đây **không
có lối thoát** để chạy code. Giờ có công tắc tổng `enabled` (mặc định bật) ở
Plugins → Sandbox; tắt ⇒ `direct` chạy như tiến trình thường, vẫn trong Job Object
(trần RAM + giết cả cây), ghi `isolation: "none"`. Xem `docs/sandbox-guide.md` §2.

> Không cross-compile được cả crate trên macOS: `ring` và `libsqlite3-sys` có
> build script C cần toolchain MSVC. Nên module Windows được type-check tách
> riêng, bằng chính source thật cộng vài kiểu giả lập cho phần crate nội bộ.

## Kết luận ngắn

**Làm được.** Windows có sẵn thứ tương đương Seatbelt (macOS) và bubblewrap
(Linux): **AppContainer**, cộng với **Job Object** cho giới hạn tài nguyên.
Cả hai đều là API nhân hệ điều hành, **không cần quyền admin**, không cần
Hyper-V, không cần Docker.

Manifest hiện ghi *"Windows chỉ có Docker"*. Đó là mô tả đúng của bản đang
chạy, không phải giới hạn của Windows.

## AppContainer: tương đương Seatbelt/bubblewrap

Điểm mấu chốt, theo [tài liệu Microsoft](https://learn.microsoft.com/en-us/windows/win32/secauthz/implementing-an-appcontainer):

> quyền truy cập được phép là **giao** của quyền cấp cho user/group SID và
> quyền cấp cho AppContainer SID

Tức là **cấm mặc định**, phải cấp mới có — đúng mô hình `strict` hiện tại. Thêm
nữa AppContainer chạy ở **Low Integrity Level**, nên ghi vào object Medium IL bị
chặn sẵn ngay cả trước khi xét DACL.

Mạng là một **capability**: không có `internetClient` thì tiến trình không mở
được kết nối ra ngoài. [fastrender](https://github.com/wilsonzlin/fastrender/blob/main/docs/windows_sandbox.md)
gọi đây là *"cách duy nhất khả thi, được hệ điều hành hỗ trợ, để chặn mạng
ra"*. Ánh xạ 1-1 với công tắc mạng hiện có, và sạch hơn `(deny network*)` của
Seatbelt.

### Các bước (đã type-check, chưa chạy)

```
CreateAppContainerProfile(moniker, …)          → Package SID
  └ ERROR_ALREADY_EXISTS → DeriveAppContainerSidFromAppContainerName
DeriveCapabilitySidsFromName("internetClient")  → chỉ khi bật mạng
SetEntriesInAclW + SetNamedSecurityInfoW        → cấp quyền thư mục cho SID đó
InitializeProcThreadAttributeList
UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES)
CreateProcessW(…, EXTENDED_STARTUPINFO_PRESENT, …)
```

Đã kiểm bằng `cargo check --target x86_64-pc-windows-msvc` với crate `windows`
0.58: mọi hàm trên tồn tại với đúng chữ ký dùng ở đây, kể cả
`SetEntriesInAclW`/`SetNamedSecurityInfoW` và `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`.
Type-check **không** chứng minh nó chạy đúng — chỉ chứng minh không viết nhầm API.

## Ánh xạ từng tính năng hiện có

| Tính năng | macOS Seatbelt | Linux bwrap | Windows (đề xuất) |
|---|---|---|---|
| Nhốt ghi trong thư mục sandbox | `(deny file-write*)` + allow subpath | không bind = không tồn tại | AppContainer cấm mặc định + cấp DACL cho đúng workdir |
| `strict` (chặn đọc đĩa) | deny `file-read*` rồi cấp lại | chỉ bind gốc hệ thống | **miễn phí** — AppContainer vốn không đọc được dữ liệu user |
| `allowlist` | cấp thêm subpath | `--ro-bind-try` | cấp DACL read cho từng thư mục khai báo |
| `open` | chỉ chặn thư mục bí mật | `--ro-bind / /` | cấp DACL read rộng, hoặc bỏ AppContainer về restricted token |
| Tắt/bật mạng | `(deny network*)` | `--unshare-net` | có/không capability `internetClient` |
| Gắn thư mục (mount) | symlink + cấp quyền | `--bind` / `--ro-bind` | cấp DACL cho thư mục nguồn + junction (`mklink /J`) vào workdir |
| Giới hạn RAM | **không cưỡng chế được** | không (chỉ docker) | **Job Object `ProcessMemoryLimit` — cưỡng chế thật** |
| Giới hạn số tiến trình | không | không | **Job Object `ActiveProcessLimit`** |
| Giết cả cây tiến trình | `setsid` + `kill(-pgid)` | như macOS | **`TerminateJobObject` / `KILL_ON_JOB_CLOSE`** |
| Theo dõi hoạt động | Python audit hook / Node preload | như macOS | **giữ nguyên** — hook ở tầng ngôn ngữ, không phụ thuộc OS |

### Windows sẽ tốt hơn macOS ở hai chỗ

1. **Giới hạn RAM cưỡng chế được.** Hiện `direct` trên macOS phải nói thật với
   người dùng là "không có trần RAM"; Job Object cho trần thật.
2. **Giết cây tiến trình đảm bảo.** `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` để
   nhân dọn toàn bộ khi handle đóng — kể cả khi app chết bất thường. Mẹo
   `setsid`+`killpg` hiện tại không có bảo đảm đó.

## Chưa kiểm chứng — đọc kỹ phần này

Không chạy được trên máy này, nên đây là các rủi ro **thật**, xếp theo mức độ
có thể làm hỏng cả hướng đi:

### 1. Trình thông dịch có khởi động nổi không (rủi ro cao nhất)

Đúng bài học `/usr` ở macOS, chỉ khác cơ chế. Windows ACL sẵn
`ALL_APPLICATION_PACKAGES` cho file hệ thống, nên DLL trong `System32` đọc
được. **Nhưng Python cài trong `%LOCALAPPDATA%\Programs\Python` gần như chắc
chắn KHÔNG có ACE đó** → AppContainer không đọc/chạy được `python.exe`.

Hướng xử lý: cấp DACL read+execute cho AppContainer SID lên thư mục cài đặt
trình thông dịch, lúc dựng sandbox. Đây chính là `SYSTEM_READ_ROOTS` phiên bản
Windows — khác ở chỗ macOS chỉ cần *không cấm*, Windows phải *cấp*.

fastrender gặp đúng vấn đề này và xử lý bằng cách copy binary sang thư mục temp
có ACL phù hợp. Cấp ACL tại chỗ sạch hơn, nhưng **ghi ACL vào thư mục cài Python
của người dùng là một hành động đáng cân nhắc** — cần hỏi ý người dùng, hoặc
làm một lần rồi ghi nhớ.

### 2. AppContainer thường hay LPAC

LPAC (Less Privileged AppContainer) chặt hơn nhưng cần capability riêng cho cả
registry và COM. Python **rất có thể** không chạy nổi trong LPAC. Đề xuất dùng
AppContainer thường; LPAC để sau, và chỉ khi đo được là chạy được.

### 3. Lỗi CPython đã biết trong AppContainer

[python/cpython#134587](https://github.com/python/cpython/issues/134587):
`mkdtemp()` hỏng trong AppContainer ở 3.12.4 (3.12.3 thì không). Temp của
AppContainer bị chuyển hướng sang `%LOCALAPPDATA%\Packages\<tên>\AC\Temp`. Thiết
kế hiện tại đã trỏ `TMPDIR` vào trong sandbox nên có thể né được, nhưng phải đo.

### 4. Job Object lồng nhau

[dotnet/runtime#107992](https://github.com/dotnet/runtime/issues/107992): tiến
trình con nằm trong job có `KILL_ON_JOB_CLOSE` mà lại đẻ con ra ngoài job thì
cây không bị giết hết. Cần đo với `python → subprocess → grandchild`.

### 5. Còn lại

- Chưa rõ `CreateAppContainerProfile` có bị chặn bởi chính sách nhóm trong môi
  trường doanh nghiệp không.
- Mount bằng junction cần kiểm: junction trỏ ra ngoài có bị AppContainer chặn
  không, hay chỉ cần ACL ở đích là đủ.
- Windows ARM64 chưa xét.

## Kế hoạch kiểm chứng (cần một máy Windows)

Theo đúng thứ tự này — bước 2 hỏng thì cả hướng phải xem lại:

1. `CreateAppContainerProfile` chạy được với user thường, không admin.
2. **`python -c "print(1)"` chạy được trong AppContainer** sau khi cấp ACL cho
   thư mục cài Python. *Đây là bước quyết định.*
3. Ghi ra ngoài workdir → bị chặn.
4. Đọc `%USERPROFILE%\Documents` → bị chặn (đây là `strict`).
5. Không có capability → `socket.connect` thất bại; có `internetClient` → thành
   công.
6. Job Object: cấp 64 MB rồi cấp phát 500 MB → bị chặn.
7. `TerminateJobObject` giết được `python → subprocess → grandchild`.
8. Hook theo dõi (`sitecustomize.py`) vẫn chạy — dự đoán là có, vì thuần Python.

Mỗi bước nên thành một test `#[cfg(windows)]` trong `runner.rs`, tự bỏ qua trên
OS khác, giống cách nhóm test Seatbelt đang làm.

## Đã làm gì (bản gốc 2026-09-12)

| Phần | Trạng thái |
|---|---|
| `backend/direct_windows.rs` | ✅ AppContainer + Job Object, ~470 dòng |
| `caps.rs` | ✅ `DirectKind::AppContainer`, Windows không còn báo "chỉ Docker" |
| `ExecSpec.argv` | ✅ Windows cần chương trình đã phân giải sẵn (không có `/bin/sh`) |
| `code.rs` / `runner.rs` | ✅ phân giải trình thông dịch trong Rust; lệnh shell ghi ra `.cmd` rồi gọi `cmd.exe` |
| `mounts.rs` | ✅ dùng lại đường cấp ACL, không cần symlink |
| `examples/win_sandbox_probe.rs` | ✅ chương trình dò 8 bước |
| `monitor.rs` (CPU/RAM, danh sách tiến trình) | ❌ **chưa** — `ps -axo` là Unix-only |
| Terminal tương tác | ❌ **chưa** — báo lỗi rõ ràng, hướng người dùng sang tab Chạy |

### Hai chỗ hành vi khác Unix, cố ý

1. **`FsMode::Open` trên Windows không mở toàn đĩa.** Muốn "đọc được cả đĩa"
   thì phải viết lại DACL của mọi thư mục người dùng — một thay đổi phá hoại,
   phạm vi toàn máy, làm thay người dùng thì không chấp nhận được. Nên `open`
   trên Windows hành xử như `strict` cộng danh sách cho phép, và điều đó được
   ghi thẳng trong mã chứ không giấu.
2. **Không có shell.** Đoạn mã được phân giải trình thông dịch trong Rust rồi
   gọi thẳng; lệnh shell thì ghi ra file `.cmd` rồi trỏ `cmd.exe` vào. Vẫn giữ
   nguyên tính chất "không bao giờ nội suy chữ của người dùng vào dòng lệnh".

## Việc còn lại

1. **Chạy chương trình dò trên máy Windows thật.** Bước 2 (Python khởi động)
   quyết định cả hướng đi.
2. Nếu bước 2 hỏng kể cả sau khi cấp ACL → phương án lùi là **restricted token +
   Low IL**: yếu hơn hẳn (fastrender nói thẳng là mạng *không* chặn được đáng
   tin), lúc đó phải báo cáo trung thực như tầng `degraded` chứ không được gọi
   là cách ly.
3. `monitor` cho Windows: Job Object accounting
   (`QueryInformationJobObject`) cho RAM đỉnh và số tiến trình, hoặc ToolHelp32
   cho danh sách.
4. Terminal: ConPTY trong AppContainer — chưa nghiên cứu.
