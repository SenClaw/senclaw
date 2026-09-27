# Instagram — Nghiên cứu tích hợp (đăng bài · nhắn tin · tìm kiếm · duyệt post) cho `apps/social`

> Trạng thái: **Nghiên cứu / thiết kế** — 2026-07-20. Chưa implement.
> Phạm vi: Instagram là **một nền tảng bên trong `apps/social`**, KHÔNG phải một Space App mới. Toàn bộ khung (extbridge WS :9223, extension bắt token, cadence governor, official-API stubs, MCP `social-mcp` 10 tool) đã có sẵn và chạy được. Tài liệu này chỉ đào sâu phần **Instagram-specific** còn thiếu.
> Liên quan: [`social-app-extension-design.md`](social-app-extension-design.md) (kiến trúc chung), [`social-extension-multiplatform.md`](social-extension-multiplatform.md) (ma trận đa nền tảng), [`shopee-app-research.md`](shopee-app-research.md) (ranh giới "chỉ official API").
> Nguồn dữ kiện: Meta for Developers docs (developers.facebook.com/docs/instagram-platform), thư viện tham chiếu `instagrapi`/`aiograpi`, fact-check lại các con số nhạy-cảm-thời-gian ngày 2026-07-20.

---

## 0. TL;DR (đọc cái này trước)

1. **Instagram = 2 tầng, bắt buộc lai (hybrid).** Chỉ **đăng bài** (Business/Creator) là có API chính thức. **Nhắn tin, tìm kiếm, duyệt feed, group-DM** thực tế phải đi qua **phiên đăng nhập thật trong extension** (private web-API). Không có cách nào khác.
2. **"Hôi nhóm" cần làm rõ.** Instagram **KHÔNG có nhóm kiểu Facebook Group.** "Nhóm" trên IG chỉ có thể là: (a) **group DM** — thread chat nhiều người (làm được qua private API `direct_v2/create_group_thread`), (b) **Broadcast Channel** (KHÔNG có API gửi, chỉ tạo/gửi tay trong app), hoặc (c) cộng đồng theo **hashtag**. Xem [§2](#2-làm-rõ-hôi-nhóm-instagram-không-có-group). Việc "duyệt hội nhóm" như trên Facebook chỉ áp dụng cho Facebook, không áp dụng cho IG.
3. **DM chính thức KHÔNG cold-DM được.** API messaging của Meta là *reactive*: chỉ nhắn được cho người đã nhắn mình trước, trong **cửa sổ 24 giờ**, định danh bằng **IGSID** (không nhắn theo @username). Muốn chủ động nhắn / nhắn hàng loạt → chỉ còn đường private API, rủi ro khoá tài khoản cao — đây là hành vi bị 2025–2026 quét mạnh nhất.
4. **Chống bị chặn = đi đúng đường, không phải né kỹ.** Đường ít-bị-phát-hiện-nhất là **`fetch` same-origin chạy TRONG tab instagram.com đã đăng nhập** (content script), daemon chỉ ra lệnh qua WS. Đây thực sự *là* trình duyệt thật của user (cùng IP/cookie/TLS/device) nên không có gì để "giả". Server-side replay sessionid từ IP/máy khác là cái bẫy tự bật checkpoint. Xem [§6](#6-chống-bị-chặn-a1-vs-b).
5. **Scaffold hiện tại có 3 lỗ hổng Instagram-specific** cần vá (xem [§7](#7-ánh-xạ-vào-scaffold-hiện-có--việc-cụ-thể)): (i) extension chỉ bắt `authorization/x-csrf-token`, **thiếu** `x-ig-app-id / x-ig-www-claim / x-asbd-id / csrftoken`; (ii) `replayApi()` đang `fetch` trong **service worker** (cross-origin, kém tàng hình) — với IG phải chuyển sang **fetch same-origin trong content script IG**; (iii) `channels/instagram.rs` + `injected.js` IG signer còn là stub.

---

## 1. Nhu cầu của Sếp ↔ đường khả thi

| Nhu cầu | Official API (ToS-clean) | Private web-API qua extension (vi phạm ToS) |
|---|---|---|
| **Đăng bài** (ảnh/video/reel/carousel/story) | ✅ Có — tài khoản Business/Creator, 2 bước container→publish | ✅ Có (`media/configure`) nhưng **không nên** khi official đã đủ |
| **Nhắn tin (DM)** | 🟡 Chỉ *reactive* trong cửa sổ 24h, không cold-DM, không group | ✅ Đầy đủ (gửi chủ động, group DM) — **rủi ro cao nhất** |
| **Tìm kiếm** (user/hashtag/place/top) | ❌ Chỉ Hashtag Search (≤30 tag/7 ngày, cần app review) | ✅ `fbsearch/topsearch_flat`, `users/search`, `tags/search`, `fbsearch/places` |
| **Duyệt post / feed** (home/explore/hashtag/của người khác) | ❌ Không có (Basic Display API đã đóng 04-12-2024) | ✅ `feed/timeline`, `discover/topical_explore`, `feed/tag/{tag}`, `feed/user/{id}`, `clips/*` |
| **"Duyệt hội nhóm"** | ❌ IG không có Group | 🟡 Chỉ có *group DM thread* (`direct_v2`) — không phải "nhóm" theo nghĩa FB |

**Kết luận:** đăng bài → ưu tiên official. Mọi thứ còn lại (tìm kiếm, duyệt, DM chủ động, group DM) → chỉ private API qua extension. Đây đúng là mô hình 2-tầng mà `apps/social` đã dựng.

---

## 2. Làm rõ "hôi nhóm": Instagram KHÔNG có Group

Đây là điểm phải chốt với Sếp trước khi build, vì nó đổi cả thiết kế:

- Instagram **không có** khái niệm "Group/Hội nhóm" như Facebook Groups. Không có API để duyệt/đăng vào nhóm.
- Ba thứ dễ bị gọi nhầm là "nhóm":
  1. **Group DM** — cuộc trò chuyện chat có ≥3 người. **Làm được** qua private API: tạo `direct_v2/create_group_thread/`, đọc `direct_v2/inbox/`, gửi `direct_v2/threads/{id}/broadcast/text/`. Không có API official.
  2. **Broadcast Channel** — kênh phát một chiều của creator. **KHÔNG có API gửi/tạo** (chỉ thao tác tay trong app; official chỉ có luồng *đọc* qua Content Library cho nghiên cứu). Bỏ khỏi phạm vi tự động hoá.
  3. **Cộng đồng hashtag** — không phải nhóm, chỉ là feed theo tag; duyệt qua `feed/tag/{tag}/`.
- ⇒ Trong `apps/social`, tool `social_groups` **chỉ có nghĩa với Facebook**. Với Instagram nên map "nhóm" thành **group DM** và đặt tên đúng để agent không hiểu lầm.

---

## 3. Đường CHÍNH THỨC — Instagram Graph / "Instagram API with Instagram Login"

### 3.1 Hai cấu hình official (chọn 1)

| | **Instagram API with Instagram Login** (khuyên dùng, ra 07/2024) | **Instagram API with Facebook Login** (cũ) |
|---|---|---|
| Host | `graph.instagram.com` | `graph.facebook.com` |
| Cần Facebook Page? | **KHÔNG** | **CÓ** (IG phải liên kết 1 Page) |
| Đăng nhập bằng | Tài khoản Instagram | Tài khoản Facebook |
| Scope (publish) | `instagram_business_basic` + `instagram_business_content_publish` | `instagram_basic` + `instagram_content_publish` + `pages_*` |
| Scope (messaging) | `instagram_business_manage_messages` | `instagram_manage_messages` |
| Đánh đổi | KHÔNG gắn được product/user tag, không chạy ads qua path này | Có tagging/ads |

> ⚠️ Scope cũ `instagram_basic`/`instagram_content_publish` đã bị **deprecate cho path Instagram-Login từ 27-01-2025**; build mới phải dùng `instagram_business_*`. (Đã fact-check lại 2026-07-20.)

**Yêu cầu chung:** tài khoản phải là **Professional (Business hoặc Creator)** — tài khoản cá nhân KHÔNG đăng qua API được. Phục vụ tài khoản *của người khác* cần **App Review + Business Verification** (Advanced Access, ~2–6 tuần, quay 1 screencast/permission). Phục vụ tài khoản *mình sở hữu* thì Standard Access là đủ (dev/tester mode).

### 3.2 Đăng bài — luồng 2 bước (đã verify khớp docs Meta)

```
1. POST /<IG_USER_ID>/media           → tạo "container", trả creation_id
     params: image_url | video_url (URL công khai) ; caption ;
             media_type=IMAGE|VIDEO|REELS|STORIES|CAROUSEL ; ...
2. (video/reel/story) GET /<creation_id>?fields=status_code  → poll tới FINISHED
3. POST /<IG_USER_ID>/media_publish   → creation_id  → đăng thật
```

- **Loại đăng được:** ảnh JPEG đơn, video, **Reels** (3s–15ph), **carousel** ≤10 media (mỗi con `is_carousel_item=true` → container cha `CAROUSEL` với `children=`), **Stories** (video ≤60s).
- **Caption:** tối đa 2200 ký tự / 30 hashtag / 20 @mention.
- **Ràng buộc:** media phải nằm ở **URL công khai** (`image_url`/`video_url`) — không upload binary trực tiếp, trừ luồng **resumable** (`upload_type=resumable`) qua `rupload`. Chỉ JPEG cho ảnh đơn (MPO/JPS bị từ chối). Video/Reels/Stories publish **bất đồng bộ** → bắt buộc poll `status_code` tới `FINISHED` trước khi `media_publish`. Không áp filter, không shopping tag, không schedule server-side (app phải tự giữ và hẹn giờ gọi publish).
- **Rate limit:** ~**100 post / 24h trượt**/tài khoản (carousel tính 1). Lưu ý mâu thuẫn tài liệu: reference `GET /content_publishing_limit` vẫn ghi `quota_total=50` — **coi 50 là sàn an toàn**, và đọc `quota_usage` sống tại runtime. (Đã verify: mâu thuẫn này có thật, còn tồn tại 2026-07.)

⇒ Đây là chỗ để vá `channels/instagram.rs` (hiện là stub) — xem [§7](#7-ánh-xạ-vào-scaffold-hiện-có--việc-cụ-thể).

### 3.3 Nhắn tin — Messaging API (đã verify)

```
POST /me/messages   (host graph.instagram.com với Instagram-Login)
  body: { recipient: { id: <IGSID> }, message: { text: "..." } }
```

- **Cửa sổ 24 giờ, reactive.** Chỉ nhắn được cho người đã nhắn/tương tác với mình, trong 24h kể từ tương tác cuối. **Không cold-DM** kể cả với follower của chính mình. API này hình dạng "chăm sóc khách hàng", không phải marketing outbound.
- **Định danh bằng IGSID**, không nhắn theo @username. IGSID chỉ lấy được qua **webhook** (`messages`) khi có người nhắn tới, hoặc **Conversations API** (`GET /me/conversations?platform=instagram`, chỉ trả ~20 tin gần nhất/thread).
- **HUMAN_AGENT tag**: gia hạn tới **7 ngày** nhưng chỉ cho **người thật** trả lời hỗ trợ (không automation/promotion), và cần permission App-Review riêng.
- **Không có group DM official.** Messaging chỉ 1-1.
- Rate: 100 req/s (text/link/reaction/sticker), 10 req/s (audio/video)/tài khoản; nguồn bên thứ ba ghi thêm trần chống-spam ~**200 DM tự động/giờ** từ 10/2024. Message tag cũ (`CONFIRMED_EVENT_UPDATE`…) bị **error 100 từ 27-04-2026**.

⇒ Với `apps/social`: `social_send_dm` chỉ nên đi official-messaging khi là **trả lời trong cửa sổ 24h**; mọi nhu cầu chủ động/group phải chuyển sang private (rủi ro cao — mặc định tắt).

---

## 4. Đường PRIVATE — web/mobile API (qua phiên thật trong extension)

Đây là đường cho **tìm kiếm / duyệt feed / DM chủ động / group DM** — những thứ official không cho. Thư viện tham chiếu để soi *shape* endpoint (KHÔNG chạy trực tiếp trong app): **`instagrapi`** (`subzeroid/instagrapi`, Python, còn sống, chuẩn nhất) và fork async **`aiograpi`**; `dilame/instagram-private-api` (Node) đã cũ. **Không có crate Rust trưởng thành** — phải tự dựng lớp endpoint.

### 4.1 Hai base endpoint

- `https://i.instagram.com/api/v1/` — API **mobile** (đầy đủ nhất: post/reels/DM).
- `https://www.instagram.com/api/v1/` — API **web** (ít cần ký hơn, hợp với đường same-origin trong extension).

### 4.2 Phiên đăng nhập gồm gì (extension phải bắt đủ)

| Loại | Giá trị | Ghi chú |
|---|---|---|
| Cookie | `sessionid`, `csrftoken`, `ds_user_id`, `mid` | `sessionid` là httpOnly — content-script `document.cookie` KHÔNG đọc được; cần quyền `cookies` + `chrome.cookies.get`, hoặc để trình duyệt tự đính khi fetch same-origin |
| Header | `X-IG-App-ID: 936619743392459` (web app id) | **bắt buộc**; hiện scaffold chưa bắt |
| Header | `X-CSRFToken` = giá trị cookie `csrftoken` | |
| Header | `X-IG-WWW-Claim` | phải **echo** từ response header `x-ig-set-www-claim`; quên → 403 lẻ tẻ |
| Header | `X-ASBD-ID`, `X-IG-App-ID`, `X-Requested-With` | `X-ASBD-ID` đổi theo bản build → **sniff sống**, đừng hardcode |
| UA | User-Agent thật của trình duyệt | web session dùng UA web; đừng trộn UA mobile |

**Đăng nhập:** ưu tiên **tái dùng phiên trình duyệt thật** (user tự đăng nhập trong Chrome), KHÔNG login bằng mật khẩu trong app (web cần `enc_password` AES-256-GCM + NaCl sealed-box theo public key xoay; mobile bọc trong Bloks — cả hai bật challenge/2FA nhiều hơn hẳn). `instagrapi` có `login_by_sessionid()` đúng cho việc bootstrap từ sessionid.

**Ký request:** **không cần** HMAC nữa — gửi `signed_body=SIGNATURE.<json-urlencoded>` (chữ "SIGNATURE" là literal); endpoint web `/api/v1` không cần ký gì. (⇒ IG signer trong `injected.js` **nhẹ hơn nhiều** so với TikTok X-Bogus: chủ yếu là gắn header + echo www-claim, không phải tái tạo thuật toán obfuscated.)

### 4.3 Bảng endpoint theo thao tác

| Thao tác | Endpoint private | Tham chiếu instagrapi |
|---|---|---|
| Đọc home feed | `POST feed/timeline/` | `get_timeline_feed` |
| Post của 1 user | `GET feed/user/{user_id}/` | `user_medias` |
| Resolve @username | `GET users/web_profile_info/?username=` | `user_info_by_username` |
| Chi tiết 1 media | `GET media/{id}/info/` | `media_info` |
| Feed hashtag | `GET feed/tag/{tag}/` + `tags/{tag}/sections/` | `hashtag_medias_recent/top` |
| Explore | `GET discover/topical_explore/` | — |
| Reels feed | `GET clips/discover/`, `clips/user/{id}/` | — |
| **Top search** (user+tag+place) | `GET fbsearch/topsearch_flat/` | `fbsearch_topsearch` |
| Tìm user / tag / place | `users/search/`, `tags/search/`, `fbsearch/places/` | `search_users/…` |
| Đăng ảnh/video/reel/story | upload `rupload_igphoto`/`rupload_igvideo` → `POST media/configure/` (`configure_to_clips`/`_to_story`/`configure_sidecar`) | `photo_upload/clip_upload/…` |
| Gửi DM | `POST direct_v2/threads/broadcast/text/` | `direct_send` |
| Đọc inbox / thread | `GET direct_v2/inbox/`, `direct_v2/threads/{id}/` | `direct_threads` |
| **Tạo group DM** | `POST direct_v2/create_group_thread/` | `direct_thread` (nhiều user_id) |
| Like/comment/follow | `media/{id}/like/`, `media/{id}/comment/`, `friendships/create/{id}/` | `media_like/…` |

Phân trang: cursor `max_id` → `next_max_id`.

---

## 5. Bắt token cho Instagram (chỉnh trong extension)

Hiện `apps/social/extension/background.js` bắt được (đúng cho FB/X/TikTok) nhưng **thiếu cho IG**:

```js
// HIỆN TẠI (background.js) — chỉ bắt 3 header, không đủ cho IG:
if (["authorization","x-csrf-token","x-secsdk-csrf-token"].includes(name)) grab[name]=h.value;
```

**Cần bổ sung cho host instagram.com** danh sách header:
`x-ig-app-id`, `x-ig-www-claim`, `x-asbd-id`, `x-csrftoken`, `x-instagram-ajax` — và echo `x-ig-set-www-claim` từ **response** (dùng `onHeadersReceived`). Cookie `sessionid/csrftoken/ds_user_id/mid` thì `hostsReady()` đã kiểm `sessionid` (đúng), nhưng để *replay same-origin* thì không cần đọc cookie ra JS — trình duyệt tự đính.

**Điểm quan trọng nhất (chống chặn):** `replayApi()` hiện chạy `fetch(url,{credentials:'include'})` **trong service worker**. Với IG đây là request **cross-origin** (Origin = `chrome-extension://…`, `Sec-Fetch-Site: cross-site`) → IG dễ gắn cờ, và mất lợi thế "same-origin như web app thật". **Sửa cho IG:** đẩy lệnh xuống **content script trong tab instagram.com** và fetch **same-origin** ở đó (hoặc dùng offscreen/pinned IG tab). Khi đó request có `Sec-Fetch-Site: same-origin`, cookie httpOnly tự đính, JA3/TLS/IP là Chrome thật của user → gần như không phân biệt được với người dùng thật. Xem [§6](#6-chống-bị-chặn-a1-vs-b).

> Token vẫn **không rời máy**: extension giữ, app chỉ biết `hosts_ready` (đã đúng với triết lý privacy của repo). Không đưa token vào URL/param gửi về app.

---

## 6. Chống bị chặn: A1 vs B

Ba cách, xếp theo độ an toàn:

- **A1 — fetch same-origin trong tab IG thật (KHUYẾN NGHỊ).** Content script trong tab `instagram.com` gọi `fetch('/api/v1/...')`. Trình duyệt tự đính cookie httpOnly `sessionid`, dùng TLS/JA3 BoringSSL thật, ra IP residential của user, UA thật, và ăn theo vòng xoay `X-IG-WWW-Claim` của chính web app. Ở mọi tầng IG thực sự soi (IP/ASN, JA3, device trust, cookie, www-claim) — đây **là** trình duyệt của user, không có gì để giả, không có gì để lệch.
- **A2 — bẫy.** Bắt cookie/header rồi cho **daemon** gọi server-side. Chỉ giữ được lớp rẻ (chuỗi cookie/header), vứt lớp đắt (IP, TLS, device) → hồ sơ phát hiện = B. **Không dùng làm đường chính.** (Đây chính là điều `replayApi` trong SW đang vô tình tiến gần tới — xem §5.)
- **B — replay sessionid từ server IP/máy khác (KHÔNG dùng).** Bật cùng lúc ≥4 tín hiệu: IP datacenter/nước ngoài, JA3 không-phải-Chrome, device-fingerprint lạ (checkpoint máy mới), `X-IG-WWW-Claim` cũ/thiếu, "đăng nhập đồng thời nhiều nơi". Đây là công thức tạo `challenge_required`.

**Nhưng tàng hình fingerprint ≠ hết rủi ro.** Vẫn phải chặn hành vi bất thường:

- **Cadence numbers cho IG** (đặt trong `cadence.rs`, chạy ~50–70% trần của tài khoản *già*, khoẻ):
  - like: ~30/giờ, 200/ngày · comment: ~12/giờ, 80/ngày · follow: ~10/giờ, 60–80/ngày
  - **DM chủ động: 20–40/giờ, 50–100/ngày** (đừng chạm trần 200/giờ) — cold-DM số lượng lớn là hành vi bị quét mạnh nhất, **nên tránh hẳn**.
  - đọc/search/feed: thoáng hơn nhưng vẫn min-gap + jitter.
- **Warm-up bắt buộc cho account mới**: ngày 1–3 chỉ đọc; tuần 2 mới bật automation ~50 action/ngày; tuần 3 mới lên ~100–150.
- **Human layer**: delay ngẫu nhiên (base 1–3s + cooldown 30–90s giữa các write), giờ hoạt động (không 24/7), "đọc trước khi ghi", đổi nội dung (không 2 tin trùng byte).
- **Error state machine** (nhánh theo lỗi, KHÔNG retry mù):
  - `challenge_required`/`checkpoint_required` (có `challenge` + `api_path`) → **DỪNG** account, freeze 6–12h, nếu `native_flow`/Bloks/selfie → **báo người** verify tay (không tự động bao giờ).
  - `feedback_required` ("action blocked") → freeze account+action đó 24–48h leo thang.
  - `429`/`please_wait_a_few_minutes` → backoff mũ, giảm concurrency.
  - `login_required` → relogin **một lần**, không loop.
- **Shadowban im lặng**: không có lỗi API; tín hiệu duy nhất là engagement từ non-follower rớt 50–70% → theo dõi chứ đừng chỉ dựa lỗi.

> Con số là quy ước cộng đồng, KHÔNG phải cam kết của Meta; từ ~2024 hạn mức được cá nhân hoá theo tuổi/độ tin account. Luôn coi là trần để đứng xa dưới.

---

## 7. Ánh xạ vào scaffold hiện có — việc cụ thể

Không tạo app mới. Việc Instagram-specific:

| # | File | Việc |
|---|---|---|
| 1 | `apps/social/extension/background.js` | Bổ sung capture header IG (`x-ig-app-id/x-ig-www-claim/x-asbd-id/x-csrftoken`) + `onHeadersReceived` để echo `x-ig-set-www-claim`. |
| 2 | `apps/social/extension/content.js` (+ background) | **Thêm đường replay same-origin trong tab IG** (A1): background gửi lệnh xuống content script IG, content script `fetch('/api/v1/…')` same-origin, trả kết quả về. Đây là thay đổi kiến trúc quan trọng nhất cho IG (thay vì fetch trong SW). |
| 3 | `apps/social/extension/injected.js` | IG signer **nhẹ**: chỉ cần đọc `csrftoken`/`X-IG-WWW-Claim`, không phải tái tạo thuật toán như TikTok. Không cần HMAC (`signed_body=SIGNATURE.`). |
| 4 | `apps/social/src/web_ops.rs` | Định nghĩa `op` cụ thể cho IG: `ig_top_search`, `ig_user_search`, `ig_tag_feed`, `ig_timeline`, `ig_explore`, `ig_user_medias`, `ig_inbox`, `ig_send_dm`, `ig_create_group_thread`. Mỗi op ánh xạ tới 1 endpoint §4.3, gửi `params.url` cho `ReplayApi`. |
| 5 | `apps/social/src/channels/instagram.rs` | Hoàn thiện `official_post`: 2 bước container→publish (§3.2), poll `status_code`, đọc `content_publishing_limit`. Cần `ig_user_id` + `access_token` trong `official_config` (schema đã có cột). |
| 6 | `apps/social/src/cadence.rs` | Thêm policy IG-specific (số ở §6). Hiện `policy_for` chung cho mọi platform — nên rẽ theo platform để IG chặt hơn TikTok ở DM. |
| 7 | `apps/social/src/mcp.rs` | MCP tool **đã đủ** (`social_post/search/feed/inbox_poll/send_dm/groups`), chỉ cần nhận `platform:"instagram"`. Cân nhắc đổi ý nghĩa `social_groups` cho IG = group DM (hoặc trả lỗi rõ "IG không có group, ý bạn là group DM?"). |
| 8 | `apps/social/src/db.rs` schema | `accounts.official_config` (JSON) giữ `ig_user_id/access_token/token_expiry`; `inbox`/`post_log`/`action_log` đã có sẵn — dùng lại. |

**Lưu ý bridge LLM** (soạn caption/reply bằng AI): `llm.request` chỉ có `{system,prompt,maxTokens,profile}` — **không có temperature**; "độ sáng tạo" phải nhét trong prompt. Có **trần output cố định**: prompt quá to bị tóm tắt âm thầm (`finish=="stop"`), `finish=="length"` là bị cắt — chia nhỏ.

---

## 8. Rủi ro & tuân thủ (nói thẳng với Sếp)

- **Đường private API vi phạm Instagram/Meta ToS**, kể cả trên tài khoản của chính mình. Rủi ro thật: action-block → feature-block → checkpoint → khoá vĩnh viễn. Không có kỹ thuật nào bảo đảm 100% không bị chặn — A1 chỉ **giảm bề mặt fingerprint**, không làm hành vi trở nên hợp lệ hay vô hình.
- IG nằm ở nhóm **rủi ro rất cao** khi tự động hoá (theo ma trận multiplatform: LinkedIn ≳ IG ≳ Zalo ≳ TikTok > FB > X > YouTube).
- **Khuyến nghị**: (a) đăng bài đi **official** (Business account); (b) mọi write private để mặc định **draft → approve → live** (autonomy gate như moltbook), không tự chạy ngầm; (c) **không cold-DM số lượng lớn**; (d) dùng account không-trọng-yếu để thử; (e) hiển thị cảnh báo rủi ro per-account trong UI (scaffold đã có chỗ).
- Ranh giới của `provider.rs` (chỉ-official) và của `web_ops.rs`/extension (private) đang **cùng tồn tại** trong repo — đây là mâu thuẫn triết lý cần Sếp chốt: `provider.rs` nói rõ "không harvest cookie, không né fingerprint", trong khi cả app `social` lại được thiết kế để làm đúng những việc đó. Nên thống nhất: `provider.rs` = tầng official, `web_ops.rs` = tầng private, và ghi rõ tầng private là opt-in có cảnh báo.

---

## 9. Build order Instagram (khi bắt tay code)

1. **Official publish trước** (an toàn, chứng minh khung): hoàn thiện `channels/instagram.rs` 2 bước + poll + `content_publishing_limit`; nối `social_post{platform:"instagram"}`. Test với 1 Business account của mình (Standard Access).
2. **Extension IG capture + A1 replay** (§7 #1–#3): bắt đủ header, chuyển replay sang same-origin content-script, chứng minh bằng 1 call read-only (`users/web_profile_info`).
3. **Read private** qua cadence: `ig_top_search`, `ig_tag_feed`, `ig_timeline`, `ig_user_medias` → nối `social_search`/`social_feed`.
4. **Cadence IG-specific** (§6) + error state machine (challenge/feedback/429).
5. **DM reactive official** (webhook + cửa sổ 24h) trước; DM chủ động private để **sau, mặc định tắt**, có cảnh báo.
6. **Group DM** (`create_group_thread`) — chỉ nếu Sếp thực sự cần, đặt tên đúng (không gọi là "nhóm").
7. Skill + persona IG + UI (Composer/Inbox/Feed) + polish.

Mỗi bước verify độc lập trước khi sang bước sau.

---

## 10. Cần verify-live trước khi build

- Trần post thật của account cụ thể (`GET /content_publishing_limit` → `quota_usage`) — docs mâu thuẫn 50 vs 100.
- `X-ASBD-ID` và `X-IG-WWW-Claim` hiện hành (sniff từ phiên thật, đừng hardcode — đổi theo bản build IG).
- doc_id GraphQL web xoay ~2–4 tuần: nếu dùng `graphql/query` thay `/api/v1`, phải bắt doc_id sống.
- Instagram-Login messaging (`graph.instagram.com/me/messages`) có bật đủ cho tài khoản Creator không, hay chỉ Business.
- Chính sách HUMAN_AGENT permission (App Review) nếu cần trả lời hỗ trợ >24h.
