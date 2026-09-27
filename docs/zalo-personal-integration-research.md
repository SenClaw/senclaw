# Nghiên cứu: App + Chrome Extension kết nối Zalo cá nhân (token web, remote control, nhắn tin/nhóm/tìm kiếm)

> Trạng thái: **RESEARCH / DESIGN** — chưa code. Ngày 2026-07-20.
> Phạm vi: tự động hóa **tài khoản Zalo cá nhân của chính người dùng** cho SenClaw agent
> (đăng bài, nhắn tin, duyệt nhóm, tìm kiếm), qua session Zalo Web, có Chrome extension
> để lấy token + remote control, với chiến lược giảm rủi ro bị Zalo khóa.

---

## 0. TL;DR

- Yêu cầu của bạn (đăng bài cá nhân, nhắn tin bất kỳ, duyệt hội nhóm, tìm kiếm) **không nằm trong khả năng của Zalo OA API** — cái mà SenClaw đang dùng ở `apps/ai-chat` và `apps/crm`. OA chỉ là kênh CSKH doanh nghiệp (khách phải nhắn trước, cửa sổ 48h, ZNS template, không có nhóm/tìm kiếm/feed).
- Đúng hướng là **Zalo Web cá nhân** qua thư viện **`zca-js`** (unofficial, 149 method: `sendMessage`, `createGroup`, `getAllGroups`, `findUser`, `getMultiUsersByPhones`, `addUserToGroup`, `joinGroupLink`, `listen` realtime…). Xác thực bằng **QR** hoặc bằng **cookie + imei + userAgent** trích từ Zalo Web.
- **Chrome extension** đóng 2 vai trò: (1) **trích session** (cookie/imei/userAgent từ `chat.zalo.me`) đưa về daemon; (2) tùy chọn **thực thi in-page** các hành động rủi ro cao ngay trong tab Zalo thật để traffic mang đúng fingerprint/cookie/IP của người dùng → khó bị chặn nhất.
- **Đăng bài (feed/nhật ký) là điểm yếu**: zca-js không có API post feed cá nhân → phải làm bằng **DOM automation trong extension**. Nhắn tin / nhóm / tìm kiếm thì zca-js làm tốt.
- Rủi ro **bị khóa tài khoản là thật và Zalo chủ động phát hiện**. Toàn bộ thiết kế dưới đây tối ưu cho *một tài khoản của chính bạn, dùng cá nhân/agent*, **không** cho spam hàng loạt hay nhắm người lạ — đó mới là thứ kích hoạt ban.

---

## 1. Hai thế giới Zalo — chọn đúng lớp

| | **Zalo OA API (official)** | **Zalo Web cá nhân (zca-js, unofficial)** |
|---|---|---|
| SenClaw hiện dùng | ✅ `apps/ai-chat/src/channels/zalo.rs`, `apps/crm/src/channels/zalo.rs` | ❌ chưa có |
| Endpoint | `openapi.zalo.me/v2.0|v3.0/oa`, `oauth.zaloapp.com/v4` | `chat.zalo.me`, `id.zalo.me`, `wpa.chat.zalo.me`, `*.chat.zalo.me` |
| Xác thực | app_id/app_secret + access/refresh token nhập tay, rotate refresh | cookie + imei + userAgent (QR hoặc trích từ browser) |
| Nhắn tin | Chỉ khách đã nhắn OA trước, cửa sổ CSKH / ZNS template | Nhắn **bất kỳ** user/nhóm mình có quyền |
| **Đăng bài cá nhân** | ❌ Không | ⚠️ Không có API (làm bằng DOM) — xem §6 |
| **Duyệt hội nhóm** | ❌ Không | ✅ `getAllGroups`, `getGroupInfo`, `getGroupMembersInfo` |
| **Tìm kiếm user** | ❌ Không | ✅ `findUser`, `findUserByUsername`, `getMultiUsersByPhones` |
| Realtime nhận tin | Polling REST | ✅ WebSocket `listen()` |
| ToS / rủi ro ban | Hợp lệ, ổn định | Vi phạm ToS, **rủi ro khóa acc** |
| Hợp pháp/ổn định | Cao | Thấp — cần chiến lược chống chặn (§7) |

**Kết luận:** giữ OA adapter cho kênh CSKH doanh nghiệp; thêm một **kênh mới "zalo-personal"** cho các yêu cầu của bạn. Đây là hai kênh song song, không thay thế nhau.

---

## 2. zca-js — thư viện lõi (đã kiểm chứng)

- npm: `zca-js@2.1.2`, MIT, repo `github.com/RFS-ADRENO/zca-js`.
- Deps hé lộ kiến trúc: `crypto-js` (AES giải mã `zpw_enk`), `ws` (WebSocket listener), `tough-cookie` (cookie jar), `pako` (giải nén gzip/deflate của Zalo), `json-bigint`, `spark-md5`.
- **149 API method** đã liệt kê từ tarball. Nhóm liên quan yêu cầu của bạn:
  - **Nhắn tin:** `sendMessage`, `sendVoice`, `sendVideo`, `sendSticker`, `sendCard`, `sendLink`, `forwardMessage`, `uploadAttachment`, `deleteMessage`, `undo`, `addReaction`, `sendTypingEvent`, `sendSeenEvent`.
  - **Nhóm:** `createGroup`, `getAllGroups`, `getGroupInfo`, `getGroupMembersInfo`, `addUserToGroup`, `removeUserFromGroup`, `changeGroupName`, `changeGroupAvatar`, `addGroupDeputy`, `enableGroupLink`/`disableGroupLink`, `getGroupLinkInfo`, `joinGroupLink`, `inviteUserToGroups`, `leaveGroup`, `disperseGroup`, `reviewPendingMemberRequest`, `getPendingGroupMembers`, `upgradeGroupToCommunity`.
  - **Tìm kiếm / danh bạ:** `findUser`, `findUserByUsername`, `getMultiUsersByPhones` (tra theo SĐT), `getUserInfo`, `getAllFriends`, `sendFriendRequest`, `acceptFriendRequest`, `getFriendRecommendations`.
  - **Feed/board (đọc):** `getListBoard`, `getFriendBoardList`, `createNote` (ghi chú nhóm) — **không** có "post feed cá nhân".
  - **Realtime:** `listen()` → `listener.on("message" | ...)`. **Chỉ 1 web-listener/acc**; mở Zalo Web trên trình duyệt sẽ tự ngắt listener.
  - **Giữ session:** `keepAlive`, `getContext`, `getCookie`, `getOwnId`, `fetchAccountInfo`.

### Xác thực (2 cách)
```ts
// Cách A: QR — quét bằng app Zalo trên điện thoại
const api = await zalo.loginQR();               // GET id.zalo.me/account/authen/qr/generate → waiting-scan → waiting-confirm

// Cách B: cookie (headless, không cần quét lại) — chính là thứ extension trích ra
type Credentials = { imei: string; cookie: Cookie[]; userAgent: string; language?: string };
const api = await zalo.login(credentials);       // loginCookie(ctx, credentials)
```
Login sau đó gọi `id.zalo.me/account/logininfo` + `wpa.chat.zalo.me/api/login/get` để lấy **`zpw_enk`** (secret key).

### Mã hóa Web API (để hiểu vì sao phải dùng lib, không tự gọi HTTP)
- Zalo mã hóa **cả request params và response** ở tầng ứng dụng (trên HTTPS).
- Secret key `zpw_enk` lấy từ `logininfo`, **AES-CBC / PKCS7, IV = 16 byte 0, salt rỗng**, key Base64.
- Client tự `encodeAES(params)` khi gửi và `decodeAES(response)` khi nhận; lỗi giải mã → retry ≤3 lần rồi "Change zkey" (refresh key). → **Không thể chỉ đơn thuần gọi endpoint bằng reqwest**; phải tái hiện encode/decode (zca-js đã làm) hoặc thực thi trong page (extension) nơi crypto có sẵn.

**Ngụ ý kiến trúc:** SenClaw là Rust. Không port lại toàn bộ crypto + 149 method sang Rust ngay. Chạy **zca-js làm Node sidecar** do daemon điều khiển (giống mô hình sidecar `ort`/VieNeu-TTS trong repo). Về sau nếu cần mới port dần lớp encode/decode + vài method nóng sang Rust.

---

## 3. Trạng thái SenClaw hiện tại (từ map codebase)

- **Chưa có Zalo cá nhân.** Hai adapter tồn tại đều là **OA official + polling**:
  - `apps/ai-chat/src/channels/zalo.rs` (226 dòng) — bản tham chiếu.
  - `apps/crm/src/channels/zalo.rs` (253 dòng) — bản sao gần như y hệt.
  - Endpoint: `OA_V2=openapi.zalo.me/v2.0/oa`, `OA_V3=.../v3.0/oa`, `OAUTH=oauth.zaloapp.com/v4/oa/access_token`. Poll `listrecentchat`→`conversation`, gửi `v3.0/oa/message/cs`. Refresh khi lỗi `-216`. Token nhập tay, chỉ rotate refresh — **không có OAuth code / QR**.
  - Convention sub-app: mỗi channel là cặp hàm `poll(db, ch) -> Vec<Inbound>` và `send(db, ch, external_id, text)`; scheduler `apps/ai-chat/src/channels/mod.rs::poll_scheduler` chạy 15s/lần cho `kind ∈ {zalo, facebook, tiktok}`.
- **Daemon chính (`src/channels/`)** có interface trait `#[async_trait] pub trait Channel` (mirror `IChannel` cũ): `connect/disconnect/is_connected/send_message/send_file/owns_jid/on_message/on_metadata`. Đã hiện thực: `telegram`, `feishu` (submodule + **WebSocket** `feishu/ws.rs`), `wechat`, `qq`, `app`. **Feishu là khuôn mẫu tốt nhất** cho một kênh dùng WebSocket + token refresh — copy layout của nó cho Zalo cá nhân.
- **Chrome extension** `senclaw-extension-chrome/` (WXT + React MV3) **đã có nhưng KHÔNG có code Zalo** — nó là remote browser-control agent:
  - `wxt.config.ts`: permissions `tabs, tabGroups, activeTab, storage, scripting, sidePanel, alarms`, `host_permissions: <all_urls>`, side panel.
  - `entrypoints/background.ts` (MV3 service worker) mở WS `ws://<host>:<port>/browser`, backoff `[1,2,4,8,16,30]s`, heartbeat 15s mang `agent_tabs`, `chrome.alarms` keepalive ~0.4 phút, `withTabLock` serialize theo tab.
  - `agent/`: `DomExtractor`, `DomTreeBuilder`, `ActionExecutor`, `TabsController`, `SearchEngine`, `CrawlEngine`, `InteractiveDetector`… → **đã có sẵn hạ tầng DOM automation** để tái dùng cho phần "đăng bài" của Zalo.
  - Phía daemon: `src/agent/workbench_bridge.rs`, `src/agent/agent_pool/`.

**→ Tận dụng:** extension đã có WS-to-daemon + DOM engine; chỉ cần thêm một "Zalo module" (content script chạy trên `*.zalo.me` + vài lệnh mới trong protocol). Không phải làm extension từ đầu.

---

## 4. Kiến trúc đề xuất (hybrid: sidecar + in-page)

```
                    ┌────────────────────────────────────────────┐
                    │  SenClaw daemon (Rust)                       │
                    │                                              │
   Zalo Personal    │  src/channels/zalo_personal/ (Channel trait) │
   Channel  ◄───────┤    ├─ session store (cookie/imei/UA/zpw_enk) │
                    │    ├─ router: chọn SIDECAR hay IN-PAGE        │
                    │    └─ rate-limiter / human-pacer (§7)        │
                    │            │                    │            │
                    │   WS /zalo │            stdio   │            │
                    └────────────┼────────────────────┼───────────┘
                                 │                    │
                 ┌───────────────▼──────┐   ┌─────────▼───────────────┐
                 │ Chrome Extension      │   │ zca-js Node sidecar     │
                 │ (Zalo module)         │   │ (loginCookie + 149 API) │
                 │  • trích cookie/imei/ │   │  • sendMessage/group/   │
                 │    UA từ chat.zalo.me │   │    findUser realtime     │
                 │  • in-page executor   │   │  • WebSocket listen()    │
                 │    (post feed / hành  │   └──────────┬──────────────┘
                 │    động rủi ro cao)   │              │
                 └──────────┬────────────┘        chat.zalo.me API
                            │  (tab Zalo thật của user)
                     chat.zalo.me (DOM + fetch same-origin)
```

### Hai đường thực thi (router chọn theo mức rủi ro)
1. **SIDECAR (mặc định, nhanh):** daemon → zca-js sidecar → gọi thẳng API Zalo. Dùng cho: `findUser`, `getAllGroups`, `getGroupInfo`, `getMultiUsersByPhones`, nhận tin realtime (`listen`), gửi tin 1-1 với người đã là bạn/nhóm. Traffic đi từ **IP máy chủ** — cần cookie/imei/UA khớp thật (extension cung cấp), nhưng IP/geo có thể khác → rủi ro trung bình.
2. **IN-PAGE (extension, khó phát hiện nhất):** daemon → extension → chạy trong tab `chat.zalo.me` đang đăng nhập của user. Traffic mang **đúng cookie + fingerprint + IP + TLS** của người dùng → gần như không phân biệt được với thao tác tay. Dùng cho: **đăng bài feed** (bắt buộc, vì không có API — §6), các hành động nhạy cảm (kết bạn hàng loạt nhẹ, join nhóm, gửi burst). Có 2 kiểu:
   - **DOM automation** (dùng lại `ActionExecutor`/`DomExtractor` sẵn có): click/gõ như người.
   - **In-page fetch**: inject script gọi API web Zalo ngay trong context page → hàm encrypt/decrypt của Zalo có sẵn, không cần tự làm crypto.

### Vì sao hybrid chứ không chọn 1
- Chỉ sidecar: nhanh nhưng IP máy chủ + không đăng bài được + dễ bị "verify-client".
- Chỉ extension in-page: an toàn nhất nhưng chậm, phụ thuộc tab mở, khó chạy 24/7.
- **Hybrid**: đọc/tra cứu/nhận tin qua sidecar (rẻ, realtime); ghi/nhạy cảm/đăng bài qua in-page (an toàn). Router trong daemon quyết định theo bảng rủi ro (§7).

---

## 5. Vai trò Chrome Extension — chi tiết

### 5.1 Trích session (bootstrap token)
Content script trên `*.zalo.me` thu:
- **cookie**: `chrome.cookies.getAll({ domain: ".zalo.me" })` (cần permission `cookies` + host `*://*.zalo.me/*`). Lấy cả `zpw_sek`, `zpsid`, `app.com`… dạng mảng `SerializedCookie` mà zca-js `loginCookie` nhận.
- **imei**: đọc từ `localStorage`/IndexedDB của Zalo Web (khóa chứa device id) — zca-js `ZaloDataExtractor` làm đúng việc này.
- **userAgent**: `navigator.userAgent` của chính browser đó (phải khớp với cookie).
Gửi bộ 3 về daemon qua WS `/zalo` (frame mới `ZaloSession { cookie, imei, userAgent }`). Daemon lưu vào session store, nạp cho sidecar `login()`.

> Lưu ý an toàn/quyền riêng tư: cookie Zalo là bí mật cấp session. Lưu **mã hóa tại chỗ** (giống cách OA adapter redact secret `••••••` ở `apps/*/src/api.rs`), không log ra ngoài, không đẩy lên đâu ngoài daemon local.

### 5.2 In-page executor (remote control)
Mở rộng protocol WS hiện có (`types/protocol.ts`) thêm lệnh Zalo:
- `ZaloPostFeed { text, images[] }` → DOM automation (§6).
- `ZaloDomAction { selector, action, value }` → dùng `ActionExecutor` sẵn có.
- `ZaloInPageFetch { url, encryptedParams }` → inject fetch same-origin, trả response đã giải mã.
Extension đã có `withTabLock`, heartbeat, backoff → tái dùng nguyên. Chỉ cần một content script `entrypoints/zalo.content.ts` match `*://chat.zalo.me/*`.

### 5.3 "Extension gọi API để không bị chặn"
Đây chính là lý do in-page tồn tại: khi extension gọi/điều khiển **trong tab thật**, request đi kèm cookie httpOnly, TLS fingerprint của Chrome thật, IP thật, và pattern thời gian do người-hoặc-pacer điều phối → Zalo rất khó tách khỏi hành vi người dùng. Ngược lại, sidecar server-side dễ lộ nếu IP/UA/nhịp bất thường. Router nên đẩy **mọi hành động ghi có rủi ro** qua đường in-page khi tab Zalo đang mở; fallback sidecar khi không có tab.

---

## 6. "Đăng bài" — điểm yếu, phải xử riêng

- zca-js **không** có method đăng bài lên nhật ký/tường cá nhân (chỉ có `getListBoard`/`getFriendBoardList` để *đọc*, và `createNote` cho ghi chú **nhóm**).
- Do đó **đăng bài chỉ làm được qua extension in-page**:
  1. Điều hướng tab tới khu vực soạn bài trên Zalo Web.
  2. `ActionExecutor`: click ô soạn → gõ nội dung → đính ảnh (upload qua input file) → click "Đăng".
  3. Hoặc reverse endpoint feed và in-page fetch (rủi ro hơn nếu Zalo đổi API).
- Cần chấp nhận: UI Zalo Web đổi → selector gãy → cần lớp selector tự phục hồi (repo đã có `SelectorMap`, `LayoutCache`, `InteractiveDetector` để dựa vào).
- Nếu chỉ cần đăng bài "kiểu doanh nghiệp", cân nhắc dùng OA/ZNS thay vì feed cá nhân.

---

## 7. Chống bị Zalo chặn (cho 1 tài khoản của bạn, dùng cá nhân)

> Mục tiêu: giữ **một** tài khoản của chính bạn hoạt động ổn định cho agent cá nhân. Đây là kỹ thuật *độ tin cậy / tránh false-positive chống lạm dụng*, **không** phục vụ spam hàng loạt hay nhắm người lạ (đó mới là thứ Zalo ban mạnh nhất và nằm ngoài phạm vi hỗ trợ).

**Nguyên tắc "một session thật, hành xử như người":**
1. **Fingerprint khớp tuyệt đối:** cookie + imei + userAgent phải là bộ trích từ **đúng** browser/máy user. Đừng random imei/UA — lệch bộ ba là cờ đỏ đầu tiên Zalo kiểm (`verify-client`).
2. **Một listener/acc:** không chạy `listen()` khi Zalo Web đang mở; không mở 2 session song song.
3. **`keepAlive` thay vì reconnect bão:** giữ WebSocket sống bằng heartbeat, tránh login lại liên tục.
4. **Human-pacer trong daemon:** hàng đợi hành động có throttle + jitter ngẫu nhiên (vd 3–8s giữa tin nhắn), giới hạn/ngày, gửi `sendTypingEvent`/`sendSeenEvent` trước khi trả lời để giống người.
5. **Ưu tiên in-page cho hành động ghi:** như §5.3.
6. **Tránh các hành vi kích hoạt ban** (không hỗ trợ tự động hóa các việc này ở quy mô lớn): kết bạn hàng loạt người lạ, quét/thêm SĐT lạ vào nhóm, join nhóm ồ ạt, gửi nội dung giống nhau cho nhiều người. Đây là ranh giới: agent cá nhân trả lời hội thoại của **chính bạn** thì ổn; biến nó thành máy gửi tin đại trà thì vừa bị ban vừa ngoài phạm vi.
7. **Xử lý challenge nhẹ nhàng:** bắt lỗi session hết hạn / `-216` / `verify-client` → tạm dừng, phát QR để đăng nhập lại (đừng thử-lại vô hạn — đã có bài học `error-loop-guard` trong repo).
8. **Warm-up:** tài khoản mới/ít hoạt động thì tăng tần suất từ từ, đừng bật full-throttle ngày đầu.

**Bảng router rủi ro (gợi ý):**

| Hành động | Đường mặc định | Ghi chú |
|---|---|---|
| `findUser`, `getUserInfo`, `getAllGroups`, `getGroupInfo` | Sidecar | đọc, rủi ro thấp |
| Nhận tin realtime | Sidecar `listen()` | 1 listener |
| Trả lời tin trong hội thoại có sẵn | Sidecar (in-page nếu tab mở) | + typing/seen event |
| Gửi tin cho người mới / nhiều người | In-page + pacer | rủi ro cao |
| Kết bạn / join nhóm / thêm thành viên | In-page + pacer + cap thấp | rủi ro cao |
| **Đăng bài feed** | In-page DOM (bắt buộc) | không có API |

---

## 8. Tích hợp vào SenClaw (kế hoạch triển khai)

### Phase 0 — Spike (chứng minh khả thi, 1–2 ngày)
- Dựng `apps/zalo-personal/sidecar/` (Node) dùng `zca-js`: `loginQR` → in cookie/imei/UA → `getAllGroups`, `findUser`, `sendMessage` cho chính mình. Xác nhận luồng chạy được với **tài khoản test của bạn**.

### Phase 1 — Sidecar + session store
- `src/channels/zalo_personal/` theo khuôn `feishu/` (submodule + WS): `channel.rs` (impl `Channel` trait), `session.rs` (lưu cookie/imei/UA/zpw_enk mã hóa), `sidecar.rs` (spawn + JSON-RPC qua stdio tới Node), `types.rs`.
- Wire vào boot sequence như các channel khác; realtime qua `listen()` → `on_message`.

### Phase 2 — Extension Zalo module
- Thêm `senclaw-extension-chrome/src/entrypoints/zalo.content.ts` (match `*.zalo.me`), permission `cookies`.
- Mở rộng `types/protocol.ts` + `background.ts` switch: `ZaloSession`, `ZaloPostFeed`, `ZaloDomAction`, `ZaloInPageFetch`.
- Daemon: WS endpoint `/zalo` (hoặc mở rộng `/browser`) nhận session + đẩy lệnh in-page.

### Phase 3 — Router + human-pacer
- Lớp `rate.rs`/`pacer.rs`: hàng đợi có throttle/jitter/cap, chọn sidecar vs in-page theo bảng §7.
- Bắt & báo cáo challenge (`verify-client`/`-216`) → phát QR re-login qua UI.

### Phase 4 — UI + MCP
- UI kênh (theo `Channels.tsx`): trạng thái session, nút "Quét QR / Nạp từ extension", đèn health.
- MCP server `senclaw-zalo` (theo naming convention CLAUDE.md: `mcp__senclaw-zalo__zalo_*`): `zalo_send`, `zalo_find_user`, `zalo_list_groups`, `zalo_group_info`, `zalo_post_feed`… để agent gọi.

### Rủi ro kỹ thuật cần theo dõi
- zca-js phụ thuộc endpoint web nội bộ → Zalo đổi là gãy; pin version + có test smoke.
- Selector feed đổi → đăng bài gãy; dựa `SelectorMap`/`InteractiveDetector`.
- Node sidecar là thành phần lạ trong repo Rust → theo mô hình sidecar đã có (ort/VieNeu), đóng gói kèm bản phát hành.
- Cookie hết hạn định kỳ → cần luồng re-auth mượt, đừng để loop lỗi.

---

## 9. Nguồn tham khảo

- Zalo For Developers (OA/OAuth chính thức) — https://developers.zalo.me/docs
- User Access Token V4 (social-api) — https://developers.zalo.me/docs/api/social-api/tham-khao/user-access-token-post-4316
- `zca-js` (unofficial Zalo API, JS) — https://github.com/RFS-ADRENO/zca-js · npm https://www.npmjs.com/package/zca-js
- `openzca` (CLI trên zca-js) — https://openzca.com/ · https://github.com/darkamenosa/openzca
- OpenClaw Zalo personal plugin (mẫu tích hợp agent) — https://docs.openclaw.ai/plugins/zalouser · https://github.com/caochitam/zalo-personal
- Phân tích mã hóa Web API Zalo (AES-CBC, `zpw_enk`, `logininfo`) — https://viblo.asia/p/zalo-da-ma-hoa-web-api-cua-ho-nhu-the-nao-yMnKMY4zK7P
- Zalo OA API wrappers (đối chiếu official) — https://github.com/nh4ttruong/zalo-oa-api-wrapper · https://github.com/kyled7/zalo-api
