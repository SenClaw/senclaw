# Sandbox — thiết kế

Chạy lệnh shell và mã nguồn tách khỏi máy thật. Tính năng này có **hai hiện
thân** — cùng một engine, nhưng là hai bản sao độc lập đã bắt đầu tách nhau:

| | **Engine built-in (core)** | **Space App (bản gốc)** |
|---|---|---|
| Mã nguồn | `src/sandbox/` (17 module) | `apps/sandbox/` |
| Chạy ở đâu | ngay trong daemon `senclaw` | tiến trình riêng, cổng **4730** |
| MCP | **`senclaw-sandbox`** (stdio, `senclaw sandbox-server`) | `sandbox-mcp` (HTTP/SSE) |
| REST | `/api/sandbox/*` trên UI daemon | `/api/*` trên 4730 |
| Dữ liệu | `~/.senclaw/sandbox/sandbox.sqlite` | `~/.senclaw/space-app-data/sandbox/app.sqlite` |
| UI | **Plugins → Sandbox** | web UI riêng (đầy đủ hơn: file, terminal, mount, trace) |
| Cưỡng chế Bash/Python/Node/scheduler | ✅ `ExecPolicy` | ❌ |
| Cách ly loopback | ✅ `ports.loopback` | ❌ — **vẫn còn lỗ hổng đã vá ở core** |

Tài liệu này mô tả **bản core**; chỗ nào Space App khác thì nói rõ. Cả hai có
đúng **22 tool `sbx_*`** cùng tên; `fsmode.rs`, `trace.rs`, `code.rs` và
`schema.sql` giống nhau từng dòng, còn `ports.rs` / `runner.rs` / `policy.rs`
thì chỉ core mới có phần mới. Skill đi kèm: `skills/sandbox-runner/SKILL.md`
(tool `mcp__senclaw-sandbox__sbx_*`).

Yêu cầu ban đầu: *"app sandbox chuyên cho việc thực thi lệnh máy ảo, và thực thi
lệnh trên máy này, như chạy các lệnh python trên máy này cách ly độc lập với máy
thật — trên mac/linux hỗ trợ chạy trực tiếp hoặc docker image, trên windows chỉ
hỗ trợ docker image."*

## Hai backend

| | `direct` | `docker` |
|---|---|---|
| Cơ chế | macOS Seatbelt (`sandbox-exec`) / Linux `bwrap` / Windows AppContainer | container |
| Cần gì | không | Docker daemon đang chạy |
| Khởi động | tức thì | vài giây (+ pull image lần đầu) |
| Chặn ghi ra ngoài sandbox | ✅ | ✅ |
| Chặn đọc `~/.ssh`, `~/.aws`, Keychain, `~/.senclaw` | ✅ | ✅ |
| Chặn đọc phần còn lại của đĩa | ✅ ở mức `strict`/`allowlist` (mặc định) | ✅ |
| Giới hạn CPU/RAM/pids | ❌ trên Unix · ✅ trên Windows (Job Object) | ✅ |
| Windows | ✅ AppContainer + Job Object — **chưa kiểm chứng trên máy thật** | ✅ |

Windows không còn là "chỉ docker": `backend/direct_windows.rs` dựng AppContainer
cho cách ly và Job Object cho giới hạn tài nguyên, `caps.rs` báo
`available: true` và `is_enforced()` đúng. Nhưng phần mô tả khả năng nói thẳng
là **chưa có ai chạy thử trên một máy Windows thật** — xem
`docs/sandbox-windows-research.md`.

Khi máy thiếu `sandbox-exec` / `bwrap`, `direct` rơi về **`Degraded`**: tiến
trình con chạy như bình thường, **không có rào hệ điều hành nào**. Đó là trạng
thái được đặt tên và báo ra, không phải im lặng giả vờ đã cách ly.

## Ba mức cách ly ĐỌC (`fsmode.rs`)

Ghi luôn bị nhốt trong thư mục sandbox. Đọc thì chọn được:

| Mức | Đọc được gì |
|---|---|
| `strict` (**mặc định**) | thư mục sandbox + thư mục đã gắn + thư viện hệ thống |
| `allowlist` | như trên, cộng các thư mục khai trong cài đặt app |
| `open` | cả đĩa, trừ `~/.ssh`, `~/.aws`, Keychain, `~/.senclaw` |

**Thư viện hệ thống luôn đọc được, kể cả ở `strict`** (`/usr`, `/System`,
`/opt/homebrew`…). Đó không phải lỗ hổng bỏ sót — đó *là* trình thông dịch:
Python nằm ở đó, thư viện chuẩn nằm ở đó, cache của dynamic linker nằm ở đó. Một
read-jail loại chúng ra thì không phải cách ly Python, mà là làm Python không
khởi động nổi. Cái `strict` thật sự cắt là dữ liệu của người dùng: tài liệu, dự
án, file của app khác, phần còn lại của `$HOME`.

Cách hai backend dựng jail khác nhau về bản chất:

* **Seatbelt**: cả đĩa vẫn hiện diện, chỉ có luật ngăn — `(deny file-read*)`
  rồi cấp lại từng `subpath`.
* **bubblewrap**: đường nào không bind thì **không tồn tại** trong mount
  namespace. Không có luật nào để viết sai. Dùng `--ro-bind-try` vì danh sách
  gốc hệ thống cố tình rộng cho nhiều distro (`/lib64` glibc, `/nix/store`
  NixOS) và thiếu một mục thì phải bỏ qua chứ không được làm hỏng sandbox.
* **docker**: bỏ qua thiết lập này — container vốn khởi đi từ image, không có
  đĩa máy thật để mà nhốt.

Mặc định do `settings.rs` giữ (`default_fs_mode`, mặc định `strict` = "chỉ
map"), sandbox mới kế thừa, sandbox cũ giữ nguyên. Giá trị lạ trong DB rơi về
`strict` chứ không rơi về `open` — một lỗi chính tả không được âm thầm mở đĩa.

**Một ngoại lệ dễ nhầm:** shell của agent khi bị cưỡng chế **không** dùng mặc
định này mà dùng `ExecPolicy.exec_fs_mode`, mặc định là **`open`**. Lý do ở mục
ExecPolicy bên dưới: agent đã đọc được cả đĩa bằng tool `Read` của nó, siết chặt
riêng đường shell chỉ làm hỏng workflow chứ không đóng thêm cánh cửa nào.

### macOS: Seatbelt profile

Sinh lại mỗi lần chạy (`backend/direct.rs::seatbelt_profile`), thứ tự quan
trọng vì **luật khớp sau thắng**:

```
(allow default)              ; deny-by-default chặn luôn hàng chục mach service
(deny file-write*)           ; …rồi khoét dần
(allow file-write* (subpath "<workdir>") (subpath "/dev"))
(deny file-read* … ~/.ssh ~/.aws ~/.gnupg ~/Library/Keychains ~/.senclaw …)

;; chỉ ở strict/allowlist — nhốt luôn chiều đọc
(deny file-read*)
(allow file-read-metadata)                        ; stat vẫn phải chạy được
(allow file-read* (subpath "/usr") (subpath "/System") …)   ; thư viện hệ thống
(allow file-read* (subpath "<mount source>") …)             ; thư mục đã gắn

(allow file-read* (subpath "<workdir>"))   ; workdir nằm dưới ~/.senclaw, phải mở lại

;; phần mạng
(deny network*)                                        ; khi sandbox tắt mạng
(allow network-outbound (remote ip "*:443"))           ; từng cổng `connect`
(allow network-bind     (local ip "*:8000"))           ; từng cổng `listen`
(allow network-inbound  (local ip "*:8000"))           ; …bind không thôi thì không ai vào được
(allow network-outbound (literal "/private/var/run/mDNSResponder"))  ; DNS — xem mục cổng
(deny  network-outbound (remote ip "localhost:*"))     ; ← CUỐI CÙNG, kể cả khi mạng bật
(allow network-outbound (remote ip "localhost:8899"))  ; …rồi trả lại từng cổng `loopback`
```

### Linux: bubblewrap

```
bwrap --die-with-parent --new-session --unshare-pid --unshare-ipc --unshare-uts
      [--unshare-net] --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp
      --tmpfs $HOME              # phủ trắng home = giấu sạch dotfile bí mật
      --bind <workdir> <workdir> # …rồi gắn lại workdir, PHẢI sau tmpfs
      --chdir <workdir> -- /bin/sh -s
```

### Docker

`docker run -d` một container `sleep infinity`, mỗi lần chạy là một
`docker exec`. Nhờ vậy `pip install` ở lần gọi này vẫn còn ở lần gọi sau.
Thư mục sandbox trên host được bind-mount vào `/work`, nên trình duyệt file đọc
thẳng từ host, không qua Docker.

Cờ đáng chú ý: `--network none` mặc định, `--memory-swap` bằng `--memory` (thiếu
nó thì container swap vượt cap và giới hạn gần như vô nghĩa), `--cap-drop ALL`,
`--security-opt no-new-privileges`, `--entrypoint sh` (entrypoint của image
python/node sẽ nuốt mất `sleep`).

## Ba quyết định cốt lõi

### 1. Script vào bằng stdin, không bao giờ nội suy vào dòng lệnh

Mọi backend chạy `sh -s` và ghi chương trình vào stdin. Không có `sh -c "…"` ở
đâu cả. Một dấu nháy đơn trong đoạn Python của người dùng, nhẹ thì thành lỗi cú
pháp, nặng thì thành *lệnh khác*. Tương tự, đoạn mã được ghi ra file rồi trỏ
trình thông dịch vào file, chứ không phải `python -c`, nên traceback có tên file
và số dòng thật.

Có test riêng cho việc này: một đoạn Python chứa cả `'`, `"`, `$(whoami)`,
`` `id` ``, `;|&` phải chạy ra đúng chuỗi đó.

### 2. Môi trường được dựng mới, không kế thừa

Tiến trình daemon giữ môi trường đầy đủ — `SENCLAW_*`, API key, token. Con của
nó nhận đúng một tập biến được dựng tường minh (`backend/mod.rs::build_env`) và
không gì khác. `HOME` trỏ vào chính thư mục sandbox, nên pip cache / npm config
ghi vào đúng chỗ duy nhất được phép ghi.

### 3. Khả năng của máy được **đo**, không đoán

Máy phát triển tính năng này có Docker CLI trên PATH, `docker --version` chạy
vui vẻ, và daemon thì chết ("Docker Desktop is unable to start"). Một probe dừng
ở "binary có tồn tại không" sẽ báo Docker dùng được, rồi mọi sandbox chết lúc
chạy với lỗi khó hiểu. Nên `caps.rs` hỏi **daemon**, và bọc mọi tiến trình con
trong timeout cứng 4 giây có kill — `docker info` với Docker Desktop hỏng treo
hàng phút, và một capability probe treo thì kéo cả UI treo theo.

Kết quả probe được cache 20 giây và có nút "Kiểm tra lại" (`refresh=true`) cho
tình huống người dùng vừa mở Docker Desktop. **Hai cache tách rời** — hỏi khả
năng `direct` không được chạm vào Docker (lý do ở mục Bẫy).

## Bề mặt

REST `/api/sandbox/*` (28 route, nest trong UI server của daemon) và MCP
`sbx_*` đi qua cùng một `runner.rs`, nên giới hạn ép ở một bên thì bên kia cũng
có — nếu không, luật ép ở HTTP handler mà không ép ở MCP tool thì coi như không
có luật, vì agent đi đường MCP. Engine hỏng thì cả nhánh `/api/sandbox` trả
**503** chứ không biến mất route: một 404 sẽ được đọc nhầm thành "phiên bản cũ".

**22 tool**: `sbx_capabilities`, `sbx_run` (một lần, tự dọn — **mặc định**),
`sbx_create` / `sbx_list` / `sbx_update` / `sbx_delete`, `sbx_exec` /
`sbx_run_in`, `sbx_install`, `sbx_files` / `sbx_file_read` / `sbx_file_write`,
`sbx_runs`, `sbx_stats` / `sbx_kill`, `sbx_mount` / `sbx_unmount`,
`sbx_fs_mode` / `sbx_settings`, `sbx_trace` / `sbx_events`, `sbx_ports`.

Bản core phục vụ MCP qua **stdio** (`senclaw sandbox-server`, đăng ký trong
`.mcp.json` và đẩy vào roster agent ở `agent_pool/pool.rs`); Space App phục vụ
qua HTTP/SSE (`/api/mcp/sse`) — core không có hai route đó.

## ExecPolicy — cưỡng chế những đường chạy sẵn có

Đây là thứ bản core có mà Space App không: không chỉ *cung cấp* sandbox cho ai
muốn dùng, mà **bắt các đường thực thi vốn đã tồn tại đi qua nó**. Cấu hình nằm
ở `policy.rs`, lưu một row DB, đọc/ghi qua `GET|PUT /api/sandbox/exec-policy`,
và bật/tắt bằng công tắc ở **Plugins → Sandbox**.

| Khoá JSON | Mặc định | Ý nghĩa |
|---|---|---|
| `execShell` | **false** | Tool `Bash` của agent chạy trong OS sandbox, write-jail vào đúng working dir của chat |
| `execNetwork` | true | Mạng cho shell bị cưỡng chế |
| `execFsMode` | **`open`** | Mức đọc cho shell bị cưỡng chế |
| `execLoopback` | `[]` | Cổng trên máy này shell được gọi. Rỗng = không cổng nào |
| `runPython` | true | Cho phép Python thật (REPL + `/api/code/run` + sandbox) |
| `runNode` | true | Cho phép Node.js thật |
| `codeNetwork` | false | Mạng cho REPL python/node |
| `schedulerScript` | **false** | Task `script` / `script-agent` chạy trong sandbox dùng-một-lần |
| `schedulerNetwork` | true | Mạng cho scheduler script |

**"Tắt" nghĩa khác nhau tuỳ trường, và sự khác nhau đó là có chủ ý:**

* `execShell` và `schedulerScript` tắt ⇒ chạy **như cũ**, không sandbox
  (`isolation: "none"`). Hai đường này vốn tồn tại trước khi có sandbox; tắt là
  quay về hành vi gốc.
* `runPython` / `runNode` tắt ⇒ **từ chối hẳn**, kèm câu chỉ đường
  ("switched off (Plugins → Sandbox)"). Hai runtime này chưa từng chạy ngoài
  sandbox nên không có "bản gốc" nào để rơi về.

**Bật mà engine hỏng thì lệnh FAIL, không lặng lẽ rơi về shell thô.** Một công
tắc an ninh mà tự tắt khi gặp khó thì không phải công tắc an ninh.

Gate đặt ở bốn chỗ, nhưng chỗ sâu nhất mới là chỗ đáng tin: `runner::run_code`.
Mọi lối vào — REST, MCP, REPL, scheduler — đều chui qua đó, nên không có đường
vòng nào lọt qua bằng cách gọi API khác.

**Bẫy đã biết của `PUT /exec-policy`:** struct có `#[serde(default)]` ở mức
struct, nên **PUT thiếu trường sẽ reset trường đó về mặc định**, không phải giữ
nguyên. UI luôn gửi đủ object; script gọi tay thì phải `GET` rồi merge. (Ngược
lại `PUT /settings` không có `serde(default)` nên body thiếu trường bị 422 —
hai endpoint cạnh nhau, hai hành vi khác nhau.)

## Cách ly cổng (`ports.rs`)

Công tắc mạng là thô: bật hoặc tắt. Phần này thêm trạng thái ở giữa — **không
có mạng chung, nhưng những cổng này mở** — đúng thứ cần để chạy một app trong
sandbox: phục vụ trên 8000, xem được bằng trình duyệt, và không với tới gì khác.

Ba danh sách, vì đó là ba quyền khác nhau:

* `listen` — sandbox được **bind** cổng đó, và bạn vào được ở `127.0.0.1:<cổng>`.
  Phải ≥ 1024.
* `connect` — cổng **từ xa** duy nhất nó được gọi ra. `connect: [443]` là "chỉ
  được nói HTTPS".
* `loopback` — dịch vụ **trên chính máy này** nó được gọi. **Rỗng là mặc định,
  và rỗng nghĩa là không dịch vụ nào.**

**Không cần bật `network`** — luật cổng chính là toàn bộ quyền. Đó là điểm mấu
chốt: một app phục vụ trên 8000 không đồng thời được quyền gọi về nhà.

### Loopback bị chặn kể cả khi mạng bật

`*:443` bao gồm cả `127.0.0.1:443`, và `network: true` bao gồm **mọi dịch vụ
trên máy** — trong đó có REST API của chính SenClaw, thứ không có xác thực vì
ranh giới tin cậy của nó là loopback. Tổ hợp đó gỡ bỏ toàn bộ sandbox:

* mã không đọc được `~/.senclaw/oauth.json` ở tầng file thì **nhờ daemon đọc hộ
  qua HTTP**;
* và `POST /api/sandbox/sandboxes` để tự dựng cho mình một sandbox thứ hai với
  `fsMode: open` mount cả đĩa.

Cả hai đã được **chứng minh trên daemon sống** trước khi có luật deny này
(`docs/sandbox-security-experiment.md` §2). Nên profile luôn phát
`(deny network-outbound (remote ip "localhost:*"))` **cuối cùng** — luật khớp
sau thắng — rồi trả lại đúng những cổng khai trong `loopback`.

Test canh chuyện này chạy một listener thật trên máy rồi kiểm tra sandbox không
với tới được, bật mạng và tất cả: `this_machines_services_are_unreachable_even_
with_the_network_on`.

### DNS trên macOS không đi qua cổng 53

`connect: [53, 443]` mà vẫn không `fetch` được `https://…` — đo được, và lý do
không nằm ở luật cổng: `getaddrinfo` **không tự gửi gói UDP**, nó hỏi
mDNSResponder qua một Unix socket. Nên profile cấp
`(allow network-outbound (literal "/private/var/run/mDNSResponder"))`.

Đánh đổi được nói thẳng: một resolver là kênh rò rỉ dữ liệu (nhét dữ liệu vào
tên miền). Vì vậy socket này **chỉ được cấp khi sandbox vốn đã có quyền ra
ngoài** (`network: true` hoặc `connect` khác rỗng), không bao giờ cấp cho
sandbox tắt mạng.

### "Chỉ cho gọi một website" không thể là luật hệ điều hành

Seatbelt **không lọc được theo host** — parser từ chối thẳng:
`sandbox-exec: host must be * or localhost in network address`. Nghĩa là
`connect: [443]` = *mọi* site trên 443, không phải một site.

Cách làm được (đã chứng minh 4/4 phép đo): một **proxy allowlist chạy ngoài
sandbox**, rồi `connect: []` + `loopback: [<cổng proxy>]` + biến môi trường
`HTTPS_PROXY`. App nào lờ proxy thì đâm tường — hỏng theo chiều đóng — và vì
không có `connect` nên cũng không có resolver, khoá luôn đường DNS tunnel.

### Mỗi backend cưỡng chế được tới đâu

| Backend | `listen` | `connect` | `loopback` |
|---|---|---|---|
| macOS Seatbelt | chính xác từng cổng | chính xác từng cổng | chính xác từng cổng |
| Docker | publish ra `127.0.0.1`, chính xác | **không lọc được** | **không lọc được** |
| Linux bubblewrap | chạy được, nhưng mất namespace mạng | **không lọc được** | **không lọc được** |

Seatbelt là cái chính xác. **Đã đo trên máy thật trước khi viết module**: với
profile chỉ cho `*:53` ra ngoài, connect `:443` bị từ chối; bind một cổng không
khai báo cũng bị từ chối.

Docker và bubblewrap không lọc được cổng ra nếu không dựng thêm firewall hay
proxy bên trong. Nặng hơn: trên cả hai, **mở một cổng lắng nghe là mất luôn cách
ly mạng** — container `--network none` không publish được gì, và bwrap
`--unshare-net` thì không có đường về máy chủ. Nên trên hai backend đó, xin một
cổng lắng nghe là được cấp mạng — và điều đó được **báo ra** (`ports::note_for`)
chứ không làm lén. Với Docker còn thêm một đường loopback nữa không chặn được:
`host.docker.internal`.

Cổng publish luôn gắn `127.0.0.1`, không phải `0.0.0.0`: `-p 8000:8000` trần sẽ
đưa app trong sandbox ra cả LAN — đúng lỗi repo này từng mắc một lần với
`SENCLAW_BIND_HOST`.

## Giao diện: Plugins → Sandbox

Một trang cuộn (không phải tab) trong Plugins, deep-link `/plugins?nav=sandbox`.
Web (`SandboxPanel.tsx`) và desktop (`sandbox_panel.dart`) cùng REST, cùng ngữ
nghĩa, **song ngữ EN/VI** dùng chung khoá `senclaw:lang`.

| Card | Làm được gì |
|---|---|
| Available isolation | Xem máy này hỗ trợ cách ly nào, nút kiểm tra lại |
| **Security enforcement** | 9 công tắc/ô của `ExecPolicy` (mục trên) |
| Managed sandboxes | Bảng sandbox: backend, mức đọc, mạng, giới hạn, trạng thái. Stop all / Stop container / Delete / Delete with files; mở rộng dòng để xem tiến trình và kill từng pid |
| Recent runs | 30 lần chạy gần nhất, kèm cột **Isolation** — cách ly *thực sự* đã áp |
| Defaults for new sandboxes | Mức đọc, mạng, RAM, CPU, deadline mặc định |
| Allowlist | Thêm/bỏ thư mục đọc thêm cho chế độ `allowlist` (desktop có nút chọn thư mục) |

Trang này **cố ý không** làm: tạo sandbox, mount, exec, đổi cổng, terminal, xem
trace, duyệt file. Những việc đó đi qua **MCP tool** (agent làm) hoặc REST — còn
UI đầy đủ cho người thao tác tay thì nằm ở web UI của Space App. Endpoint
terminal (`/sandboxes/:id/terminal`, WebSocket) có trong core nhưng chưa UI nào
của daemon gọi tới.

## Theo dõi hoạt động cho kiểm thử (`trace.rs`)

Tuỳ chọn, mặc định tắt. Trả lời câu hỏi "đoạn mã này *thực sự* đụng vào những
gì": đọc/ghi file nào, khởi tạo tiến trình gì, kết nối tới địa chỉ nào.

### Vì sao trace ở tầng ngôn ngữ chứ không phải syscall

Cách hiển nhiên là syscall tracer. Không dùng được:

* **macOS** — `dtrace`, `dtruss`, `ktrace` đều từ chối chạy khi SIP bật, mà SIP
  bật là trạng thái mặc định. Đo trên máy phát triển: *"DTrace requires
  additional privileges"*, *"ktrace must be run as root"*. Endpoint Security
  framework thì cần entitlement Apple cấp cộng quyền root. Không có đường nào
  cho một tính năng chạy ở userspace, và bảo người dùng tắt SIP chỉ để xem danh
  sách file là một cái giá quá đắt.
* **Linux** — `strace` chạy được và thật sự tốt hơn, nhưng chỉ có trên Linux và
  không phải máy nào cũng cài.

Nên cơ chế chính là **hook trong tiến trình**, tiêm vào trước khi workload chạy:

| Runtime | Cách |
|---|---|
| Python | `sys.addaudithook` (PEP 578) qua `sitecustomize.py` trên `PYTHONPATH` |
| Node | `--require` preload, vá `fs`, `child_process`, `net`, `dns` |
| còn lại | so sánh thư mục sandbox trước/sau (chỉ thấy được ghi) |

`sitecustomize` và `NODE_OPTIONS` được kế thừa sang tiến trình con, nên script
gọi script khác vẫn theo dõi được.

### Chi tiết quan trọng

* Shim ghi log bằng `os.write` trên fd mở sẵn một lần. `open()` cũng phát ra sự
  kiện audit, nên ghi log bằng `open()` sẽ khiến hook tự kích hoạt chính nó, vô
  hạn. Có test canh điều này.
* Log là append-only chung cho cả sandbox; mỗi lần chạy ghi nhớ **offset** trước
  khi chạy, nếu không mọi lần chạy đều báo lại sự kiện của các lần trước.
* Lọc nhiễu: bỏ các lần **đọc** thư viện hệ thống (`/usr`, `/System`,
  `site-packages`…), bỏ file bookkeeping của chính app (`.runs/`,
  `.sandbox-profile.sb`, `.trace/`). **Không bao giờ lọc lần ghi** — ghi vào
  thư mục hệ thống chính là thứ người ta bật theo dõi lên để bắt.

### Đây KHÔNG phải bằng chứng an ninh

Hook chạy bên trong sandbox, nhật ký là một file trong chính thư mục sandbox.
Mã muốn giấu thì gỡ hook, sửa log, hoặc chỉ cần `os.write` trên fd thô mà không
chạm vào API nào bị theo dõi. Đây là bức tranh trung thực về hành vi của mã bình
thường — đúng thứ kiểm thử cần — chứ không phải chứng cứ về mã đang cố đánh lừa.
Ranh giới thật sự đứng vững trước mã độc là bản thân sandbox, do nhân cưỡng chế.
UI nói đúng câu này ngay trên bảng sự kiện chứ không giấu ở chú thích.

## Theo dõi tài nguyên và dừng tiến trình

`monitor.rs`. Backend `docker` hỏi `docker top`; backend `direct` không có
container để hỏi, nên engine **tự nhớ**: mỗi lần spawn đăng ký process group của
mình vào một registry, mỗi lần thoát gỡ đăng ký (bằng guard RAII — `exec` có
năm đường thoát, một trong số đó sẽ quên nếu viết tay). Nhóm chứ không phải pid,
vì một lần chạy là `sh` cộng mọi thứ nó đẻ ra, và `setsid` gom hết vào một nhóm.

Lấy mẫu bằng `ps -axo pid=,ppid=,pgid=,pcpu=,pmem=,rss=,etime=,comm=` rồi lọc
theo pgid **trong Rust** — cờ chọn theo nhóm của `ps` khác nhau giữa macOS và
Linux, một lần liệt kê đầy đủ rồi tự lọc thì hai hệ hành xử giống nhau.

**`sbx_kill` chỉ dừng được tiến trình thuộc nhóm mà engine đã tự khởi động cho
đúng sandbox đó.** Viết sai chỗ này thì endpoint trở thành "giết bất kỳ tiến
trình nào trên máy", pid 1 bao gồm. Đã kiểm chứng: `pid: 1` bị từ chối.

UI có biểu đồ CPU và RAM theo thời gian (`chart.tsx`, SVG tự vẽ, không thêm thư
viện). **Hai biểu đồ riêng, không phải hai đường trên một trục** — `%` và `MB`
khác đơn vị khác thang, trục kép mời người đọc suy ra quan hệ từ chỗ hai đường
cắt nhau, mà chỗ cắt đó chỉ là sản phẩm của hai thang. Trần trục snap theo bậc
thay vì bám đỉnh hiện tại, nếu không thì tải phẳng trông như đang leo vì trục
trượt bên dưới. Màu lấy từ palette đã chạy validator, hai bộ bậc riêng cho nền
sáng và nền tối.

## Gắn thư mục từ máy thật

`mounts.rs`. Một mount là `source` (đường dẫn thật) hiện ra tại `target` (đường
dẫn tương đối trong sandbox). Hai backend đặt nó ở **cùng một chỗ** —
`<gốc sandbox>/<target>` — nên đoạn mã viết cho backend này chạy được ở backend
kia.

| Backend | Cách làm |
|---|---|
| docker | bind mount thật: `-v source:/work/target[:ro]` |
| bubblewrap | bind mount thật: `--bind` / `--ro-bind` |
| Seatbelt | macOS không remap được đường dẫn cho tiến trình → **symlink** tại `<workdir>/<target>` + luật cấp quyền cho `source` trong profile |

Danh sách cấm (`forbidden_roots`) chặn `/`, `/etc`, `/usr`, `/System`, `/var`,
`~` (chính nó), và **chặn cả con** với `~/.ssh`, `~/.aws`, `~/.gnupg`, gcloud,
kube, docker, Keychains, và data dir của chính engine — gắn được data dir nghĩa
là một sandbox sửa được file của sandbox khác, kể cả Seatbelt profile mà lần
chạy sau sắp dùng. `source` được `canonicalize` **trước** khi so, nên `~/./`
không lách qua được.

`files.rs` phải biết về mount: trên macOS mount là symlink trỏ ra ngoài sandbox —
đúng cái hình dạng mà kiểm tra chống thoát sinh ra để chặn — nên `Scope` mang
theo danh sách source được phép. Đi vào mount: được. Đi thêm một bậc ra ngoài
nó (`duyet/../ngoai.txt`): vẫn bị chặn (có test).

Bật/tắt mạng đổi được bất cứ lúc nào (`PATCH /sandboxes/:id`, `sbx_update`, hoặc
công tắc trong UI). Với `direct` có hiệu lực ngay lần chạy kế tiếp vì
profile/args được dựng lại mỗi lần chạy; với `docker` container được tạo lại vì
`--network` cố định ở `docker run`. UI hỏi lại khi **bật** (chiều nới lỏng)
và không hỏi khi tắt. Kiểm chứng thật: bật → `CONNECTED`, tắt →
`Operation not permitted`.

Terminal tương tác qua WebSocket `/sandboxes/:id/terminal`, cùng giao thức
frame với terminal của `apps/code-ide` (`{"d":…}` / `{"r":[cols,rows]}`). Shell
chạy dưới đúng lớp cách ly như lệnh script — một terminal bỏ qua sandbox sẽ là
đường vòng qua mọi giới hạn khác.

## Bẫy đã gặp và cách xử lý

| Bẫy | Xử lý |
|---|---|
| **Loopback vô hiệu hoá cả sandbox.** `network: true` bao gồm `127.0.0.1`, mà trên loopback có REST API không xác thực của chính daemon — mã trong sandbox nhờ daemon đọc hộ file bị cấm, và tự tạo sandbox thứ hai mount cả đĩa. Đo được trên daemon sống | `(deny network-outbound (remote ip "localhost:*"))` phát **cuối cùng**, kể cả khi mạng bật; chỉ trả lại cổng khai trong `ports.loopback`. Test dựng listener thật để canh |
| **DNS trên macOS không đi qua cổng 53** — `connect:[53,443]` vẫn không phân giải nổi tên miền, vì `getaddrinfo` hỏi mDNSResponder qua Unix socket | Cấp `(allow network-outbound (literal "/private/var/run/mDNSResponder"))`, **chỉ** cho sandbox vốn đã có quyền ra ngoài |
| **Server nền khởi động bằng `cmd &` bị giết.** `exec` giữ nguyên tới hạn rồi kill cả process group — server chết theo | Chạy `( cmd < /dev/null > log 2>&1 & )`; `exec` trả về ngay và server sống sót. Đã ghi vào skill |
| **Mount thư mục app read-only rồi mong app chạy.** Thứ gì ghi cạnh mã của nó (sqlite, lock, cache) sẽ chết | Copy app vào workspace ghi được, chỉ mount *dữ liệu* read-only |
| **`/private/var/folders` từng nằm trong allow-list ghi.** Đó là temp/cache theo user của macOS, chứa container và saved state của app khác — sandbox ghi được vào đó là thoát thật. Test end-to-end bắt được | Bỏ khỏi allow-list; `TMPDIR` trỏ vào trong sandbox nên không cần |
| Seatbelt khớp trên đường dẫn **đã resolve**; `/tmp` và home trên macOS là symlink | `canonicalize()` workdir trước khi đưa vào profile |
| Workdir nằm dưới `~/.senclaw`, mà `~/.senclaw` bị deny đọc | Mở lại workdir **sau** luật deny (luật sau thắng) |
| Timeout giết `sh` nhưng cháu chắt vẫn chạy | `setsid()` qua `pre_exec`, rồi `kill(-pid)` cả process group |
| Timeout trong docker: giết client `docker exec` không giết tiến trình **trong** container | `docker restart` — workdir là bind mount, gói đã cài nằm ở writable layer, không mất gì |
| `&s[..N]` panic khi cắt giữa ký tự nhiều byte — output tiếng Việt là chỗ đó xảy ra | `clamp()` lùi về ranh giới ký tự |
| `bwrap` gắn mount theo thứ tự: bind workdir trước rồi tmpfs home sẽ **giấu mất** workdir | tmpfs home trước, bind workdir sau |
| Row DB nói "running" trong khi container đã bị `docker stop` từ ngoài | Đối chiếu lại trong `GET /sandboxes/:id` (không làm ở list — sẽ thành một lệnh docker mỗi dòng) |
| `SANDBOX_DATA_DIR` là biến môi trường toàn tiến trình; test chạy song song đạp nhau | Một data root dùng chung cho cả test binary qua `OnceLock` |
| **Probe gộp làm mọi lần chạy đợi Docker.** `direct_kind()` gọi probe đầy đủ, mà probe đầy đủ hỏi cả daemon Docker — trên máy Docker hỏng, một đoạn Python 38 ms mất **4,06 giây**. Đo mới thấy | Tách cache `direct_caps` (không spawn tiến trình) khỏi `docker_caps`; `create` cũng chỉ probe đúng thứ cần. Còn **0,03 s**. Có test canh mốc 3 s |
| `Sum for f64` trong Rust fold từ `-0.0`, nên sandbox rảnh serialize thành `-0.0` và UI hiện "-0.0 %" | `+ 0.0` sau `.sum()` |
| `target` kiểu `/data` bị trim thành `data` trong im lặng — người dùng tưởng mount ở `/data` trong container, thật ra ở `/work/data` | Từ chối hẳn `target` tuyệt đối, kèm gợi ý viết lại |
| **Luật "cổng đặc quyền" áp nhầm cho cả chiều ra.** Cấm bind cổng <1024 là đúng (cần root), nhưng đem áp cho `connect` thì `connect:[443]` — luật hữu ích nhất, chính là HTTPS — bị từ chối. 5 test đỏ cùng lúc vì một lỗi này | Chỉ áp cho `listen` |
| **`PUT /exec-policy` thiếu trường thì reset trường đó**, vì `serde(default)` ở mức struct | UI gửi đủ object; gọi tay thì `GET` rồi merge |
| Test mở server trong sandbox để lại tiến trình mồ côi giữ cổng, lần chạy sau `Address already in use` | `allow_reuse_address` + `timeout` phía server để nó tự thoát khi không ai gõ cửa |
| Tooltip của biểu đồ vẽ đè lên dòng tiêu đề | Chừa sẵn chiều cao tooltip phía trên vùng vẽ; kẹp vị trí ở hai mép |
| **Cột mới không tới được DB đã tồn tại.** `schema.sql` chạy với `IF NOT EXISTS`, nên cột thêm vào `CREATE TABLE` chỉ có ở DB mới; DB cũ giữ hình dạng cũ và câu lệnh đầu tiên gọi tên cột mới sẽ lỗi lúc chạy | `migrate()` đọc `PRAGMA table_info` rồi `ALTER TABLE ADD COLUMN` cho phần thiếu; chỉ thêm, không sửa/xoá |
| Thư mục tạm trên macOS tới được qua symlink, nên allowlist so trên đường dẫn đã resolve mới khớp | `canonicalize` trước khi so (test allowlist làm đúng vậy) |
| **Bộ lọc "không phải đường dẫn tuyệt đối thì bỏ"** (định bỏ `open(4)` trên fd) đã vứt luôn mọi đường dẫn **tương đối** — tức đúng những lần đọc đáng quan tâm, vì mã trong sandbox mở file của nó bằng đường dẫn tương đối. Test end-to-end bắt được | Chỉ bỏ khi target **toàn chữ số** |
| `cargo test` không làm mới `target/debug/<bin>`, nên chạy binary sau khi test là chạy bản cũ | Luôn `cargo build` trước khi khởi động lại để kiểm chứng thủ công |

## Kiểm chứng

**158 hàm test** trong `src/sandbox/` (`cargo test`, lọc `cargo test -- sandbox`;
Space App có bộ riêng 152 test, `cargo test -p sandbox`). Trong đó `ports.rs` 14
test, `runner.rs` 33 test — nhóm **thật sự chạy mã dưới Seatbelt/bwrap** tự bỏ
qua (in dòng SKIP) trên máy không có cách ly:

- Python chạy và trả đúng output
- đoạn mã đầy dấu nháy giữ nguyên
- ghi trong sandbox: được
- ghi ngoài sandbox: **bị chặn**
- vòng lặp vô tận bị giết đúng hạn
- mạng bị chặn khi sandbox tắt mạng
- `ANTHROPIC_API_KEY` của tiến trình cha **không** thấy được từ trong sandbox
- thư mục gắn đọc-ghi: đọc được và ghi ngược ra file thật
- thư mục gắn chỉ-đọc: ghi bị chặn, file thật không đổi
- trình duyệt file đi vào được mount nhưng không đi ra khỏi nó
- một lần chạy `direct` không đợi probe Docker (canh dưới 3 giây)
- `strict` chặn đọc file ngoài sandbox, **và** Python vẫn `import` được stdlib
- `allowlist` mở đúng thư mục đã khai, không mở thư mục bên cạnh
- `open` đọc được — nếu không thì phép so sánh với `strict` vô nghĩa
- thư mục đã gắn vẫn đọc được ở `strict`
- theo dõi bắt được cả bốn loại từ một lần chạy thật: ghi file, đọc file, khởi
  tạo tiến trình kèm argv, kết nối `1.1.1.1:53`, tra tên miền
- lần chạy thứ hai không phát lại sự kiện của lần thứ nhất (offset)
- tắt theo dõi thì **không** để lại thư mục `.trace` nào trong sandbox
- so sánh thư mục bắt được file do lệnh shell tạo (không qua Python/Node)
- mở cổng 18771 rồi **chạy hẳn một HTTP server trong sandbox và gọi được từ máy
  chủ** — trả về đúng nội dung
- cổng không khai báo thì **không bind được**
- `connect` chỉ mở đúng cổng từ xa đã khai, cổng khác vẫn chặn
- **dịch vụ trên chính máy này không với tới được dù mạng đang bật** — test dựng
  một listener thật rồi kiểm tra sandbox bị từ chối, và kiểm tra khai cổng đó
  vào `loopback` thì trả lại đúng một dịch vụ ấy
- resolver chỉ được cấp cho sandbox có quyền ra ngoài
- bật/tắt mạng trên sandbox **đã tồn tại** có hiệu lực ngay ở lần chạy kế tiếp —
  hàng rào được dựng lại mỗi lần chạy chứ không dựng một lần rồi cache
- shell của agent không với tới dịch vụ local nào cho tới khi khai một cổng
  (`the_agent_shell_reaches_no_local_service_until_one_is_named`)

Kiểm chứng chạy thật trên máy macOS (2026-08-01, Docker Desktop đang hỏng):

```
$ echo pwned > /tmp/x            → Operation not permitted
$ ls ~/.ssh                      → Operation not permitted
$ ls ~/.senclaw                  → Operation not permitted
$ echo pwned > ~/sbx_escape.txt  → Operation not permitted
```

Cả qua REST, qua MCP `sbx_run`, và qua terminal WebSocket.

**Đợt đo thứ hai, 2026-08-06** — đưa **app thật** vào sandbox (Python
`http.server`; Express 5 + sqlite3 native module) và kiểm chứng ba ràng buộc
"chỉ vài thư mục / chỉ truy cập local / chỉ tới một web": 19/19 phép đo khớp kỳ
vọng, và chính đợt này tìm ra lỗ loopback cùng bẫy DNS ở trên. Báo cáo:
[`docs/sandbox-security-experiment.md`](sandbox-security-experiment.md), script
tái lập: `scripts/sandbox-experiment/`.

## Tầng thứ hai: sandbox cho Space App

Engine ở trên phục vụ **mã do agent chạy** — phiên dùng-một-lần, workspace riêng,
đường dẫn được ánh xạ lại. Space App là chuyện khác: một **tiến trình sống lâu**
do daemon khởi chạy, phục vụ một cổng, và tự tính thư mục dữ liệu của nó từ
`$HOME` lúc khởi động. Vì vậy nó có một tầng riêng, dùng chung bộ dựng profile
nhưng khác ba điểm nền tảng:

| | Engine (`sbx_*`) | Space App |
|---|---|---|
| Vòng đời | một lần chạy | tiến trình sống lâu, có supervisor |
| Đường dẫn | ánh xạ vào workspace (`/work/...`) | **giữ nguyên đường thật** |
| Cấu hình | tham số mỗi lần chạy | lưu theo app, cố định **lúc khởi chạy** |
| Mạng "chỉ vài trang" | công thức tự dựng | **proxy allowlist đi kèm** |

Đường dẫn **không** được ánh xạ lại vì app đã tính sẵn thư mục dữ liệu từ `$HOME`;
đưa nó sang `/work/data` thì app không tìm thấy gì cả. Nên mọi thư mục được cấp
đều giữ đúng đường thật (macOS: rule theo path; Linux: bind `source == destination`).

Mã: [`src/sandbox/app_policy.rs`](../src/sandbox/app_policy.rs) (cấu hình + kiểm
tra hợp lệ), [`src/sandbox/app_launch.rs`](../src/sandbox/app_launch.rs) (dựng
lệnh khởi chạy), [`src/sandbox/proxy.rs`](../src/sandbox/proxy.rs) (proxy
allowlist). Hướng dẫn dùng: [`docs/space-app-sandbox.md`](space-app-sandbox.md).

### Hai bẫy chỉ lộ ra khi chạy app thật

**Cấp thư mục con chưa đủ — phải qua được thư mục cha.** Chế độ đọc `open` cấm cả
cây `~/.senclaw` rồi allow lại thư mục dữ liệu của app. Mở một file thì kernel
phân giải *mọi* thành phần đường dẫn, nên SQLite chết `SQLITE_CANTOPEN` dù đúng
thư mục đó đã được cấp — trong khi `ls` và `sqlite3` CLI trên cùng profile lại
chạy được, đủ để đổ nhầm cho app. Vá: mỗi đường dẫn được cấp mà có tổ tiên nằm
trong cây bị cấm thì tổ tiên đó được trả lại **đúng quyền metadata**
(`file-read-metadata`), không phải nội dung. Nhánh đọc `strict` không dính vì nó
vốn đã `(allow file-read-metadata)` toàn cục.

**`strict` cắt luôn runtime của app.** Node cài bằng nvm nằm dưới `$HOME`, đúng
chỗ `strict` bỏ đi → `EPERM .../npm-cli.js`. Vá: ở chế độ đọc bị nhốt, mọi mục
`PATH` **ngoài** `SYSTEM_READ_ROOTS` được cấp chỉ-đọc ở mức *thư mục cài* (không
chỉ `bin`, vì `npm-cli.js` nằm ở `../lib/node_modules`). Suy từ `PATH` chứ không
phải danh sách tên nvm/volta/pyenv — danh sách đó sẽ mục.

## Vòng đời tiến trình Space App

Ba luật, mỗi luật là lời giải cho một trạng thái đo được trên máy thật (47 app
chạy mồ côi, có tiến trình 299 giờ, và **không** app nào thực sự bị sandbox nhốt
dù cấu hình bảo có):

1. **Daemon phải bắt SIGTERM.** Trước đây `run_daemon` chỉ chờ `ctrl_c()`, trong
   khi app desktop tắt daemon bằng `kill -TERM` rồi SIGKILL sau 800 ms — khối
   shutdown chưa từng chạy theo cách người ta thật sự thoát app.
2. **Shutdown phải song song.** Giết tuần tự với 2 giây ân hạn mỗi app thì vài
   chục app cần cả phút, không thể lọt cửa sổ 800 ms. Nay: SIGTERM cho tất cả →
   chờ **một lần** 300 ms → SIGKILL phần còn lại (đo được ~300 ms tổng).
3. **Cổng của app phải đòi lại, không nhận nuôi.** Thấy cổng cố định còn khoẻ mà
   dùng luôn nghĩa là app chạy mã cũ, thư mục cũ, ngoài mọi sandbox, vô thời hạn.
   Nay daemon giết tiến trình đó rồi khởi chạy lại — **chỉ khi** thư mục làm việc
   của nó (`lsof -d cwd`) nằm trong thư mục cài app. Không xác minh được thì để
   yên và ghi log: dev server của người dùng trùng cổng không được phép trở thành
   thứ bị giết mỗi lần khởi động.

Supervisor học cùng bài học: "cổng có trả lời" **không** đồng nghĩa khoẻ. Cổng
trả lời mà không có bản ghi con là tiến trình lạ → đòi lại **một lần cho mỗi lần
daemon chạy** (đòi không được thì ghi nhận, không thử lại mỗi nhịp).

Trạng thái này hiển thị ở màn theo dõi
([`docs/space-app-monitor.md`](space-app-monitor.md)): `ngoài daemon` +
`không rõ` + `cần khởi động lại`, chứ không giả vờ là `seatbelt`.

## Đóng gói

Bản core không đóng gói riêng — nó biên dịch cùng binary `senclaw`, phục vụ MCP
qua subcommand `sandbox-server` (đăng ký trong `.mcp.json`) và được đẩy vào
roster tool của agent lúc dựng phiên.

Space App vẫn giữ đường cũ: `apps/sandbox/scripts/pack.sh` → `sandbox-app.zip`
(binary + manifest + skills + personas + `web_dist/`), giống mọi Space App khác.
