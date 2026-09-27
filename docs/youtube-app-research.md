# Nghiên cứu: YouTube Space App + Chrome Extension

> Trạng thái: **RESEARCH / PLAN** — chưa implement. Ngày: 2026-07-20.
> Mục tiêu người dùng: app YouTube kết nối để **đăng bài, nhắn tin, tìm kiếm, duyệt bài post**; dùng app + extension Chrome để **lấy được token/session của YouTube** và **remote-control** được; extension **gọi API hoặc điều khiển sao cho không bị YouTube chặn**.

---

## 0. Kết luận nhanh (TL;DR)

1. **Kiến trúc "extension chạy trong Chrome đã đăng nhập" của bạn là ĐÚNG và BẮT BUỘC**, không phải tùy chọn — vì YouTube nay dùng BotGuard/PoToken, chỉ request phát ra từ **môi trường browser thật** mới qua được. Gọi API từ server/script trần → 403.
2. **Official YouTube Data API v3 quá hẹp** cho yêu cầu này: search ~100 lần/ngày, **KHÔNG có API tạo community post**, **KHÔNG có API nhắn tin**. Chỉ dùng nó cho việc "đọc" hợp lệ, quota thấp.
3. **Con đường thực tế là InnerTube** (`youtubei/v1/search|browse|next`) — chính API nội bộ youtube.com dùng: không quota, không API key, nhưng **phải đăng nhập** và **phải chạy trong browser thật**.
4. **"Nhắn tin" cần định nghĩa lại**: DM YouTube bị khai tử 2019, đang test lại giới hạn từ 11/2025, **không có API**. Cái làm được là **comment** và **live chat**.
5. **Blueprint code đã có sẵn trong repo**: fork `apps/video-flow` (app + extension bắt token) cho phần auth/API, tùy chọn ghép `apps/mini-browser` (CDP stealth) hoặc `senclaw-extension-chrome` (DOM remote-control) cho phần điều khiển UI khi không có API.

---

## 1. YouTube làm được gì / không làm được gì

### 1.1 Official Data API v3 — hợp lệ nhưng chật

| Việc | Có API? | Chi phí quota | Ghi chú |
|---|---|---|---|
| Tìm kiếm (`search.list`) | ✅ | **100 unit/lần** | Quota mặc định 10.000/ngày → **~100 lần tìm/ngày**. Reset 0h Pacific |
| Đọc video/channel/playlist | ✅ | 1 unit | Rẻ |
| Đăng comment (`commentThreads.insert`) | ✅ (OAuth) | ~50 unit | Cần OAuth user consent |
| Trả lời / like comment | ✅ (OAuth) | ~50 unit | |
| Upload video (`videos.insert`) | ✅ (OAuth) | ~1600 unit | Mặc định 100 lần/ngày |
| **Tạo Community Post** | ❌ | — | **Không tồn tại trong API chính thức** |
| **Đọc Community Post** | ❌ | — | Không có endpoint chính thức (chỉ scraper bên thứ ba) |
| **Nhắn tin / DM** | ❌ | — | Không có API |

→ Official API chỉ đủ cho **đọc metadata + comment quota thấp**. Không đáp ứng "duyệt community post" và "đăng bài".

### 1.2 InnerTube (`youtubei/v1/*`) — API nội bộ, con đường thực tế

- Endpoint POST dưới `/youtubei/v1/`: `search`, `browse` (channel/community feed qua `browseId`), `next` (comment/continuation), `player`...
- **Không quota, không API key.** Nhưng phải gửi kèm `context.client` (clientName/clientVersion) hợp lệ; **thao tác account (comment, đăng, like) cần đăng nhập**.
- Thư viện tham chiếu để biết payload đúng: **LuanRT/YouTube.js**, `tombulled/innertube` (Python), `ToBiDi0410/IYoutube` (nhiều action ghi/comment/subscribe).
- Community post: đọc qua `browse` với `browseId` tab community của channel; đăng post thì phải mô phỏng đúng endpoint action nội bộ (không public, phải reverse từ traffic YouTube Studio / web).

### 1.3 Xác thực InnerTube khi đã đăng nhập: `SAPISIDHASH`

YouTube web không dùng bearer `ya29.` như Google Labs, mà dùng **cookie + header `Authorization: SAPISIDHASH`**:

```
Authorization: SAPISIDHASH <unix_time>_<sha1(unix_time + " " + SAPISID + " " + origin)>
```

- Cookie cần: `SAPISID` (và `SID/HSID/SSID/APISID`). Hash sinh từ `SAPISID` + timestamp + origin (`https://www.youtube.com`).
- Có nhiều biến thể mới: `SAPISIDHASH`, `SAPISID1PHASH`, `SAPISID3PHASH` (theo cookie `__Secure-1PSID/3PSID`) — xem cách yt-dlp refactor cookie auth.
- **Hệ quả kiến trúc:** hash này phải **tính TRONG page context** (nơi có cookie `SAPISID` httpOnly=false và đúng origin). Đây chính là lý do cần pattern **MAIN-world `fetch` với `credentials:'include'`** — không thể tính ở server vì server không có cookie.

### 1.4 Rào cản lớn nhất: BotGuard / PoToken

- YouTube nay yêu cầu **PoToken** (Proof-of-Origin) do **BotGuard (Web)** sinh — attestation chứng minh request đến từ client thật. PoToken **bind theo session/video**, TTL ngắn (~vài giờ), và **chỉ sinh được trong môi trường browser thật chạy được challenge JS của BotGuard**.
- Request InnerTube thiếu PoToken hợp lệ (đặc biệt từ IP datacenter) → **403 / bị coi là bot**.
- Không có cách "giả lập" nhẹ: `iv-org/youtube-trusted-session-generator` đã **deprecated**; `LuanRT/BgUtils` chạy được nhưng vẫn cần môi trường tương thích BotGuard.

> **Đây là lý do quyết định kiến trúc:** phải chạy trong **một Chrome thật đã đăng nhập** (extension hoặc CDP điều khiển Chrome cài sẵn), để (a) có cookie `SAPISID`, (b) BotGuard sinh PoToken hợp lệ, (c) request phát ra same-origin từ youtube.com. Mọi phương án gọi API từ Rust trần đều sẽ bị chặn.

---

## 2. Ba phương án kỹ thuật (đã có code mẫu trong repo)

### Phương án A — Extension bắt token + API-proxy (fork `apps/video-flow/extension`) ⭐ KHUYẾN NGHỊ CHÍNH

Đây là bản sao gần nhất với yêu cầu. `apps/video-flow/extension/` (vanilla MV3, `background.js` 1210 dòng) đã làm chính xác việc "bắt token của Google + proxy call API qua session người dùng":

- **Bắt token thụ động**: `chrome.webRequest.onBeforeSendHeaders` trên host YouTube → đọc header auth, lưu vào `chrome.storage.local`, đẩy `token_captured` về app qua WS. Với YouTube ta bắt **cookie `SAPISID`** thay vì `Bearer ya29.`.
- **Proxy call same-origin (MAIN world)**: `handleTrpcRequest` chạy `chrome.scripting.executeScript({world:'MAIN', func: () => fetch(url, {credentials:'include'})})` — **đúng pattern cần cho YouTube**: hash `SAPISIDHASH` + cookie + Origin/Referer đều khớp real web app, BotGuard sinh trong page. Đây là con đường "không bị chặn".
- **Header rewrite**: `declarativeNetRequest` (`rules.json`) chỉnh `Referer`/`Origin` cho khớp same-origin.
- **reCAPTCHA/attestation**: `injected.js` vào MAIN world gọi được `window.grecaptcha`/BotGuard artifacts nếu cần.
- **Port động**: daemon cấp port HTTP động → extension lưu `wsPort`/`httpPort` trong `chrome.storage.local`, set qua popup; reply ưu tiên **HTTP callback** (`POST /api/ext/callback`) để WS drop không mất response.

**Bridge phía app** (`apps/video-flow/src/extbridge.rs`) product-agnostic — bê nguyên: `ExtBridge::call(method, params, timeout)` → `{id,method,params}` qua WS `:9222`, chờ reply theo `id`.

### Phương án B — CDP stealth browser (fork `apps/mini-browser`)

Nếu muốn tự **launch một Chrome cài sẵn** và điều khiển hoàn toàn (không phụ thuộc extension người dùng cài):

- `apps/mini-browser` dùng `chromiumoxide` + CDP, **profile bền vững** `~/.senclaw/space-apps/<app>/profile` (giữ login YouTube của user), headful khi có display.
- **Stealth "trình bày trung thực" chứ không spoof**: đọc identity thật qua `navigator.userAgentData` (probe qua loopback secure-context), chỉ sửa token `HeadlessChrome→Chrome`, **luôn gửi `userAgentMetadata`** để không rớt `Sec-CH-UA` (spoof sai là dấu hiệu bot). `--disable-blink-features=AutomationControlled` giữ `navigator.webdriver=false`. Giữ site-isolation ON.
- **Input như người**: `input.rs` phát CDP `Input.*` — di chuột nội suy 6 bước, click có jitter 40-110ms, gõ từng ký tự 40-150ms. AI và người dùng **chia sẻ cùng 1 page** (live-view WS stream JPEG 330ms).
- Vì là Chrome thật + profile đăng nhập → BotGuard/PoToken hợp lệ tự nhiên.
- **Gotcha đã giải sẵn**: drain handler stream liên tục (`while handler.next().await.is_some()`), xoá `SingletonLock` trước launch, skip frame khi capture lỗi, sleep ~220ms sau `scrollIntoView` trước khi đọc toạ độ.

### Phương án C — DOM remote-control (`senclaw-extension-chrome`)

Extension WXT/React đã có (port `alibaba/page-agent`), nối `ws://127.0.0.1:18789/browser`. Mạnh ở **điều khiển DOM theo index** (snapshot đánh số element → LLM click `index:N`), đa-agent (mỗi agent 1 tab). **Nhưng KHÔNG có `cookies`/`webRequest`/`declarativeNetRequest`** → không bắt được token. Chỉ dùng làm **fallback thao tác UI** khi không có API (vd bấm nút đăng community post trên giao diện Studio).

### So sánh & khuyến nghị

| Tiêu chí | A. video-flow ext | B. mini-browser CDP | C. senclaw-ext DOM |
|---|---|---|---|
| Bắt token/cookie | ✅ (webRequest + MAIN fetch) | ✅ (profile Chrome thật) | ❌ |
| Gọi InnerTube API | ✅ same-origin | ✅ (execute_js trong page) | ⚠️ chỉ qua page |
| Né BotGuard | ✅ (page thật) | ✅ (Chrome thật) | ✅ (Chrome thật) |
| Điều khiển UI (không API) | ⚠️ | ✅ human-input | ✅✅ index-based |
| Cần user cài extension | ✅ | ❌ (app tự launch Chrome) | ✅ |
| Dùng lại Chrome/login sẵn của user | ✅ | ⚠️ profile riêng | ✅ |

> **Khuyến nghị: HYBRID A + (B hoặc C).**
> - **A** làm xương sống: bắt cookie `SAPISID`, proxy InnerTube same-origin (search/browse/next, comment) → độ phủ lớn nhất, ổn định nhất, né chặn tốt nhất.
> - Thêm **B** nếu muốn app tự chủ (không cần user thao tác cài extension) và cần thao tác UI phức tạp (đăng community post qua giao diện) với input giống người.
> - Hoặc thêm **C** nếu muốn ride Chrome sẵn có của user cho các thao tác UI.
> Giai đoạn 1 chỉ cần **A**.

---

## 3. Chiến lược "không bị YouTube chặn"

1. **Luôn phát request từ page context youtube.com** (MAIN-world fetch, `credentials:'include'`) — không bao giờ gọi InnerTube từ Rust/Node ngoài browser.
2. **Để BotGuard sinh PoToken tự nhiên** trong tab thật; nếu InnerTube trả về "poToken experiment" thì retry qua tab có PoToken (xem cách yt-dlp/NewPipe xử lý).
3. **`context.client` hợp lệ & khớp**: clientName/clientVersion, `visitorData`, `x-goog-authuser`, `x-youtube-client-*` phải nhất quán với UA. Sao chép từ request thật (bắt qua `webRequest`) thay vì hardcode.
4. **Rate-limit như người**: throttle, jitter, không burst; ưu tiên dùng continuation token thật thay vì đoán.
5. **Nếu dùng CDP (B): stealth trung thực** — không spoof GPU/UA/languages lệch nhau; giữ `userAgentMetadata` để `Sec-CH-UA` sống; giữ site-isolation.
6. **Input giống người** khi thao tác UI (đã có `input.rs`): chuột nội suy, gõ có delay.
7. **Không chạy trên IP datacenter** cho các luồng nhạy cảm — PoToken web bị siết mạnh với IP DC.
8. **Ưu tiên official API v3** cho việc đọc hợp lệ (rẻ, không rủi ro) và **giữ InnerTube cho phần API không cung cấp** (community, feed, đăng) — giảm bề mặt bị chặn.

---

## 4. Kiến trúc app đề xuất — `apps/youtube`

Scaffold theo `apps/mindmap` (cleanest) + phần extension/auth theo `apps/video-flow`.

```
apps/youtube/
  Cargo.toml                 # workspace member (thêm vào root Cargo.toml)
  senclaw-manifest.json      # id="youtube", port cố định CHƯA dùng (đề xuất 4490 — grep manifest để chắc)
  src/
    main.rs                  # axum: đọc PORT, serve /api + web/dist; spawn extbridge :9222
    api.rs                   # AppState + router REST/WS + /api/mcp/sse
    db.rs                    # rusqlite ~/.senclaw/space-apps/youtube/
    mcp.rs                   # JSON-RPC MCP hand-rolled (SSE+POST), tools youtube_*
    llm.rs                   # SpaceClient.llm_request (SENCLAW_SPACE_APP_ID="youtube")
    extbridge.rs             # bê nguyên từ video-flow ({id,method,params} + /api/ext/callback)
    innertube.rs             # build payload youtubei/v1 (search/browse/next), parse
    youtube.rs               # domain logic: post/comment/search/browse
  extension/                 # fork apps/video-flow/extension (vanilla MV3)
    manifest.json            #   host_permissions youtube.com + youtubei.googleapis.com
    background.js            #   bắt cookie SAPISID + proxy MAIN-world fetch
    injected.js, content.js, rules.json, popup.*, side_panel.*
  web/                       # Vite+React, base:'./', proxy /api
  skills/youtube-*/SKILL.md
  personas/youtube-operator.md
  scripts/pack.sh
```

**`senclaw-manifest.json`** (mẫu):
```jsonc
{
  "id": "youtube",
  "name": "SenClaw YouTube",
  "icon": "▶️",
  "runtime": { "kind":"server", "start":"./youtube", "healthPath":"/api/status", "port": 4490 },
  "integration": { "type":"iframe", "url":"/" },
  "bridge": { "postMessage": true, "capabilities": ["space.rest","llm.request","agent.run"] },
  "mcp": { "name":"youtube-mcp", "transport":"http", "path":"/api/mcp/sse", "autoRegister": true },
  "skills":   [ { "name":"youtube-browse", "path":"skills/youtube-browse", "triggers":[...] } ],
  "personas": [ { "name":"youtube-operator", "path":"personas/youtube-operator.md", "description":"..." } ]
}
```

**MCP tools** (hand-rolled trong `mcp.rs`, resolve thành `mcp__youtube-mcp__youtube_*`):
- `youtube_search` — tìm kiếm (InnerTube `search`, fallback Data API)
- `youtube_browse_channel` / `youtube_browse_community` — duyệt feed/community post (`browse` + browseId)
- `youtube_list_comments` / `youtube_post_comment` / `youtube_reply_comment`
- `youtube_create_post` — đăng community post (qua UI Studio nếu không có endpoint InnerTube ổn định → dùng phương án B/C)
- `youtube_get_status` — trạng thái đăng nhập/token
- (Nếu ghép B) `youtube_ui_act` / `youtube_ui_snapshot` — thao tác UI giống người

**LLM**: mọi call qua `app_space_sdk::SpaceClient.llm_request(system, user, max_tokens)` → `POST {SENCLAW_BASE_URL}/api/space/apps/youtube/bridge` action `llm.request`. Không gọi provider trực tiếp. Coi `finish=="length"` là bị cắt.

**Port động**: daemon inject `PORT`, `SENCLAW_BASE_URL`, `SENCLAW_SPACE_APP_ID`. Extension lưu port trong `chrome.storage.local`, set qua popup (không hardcode).

---

## 5. Cảnh báo về "nhắn tin"

- **YouTube DM đã khai tử 2019**, chỉ đang **test lại giới hạn từ 11/2025**, **không có API công khai**. Không nên đặt cược tính năng vào DM.
- Cái **làm được** và nên hiểu là "nhắn tin":
  - **Comment / reply comment** (Data API `commentThreads.insert` hợp lệ, hoặc InnerTube).
  - **Live chat** trong livestream (`liveChatMessages.insert` — Data API có, chỉ khi đang có live).
- **Cần chốt lại với người dùng** "nhắn tin" nghĩa là gì trước khi implement (comment vs live chat vs DM thật). Xem mục 7.

---

## 6. Rủi ro & tuân thủ

- **ToS YouTube/Google**: tự động hoá đăng bài/comment quy mô, dùng InnerTube ngoài API chính thức, mô phỏng session người dùng → **vi phạm ToS**, rủi ro **khoá account / khoá kênh**. Nên: dùng account phụ, rate-limit chặt, human-in-the-loop cho hành động ghi.
- **Community Guidelines / spam**: đăng/comment tự động dễ bị coi là spam. Cần cơ chế **draft → duyệt → gửi** (giống pattern autonomy gate của `apps/moltbook`: observe/draft/live).
- **PoToken/BotGuard thay đổi liên tục**: đây là mục tiêu di động; cần theo dõi yt-dlp/YouTube.js để cập nhật payload.
- **Không lưu cookie/token ra ngoài máy user**; giữ trong `chrome.storage.local` / profile cục bộ như các app hiện có.

---

## 7. Roadmap đề xuất (phân giai đoạn)

- **P0 — Chốt phạm vi**: "nhắn tin" = comment/live chat (không DM); phương án A (video-flow ext). ✅
- **P1 — Scaffold app** ✅ (2026-07-20): `apps/youtube` port **4491** (4490=shopee), `main/api/db/mcp/llm/extbridge/innertube/youtube.rs`, UI React, manifest `youtube-mcp`, đăng ký workspace. Compile + runtime-verified.
- **P2 — Auth qua extension (A)** ✅ code: extension MV3 bắt cookie `SAPISID` → ký `SAPISIDHASH` (SubtleCrypto SHA-1) → proxy `yt_fetch` same-origin `credentials:'include'`, DNR set Origin/Referer; reply qua `POST /api/ext/callback` + WS; scrape `ytcfg`. **Còn: cài vào Chrome + đăng nhập thật để verify `hasSapisid:true`.**
- **P3 — Đọc** ✅ code + test: `youtube_search`, `youtube_browse`, `youtube_list_comments` qua InnerTube (`search`/`browse`/`next`), parser defensive (videoRenderer/backstagePost/commentRenderer). Proxy coi HTTP 4xx là "bị chặn". **Còn: verify với phiên thật không dính 403.**
- **P4 — Ghi (human-in-the-loop)** ✅ code + test: pipeline draft→duyệt→gửi; `send_action` gọi `comment/create_comment` (2 bước: `next`→`createCommentParams`→create) + `comment/create_comment_reply`. Verify bằng harness giả lập extension. **Còn: lần gửi thật đầu tiên xác nhận token khớp shape.**
- **P5 — Community post / thao tác UI** ✅ code + test: **không** ghép mini-browser CDP (sẽ phải đăng nhập lần hai) mà mở rộng chính extension: `chrome.debugger` → `Input.*` cho **input TRUSTED** ngay trong Chrome đã đăng nhập. Thêm `yt_ui_open/snapshot/act` (chuột nội suy + jitter, gõ 40–150ms/ký tự, snapshot đánh index) → MCP `youtube_ui_*`; `send_action("community_post")` tự lái composer Studio, trả step-trace và chỉ đường fallback sang `youtube_ui_*` khi không tìm thấy target. Thêm throttle ghi 30s + jitter.
- **P6 — Skills/personas + đóng gói** ✅: `skills/youtube-browse`, `personas/youtube-operator`, `scripts/pack.sh`. **Còn: chạy pack.sh tạo zip khi cần cài.**

**Trạng thái tổng: P1–P5 code-complete, 10 test pass, 11 MCP tool. Chặn duy nhất còn lại là verify với một phiên YouTube đăng nhập thật** (không làm được trong môi trường headless này) — cần confirm token shape (`createCommentParams`/`createReplyParams`) và selector composer.

### Vì sao chọn extension + `chrome.debugger` thay vì mini-browser CDP cho P5

| | Extension + chrome.debugger | mini-browser (chromiumoxide) |
|---|---|---|
| Phiên đăng nhập | Dùng lại Chrome user đã login | Phải đăng nhập lại vào profile riêng |
| Input trusted | ✅ (CDP `Input.*`) | ✅ |
| BotGuard/PoToken | ✅ (chính tab thật) | ✅ |
| Chi phí | 0 process thêm | +1 Chrome + profile |
| Nhược điểm | Chrome hiện banner "đang gỡ lỗi" | Nặng, login riêng |

→ Extension thắng rõ vì đã có sẵn phiên đăng nhập; banner gỡ lỗi là cái giá chấp nhận được.

---

## 8. Nguồn tham khảo

- YouTube Data API v3 — quota/methods: developers.google.com/youtube/v3/determine_quota_cost
- InnerTube reverse engineering: LuanRT/YouTube.js, tombulled/innertube, ToBiDi0410/IYoutube, tyrrrz.me "Reverse-Engineering YouTube: Revisited"
- SAPISIDHASH auth: ytjs.dev/guide/authentication, yt-dlp commit "Refactor cookie auth"
- BotGuard/PoToken: github.com/yt-dlp/yt-dlp/wiki/PO-Token-Guide, LuanRT/BgUtils
- YouTube DM (khai tử 2019, test lại 11/2025): variety.com/2019, phandroid.com/2025/11/20

## 9. Code mẫu trong repo (để fork)

- **Extension bắt token + proxy**: `apps/video-flow/extension/` (`background.js`, `injected.js`, `rules.json`), bridge `apps/video-flow/src/extbridge.rs`, LLM `apps/video-flow/src/llm.rs`.
- **CDP stealth browser**: `apps/mini-browser/src/{session,input,stealth,mcp,llm}.rs`.
- **Anatomy app chuẩn**: `apps/mindmap/` (`main/api/db/mcp/llm.rs`, `senclaw-manifest.json`, `web/` base:'./', `scripts/pack.sh`).
- **DOM remote-control**: `senclaw-extension-chrome/` (WXT/React, `ws://127.0.0.1:18789/browser`).
- **Autonomy gate (draft→duyệt→live)**: `apps/moltbook/`.
