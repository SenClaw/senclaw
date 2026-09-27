# Token truy cập của Space App (`SENCLAW_TOKEN_ACCESS_APP`) và `SENCLAW_API_VERSION`

## Vấn đề

Trước tính năng này, mọi route `/api/space/apps/<id>/…` chỉ được xác thực bằng
một điều duy nhất: **request đến từ loopback**. Đó là hàng rào quanh *cái máy*,
không phải quanh *cái app*. Bên trong hàng rào đó:

- app A `POST /api/space/apps/B/bridge` với action `agent.run` → chạy nguyên một
  agent đầy đủ tool dưới danh nghĩa app B;
- `GET /api/space/apps/B/config` → đọc sạch settings của B, nơi cất API key,
  cookie, token OAuth;
- `POST /api/space/apps/B/sqlite/query` → truy vấn thẳng database của B.

Thứ duy nhất cần có là **id của app**, mà id là công khai (nằm trong URL, trong
manifest, trong danh sách `/api/space/apps`). Bật sandbox riêng cho app cũng
không cứu được: app nào được phép nói chuyện với daemon thì được phép nói chuyện
với *toàn bộ* daemon.

Chiều ngược lại cũng hở: API của chính Space App (REST + MCP trên cổng riêng của
nó) **không có xác thực nào cả**. Cứ tiến trình nào trên máy biết cổng là gọi
được.

## Cách giải quyết

Daemon phát cho mỗi app đã cài **một token bí mật**, đưa vào tiến trình của app
qua biến môi trường `SENCLAW_TOKEN_ACCESS_APP`, và coi token đó là *tên* của app.

| | |
|---|---|
| Hình dạng | `sca_<64 ký tự hex>` — 32 byte entropy sau tiền tố cố định |
| Nơi lưu | bảng `space_app_tokens` (`app_id` PK, `token` UNIQUE) |
| Phát khi | lần launch đầu tiên (`ensure`), nên app cài từ trước tự có, không cần migration |
| Thu hồi khi | gỡ app — id có thể dùng lại, không được thừa kế bí mật của bản cài cũ |
| Header | `X-SenClaw-App-Token`, hoặc `Authorization: Bearer sca_…`, hoặc `?app_token=` |

Tiền tố `sca_` là thứ có tải trọng: token của app và token API của daemon cùng
đi qua `Authorization: Bearer`, và tiền tố là cách middleware phân biệt hai loại
mà không phải truy vấn DB mỗi request.

Mã: [`src/apps/token.rs`](../src/apps/token.rs) (phát/xác minh/xoay vòng) và
[`src/gateway/ui_server/app_auth.rs`](../src/gateway/ui_server/app_auth.rs)
(middleware).

## Luật thực thi

Middleware `app_auth_mw` hỏi ba câu, theo thứ tự:

1. **Client khai API version nào?** Version mới hơn daemon → `426 Upgrade
   Required`, chứ không phục vụ nửa vời. Version cũ hơn vẫn phục vụ.
2. **Token có thật không, và có đúng là của app này không?** Token của app khác →
   **403**, không bao giờ âm thầm chuyển hướng về dữ liệu của bên gọi. Điều này
   đúng ở **mọi** mode — kể cả `off`, nếu không `off` sẽ thành cách né việc xoay
   vòng token.
3. **Không có token thì có được đi tiếp không?** Đây mới là câu mà
   `SENCLAW_APP_TOKEN_MODE` trả lời.

### `SENCLAW_APP_TOKEN_MODE`

| Giá trị | Hành vi khi request **không** mang token |
|---|---|
| `strict` *(mặc định)* | Từ chối, trừ khi request đến từ UI của chính daemon. |
| `warn` | Phục vụ, nhưng log một dòng cho mỗi app mỗi lần chạy daemon — cách để biết app nào sẽ chết dưới `strict` mà chưa làm nó chết. |
| `off` | Phục vụ, y như trước khi có tính năng này. Lối thoát cho app tự gọi daemon bằng HTTP client riêng mà không dạy được cách gửi token. |

Mặc định là `strict`: một app không chứng minh được mình là ai thì không lấy được
dữ liệu của app khác. Mọi SDK đều tự gửi token, và proxy của daemon tự đóng dấu
lên mọi thứ nó chuyển tiếp — nên thứ duy nhất gãy là app tự tay dựng HTTP client
gọi `/api/space/apps/<id>/…`, và nó gãy *ồn ào*: 401 kèm đúng tên biến cần đặt.

Chuỗi viết sai **không** rơi về `off`. `SENCLAW_APP_TOKEN_MODE=of` sẽ được hiểu là
`strict` kèm một dòng cảnh báo — một lỗi gõ âm thầm tắt kiểm soát an ninh chính là
thứ tính năng này sinh ra để tránh. Muốn tắt thì phải viết đúng `off`.

Đường nâng cấp an toàn cho một hạm đội app cũ: chạy `warn` một vòng, đọc log xem
app nào chưa gửi token, sửa hoặc để riêng chúng ở `off`, rồi bỏ biến đi để về
mặc định.

### Đổi mode trong UI

**Settings → Space Apps → Token truy cập của app** (cả Web UI lẫn app desktop) —
ba lựa chọn Bắt buộc / Chỉ cảnh báo / Tắt.

Khác với công tắc bind host bên cạnh, cái này **không cần khởi động lại daemon**:
middleware đọc nó ở từng request, nên lời gọi kế tiếp của app đã bị xử theo thiết
lập mới. Lựa chọn nằm trong DB (`router_state`, khoá `space:appTokenMode`) và
**đè lên** `SENCLAW_APP_TOKEN_MODE` — nếu không thì trên máy có đặt biến đó, nút
bấm sẽ trông như không làm gì.

UI hiển thị luôn *nguồn* của giá trị đang có (`ui` / `env` / `default`) vì ba
nguồn hành xử khác nhau khi người dùng muốn đổi: lựa chọn trong UI thì họ tự gỡ
được, biến môi trường thì không (nó quay lại ở lần khởi động sau).

| | |
|---|---|
| `GET /api/space/app-token-mode` | `{mode, source, envMode, envSet, defaultMode, apiVersion}` |
| `PUT /api/space/app-token-mode` | `{mode: "off"\|"warn"\|"strict"}`, hoặc `null` để trả quyền quyết định lại cho môi trường. Chuỗi lạ trả **400** chứ không tự quy về giá trị nào. |

Route này cố ý **không** nằm dưới `/api/space/apps/` — mọi thứ trong đó bị
`app_auth` siết theo từng app id, và dưới `strict` thì chính cái nút để tắt
`strict` sẽ bị khoá sau token của một app.

### Route nào bị siết

`strict` chỉ đòi token trên **route phục vụ dữ liệu của chính app**:

```
/bridge   /config   /config/<key>   /sqlite/query   /mcp/register   /env   /token
```

Không siết:

- **Route quản trị** (`/start`, `/stop`, `/update`, `/sandbox`, `/runtime`, …) —
  đó là nút bấm của người dùng, và app thì không có việc gì phải gọi chúng cho
  *bất kỳ ai*, kể cả chính nó.
- **`/proxy/*` và `/static/*`** — chúng mang request *vào trong* app; daemon tự
  đóng dấu token của app lên đường ra (xem dưới). Đòi token ở đường vào chỉ làm
  iframe của UI không bao giờ tải được.

### `strict` thực sự mua được gì

Nó chặn một app **định địa chỉ** tới app khác: HTTP client của nó có đúng một
token, token đó gọi tên đúng một id, mọi id khác trả 403.

Nó **không** chặn một chương trình đọc được `~/.senclaw/senclaw.db` — chương
trình đó đọc được mọi token trong bảng, và đọc luôn dữ liệu của mọi app. Hàng rào
làm cho `strict` có ý nghĩa là **sandbox từng app**
([docs/space-app-sandbox.md](space-app-sandbox.md)), thứ giữ app tránh xa file đó
ngay từ đầu. Hai tính năng này sinh ra để dùng cùng nhau.

UI của daemon không mang token app (nó quản lý *mọi* app, siết vào một app là
sai). Nó được nhận ra qua dấu vết trình duyệt để lại trên một fetch same-origin
(`Sec-Fetch-Site`, `Origin` loopback, cookie `senclaw_token`). Một tiến trình cục
bộ quyết tâm cũng đặt được các header đó — xem lại đoạn trên về việc vì sao đó là
việc của sandbox chứ không phải của middleware.

## Đường vào: bảo vệ API của chính app

Proxy của daemon đóng dấu `X-SenClaw-App-Token` (và `X-SenClaw-Api-Version`) lên
**mọi** request nó chuyển tiếp — iframe UI, fetch của chính app, và mọi lời gọi
MCP. Header do client gửi lên bị **gỡ bỏ** trước khi chuyển tiếp, nếu không thì
bất kỳ trang nào chạm được route proxy cũng đưa cho app một token tùy ý.

Với app **background**, MCP client của agent gọi thẳng cổng của app chứ không đi
qua proxy — nên token cũng được nhét vào `headers` của cấu hình MCP server
(transport `http`/`sse`) và vào `env` (transport `stdio`).

Nhờ vậy app có thể bật guard và chỉ còn daemon gọi được:

```rust
// Rust — app-space-sdk
use app_space_sdk::auth;
let app = Router::new()
    .route("/api/notes", get(list_notes))
    .layer(axum::middleware::from_fn(auth::require_app_token));
```

```go
// Go
senclaw.Serve(senclaw.Config{
    RequireAppToken: true,
    AuthSkipPaths:   []string{"/ws/*"},   // extension gọi thẳng
    HealthPath:      "/api/status",       // luôn được miễn
})
```

```python
# Python
serve(routes, health_path="/api/status", require_app_token=True,
      auth_skip_paths=["/public/*"])
```

```ts
// Node
import { requireAppToken } from '@senclaw/space-sdk/mcp';
app.use(requireAppToken({ skip: ['/health', '/public/*'] }));
```

Guard **tắt theo mặc định**, và hai điều không bao giờ bị từ chối:

- **Không có token trong env** — đó là `cargo run`/`npm start` chạy tay ngoài
  SenClaw. Trả 401 cho mọi request kể cả health check sẽ biến "chưa phát token"
  thành "app chết vĩnh viễn".
- **Đường dẫn miễn trừ** — health path luôn miễn; thêm những gì client gọi thẳng
  (WebSocket của extension trình duyệt, curl của lập trình viên).

## `SENCLAW_API_VERSION`

Phiên bản **hợp đồng** của Space-App API, hiện là **2**.

| version | hợp đồng |
|---|---|
| 1 | Thời loopback-trust: không có định danh app, không có header version. |
| 2 | Token truy cập từng app + header version. |

- Daemon nhét vào env mỗi app khi launch và gắn `X-SenClaw-Api-Version` lên mọi
  phản hồi app-scoped.
- SDK gửi kèm header trên mọi lời gọi. Daemon phục vụ hợp đồng **cũ hơn** bình
  thường (app ghim v1 có từ trước token, nâng daemon không được làm chết nó) và
  trả **426** cho hợp đồng mới hơn — kèm câu nói rõ bên nào cần nâng.
- Chỉ tăng số khi có thay đổi **phá vỡ** app viết theo bản trước. Thêm action,
  thêm trường tùy chọn thì không tăng.
- `SENCLAW_API_VERSION` trên daemon là để **ghim** một hợp đồng cũ khi debug.

## REST

| | |
|---|---|
| `GET /api/space/apps/:id/token` | Trả token (phát nếu chưa có) + `envVar`, `header`, `apiVersion`, `mode`. Để UI hiển thị cho lập trình viên chạy app bằng tay. |
| `POST /api/space/apps/:id/token` | Xoay vòng. Token cũ chết ngay; app đang chạy bị **stop** — session app tự lên lại khi dùng, background app do supervisor dựng lại, cả hai đều đọc lại env. |
| `GET /api/space/apps/:id/env` | Thêm `apiVersion` + `appTokenMode`. **Không** kèm token: endpoint này phục vụ UI trình duyệt của app, và bí mật giao cho page JS là bí mật nằm trong mọi extension người dùng đã cài. |
| bridge action `capabilities` | Thêm `apiVersion` + `appTokenMode` — một lần probe là biết đang nói chuyện với daemon nào. |

## Bẫy

- **Chạy app bằng tay.** `strict` là mặc định, nên `npm start` trần sẽ bị từ chối:
  không có env thì SDK không gửi header. Lấy token rồi export:
  `SENCLAW_TOKEN_ACCESS_APP=$(curl -s localhost:18788/api/space/apps/<id>/token | jq -r .token) npm start`.
- **Xoay vòng mà không restart.** Tiến trình đang chạy vẫn giữ token cũ trong
  env. Endpoint rotate tự stop app vì lý do này; đừng tự tay `UPDATE` bảng.
- **Gửi header rỗng.** Mọi SDK đều *bỏ hẳn* header khi không có token, thay vì
  gửi chuỗi rỗng — daemon sẽ đi resolve `""`, trượt, và từ chối một lời gọi mà
  mode `off` lẽ ra phục vụ.
- **Relay (app điện thoại).** Frame relay được dựng thành request trần, không có
  dấu vết trình duyệt, nên `strict` sẽ tưởng là app lạ. Relay bridge gắn extension
  `TrustedOperator` — một *request extension*, không phải header, vì không gì
  trên mạng dựng được extension. Đừng đổi thành header.
- **Bật guard đường vào cho app có extension trình duyệt.** Extension gọi thẳng
  `ws://127.0.0.1:<port>`, không qua proxy → phải liệt kê đường dẫn đó trong
  `AuthSkipPaths`, nếu không extension chết im lặng.
- **Token của daemon ≠ token của app.** Cả hai đi qua `Authorization: Bearer`.
  Phân biệt bằng tiền tố `sca_`; đừng bỏ kiểm tra tiền tố, nếu không token của
  operator sẽ bị tra như token app, trượt, và 401 mọi request từ xa.
