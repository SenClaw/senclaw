# Hướng dẫn sử dụng Provider Sign-in

**Provider Sign-in** (Settings → Provider Sign-in, có trên cả Web UI và desktop app)
cho phép chạy model trong SenClaw bằng hai nguồn:

1. **Subscription accounts** — đăng nhập OAuth bằng tài khoản thuê bao có sẵn
   (Claude Code, OpenAI Codex, Antigravity, GitHub Copilot, Qwen, Kimi, Grok,
   iFlow, Gemini CLI). Không cần API key.
2. **Free-tier providers** — catalog các endpoint có hạn mức miễn phí
   (Google AI Studio, NVIDIA NIM, …). Mỗi cái cần API key riêng trừ khi ghi khác.

> ⚠️ **Đọc trước khi dùng — banner cảnh báo đầu trang.** Credential thuê bao được
> vendor cấp phép cho *client của chính họ*. Dùng từ SenClaw có thể bị **khoá tài
> khoản**, và vendor phát hiện được. SenClaw cố tình **tự xưng danh trung thực**
> thay vì giả mạo client của vendor — provider nào chặn bên thứ ba sẽ trả lỗi rõ
> ràng chứ không hỏng ngầm. **Việc gì quan trọng, hãy dùng API key.** Mỗi card
> provider còn có icon ⚠ riêng (hover để đọc rủi ro cụ thể của provider đó).

## 1. Danh sách provider thuê bao

| Provider | Kiểu đăng nhập | Ghi chú |
|---|---|---|
| Claude Code | Browser redirect | |
| OpenAI Codex | Browser redirect | **Cần port 1455 trống** — OpenAI chỉ đăng ký đúng một redirect loopback. Port bận là fail ngay khi bấm Connect. |
| Antigravity | Browser redirect | Context 1M. Lần chat đầu tự khám phá Code Assist project id. |
| GitHub Copilot | Device code | Dùng được từ xa/SSH |
| Qwen Code | Device code | Dùng được từ xa/SSH |
| Kimi for Coding | Device code | Dùng được từ xa/SSH |
| Grok CLI | Device code | Dùng được từ xa/SSH |
| iFlow | Browser redirect | |
| Gemini CLI | Browser redirect | Context 1M |

Hai kiểu đăng nhập:

- **Browser redirect** (auth-code + PKCE): daemon mở một listener tạm trên
  `127.0.0.1`, trình duyệt đăng nhập xong redirect code về đó. Nhanh nhất,
  nhưng **chỉ hoạt động khi trình duyệt và daemon cùng một máy**.
- **Device code**: hiện một mã, bạn tự mở trang xác minh của vendor và nhập mã.
  Không cần listener → hoạt động cả khi mở Web UI từ máy khác hoặc qua SSH.

## 2. Đăng nhập kiểu Browser redirect

1. Bấm **Connect** trên card provider. Một tab trình duyệt mở ra
   (toast: *"Finish the sign-in in the browser tab that just opened."*).
2. Đăng nhập tài khoản vendor và bấm cho phép (Allow/Authorize).
3. Trình duyệt hiện *"Returning to SenClaw to finish connecting… You can close
   this tab."* — **đây chưa phải xác nhận thành công**; SenClaw còn kiểm tra và
   đổi code lấy token phía sau. Kết quả thật hiển thị trong SenClaw (UI tự poll
   mỗi 2 giây).
4. Thành công → account xuất hiện trong bảng **Connected accounts**.

Lưu ý:

- **Timeout 5 phút** — rời đi giữa chừng thì bấm Connect lại từ đầu.
- Bấm **Deny** trên trang vendor → toast lỗi "…denied the sign-in".

## 3. Đăng nhập kiểu Device code

1. Bấm **Connect** → modal hiện **mã lớn** kèm nút **Copy code** và **Open page**.
2. Mở trang xác minh (một số provider mở sẵn URL đã điền mã), nhập mã, đồng ý.
3. SenClaw tự phát hiện khi vendor xác nhận và đóng modal; account xuất hiện
   trong bảng.

Lưu ý: mã có hạn dùng (tối đa 5 phút). Đóng modal chỉ ẩn UI — muốn làm lại thì
bấm Connect lần nữa. Hết hạn → lỗi "the device code expired — start the sign-in again".

## 4. Dùng từ xa (Web UI mở từ máy khác máy chạy daemon)

Browser redirect **không hoạt động từ xa**: trang vendor redirect về
`http://localhost:<port>` — tức là máy đang chạy *trình duyệt*, không phải máy
chạy daemon; listener bên daemon không bao giờ nhận được code và flow treo đến
timeout. Đây là chủ ý an toàn (listener bind cứng loopback, không có knob mở LAN
— mở ra là ai trên mạng cũng đua lấy được authorization code).

→ Từ xa hãy dùng 4 provider **Device code**: GitHub Copilot, Qwen, Kimi, Grok.

## 5. Sau khi kết nối — bảng Connected accounts

Mỗi account một dòng với:

- **Label + email** (email chỉ có khi provider trả về — xem lưu ý Add another bên dưới).
- **Pill thời hạn token**: `45m left` / `2h left`… — cam khi còn dưới 10 phút,
  `Expired` khi hết, `No expiry reported` khi provider không báo.
- Tag **`No auto-refresh`**: provider không cấp refresh token — hết hạn phải
  đăng nhập lại bằng tay (nút Refresh bị mờ).
- Tag **`Needs attention`** (đỏ): refresh bị vendor từ chối — hover đọc lỗi,
  thường phải Disconnect rồi Connect lại.

Các nút trên mỗi account:

| Nút | Tác dụng |
|---|---|
| **Use as model** | Tạo cấu hình model chạy bằng account này (xem §6) |
| **Refresh** ⟳ | Làm mới token ngay (mờ nếu không có refresh token) |
| **Disconnect** | Quên token đã lưu. Cảnh báo: mọi model đang gắn account này ngừng chạy đến khi kết nối lại |
| **Connect / Add another** | Đăng nhập thêm tài khoản cùng provider |

**Token tự làm mới**: daemon kiểm tra mỗi 60 giây và refresh trước khi hết hạn;
nếu token hết hạn giữa chừng, request bị 401 sẽ tự refresh và gửi lại một lần.
Bình thường bạn không phải bấm Refresh tay.

**Add another**: các provider không trả email (Claude, Copilot, Kimi, iFlow,
Gemini CLI) sẽ tạo **bản ghi mới mỗi lần đăng nhập** (không dedup được) — nếu
lỡ tạo trùng, Disconnect bản thừa.

## 6. Chạy model bằng tài khoản đã kết nối

1. Bấm **Use as model** trên account → SenClaw tải danh sách model của account
   (hỏi trực tiếp vendor; nếu vendor không công bố thì dùng danh sách tĩnh kèm
   ghi chú). Chọn model → **Add model**.
2. Một cấu hình LLM mới xuất hiện ở **Settings → LLM** với nhãn
   `"{Provider} — {model}"`. Cấu hình đầu tiên tự thành active. Đặt nó làm
   **Main / Quick / Cognitive** tại đó.
3. **Danh sách model chỉ là gợi ý** — gói thuê bao của bạn có thể liệt kê chục
   model nhưng chỉ phục vụ vài cái. Bấm **Test this model** (hoặc **Test all N**)
   để chạy một completion thật (`"Reply with the single word: ok"`) và biết chắc.
   Test tốn quota thật; Test all chạy tuần tự để tránh rate limit.

Cấu hình model **không chứa token** — chỉ tham chiếu id account. Token nằm riêng
(xem §8).

## 7. Free-tier providers

Phần dưới trang: *"Ready-made endpoints with a free allowance. Each needs its
own API key unless marked otherwise."*

| Provider | Ghi chú |
|---|---|
| Google AI Studio | Free tier Gemini hào phóng; key từ AI Studio, không cần billing |
| BazaarLink | Aggregator có route `auto:free` |
| Kilo Gateway | Free tier có model Nemotron, Kat Coder `:free` |
| NVIDIA NIM | Developer credits trên build.nvidia.com |
| Kimchi | Miễn phí họ Kimi và MiniMax |
| BytePlus Ark | Model coding Seed 2.0; quota free trên endpoint coding |
| LLM7 | Relay miễn phí nhiều model frontier |
| API Airforce | Relay nhỏ; **rate limit chặt** |
| Poolside | Model Laguna của Poolside |
| Cloudflare Workers AI | Badge **`needs accountId`** — cần Cloudflare account id (điền ở ô đầu modal Add, được thay vào URL endpoint) |
| Xiaomi MiMo (open) | Badge **`No key`** — endpoint mở, không cần credential; khả dụng best-effort |

Thao tác:

- **Get key** — mở trang lấy API key của vendor (tab mới).
- **Add** — modal nhập API key (+ accountId nếu cần) và chọn model →
  **Add model** tạo một cấu hình LLM **API-key thông thường** ở Settings → LLM
  (không liên quan OAuth). Sau đó đặt Main/Quick/Cognitive như mọi cấu hình khác.

## 8. Token lưu ở đâu — an toàn

- Token OAuth nằm ở **`~/.senclaw/oauth.json`**, quyền file **0600**, ghi atomic.
- Token **cố tình không nằm trong `config.json`** — vì `GET /api/llm-config` trả
  nguyên `config.json` và daemon chạy CORS lỏng; mọi API `/api/oauth/*` chỉ trả
  bản **redacted** (không bao giờ chứa token).
- **Đổi máy / backup**: `config.json` chỉ chứa `oauthAccountId` — mang sang máy
  mới phải mang kèm `oauth.json` (giữ quyền 0600), không thì mọi model OAuth chết.
- Xoá `oauth.json` = quên sạch mọi account (không báo lỗi) — đăng nhập lại từ đầu.
- Client id/secret của vài provider (Antigravity, iFlow, Gemini CLI) là loại
  "công khai" theo mô hình installed-app của vendor — không phải bí mật của bạn.

## 9. Troubleshooting

| Triệu chứng | Nguyên nhân / cách xử lý |
|---|---|
| Codex: "port 1455 is required … already in use" | Có process khác đang nghe port 1455. Đóng nó rồi Connect lại (lỗi báo ngay khi bấm, chưa mất công đăng nhập). |
| "sign-in timed out after 300s" | Quá 5 phút chưa xong flow — bấm Connect lại. |
| Trang "Returning to SenClaw…" đã hiện mà account không xuất hiện | Xem toast lỗi trong SenClaw: state không khớp (thử lại), hoặc exchange token fail. |
| Nút Connect kẹt loading mãi | Daemon restart giữa flow (trạng thái flow chỉ nằm trong RAM) — tải lại trang và Connect lại. |
| Tag `Needs attention` — "sign in again — refresh rejected" | Vendor thu hồi refresh token (`invalid_grant`…). Disconnect rồi Connect lại. |
| `Expired` + `No auto-refresh` | Provider không cấp refresh token — đăng nhập lại bằng tay. |
| Windows: đăng nhập xong nhưng SenClaw không nhận code | `localhost` trên Windows ưu tiên `::1`; nếu app khác chiếm `::1:<port>` thì code đi lạc. SenClaw đã cố bind cả hai stack — nếu vẫn dính, đóng app kia và thử lại. |
| Antigravity/Gemini: 403 `PERMISSION_DENIED` / `CONSUMER_INVALID` | Project id cache bị vendor từ chối — SenClaw tự xoá cache; thử lại request. |
| Model có trong danh sách nhưng gọi lỗi | Entitlement theo account — danh sách là gợi ý. Dùng Test this model để lọc model thật sự chạy được. |

## 10. REST API (tham khảo nhanh)

```
GET    /api/oauth/providers                  # registry provider (kèm riskNotice, models)
GET    /api/oauth/accounts                   # account đã kết nối (redacted)
POST   /api/oauth/:provider/start            # bắt đầu flow → {flowId, authorizeUrl, kind, userCode?}
GET    /api/oauth/flows/:id                  # poll: pending | awaiting_user_code | completed | failed
POST   /api/oauth/accounts/:id/refresh       # refresh tay (502 nếu fail)
DELETE /api/oauth/accounts/:id               # disconnect
GET    /api/oauth/accounts/:id/models        # model list (discovered | registry)
POST   /api/oauth/test-model                 # {accountId, modelName} → probe completion thật
POST   /api/oauth/bind                       # {accountId, modelName, label?} → tạo LlmConfig auth:"oauth"
GET    /api/provider-catalog                 # catalog free-tier
```

## 11. Tham chiếu code

Registry provider + model mặc định: [`src/providers/oauth/provider.rs`](../src/providers/oauth/provider.rs) ·
flow + callback listener: [`src/providers/oauth/flow.rs`](../src/providers/oauth/flow.rs) ·
token store: [`src/providers/oauth/store.rs`](../src/providers/oauth/store.rs) ·
free-tier catalog: [`src/providers/mod.rs`](../src/providers/mod.rs) ·
REST: [`src/gateway/ui_server/oauth.rs`](../src/gateway/ui_server/oauth.rs) ·
UI web: [`web/src/components/settings/OAuthSettings.tsx`](../web/src/components/settings/OAuthSettings.tsx) ·
UI desktop: [`desktop_app/lib/features/settings/provider_signin_section.dart`](../desktop_app/lib/features/settings/provider_signin_section.dart).
Thêm provider mới = thêm 1 const vào registry (`provider.rs`) — test registry tự cover.
