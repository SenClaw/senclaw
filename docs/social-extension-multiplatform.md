> ⚠️ **SUPERSEDED for the go-forward plan by [`social-unified-design.md`](social-unified-design.md)** (2026-07-20). That doc adopts this research's best ideas (two-tier adapters, PlatformAdapter registry, autonomy gate, the per-platform token/capability matrices) but reconciles them onto the **already-built** `apps/social` (port 4520, not 4510). This file stays valuable as the **deep per-platform research** (incl. the 4 platforms out of unified scope: YouTube/Reddit/LinkedIn/Zalo).

# Social Extension đa nền tảng — Nghiên cứu & thiết kế

> Trạng thái: **RESEARCH / DESIGN** (2026-07-20). Chưa code.
> Mục tiêu: một Space App `apps/social` + mở rộng `senclaw-extension-chrome` để một agent cá nhân
> **đăng bài / nhắn tin / tìm kiếm / duyệt post** trên nhiều mạng xã hội, dùng chính phiên đăng nhập
> thật của người dùng để giảm rủi ro bị chặn.

---

## 0. Kết luận cốt lõi

1. **Không nền tảng nào cho một con đường "sạch" phủ hết 4 việc (post/DM/search/browse) trên tài khoản cá nhân.** Mô hình chung cho mọi nền tảng là **hai tầng**:
   - **Tầng API chính thức** — dùng khi có và hợp lệ (YouTube Data API, Threads API, Reddit API, X post API, đăng Page FB/IG). ToS-clean, ổn định.
   - **Tầng phiên-đăng-nhập (session-riding)** — extension điều khiển tab thật của người dùng, cho *mọi thứ API từ chối*: DM cá nhân, search/browse sâu, đăng lên profile/group cá nhân. Vi phạm ToS, hay gãy, có rủi ro khoá tài khoản.

2. **Extension chạy trong Chrome thật của người dùng là kiến trúc ít bị chặn nhất** — thừa hưởng IP nhà, fingerprint thiết bị đã được tin, cookie hợp lệ tự xoay vòng, và (với TikTok) chính VM ký request của trang. Server gọi API/headless-trên-VPS là bộ ba cờ đỏ (IP datacenter + fingerprint headless + thiết bị lạ) bị checkpoint nhanh nhất.

3. **Codebase đã có sẵn khung:** `senclaw-extension-chrome` (WXT/MV3, WS tới daemon, DOM layer page-agent), pattern Space App (`apps/moltbook`), stealth thật (`apps/mini-browser`). App mới = ghép lại + thêm **tầng bắt token** (chưa có) + **pairing secret** (hiện chỉ tin localhost).

4. **Rủi ro ToS là thật.** Xếp hạng độ nguy hiểm khi tự động hoá tài khoản cá nhân: **LinkedIn ≳ Instagram ≳ Zalo ≳ TikTok > Facebook > X > YouTube > Reddit**. Mọi hành động *write* phải qua **draft → approve → live** (autonomy gate như moltbook).

---

## 1. Ma trận năng lực theo nền tảng

Ký hiệu: ✅ API chính thức làm được (tài khoản cá nhân) · 🟡 chỉ Business/Page/Creator hoặc gated nặng · 🌐 chỉ web-session · ❌ không có.

| Nền tảng | Đăng bài | DM / nhắn tin | Search | Duyệt feed/post người khác | Blocker chính | Rủi ro khoá |
|---|---|---|---|---|---|---|
| **X (Twitter)** | ✅ API ($0.015, link $0.20) · 🌐 | 🟡 API mù mờ · 🌐 | ✅ 7 ngày · 🌐 sâu | ✅ đọc rẻ · 🌐 | ct0/csrf, xoay GraphQL id | Trung bình (ban wave 3/2026) |
| **Facebook** | ❌ profile (2018) · 🟡 Page · 🌐 | 🟡 Messenger Page (24h) · 🌐 cá nhân | 🌐 | 🌐 (group xoá API 2024) | fb_dtsg + doc_id | Cao |
| **Instagram** | 🟡 publish (Business, ~50/24h) · 🌐 | 🟡 24h window Business · 🌐 | ❌ (hashtag ≤30/7d) · 🌐 | 🌐 | X-IG-App-ID, X-IG-WWW-Claim | **Rất cao** |
| **Threads** | ✅ API (250/24h, có reply) | ❌ (Threads không có DM) | ✅ **keyword search API** | 🟡 read-own · 🌐 rộng | web read-only, write qua mobile | Cao (share auth IG) |
| **LinkedIn** | ✅ "Share on LinkedIn" (~100/ngày) | ❌ (partner-gated) · 🌐 Voyager | 🌐 Voyager | 🌐 Voyager | AED quét extension, isTrusted | **Cực cao** (ban vài ngày) |
| **Reddit** | ✅ API | ✅ API (DM+modmail) | ✅ API | ✅ API | Responsible-Builder approval | **Thấp** (chỉ shadowban nếu spam) |
| **TikTok** | 🟡 audit cho Direct Post; Upload-inbox không cần audit · 🌐 | ❌ (không có API) · 🌐 | ❌ (Research=academic) · 🌐 | 🌐 | X-Bogus/X-Gnarly/msToken (VM ký) | Cao |
| **YouTube** | ✅ Data API (`videos.insert` ~100 units từ 12/2025) | ❌ (không có DM) | ✅ `search.list`=100 units · 🌐 InnerTube quota-free | ✅ đọc (1 unit) · 🌐 | Quota / SAPISIDHASH | **Rất thấp** |
| **Zalo** | 🌐 (OA≠cá nhân) | 🌐 zca-js (OA có ZNS trả phí) | 🌐 | 🌐 | cookie **+ IMEI + UA** khớp bộ | **Rất cao** |

**Hệ quả thiết kế:** mỗi adapter có 2 tầng. Nền tảng "API-friendly" (Reddit, YouTube, Threads) ưu tiên tầng chính thức; nền tảng "session-only" (Zalo, TikTok DM, IG cá nhân, FB group) buộc tầng phiên.

---

## 2. Cơ chế bắt token/session mỗi nền tảng

Đây là phần **MỚI HOÀN TOÀN** — extension hiện chỉ điều khiển DOM, chưa có đường bắt credential.
Cookie HttpOnly (`auth_token`, `sessionid`, `li_at`, `c_user/xs`, `zpw_sek`…) **không đọc được qua `document.cookie`** → phải dùng **`chrome.cookies.get` trong background** với quyền `cookies` + host tương ứng. Token CSRF trong trang (`ct0`, `fb_dtsg`, `X-CSRFToken`, `JSESSIONID`, `modhash`, `msToken`) → content script scrape từ DOM/JS hoặc bắt từ network request.

| Nền tảng | Cookie phiên (HttpOnly) | Token CSRF / phụ | Endpoint nội bộ |
|---|---|---|---|
| X | `auth_token` | `ct0`(=x-csrf-token), bearer cứng trong JS | `x.com/i/api/graphql/*` |
| Facebook | `c_user`, `xs`, `datr` | `fb_dtsg`, `jazoest`, `lsd`, token `EAAB…` | `facebook.com/api/graphql/` (doc_id) |
| Instagram | `sessionid`, `ds_user_id` | `csrftoken`→`X-CSRFToken`, `X-IG-App-ID: 936619743392459`, `X-IG-WWW-Claim`, `fb_dtsg` | `i.instagram.com/api/v1/`, `www.instagram.com/graphql/query` |
| Threads | (dùng chung phiên IG) | `X-IG-App-ID: 238260118697367` | web **read-only**; write qua mobile Bloks |
| LinkedIn | `li_at` | `JSESSIONID` (=csrf, **bỏ dấu ngoặc kép**), `x-restli-protocol-version: 2.0.0` | `/voyager/api/*` |
| Reddit | `reddit_session` | `modhash`/`uh` → `X-Modhash` | `*.json`, GraphQL `gql` |
| TikTok | `sessionid` | `tt_csrf_token`, **`msToken`**, ký **`X-Bogus`/`X-Gnarly`** (webmssdk VM) | `tiktok.com/api/*` |
| YouTube | `SAPISID`, `__Secure-3PAPISID`, `SID/HSID/SSID` | header `Authorization: SAPISIDHASH <ts>_<sha1(ts+" "+SAPISID+" "+origin)>` (+ biến thể 1PHASH/3PHASH), `Origin` phải khớp | InnerTube `youtubei/v1/*` |
| Zalo | `zpw_*` (gồm `zpw_sek`) | **IMEI** + **User-Agent** (phải khớp bộ 3) | `chat.zalo.me` web |

**Điểm chốt bảo mật:** một khi extension mang credential nhạy cảm, WS bridge (`src/browser/bridge.rs`, hiện bind `127.0.0.1` + tin mọi kết nối localhost, **không auth**) phải thêm **pairing secret**. Dependency `qrcode.react` đã có sẵn trong extension để làm luồng QR-pairing. Token chỉ lưu trong SQLite `settings` cục bộ của app, **chỉ gửi tới đúng origin nền tảng**, đúng như contract của moltbook.

---

## 3. Kiến trúc extension đa nền tảng — PlatformAdapter

Không nhồi logic từng site vào `background.ts`. Trừu tượng hoá thành **adapter theo nền tảng**, một registry chọn adapter theo hostname của tab.

```
senclaw-extension-chrome/src/
  entrypoints/
    background.ts            # router: chọn adapter theo tab host, gọi capability
    content.ts               # DOM layer page-agent (đã có) + hook capture theo site
  social/                    # ── MỚI ──
    types.ts                 # PlatformCapability, CapturedAuth, PostArgs, DmArgs, SearchArgs
    registry.ts              # hostname → adapter
    base.ts                  # interface PlatformAdapter
    adapters/
      x.ts  facebook.ts  instagram.ts  threads.ts
      linkedin.ts  reddit.ts  tiktok.ts  youtube.ts  zalo.ts
    capture.ts               # chrome.cookies.get + scrape CSRF từ DOM/JS
    sink.ts                  # POST credential về app /api/auth/capture (qua pairing secret)
```

Interface tối thiểu mỗi adapter (TS):

```ts
interface PlatformAdapter {
  id: 'x' | 'facebook' | 'instagram' | 'threads'
    | 'linkedin' | 'reddit' | 'tiktok' | 'youtube' | 'zalo';
  matches(host: string): boolean;                    // registry dùng để định tuyến
  capabilities(): PlatformCapability[];              // ['post','dm','search','browse']

  captureAuth(): Promise<CapturedAuth>;              // cookie (background) + CSRF (content)
  post(args: PostArgs): Promise<Result>;             // tầng chính thức HOẶC drive DOM
  dm(args: DmArgs): Promise<Result>;
  search(args: SearchArgs): Promise<Item[]>;
  browse(args: BrowseArgs): Promise<Item[]>;
}
```

**Hai chiến lược thực thi mỗi capability**, adapter tự chọn theo ma trận §1:
- **Replay request nội bộ** — dùng credential đã bắt, gọi thẳng endpoint (X GraphQL, Voyager, InnerTube…). Nhanh, nhưng phải khớp header/CSRF/signature. **Để trang tự ký** khi có VM (TikTok) bằng cách `chrome.scripting` inject vào world của trang thay vì tự tái hiện X-Bogus.
- **Drive DOM** — gõ vào composer, bấm Post qua `ActionExecutor` sẵn có. Chậm hơn nhưng tự nhiên nhất (đặc biệt LinkedIn: tránh `isTrusted:false`, ưu tiên navigation/input thật thay vì `.click()` tổng hợp).

**Protocol phải sync 2 phía** (đây là hợp đồng chịu tải): thêm biến thể vào **cả** `src/browser/protocol.rs` **và** `senclaw-extension-chrome/src/types/protocol.ts`:
- `DaemonMessage::SocialCapture { platform }` → `ExtensionMessage::SocialAuth { platform, cookies, csrf, extra }`
- `DaemonMessage::SocialPost { platform, args }` / `SocialDm` / `SocialSearch` / `SocialBrowse` → `ExtensionMessage::SocialResult { request_id, status, data }`

`wxt.config.ts`: thêm `permissions: [..., 'cookies']` và `host_permissions` cho 9 origin (`*.x.com`, `*.facebook.com`, `*.instagram.com`, `*.threads.net`, `*.linkedin.com`, `*.reddit.com`, `*.tiktok.com`, `*.youtube.com`, `*.zalo.me`).

---

## 4. Backend Space App — `apps/social`

Scaffold từ `apps/moltbook/` (connector mạng xã hội, analog gần nhất). Cấu trúc `main.rs / api.rs / mcp.rs / db.rs / llm.rs / senclaw.rs`, manifest, `scripts/pack.sh`, `web/`. Chọn port trống — **4490 đã bị Shopee app dùng**, nên chọn **4510** (moltbook 4430, mini-browser 4360, video-cloner 4480, shopee 4490, kaen 4500 đã dùng).

> Lưu ý: đã có sẵn nghiên cứu **`apps/youtube`** riêng (docs/youtube-app-research.md — InnerTube > Data API, extension né BotGuard). App `social` nên **tái dùng/hợp nhất** phần YouTube đó thay vì làm lại; coi YouTube adapter ở đây là bản rút gọn trỏ về app youtube.

- **MCP** (JSON-RPC over HTTP+SSE tại `/api/mcp/sse` + `/api/mcp/message`, `autoRegister:true`): tools đặt tên `social_<platform>_<verb>` — vd `social_x_post`, `social_fb_group_post`, `social_ig_dm`, `social_reddit_search`, `social_tiktok_upload`, `social_yt_publish`, `social_zalo_send`. Mỗi tool định tuyến: nếu tầng chính thức khả dụng → gọi trực tiếp từ Rust; nếu tầng phiên → relay qua `BrowserBridge` xuống extension.
- **Autonomy gate** (bê nguyên từ moltbook): observe/draft/live. Mọi `post`/`dm` sinh **nháp** cho người duyệt trước khi `live`. Vừa an toàn nội dung, vừa tạo nhịp giống người → giảm khoá.
- **Secrets**: SQLite `settings` kv, "chỉ gửi tới đúng origin nền tảng".
- **LLM/memory/wiki**: không gọi provider trực tiếp — qua `{SENCLAW_BASE_URL}/api/space/apps/{id}/bridge` (`llm.request`, `knowledge.*`) và `/api/wiki/*`. Xem `app-space-sdk/src/bridge.rs` `SpaceClient`.
- **Tầng chính thức trong Rust** (không cần extension): OAuth2+PKCE cho X/Threads/LinkedIn/Reddit/YouTube/TikTok, refresh token lưu `settings`. Đây là phần ToS-clean nên tách riêng module `official/<platform>.rs`.

**DM là MCP tool (v1)** cho đơn giản. **v2**: nâng X-DM / FB-Messenger / Zalo thành `Channel` thật (`src/channels/`, implement trait `Channel` như `app.rs`) để tin nhắn đến route vào agent như Telegram.

---

## 5. Nguyên tắc chống bị chặn (từ `apps/mini-browser/src/stealth.rs`)

*Đừng giả mạo — dùng danh tính thật, chỉ sửa đúng thứ đang sai.* Bản cũ fake identity thì **dễ bị phát hiện hơn**.

1. **Ưu tiên tuyệt đối: điều khiển Chrome thật của người dùng qua extension.** Cùng IP nhà, cùng fingerprint đã được tin, cookie hợp lệ tự xoay.
2. **Nếu cần chạy khi Chrome không mở → CDP kiểu mini-browser**: giữ `Sec-CH-UA`, không cờ `--enable-automation`, `navigator.webdriver=false`, giữ site-isolation (profile chứa login thật), input giống người (`input.rs`: di chuột 6 bước jitter, gõ từng ký tự, không paste tức thì).
3. **Nhịp người:** delay ngẫu nhiên (giây→phút, không cố định), cap ngày dưới ngưỡng dân gian, ramp dần với account mới, tôn trọng giờ hoạt động, không burst, back off ngay khi gặp `challenge_required`/checkpoint, không song song hoá thao tác trên một account.
4. **Đọc nhiều — ghi ít.** Đăng nội dung của chính mình ở nhịp người là vùng rủi ro thấp nhất.
5. **Riêng LinkedIn:** tránh synthetic click (`isTrusted` không giả được), ưu tiên tạo cụm request tự nhiên (drive DOM) hơn gọi Voyager trần, giả định extension có thể đã bị fingerprint (AED quét ~6167 cặp extension-id).
6. **Riêng TikTok:** để trang tự ký (inject vào world của trang), đừng tự tái hiện X-Bogus (rot liên tục). Tránh sign-server dùng chung (một sự cố phát hiện làm cháy cả thư viện).
7. **Riêng Zalo:** giữ **cookie + IMEI + UA** đúng một bộ khớp; phiên từ IP lạ dễ bị khoá nhất.

---

## 6. Thư viện tham khảo (chỉ để soi shape endpoint, KHÔNG chạy trực tiếp)

| Nền tảng | Chính thức (khoẻ) | Không chính thức (tham chiếu) | Tình trạng |
|---|---|---|---|
| X | — | twikit, twscrape, agent-twitter-client | sống, hay gãy |
| Facebook | Graph (Page) | fbchat | **chết** (2020) |
| Instagram | Graph (Business) | **aiograpi** (async, sống), instagrapi | sống, mong manh |
| Threads | **threads-go** (official) | threads-re (writeup), Danie1/threads-api | official khoẻ; unofficial ọp ẹp |
| LinkedIn | Share-on-LinkedIn | linkedin-api (Tom Quirk) | **ban vài ngày** nếu chạy thật |
| Reddit | **PRAW** (official, khoẻ) | — | dùng thẳng PRAW-equivalent |
| TikTok | Content Posting API | TikTokApi + tiktok-signature/eulerstream | maintenance-heavy |
| YouTube | google-api-python-client; **yt-dlp** (read) | YouTube.js, innertube | khoẻ |
| Zalo | OA/ZNS (business) | **zca-js** (JS/TS, sống, v2.1.1) / zlapi (Py) | sống, cảnh báo ban |

---

## 7. Lộ trình đề xuất

| GĐ | Việc | File |
|---|---|---|
| 0 | Scaffold `apps/social` từ moltbook, port 4490, manifest, MCP rỗng | `apps/social/*` |
| 1 | Tầng chính thức Rust: Reddit (PRAW-eq), YouTube Data, Threads, X post — OAuth2+PKCE, refresh token | `official/*.rs`, `db.rs` |
| 2 | Protocol: thêm `SocialCapture/SocialAuth/SocialPost/…` 2 phía + quyền `cookies`+host | `src/browser/protocol.rs`, `types/protocol.ts`, `wxt.config.ts` |
| 3 | Pairing secret (QR, `qrcode.react`) + check ở `bridge.rs` | `bridge.rs`, `SidePanelApp.tsx`, `storage.ts` |
| 4 | Extension: `capture.ts` + adapter registry + 2 adapter đầu (X, Reddit) | `social/*` |
| 5 | Adapter session-only: FB group, IG DM, TikTok, Zalo (bắt buộc extension) | `social/adapters/*` |
| 6 | Autonomy gate draft→live + UI web (feed/drafts/inbox) | `api.rs`, `web/` |
| 7 | (v2) DM thành Channel thật cho X/FB/Zalo | `src/channels/*` |

**Thứ tự khuyên:** làm nền tảng **rủi ro thấp trước** (Reddit → YouTube → Threads → X) để validate khung 2-tầng + protocol capture, rồi mới tới **session-only rủi ro cao** (FB group, IG, TikTok, Zalo, LinkedIn).

---

## 8. Cảnh báo tuân thủ (đọc kỹ)

- Tầng **API chính thức** (post X, Data API YouTube, Threads API, Reddit API, đăng Page FB/IG) là **hợp ToS** cho tài khoản của chính mình.
- Tầng **session-riding** (mọi DM cá nhân, search/browse sâu, đăng profile/group cá nhân) **vi phạm ToS** của từng nền tảng, kể cả tài khoản của mình — vì đều là hành vi **đã đăng nhập** (án Meta v. Bright Data chỉ bảo vệ scrape *logged-off* dữ liệu *public*, không giúp gì ở đây; LinkedIn thắng bằng lý thuyết breach-of-contract §8.2).
- Hệ quả thực tế: throttle → checkpoint → **khoá tài khoản thật**. Người dùng đặt cược tài khoản cá nhân.
- **Bắt buộc**: autonomy gate draft→approve→live cho mọi write; nhịp bảo thủ; đọc-nhiều-ghi-ít; hiển thị rõ rủi ro cho người dùng.

---

## 9. Các con số/điểm cần verify-live trước khi build

- X: rate-limit DM trên pay-per-use (docs mù mờ) — kiểm tra Developer Console.
- Reddit: `prefs/apps` self-service vs "Responsible Builder" approval — nguồn ồn ào có xung đột lợi ích, kiểm tra thực tế.
- YouTube: `videos.insert` ~100 units (giảm 12/2025 từ 1600) + cap ~100 call/ngày riêng cho search/upload — soi Revision History.
- TikTok: version `webmssdk`/thuật toán X-Gnarly đổi liên tục — đừng hardcode signer, để trang tự ký.
- Zalo: cửa sổ trả lời tự do của OA (~48h) — xác nhận Zalo Business Solutions docs.
- IG: quota ~50 post/24h và các ngưỡng action đều xấp xỉ, scale theo độ tin account.
