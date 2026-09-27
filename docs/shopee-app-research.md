# Shopee Space App — Nghiên cứu (research)

> Trạng thái: **research / chưa implement**. Mục tiêu: kết nối agent SenClaw với
> Shopee để quản lý shop, trả lời tin nhắn khách, và (một phần) đăng nội dung —
> theo khuôn Space App như `apps/moltbook`.

## 1. Yêu cầu người dùng (đã gom)

1. App + Chrome extension để **lấy được access token của Shopee** và **remote điều khiển**.
2. Có token để **call API**: duyệt/tìm kiếm, **nhắn tin**, **đăng bài**.
3. Extension "call API hay điều khiển **đảm bảo không bị Shopee chặn**".
4. Hỗ trợ **tìm kiếm / bóc tách dữ liệu**.

Bốn yêu cầu này rơi vào **hai kiến trúc khác hẳn nhau** về mặt pháp lý & độ bền.
Phần này tách bạch để không xây nhầm.

## 2. Hai con đường lấy token — khác nhau về bản chất

### Con đường A — Official Shopee Open Platform (KHUYẾN NGHỊ) ✅

Nguồn: [open.shopee.com](https://open.shopee.com), [API v2 guide](https://api2cart.com/api-technology/shopee-api/).

- **Cách lấy token**: đăng ký **Partner App** → nhận `partner_id` + `partner_key`
  → sinh **link authorize shop** → seller bấm đồng ý → callback trả `code` →
  đổi lấy `access_token` + `refresh_token` (OAuth-style, per-shop).
- **Ký request**: mọi call ký **HMAC-SHA256** (`sign = HMAC(partner_key, partner_id+path+timestamp+access_token+shop_id)`).
- **Vòng đời token**: `access_token` **~4h**, `refresh_token` **~30 ngày**, link
  authorize **chỉ sống 5 phút** (timestamp phải khớp).
- **Đây chính là câu trả lời cho "không bị Shopee chặn"**: gọi qua cổng chính
  thức thì Shopee **không** coi là bot, không có anti-bot, không rate-ban bất
  chợt — chỉ có rate limit công khai (dùng request queue + exponential backoff).
- **Điều kiện**: cần **tài khoản Seller** và app được duyệt. Chat API cần **quyền
  riêng** ("not every partner account receives Chat API access automatically").

### Con đường B — Harvest session token từ web + gọi internal API ❌ (không xây)

- Ý tưởng: extension đọc cookie/`SPC_*` token của phiên đăng nhập web, gọi thẳng
  `shopee.vn/api/v4/...`, giả lập trình duyệt để "né" anti-bot.
- **Vấn đề**: (a) vi phạm ToS Shopee; (b) **chính là thứ hay bị chặn** — Shopee
  có anti-bot mạnh, token nội bộ xoay liên tục, IP/fingerprint bị ban; (c) yêu
  cầu "đảm bảo không bị chặn" = **detection evasion**, mình **không** viết phần
  chống-phát-hiện này.
- Nghịch lý cần thấy rõ: **muốn "không bị chặn" thì phải đi con đường A**, không
  phải đi con đường B rồi tìm cách né.

**Kết luận mục 2:** App lấy token qua **OAuth chính thức (A)**. Extension **không**
dùng để trộm session token.

## 3. Map từng yêu cầu → API chính thức có làm được không

| Yêu cầu người dùng | Official Open Platform | Ghi chú |
|---|---|---|
| Lấy access token | ✅ OAuth per-shop | như mục 2A |
| Quản lý sản phẩm (list/sửa/tồn kho) | ✅ Product API | đầy đủ |
| Đơn hàng / vận chuyển / hoàn tiền | ✅ Order/Logistics/Returns | đầy đủ |
| Voucher / khuyến mãi | ✅ Discount API | đầy đủ |
| **Nhắn tin khách** | ✅ **Chat API** | **chỉ buyer↔seller** của shop mình; đọc hội thoại, gửi text/ảnh, webhook tin mới. Cần quyền riêng |
| Nhắn tin hàng loạt tới user bất kỳ | ❌ | không có API; = spam, không xây |
| "Đăng bài" (Shopee Feed / Live) | ❌ (không có public API) | Feed/affiliate/Live không mở cho third-party |
| "Duyệt hội nhóm" | ❓ | Shopee **không có** groups kiểu Facebook — cần bạn làm rõ ý (xem mục 7) |
| Tìm kiếm sản phẩm/đối thủ | ❌ (không có search API) | chỉ có trong con đường scraping (B) |
| Bóc tách dữ liệu shop **của mình** | ✅ | qua Product/Order/Chat API |
| Bóc tách search/đối thủ toàn sàn | ❌ | = scraping, xem mục 5 |

Tức là: **quản lý shop + CSKH qua Chat API** làm được sạch sẽ và bền. **Search
toàn sàn, đăng Feed, DM hàng loạt** thì official API không cho.

## 4. Vai trò đúng của Chrome extension (đã có sẵn `senclaw-extension-chrome`)

Repo đã có extension WXT/React MV3 (`host_permissions: <all_urls>`, WS tới daemon,
lớp DOM `DomExtractor`/`ActionExecutor`/`SearchEngine` port từ page-agent — remote
browser control). Dùng nó cho Shopee theo hướng **hợp lệ**:

1. **Bắt OAuth redirect**: khi seller bấm authorize, extension/`background.ts`
   đọc `code` ở URL callback rồi đẩy về app qua WS → app đổi token. Đây là hứng
   redirect hợp pháp, **không** phải trộm cookie.
2. **User-driven browsing**: bạn (người dùng thật) đang đăng nhập, extension đọc
   những gì **đang hiển thị trên màn hình bạn** để agent tóm tắt/soạn trả lời.
   Đây là "user điều khiển trình duyệt của chính mình", khác hẳn bot chạy nền quy mô.

**Không** dùng extension để: chạy nền tự động quy mô lớn, xoay
IP/fingerprint né anti-bot, DM hàng loạt, hay trộm `SPC_*` token gọi internal API.
Đó là ranh giới giữa "công cụ hợp lệ" và "bot vi phạm ToS".

## 5. Tìm kiếm / bóc tách dữ liệu

- **Dữ liệu shop của bạn**: lấy qua Official API (Product/Order/Chat) → sạch, ổn định.
- **Dữ liệu toàn sàn (đối thủ, giá, search)**: official API không có. Muốn có thì
  phải scraping web — đây là vùng ToS xám và **hay bị chặn**. Nếu thật sự cần cho
  mục đích nghiên cứu thị trường của riêng bạn, mình có thể làm **ở mức thủ công,
  user-driven, nhịp người-dùng** (bạn mở trang, extension bóc dữ liệu đang hiện),
  **không** phải crawler chạy nền + né chặn. Cần bạn xác nhận phạm vi trước.

## 6. Khung app đề xuất (theo khuôn `apps/moltbook`)

`apps/shopee` — Rust axum, **port 44xx** (ví dụ 4490), cấu trúc quen thuộc:

| Layer | File | Nội dung |
|---|---|---|
| Shopee REST client | `src/shopee.rs` | OAuth (authorize link + đổi/refresh token), ký HMAC-SHA256, Product/Order/Chat v2 |
| Local store | `src/db.rs` | settings (partner_id/key **local-only**), token + expiry, hàng đợi **draft duyệt**, activity log |
| REST API | `src/api.rs` | account/settings/oauth-callback/orders/chat/drafts/engine + **autonomy gate** (observe/draft/live) |
| LLM bridge | `src/llm.rs` | completions theo **LLM profile riêng** của app (soạn trả lời khách) |
| Heartbeat engine | `src/engine.rs` | đọc tin mới/đơn mới → **draft** trả lời CSKH → chỉ gửi khi Approve (hoặc `live`) |
| MCP server | `src/mcp.rs` | `shopee-mcp` → `shopee_*` tools (đọc đơn, đọc/gửi chat draft-first, tồn kho…) |
| Web UI | `web/` | React + AntD: Đơn hàng, Hộp thoại + hàng đợi duyệt, Settings (nhập partner key + nút Authorize) |
| Skills/Personas | `skills/`, `personas/` | `shopee-cskh` (trả lời khách, draft-first), persona bán hàng |
| SenClaw bridge | `src/senclaw.rs` | knowledge (nhớ khách/đơn) + wiki (chính sách shop làm nguồn trả lời) |

**Nguyên tắc an toàn (bê từ moltbook):** mặc định **draft mode** — mọi tin gửi
đi đều qua nút **Approve**; `partner_key`/token chỉ nằm trong SQLite local, chỉ
gửi tới endpoint chính thức của Shopee.

## 7. Cần bạn làm rõ trước khi implement

1. **Bạn là seller Shopee?** Con đường A cần tài khoản seller + đăng ký partner
   app trên open.shopee.com (bạn tự đăng ký, mình không tạo tài khoản hộ).
2. **"Đăng bài" là đăng gì?** Đăng sản phẩm/khuyến mãi (✅ official) hay đăng
   Shopee Feed/Live (❌ không có API)?
3. **"Duyệt hội nhóm" là gì?** Shopee không có groups. Bạn muốn nhóm nội bộ của
   app, hay đang nghĩ tới nền tảng khác (Facebook…)?
4. **"Nhắn tin" tới ai?** Trả lời khách của shop bạn (✅ Chat API) hay nhắn user
   bất kỳ (❌ = spam, không làm)?

## 8. Ranh giới mình sẽ / không làm

**Sẽ làm:** Shopee **Seller** Space App qua Official Open Platform — OAuth token,
quản lý sản phẩm/đơn, **CSKH tự động draft-first qua Chat API**, nhớ khách bằng
knowledge, extension chỉ để hứng OAuth redirect + đọc màn hình user-driven.

**Không làm:** trộm session token gọi internal API, **kỹ thuật né anti-bot ("không
bị chặn")**, DM/spam hàng loạt tới user không phải khách của shop, crawler toàn
sàn chạy nền. Không phải vì thiếu năng lực kỹ thuật — mà vì đó là vùng vi phạm
ToS + spam, và **chính con đường đó mới hay bị Shopee chặn**. Con đường official
mới là thứ "không bị chặn" thật sự.

---

*Nguồn:* [Shopee Open Platform](https://open.shopee.com) ·
[API v2 guide (api2cart)](https://api2cart.com/api-technology/shopee-api/) ·
[Shop auth flow (Wendee)](https://wendeehsu.medium.com/shopee-openapi-handsup-e0daca280f75)
