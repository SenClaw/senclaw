# Nghiên cứu: đồng bộ dữ liệu của Space App giữa hai SenClaw qua SLink (ví dụ ssh-manager)

Ngày 2026-10-22. Chỉ nghiên cứu, chưa viết code. Mã nguồn tham chiếu: senclaw `main` = v0.2.4 (7494c12), `app-ssh` 23d472b (hub version 1.2.0), `app-space-sdk` trong worktree `senclaw-main-merge`.

## 1. Kết luận ngắn

- **Daemon lo đường truyền, niềm tin và sự đồng ý; app lo ý nghĩa dữ liệu.** Daemon không hiểu "host" hay "keychain"; app không biết mạng, ghép cặp hay mã hoá. Hai bên gặp nhau ở một *hợp đồng đồng bộ* nhỏ: app khai trong manifest (khối `sync`) và phục vụ 4 endpoint cục bộ; daemon chở các lời gọi đó qua kênh SLink đã mã hoá bằng các phương thức mới `appsync/*`.
- **Đồng bộ theo trạng thái, không theo nhật ký thay đổi.** Hai bên trao đổi bản tóm tắt `(id, hlc)` của từng collection, so, rồi kéo/đẩy đúng các bản ghi lệch. Không cần hàng đợi khi bên kia offline: lần sau kết nối là tự bắt kịp. Mỗi bản ghi mang `id` ổn định, đồng hồ lai `hlc`, và bia mộ khi xoá; xung đột giải bằng "bản mới hơn thắng" ở mức bản ghi.
- **Hai đầu cùng mở thì mới chảy.** Mỗi bên chỉ quyết định cho phía mình (gửi / nhận, theo từng app, từng collection, từng peer). Dữ liệu chỉ đi khi `gửi` của bên này và `nhận` của bên kia cùng bật. Mặc định đóng, giống chính sách tri thức đã có.
- **Bằng chứng từ máy này làm đổi trọng tâm: mật khẩu nằm trong `hosts`, không nằm trong keychain.** 14 host, cả 14 có mật khẩu ghi thẳng trong `hosts.json`; 0 host dùng `keychain_id`; file `keychain.json` không tồn tại. Nếu chỉ "bảo vệ keychain" thì không bảo vệ được gì. Vì vậy trường bí mật phải được **khai theo trường** (`secretFields`) và daemon tự cắt ở lớp chính sách, không tin app.
- **Rủi ro lớn nhất không phải rò rỉ mà là chuyển hướng thông tin xác thực.** Bên kia đổi `host` của một máy đã có mật khẩu sang máy của kẻ tấn công; lần sau agent "ssh vào" là gửi mật khẩu đi. Quy tắc bắt buộc: thay đổi địa chỉ/user của bản ghi có thông tin xác thực luôn vào hộp duyệt, kể cả chế độ tự áp dụng; bản ghi nhận về không bao giờ tự gắn vào bí mật cục bộ.
- **Không có việc nào trong đây tự mở cổng.** Quy tắc port-forward nhận về luôn `active=false`; trường chạy-thật (`active`) không bao giờ rời máy và không bao giờ bị ghi đè.
- **Khuyến nghị làm theo bậc:** (P0) app ssh-manager có siêu dữ liệu + endpoint đồng bộ, thử bằng hai bản cục bộ; (P1) daemon: chính sách, phương thức `appsync/*`, thương lượng khả năng, API; (P2) giao diện web/desktop: chọn app, xem trước, đồng bộ, hộp duyệt; (P3) phát hành ssh-manager 1.3.0 với hosts + snippets + port-forward, bí mật tắt; (P4) bật bí mật theo từng peer và SDK dùng chung để app thứ hai dùng lại.

## 2. Bạn muốn luồng nào (diễn đạt lại để chốt phạm vi)

1. Hai máy cùng cài ssh-manager.
2. Hai SenClaw đã ghép cặp qua SLink (đã có).
3. Trên mỗi máy, người dùng vào **SLink → peer → Ứng dụng**, thấy "ssh-manager: cả hai bên đều có, đều hỗ trợ đồng bộ", chọn collection nào (hosts, keychain, port-forwarding, snippets), chọn hướng, xem trước, bấm đồng bộ.
4. Từ đó hai bên giữ dữ liệu giống nhau; sửa/xoá/thêm bên này thì bên kia thấy.
5. Nếu app (hoặc daemon) chưa hỗ trợ, có đường cập nhật rõ ràng cho cả hai máy.

Không nằm trong phạm vi: đồng bộ qua Internet/hub (SLink chỉ LAN), đồng bộ cho app không khai `sync`, để agent tự bấm đồng bộ.

## 3. Hiện trạng (bằng chứng)

### 3.1 Dữ liệu của ssh-manager

Thư mục dữ liệu cố định `~/.senclaw/space-apps-data/ssh-manager/` (`dirs::home_dir()`), mỗi collection một file JSON, ghi lại toàn bộ sau mỗi thay đổi.

| Collection | Khoá | Tham chiếu | Trường bí mật | Trường theo máy | Mốc thời gian | Đã có trên máy này |
|---|---|---|---|---|---|---|
| `hosts` | UUID cấp lúc tạo | `keychain_id` → keychain | **`password`** (tuỳ chọn) | — | **không có** | 14 host, **14 có mật khẩu**, 0 dùng keychain |
| `keychain` | UUID | — | **`value`** (mật khẩu hoặc khoá riêng, dạng văn bản) | — | không có | 0 (file chưa tạo) |
| `port_forwarding` | UUID | `host_id` → hosts | — | **`active`** (đang mở thật; app tự mở lại lúc khởi động) | không có | 1 quy tắc, 0 đang chạy |
| `snippets` | UUID | — | — | — | `created_at` (ms), duy nhất | 3 |
| `settings` | (một đối tượng) | — | — | gần như toàn bộ (chính sách lệnh, retention, theme) | không có | 1 |

Hệ quả cho thiết kế:

1. **Không có `updatedAt`, không có bia mộ.** `delete` là xoá cứng. Đồng bộ không phân biệt "bên kia xoá" với "bên này mới thêm". Cần siêu dữ liệu mới và một lần di trú.
2. **ID là UUID cấp riêng ở từng máy.** Cùng một host gõ tay ở hai máy có hai ID khác nhau → nhân đôi ngay lần đồng bộ đầu nếu không có bước ghép theo khoá tự nhiên.
3. **Có tham chiếu chéo** (host→keychain, quy tắc→host) nên thứ tự áp dụng và xử lý tham chiếu treo là việc của đồng bộ, không phải chi tiết phụ.
4. **Bí mật nằm ở hai nơi** (`hosts.password`, `keychain.value`), đều là văn bản thuần, và cả bốn file trên máy này mang quyền `0644` (mọi người dùng cục bộ đọc được). Đây là phát hiện phụ, không thuộc phạm vi đồng bộ nhưng nên sửa khi đụng vào lớp lưu trữ (`0600`).
5. `settings` chứa chính sách an ninh của từng máy (`ssh_command_policy`, danh sách cho phép/cấm lệnh). **Một peer không bao giờ được nới chính sách này** → loại khỏi đồng bộ.
6. Dữ liệu nhỏ (hàng chục đến vài trăm bản ghi), nên bản tóm tắt đầy đủ vài chục KB là chấp nhận được; chưa cần cây băm.

Chưa kiểm chứng: ssh-manager **không** nằm trong `~/.senclaw/apps` của máy này (thư mục dữ liệu thì có, ghi lần cuối 2026-10-20 22:00), tức nó đang chạy theo đường khác (đăng ký cục bộ). Cần xác nhận lại khi thử cập nhật qua hub.

### 3.2 Hợp đồng Space App hôm nay

- App là tiến trình riêng nói chuyện với daemon qua HTTP loopback. Daemon cấp mỗi app một token `sca_…`; proxy `/api/space/apps/:id/proxy/*` **tự đóng dấu** `X-SenClaw-App-Token` và `X-SenClaw-Api-Version` lên mọi request gửi vào app, và gỡ header cùng tên do client gửi. App bật `require_app_token` thì chỉ còn daemon gọi được (mặc định tắt; ssh-manager hiện không bật).
- Nếu app chưa chạy, proxy gọi `ensure_running` (giành lại cổng, chờ khoẻ) rồi chuyển tiếp: đây là đường gọi vào app có sẵn, tái dùng được cho đồng bộ.
- **App không có API nào để nói chuyện với peer.** SDK (`SpaceClient`) chỉ có LLM, tri thức, mô hình, usage. Không có "danh sách peer", không có "gọi peer".
- Cập nhật app: hub phát hành phiên bản bất biến `name@version`; máy người dùng gọi `GET /api/space/apps/updates` rồi `POST /api/space/apps/:id/update`. Manifest cần có nguồn hub; `senclaw-hub.json` của ssh-manager ghi `updater: none` (cần kiểm tra thực tế đường cập nhật này chạy được với ssh-manager).
- Manifest được phân tích khoá-từng-khoá (`src/apps/manifest.rs`), nên thêm khối `sync` là thay đổi cục bộ, các app cũ không bị ảnh hưởng.

### 3.3 SLink hôm nay

- Kênh đã niêm phong (SPAKE2 → khoá cặp; AES-256-GCM; AAD gắn chiều và cả hai daemon id; id request + cửa sổ đồng hồ + chống phát lại). Phương thức nằm trong `handle()` (`src/sen_link/server.rs:215`): `SendMessage/GetTask/ListTasks/CancelTask` và `knowledge/recall`. Phương thức lạ trả `-32601`: tín hiệu "không hỗ trợ" sạch.
- Chính sách theo peer nằm trong `peers.json` (`autoRun`, `assignee`, `knowledge`), mặc định đóng. Khả năng được quảng bá bằng tham số của extension trong Agent Card (`knowledge: <version>`). Đã có mã lỗi `-32010` (không chia sẻ), `-32011` (quá nhiều yêu cầu).
- Giới hạn kênh đo ngày 20/10: thân request ≤ 1 MiB (thực tế ~700 KB), trả lời ≤ 5 MiB; RTT p50 5 ms. Lô đồng bộ nên ≤ ~500 KB.
- `LinkState` đã có chỗ cho dịch vụ tuỳ chọn (`knowledge: Option<Arc<…>>`, gắn bằng `with_knowledge`) → mẫu để gắn `app_sync`.

## 4. Các phương án

| # | Phương án | Ưu | Nhược | Đánh giá |
|---|---|---|---|---|
| A | **Daemon trung gian, app định nghĩa dữ liệu** (khuyến nghị) | Một nơi lo mã hoá, đồng ý, giới hạn, nhật ký; app không cần mạng/khoá; dùng lại được cho mọi app; kênh đã kiểm chứng | Cần hợp đồng mới (manifest + endpoint) và sửa từng app | Chọn |
| B | App tự nói chuyện SLink (SDK thêm API peer) | App linh hoạt | Mỗi app có thể gửi gì cũng được tới peer; bề mặt tấn công lớn; chính sách phân tán | Loại |
| C | Daemon chép nguyên thư mục dữ liệu của app (kiểu rsync) | Không sửa app | Không hợp nhất ngữ nghĩa, ghi đè cả trường theo máy (`active`, cài đặt an ninh), mang theo đường dẫn tuyệt đối, đồng bộ bí mật nguyên khối | Loại |
| D | CRDT đầy đủ (Automerge/Yjs) | Hội tụ không cần tính toán thủ công | Nặng cho vài trăm bản ghi; ép app đổi cách lưu; khó xem trước và khó cắt trường bí mật | Chưa cần |
| E | Hub/máy chủ trung tâm | Dễ | Phá thiết kế "chỉ LAN, không hub" mà bạn đã chọn | Loại |
| F | Đồng bộ chung cho app lưu trong `space_app_config`/sqlite do daemon giữ | Không cần app làm gì | ssh-manager lưu JSON riêng; chỉ phục vụ nhóm app khác | Ý tưởng sau |

## 5. Thiết kế khuyến nghị

### 5.1 Đường đi của một lần đồng bộ

```
 ssh-manager (A) ◄─ loopback + token ─► daemon A ◄══ SLink niêm phong ══► daemon B ◄─ loopback + token ─► ssh-manager (B)
   /sync/* (4 endpoint)                  chính sách, cắt bí mật,             chính sách, cắt bí mật,              /sync/*
                                         giới hạn, nhật ký, phối hợp         giới hạn, nhật ký
```

Một lời gọi từ daemon A tới app A dùng đúng đường proxy hiện có (đóng dấu token, tự khởi động app nếu đang nghỉ). Một lời gọi tới app B là `appsync/*` qua kênh niêm phong, daemon B kiểm tra chính sách rồi gọi app B cục bộ. App không bao giờ thấy mạng.

### 5.2 Khai báo trong manifest (khối `sync`)

```json
"sync": {
  "version": 1,
  "collections": [
    { "id": "keychain",        "secretFields": ["value"],    "matchKey": ["name", "item_type"] },
    { "id": "hosts",           "secretFields": ["password"], "matchKey": ["host", "port", "user"],
      "refs": { "keychain_id": "keychain" },
      "guardFields": ["host", "port", "user", "keychain_id"] },
    { "id": "port_forwarding", "localFields": ["active"],
      "refs": { "host_id": "hosts" },
      "matchKey": ["host_id", "local_port", "destination_address", "destination_port"] },
    { "id": "snippets",        "matchKey": ["name"] }
  ],
  "order": ["keychain", "hosts", "port_forwarding", "snippets"],
  "endpoint": "/sync"
}
```

- `secretFields`: daemon tự cắt các trường này ở cả đường gửi và đường nhận khi chính sách `secrets` tắt. Không phụ thuộc việc app có làm đúng.
- `localFields`: không bao giờ rời máy, không bao giờ bị `apply` ghi đè.
- `refs`: tham chiếu giữa các collection (để thứ tự áp dụng và kiểm tra treo).
- `matchKey`: khoá tự nhiên để ghép các bản ghi cùng nghĩa nhưng khác ID ở lần đồng bộ đầu (xem 5.5).
- `guardFields`: thay đổi các trường này trên bản ghi *đang có thông tin xác thực* luôn bị giữ lại để duyệt (xem 5.8).
- `settings` cố ý **không** có trong danh sách: collection nào không khai thì không đồng bộ được.
- Daemon từ chối liên kết nếu app khai `sync` nhưng endpoint `/sync/*` trả 200 khi gọi **không** kèm token (kiểm tra chủ động; app phải bật `require_app_token`).

### 5.3 Bốn endpoint app phải phục vụ (chỉ daemon gọi được)

| Endpoint | Việc | Ghi chú |
|---|---|---|
| `GET /sync/schema` | trả khối `sync` + `schemaVersion` của từng collection | để daemon đối chiếu với manifest |
| `POST /sync/state` `{collections}` | trả `[ {id, hlc, deleted} ]` từng collection | bản tóm tắt; không có nội dung, không có bí mật |
| `POST /sync/read` `{collection, ids}` | trả bản ghi đầy đủ (kể cả bia mộ), đã bỏ `localFields` | daemon cắt tiếp `secretFields` theo chính sách |
| `POST /sync/apply` `{records, dryRun, from}` | hợp nhất vào kho cục bộ; trả theo từng bản ghi `applied / skipped / conflict / held / error`, kèm lý do và cờ rủi ro | `dryRun:true` chỉ tính kế hoạch (dùng cho xem trước); **idempotent** |

Hợp đồng quan trọng của `apply`: nó tự quyết bằng quy tắc tất định (so `hlc`, hoà thì so `deviceId`), nên chạy hai lần hay chạy theo thứ tự khác vẫn ra cùng trạng thái. Daemon không cần phân xử.

### 5.4 Mô hình bản ghi và hợp nhất

Mỗi bản ghi gửi đi có dạng:

```json
{ "collection": "hosts", "id": "uuid", "hlc": "0192f…-0003-<deviceId>", "deleted": false,
  "data": { "name": "...", "host": "...", "port": 22, "user": "...", "tags": [] },
  "extra": { } }
```

- **`hlc` (đồng hồ lai):** (mili-giây vật lý, bộ đếm, deviceId). Mỗi lần ghi cục bộ: `hlc = max(now, last)` rồi tăng bộ đếm. Nhận bản ghi thì đẩy đồng hồ cục bộ lên không thấp hơn `hlc` nhận. Hệ quả: không phụ thuộc đồng hồ hai máy khớp nhau; thứ tự toàn phần tất định; hoà không thể xảy ra vì có `deviceId`. Từ chối bản ghi có phần vật lý vượt quá `now` cục bộ hơn một khoảng nhỏ (cùng cửa sổ mà kênh niêm phong đã áp), để một máy sai giờ không thắng mọi xung đột mãi.
- **Hợp nhất mức bản ghi (v1):** `hlc` lớn hơn thắng, kể cả bia mộ. Mức trường (hai bên sửa hai trường khác nhau của cùng host đều giữ) là nâng cấp sau vì đòi `hlc` từng trường; ghi rõ trong giao diện: sửa đồng thời cùng một bản ghi thì bản sau thắng.
- **Bia mộ:** xoá thành `{id, deleted:true, hlc}` và giữ N ngày (đề xuất 90). Peer vắng mặt lâu hơn N ngày → lần sau phải đồng bộ lại "đầy đủ có duyệt" vì không thể phân biệt "bản này chưa từng có" với "đã bị xoá rồi dọn bia mộ" (nguy cơ hồi sinh).
- **Giữ trường lạ (`extra`):** app bản cũ phải đi vòng qua các trường mới của bản ghi (`#[serde(flatten)]`), nếu không đồng bộ giữa 1.3 và 1.4 sẽ âm thầm làm rụng trường.
- **Bản ghi bắc cầu:** vì bản ghi mang `hlc` và `deviceId` gốc, A↔B và B↔C hội tụ cả A↔C dù A và C không ghép cặp.

### 5.5 Lần đồng bộ đầu: ghép bản ghi cùng nghĩa

Hai máy đã có dữ liệu riêng, ID khác nhau. Nếu cứ nhập thì nhân đôi. Cách làm:

1. Lần đầu (chưa có liên kết ID), `apply` dùng `matchKey` để tìm cặp "cùng nghĩa" (ví dụ cùng `host:port:user`).
2. Xem trước liệt kê: *mới ở bên kia*, *mới ở bên này*, *trùng nghĩa, ID khác* (đề xuất gộp), *khác nội dung*.
3. Gộp nghĩa là chọn một ID chuẩn tất định (ID nhỏ hơn theo thứ tự chữ) và **viết lại các tham chiếu** tới ID cũ trong cùng một giao dịch ở phía app (host→keychain, quy tắc→host).
4. Mặc định chính sách lần đầu cho cả collection: *giữ bản mới hơn* / *giữ của tôi* / *lấy của họ* — người dùng chọn trong bước xem trước, không phải đoán.

### 5.6 Toàn vẹn tham chiếu và thứ tự

- Áp dụng theo `order` của manifest: keychain → hosts → port_forwarding → snippets.
- Tham chiếu tới thứ không được đồng bộ hoặc không tồn tại ở máy nhận (người dùng tắt keychain, hoặc keychain chưa có) không được để treo: trường tham chiếu bị xoá và bản ghi gắn nhãn "cần nhập thông tin xác thực trên máy này"; xem trước báo số lượng.
- Xoá một bản ghi bị tham chiếu: tham chiếu của bản ghi khác được dọn theo, y như xoá cục bộ.

### 5.7 Bí mật

- Hôm nay bí mật là văn bản thuần ở rest (cả trong `hosts.password`). Kênh SLink bảo vệ khi truyền (AES-256-GCM, khoá cặp). Điểm yếu là hai đầu, không phải đường truyền.
- Chính sách `secrets` là công tắc riêng theo (peer, app), **mặc định tắt**, bật bằng xác nhận riêng ("sẽ gửi mật khẩu và khoá riêng của N máy sang <peer>"). Xem trước chỉ hiện tên, không bao giờ hiện giá trị.
- Khi tắt: bên gửi cắt `secretFields`; bên nhận *giữ nguyên* bí mật cục bộ nếu đã có (trường vắng không phải lệnh xoá). Bản ghi mới đến không kèm bí mật → hiện "cần nhập mật khẩu trên máy này". Đủ để chia sẻ danh sách máy mà không chia sẻ thông tin vào máy.
- Khi bật: lớp chính sách của **cả hai** daemon phải cho phép; mỗi bên vẫn cắt nếu phía mình chưa bật. Bí mật nhận về lưu cùng cách như bí mật cục bộ (đồng bộ không làm tệ đi, cũng không làm tốt lên; việc mã hoá ở rest là việc riêng, xem 8).
- Chưa làm: mã hoá bí mật ở rest bằng khoá trong Keychain của hệ điều hành; đây là đường cải thiện đúng nhưng nằm ngoài đồng bộ.

### 5.8 Mô hình đe doạ (theo những gì mã thật lưu và phơi ra)

Peer SLink hôm nay đã có thể gửi *việc* cho agent; nếu `autoRun` bật thì agent chạy ngay. Đồng bộ không thêm quyền ngang tầm đó, nhưng thêm các đường này:

| Mối đe doạ | Cách chặn |
|---|---|
| Đổi `host` của máy có mật khẩu sang máy kẻ tấn công → lần sau agent/người dùng ssh là gửi mật khẩu | `guardFields`: đổi `host/port/user/keychain_id` trên bản ghi đang có thông tin xác thực → luôn **giữ để duyệt**, kể cả chế độ tự áp dụng; cảnh báo hiển thị tên và địa chỉ cũ→mới |
| Bản ghi nhận về trỏ `keychain_id` vào bí mật cục bộ | Tham chiếu chỉ được giải trong cùng lô đồng bộ; id không tồn tại cục bộ-từ-peer bị xoá (5.6); không bao giờ "đoán" id |
| Quy tắc port-forward tự mở cổng nghe (`bind_address` 0.0.0.0) | Luôn nhận `active=false`; `active` thuộc `localFields`; `restore_saved_tunnels` không bị đồng bộ kích hoạt |
| Snippet nhận về chứa lệnh nguy hiểm, agent chạy nhầm | Snippet là dữ liệu; đánh dấu nguồn `peer`; chế độ mặc định *duyệt*; ssh-manager vẫn áp `ssh_command_policy` cục bộ (không đồng bộ) lên mọi lệnh |
| Tên/tag máy chứa lời chèn prompt (hiện trong tool MCP cho agent) | Dữ liệu từ peer đánh dấu không tin cậy giống `link_recall`; bỏ qua trong chỉ dẫn |
| Peer làm cạn tài nguyên (kéo liên tục, lô khổng lồ) | Giới hạn tần suất (-32011), trần kích thước lô, số bản ghi tối đa mỗi collection |
| Peer nới chính sách an ninh của ssh-manager | `settings` không có trong `sync.collections` |
| Chính sách mở từ hai phía bị nhầm | Hai đầu độc lập: gửi (bên này) AND nhận (bên kia); không có "thương lượng" để lừa |
| App lỗi/độc hại trả dữ liệu không đúng schema | Daemon kiểm schema (`/sync/schema` so với manifest), cắt trường theo khai báo, giới hạn kích thước |

### 5.9 Chính sách và đồng ý

Trong `peers.json`, cạnh `knowledge`:

```json
"appSync": {
  "ssh-manager": {
    "collections": { "hosts": "both", "snippets": "both", "port_forwarding": "send", "keychain": "off" },
    "secrets": false,
    "apply": "review"
  }
}
```

- Hướng mỗi collection: `off | send | receive | both`. Luồng A→B cho collection c chỉ chảy khi `A.send(c)` **và** `B.receive(c)`.
- `apply`: `review` (mặc định; vào hộp duyệt, người dùng duyệt từng lô) hoặc `auto` (tự áp dụng, trừ bản ghi có cờ rủi ro ở 5.8).
- Không cần "bắt tay" giao thức để bật: mỗi bên đặt chính sách của mình. Giao diện có thêm nút **Mời đồng bộ** (`appsync/invite`): chỉ để lại một lời mời hiện ở bên kia và mở sẵn hộp thoại cấu hình; lời mời **không cấp quyền gì**.
- Peer cũ (daemon chưa có `appsync`) không có tham số khả năng → giao diện báo "peer cần cập nhật SenClaw", không gọi.
- Xem trước dùng cùng quy tắc với lúc đồng bộ thật (`dryRun`), đúng tinh thần "xem trước tính bằng chính quy tắc trả lời" của chính sách tri thức.

### 5.10 Phương thức SLink mới và thương lượng khả năng

Cùng phong bì niêm phong, đặt tên theo mẫu `knowledge/recall`:

| Phương thức | Việc | Mã lỗi |
|---|---|---|
| `appsync/describe {app}` | app có ở máy này không, phiên bản, `sync` (schema), chính sách *của máy này dành cho peer đó* | `-32012` app chưa cài/không hỗ trợ đồng bộ |
| `appsync/state {app, collections}` | tóm tắt `[ {id, hlc, deleted} ]` | `-32010` chưa mở cho peer |
| `appsync/read {app, collection, ids}` | bản ghi đầy đủ (đã cắt) | `-32010`, `-32011` |
| `appsync/apply {app, records, dryRun}` | peer đẩy bản ghi cho máy này | `-32010`, `-32013` lệch schema |
| `appsync/invite {app}` | để lại lời mời | — |

- Agent Card extension thêm tham số `appSync: <version>` (0 nếu thiếu), cạnh `knowledge`.
- Lệch schema chính: `schemaVersion` khác *major* → từ chối (-32013) với câu chỉ rõ bên nào cần cập nhật. Lệch minor: dùng `extra` để giữ trường lạ.
- Mỗi lô ≤ ~500 KB; chia trang theo collection; trả lời ≤ 5 MiB.

### 5.11 Khi nào đồng bộ chạy

1. **Thủ công:** nút "Đồng bộ ngay" (kèm xem trước).
2. **Khi có thay đổi:** app báo daemon `POST /api/space/apps/:id/sync/notify` sau mỗi ghi (làm trễ ~2–5 s, gộp), daemon đẩy tới các peer đang `send` cho collection đó.
3. **Định kỳ:** đối chiếu tóm tắt mỗi vài phút khi peer thấy được (qua cơ chế khám phá đã có), để bắt kịp sau lúc offline.
4. Không có hàng đợi: trạng thái hội tụ vì so sánh tóm tắt.
5. Mỗi liên kết ghi nhật ký: lần cuối thành công, số bản ghi áp/giữ/xung đột, lỗi cuối; hiện trong giao diện và trả qua API.

### 5.12 Cập nhật app và luồng ("update app và luồng để hỗ trợ")

Các thay đổi có thể đến theo thứ tự bất kỳ trên từng máy; hệ thống phải tự giải thích:

| Daemon | App | Peer | Giao diện nói gì |
|---|---|---|---|
| cũ (chưa `appsync`) | bất kỳ | — | Không có mục Ứng dụng; (daemon cũ không biết `appsync`, peer gọi sẽ nhận -32601) |
| mới | ssh-manager 1.2.x (không có `sync`) | — | "ssh-manager chưa hỗ trợ đồng bộ — cập nhật lên 1.3.0" + nút Cập nhật (gọi `/update` có sẵn) |
| mới | 1.3.0 | peer cũ | "SenClaw của <peer> cần cập nhật để đồng bộ ứng dụng" |
| mới | 1.3.0 | peer mới nhưng app 1.2.x | "<peer> chưa cập nhật ssh-manager" (đọc từ `appsync/describe`) |
| mới | 1.3.0 | mới + 1.3.0 | Hiện công tắc từng collection |

Quy trình phát hành:
1. ssh-manager 1.3.0 phát hành lên hub bằng `scripts/publish-app.sh` (bất biến, tối đa 20 MB); manifest thêm khối `sync`; dependency đánh dấu SDK có mô-đun `sync` nếu đã làm (P4).
2. Daemon phát hành (ví dụ 0.3.0) có `appsync/*` và khả năng `appSync: 1`.
3. Người dùng cập nhật mỗi máy theo đường có sẵn (daemon qua `senclaw update`/app desktop; app qua Space Apps → Cập nhật). Thứ tự tuỳ ý (bảng trên).
4. **Di trú dữ liệu khi 1.3.0 chạy lần đầu:** sao lưu từng file thành `*.pre-sync.bak`, gán cho mọi bản ghi `rev`, `hlc` (từ thời điểm sửa file hoặc `now`), `deviceId` (sinh một lần, lưu cạnh dữ liệu), `deleted=false`, đặt quyền `0600`; `snippets` giữ `created_at`. Di trú phải idempotent và đảo ngược được bằng file `.bak`.
5. Giao diện trong app (iframe) hiện huy hiệu "Đang đồng bộ với <peer> · 2 phút trước" qua `GET /api/sen-link/app-sync/status?app=ssh-manager`.

## 6. Phần phải đổi (theo repo)

**senclaw (daemon)**
- `src/sen_link/app_sync.rs` (mới): chính sách, cắt bí mật theo schema, phối hợp phiên đồng bộ, giới hạn, nhật ký; gắn vào `LinkState` như `knowledge` (`with_app_sync`).
- `src/sen_link/server.rs`: nhánh `appsync/*` trong `handle()`; `a2a.rs`: mã lỗi `-32012/-32013`, tham số thẻ `appSync`; `store.rs`: `Peer.app_sync` + `PeerView.app_sync` (mặc định đóng, tương thích ngược khi file cũ không có khoá).
- Hàm gọi app nội bộ: tách phần chuyển tiếp trong `space_apps_proxy` (đóng dấu token, `ensure_running`) thành hàm dùng chung `call_app(app, method, path, body)`.
- `src/apps/manifest.rs`: phân tích khối `sync`, kiểm tra hợp lệ (collection, refs, order, trường).
- `src/sen_link/api.rs`: `GET/PATCH /api/sen-link/peers/:id/app-sync`, `POST …/preview`, `POST …/run`, `GET /api/sen-link/app-sync/status`, hộp duyệt; `POST /api/space/apps/:id/sync/notify` (cần token app).
- Hộp duyệt: lô giữ lại được lưu ở daemon (bảng nhỏ) cho đến khi người dùng duyệt; bí mật bị cắt khi chính sách tắt nên không nằm ở đó.
- `docs/sen-link.md`, `docs/space-app-sdk-publish-guide.md` (hợp đồng đồng bộ), `CLAUDE.md`.

**app-space-sdk**
- Mô-đun `sync` tuỳ chọn: kiểu bản ghi, `Hlc`, trait `SyncCollection`, hàm so/hợp nhất, bia mộ, bộ dựng router `/sync/*` đã gắn `require_app_token`. Lý do: không để mỗi app tự viết lại thuật toán hợp nhất (nơi dễ sai). Node/Python/Go làm sau khi hợp đồng ổn định.

**app-ssh**
- `models.rs`: thêm `hlc`, `deleted`, `extra` vào Host/Keychain/PortForwardingRule/Snippet; `delete` thành bia mộ; thao tác ghi tăng `hlc`; `deviceId` cố định.
- Lớp lưu trữ: di trú, `0600`, `.bak`.
- `sync.rs`: bốn endpoint; áp theo thứ tự; viết lại tham chiếu khi gộp; `guardFields`; `active` không ghi đè; bật `require_app_token` cho `/sync/*`.
- Bật `restore_saved_tunnels` chỉ theo `active` cục bộ (đã vậy; kiểm thử giữ nguyên).
- `senclaw-manifest.json` thêm `sync`; `senclaw-hub.json` 1.3.0.

**web-app / desktop**
- SLink → peer → tab **Ứng dụng** (cạnh Tasks/Knowledge): danh sách app, tình trạng hai phía, công tắc từng collection (hướng), công tắc *Bí mật* kèm xác nhận, *Xem trước*, *Đồng bộ ngay*, hộp duyệt, nhật ký. Mẫu giao diện: `SenLinkSettings.tsx` / `sen_link_knowledge.dart`.

## 7. Kiểm thử đề xuất (không dùng daemon thật)

- **Thuộc tính hội tụ (app/SDK):** sinh chuỗi thao tác ngẫu nhiên (thêm, sửa, xoá) trên hai bản; áp dụng theo mọi thứ tự và lặp lại; trạng thái cuối phải bằng nhau; `apply` idempotent.
- **Bia mộ & hồi sinh:** xoá bên A, sửa bên B sau đó → B thắng; xoá sau → xoá thắng; peer vắng quá N ngày → đòi đồng bộ đầy đủ có duyệt.
- **Tham chiếu:** xoá keychain đang được host tham chiếu; ghép trùng nghĩa viết lại `keychain_id`/`host_id`; tham chiếu treo bị dọn.
- **An ninh:** `secrets=false` không để lọt `password`/`value` ở cả hai chiều (kiểm bằng cách bắt gói trên testkit); đổi `host` của bản ghi có mật khẩu bị giữ ở chế độ `auto`; `active` không bao giờ được áp; `/sync/*` không token → 401.
- **Daemon:** dùng `testkit::node_with` của SLink (hai daemon trong một tiến trình); bên "app" là máy chủ giả trả dữ liệu cố định để kiểm phối hợp, ranh giới 700 KB, -32601/-32010/-32012/-32013.
- **Đầu-cuối:** hai daemon scratch (HOME + SENCLAW_HOME + cwd riêng, cổng khác, Seatbelt) và hai bản ssh-manager với `HOME` khác nhau (thư mục dữ liệu lấy từ `HOME`, nên cô lập được).

## 8. Rủi ro và việc để ngoài phạm vi

- **Mã hoá bí mật ở rest** (Keychain hệ điều hành) là việc đúng nhưng riêng; không chặn đồng bộ, và đồng bộ không làm tình hình ở rest tệ hơn.
- **Mức trường** chưa có ở v1: sửa đồng thời cùng một bản ghi thì bản sau thắng. Với host/snippet đây là mức chấp nhận được; ghi trong giao diện.
- **Hợp nhất gần đúng** (gần nghĩa nhưng khác `matchKey`) không làm; người dùng gộp tay.
- **Quy mô lớn** (nghìn bản ghi trở lên): chuyển bản tóm tắt phẳng sang cây băm theo tiền tố ID; hợp đồng đã chừa chỗ (`state` có thể chia trang).
- **App bên thứ ba** khai `sync` có thể khai dối `secretFields`: daemon chỉ cắt theo khai báo, nên giữ `secrets` tắt mặc định và để người dùng thấy danh sách trường bí mật trong hộp thoại bật.
- **Đồng hồ** sai nặng làm một máy thắng mọi xung đột; cửa sổ trôi giới hạn nhưng không loại hẳn.

## 9. Lộ trình đề xuất

| Pha | Nội dung | Kiểm chứng |
|---|---|---|
| P0 | ssh-manager: siêu dữ liệu + di trú + 4 endpoint + bật token; thử bằng hai bản cục bộ gọi nhau qua script | thuộc tính hội tụ, bia mộ, tham chiếu, `0600` |
| P1 | Daemon: `app_sync.rs`, `appsync/*`, chính sách, thương lượng khả năng, `call_app`, API | testkit hai node + máy chủ app giả |
| P2 | Web + desktop: tab Ứng dụng, xem trước, hộp duyệt, nhật ký, mời đồng bộ | ảnh chụp, thử nghiệm bố cục tiếng Việt |
| P3 | Phát hành ssh-manager 1.3.0 + daemon mới; hosts + snippets + port-forward, bí mật tắt | thử đầu-cuối trên cặp thật (sau khi cập nhật máy còn lại) |
| P4 | Công tắc bí mật theo peer; mô-đun `sync` trong SDK; app thứ hai (ví dụ app ghi chú/bookmark) để chứng minh tính tổng quát | thử với bí mật bật, kiểm chặn |

## 10. Câu hỏi còn mở (cần bạn quyết)

1. **Bí mật:** chấp nhận "mặc định không đồng bộ bí mật, bật riêng từng peer kèm xác nhận"? Với dữ liệu thật của bạn (14 host có mật khẩu), nếu để tắt thì máy kia nhận danh sách host nhưng phải nhập lại mật khẩu từng máy.
2. **Quan hệ peer:** peer trong SLink có thể là máy của người khác (đang dùng để giao việc). Có cần cờ "máy của tôi" để mặc định `apply: auto` cho thiết bị của chính chủ và `review` cho phần còn lại?
3. **Chế độ áp dụng mặc định:** `review` (an toàn, nhiều thao tác) hay `auto` (tiện, dựa vào `guardFields`)?
4. **Port-forward:** đồng bộ cả quy tắc (luôn nhận về ở trạng thái tắt) hay bỏ qua hẳn?
5. **Phạm vi bản phát hành đầu:** làm tất cả bốn collection hay chỉ `hosts` + `snippets`?
6. **Đường cập nhật của ssh-manager:** xác nhận hub phát hành và `POST /update` chạy được với ssh-manager (chưa kiểm).

## 11. Chưa kiểm chứng

- Chưa chạy thử bất cứ thứ gì: đây là nghiên cứu từ đọc mã. Số liệu máy này (14 host, 14 có mật khẩu, 0 keychain, 3 snippet, 1 quy tắc, quyền `0644`) đếm bằng `jq` mà không đọc giá trị bí mật.
- Chưa xác nhận được trên máy kia (192.168.0.93, còn v0.2.1) ssh-manager có cài hay không.
- Kích thước thực của bản tóm tắt chỉ là ước lượng (vài chục KB cho ~1000 bản ghi); cần đo khi có P0.
