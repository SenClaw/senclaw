# SenClaw Mini Browser — App Space Design (Rust, deep-AI, stealth)

Status: **IMPLEMENTED** — 2026-07-07 (built at `apps/mini-browser`, engine = `chromiumoxide` 0.7 / CDP)
Author: research pass over `apps/*`, `src/mcp/browser_server.rs`, `senclaw-extension-chrome`

> **Build result:** shipped as `apps/mini-browser` — Rust axum backend (port 4360) driving a
> real stealth Chromium via CDP; live-view WebSocket (JPEG screencast + human-like input relay);
> 19 MCP tools incl. `browser_act` / `browser_extract`; React live-view UI + AI chat/Act panel;
> `browse-web` + `web-task` skills + `web-operator` persona. Verified end-to-end (navigate, DOM
> snapshot, user-click relay, tabs) and stealth self-check passes (`navigator.webdriver`
> undefined, vi-VN languages, `window.chrome`, plugins, native `toString`). 10 unit tests + a
> live `stealth_smoke` integration test all green. Packaged via `scripts/pack.sh` →
> `mini-browser-app.zip`. The live-view uses **screenshot polling (~3 FPS)** rather than CDP
> `startScreencast` — a deliberate reliability/simplicity trade (see §7 note); screencast is a
> future optimization.

## 1. Mục tiêu

Một **mini browser** đóng gói dưới dạng **App Space** trong `apps/`, viết bằng **Rust**, với:

- **Render web thật** ngay trong app (không chỉ scrape HTML).
- **Nhúng AI sâu**: AI đọc/hiểu trang, thao tác trực tiếp (navigate/click/type/extract), chat theo ngữ cảnh trang hiện tại.
- **MCP + skills + personas** theo đúng chuẩn App Space của SenClaw.
- **User tương tác** qua UI (thanh địa chỉ, click/scroll/gõ phím trên khung xem trực tiếp).
- **User và AI dùng CHUNG một phiên/tab** → thao tác của AI không phân biệt được với thao tác người.
- **Không bị nhận diện là bot/automation** (stealth) — cả khi AI điều khiển.
- **Testing**: có harness kiểm chứng stealth + hành vi.

## 2. Bối cảnh — cái gì đã có

| Thành phần | Stack | Vai trò | Thiếu gì |
|---|---|---|---|
| `apps/playwright-browser` | Node + Playwright headless | Wrapper screenshot-stream 1 FPS, 5 MCP tool | Node (không Rust), **không stealth, không skill**, headless dễ bị phát hiện |
| `apps/mindmap` | **Rust + axum + rusqlite + app-space-sdk** | **Template App Space chuẩn** (MCP/SSE, skills, personas, manifest) | — (dùng làm khuôn) |
| `src/mcp/browser_server.rs` + `senclaw-extension-chrome` | Rust MCP + WXT extension | 30 browser tool, DOM flat-tree (port page-agent), điều khiển **Chrome ngoài** qua WS | Không render nhúng, **không stealth**, cần Chrome + extension của user |

**Kết luận nền tảng:** repo chưa có render nhúng trong Rust và chưa có lớp stealth. Có sẵn: (a) khuôn App Space từ `mindmap`, (b) logic DOM-tree/action từ extension để port lại.

## 3. Quyết định kiến trúc: engine render

Rust không có engine web riêng. Ba lựa chọn:

| Phương án | Bản chất | Iframe-embeddable? | Stealth | Async/tokio | Kết luận |
|---|---|---|---|---|---|
| **A. `chromiumoxide` (CDP)** | Điều khiển Chromium qua Chrome DevTools Protocol từ Rust | ✅ (screencast → `<img>`/canvas trong iframe) | ✅✅ (patch `navigator.webdriver`, UA, inject JS trước page script) | ✅ native tokio | **CHỌN** |
| B. `wry`/`tao` (WebView) | WebView OS thật (WebKit/WebView2) trong **cửa sổ riêng** | ❌ chạy cửa sổ GUI riêng, không nhét vào iframe SenClaw | ⚠️ UA thật nhưng khó inject CDP-level, khó headless daemon | ⚠️ cần main-thread GUI loop | Loại — vỡ mô hình App Space |
| C. Tái dùng extension | Điều khiển Chrome của user | ❌ không phải "app" | ❌ | — | Loại — không tự chứa |

→ **Dùng `chromiumoxide`**: async/tokio khớp axum, CDP cho toàn quyền inject stealth + `Page.startScreencast` (stream frame realtime, hơn hẳn vòng screenshot 1 FPS của playwright-browser). App tự bundle/tự dò một Chromium binary.

> Ghi chú: đây thực chất là bản Rust hoá `playwright-browser` nhưng **CDP trực tiếp + stealth + AI sâu + skills**, và fit khuôn `mindmap`.

## 4. Kiến trúc tổng thể

```
┌──────────────────────── apps/mini-browser (Rust, axum, port 4360) ─────────────────────┐
│                                                                                          │
│  Web UI (React+Vite, iframe)          MCP server (JSON-RPC/SSE)      REST /api/*         │
│   ├ khung xem trực tiếp (screencast)   ├ browser_navigate/click/...   ├ /status          │
│   ├ address bar, back/fwd, tabs        ├ browser_act (NL→action, LLM) ├ /session         │
│   ├ chat panel (grounded on page)      ├ browser_extract (LLM)        └ /history         │
│   └ gửi input người → CDP Input        └ browser_snapshot/screenshot                     │
│                    │                              │                        │              │
│                    └──────────────┬───────────────┘                        │              │
│                                   ▼                                         ▼              │
│                         BrowserSession (một page dùng chung)          rusqlite (history,   │
│                          - chromiumoxide Handler (CDP)                bookmarks, sessions) │
│                          - stealth injector                                                │
│                          - human-like input driver                                         │
│                          - screencast broadcaster (tokio broadcast)                        │
│                                   │                                                        │
│                                   ▼                                                        │
│                         Chromium (headful/new-headless, offscreen)                         │
│                          + stealth flags + persistent profile                             │
└──────────────────────────────────────────────────────────────────────────────────────────┘
                                    │  app-space-sdk (llm.request)
                                    ▼
                             SenClaw daemon LLM  (không cần API key riêng)
```

**Nguyên tắc "người và AI không phân biệt được":** cả input của user (từ iframe) lẫn action của AI (từ MCP) đều đổ vào **cùng một `BrowserSession` / cùng CDP session / cùng page**. Mọi thao tác đi qua **`Input.dispatchMouseEvent` / `Input.dispatchKeyEvent`** của CDP với timing người-hoá → ở tầng DOM/website không có tín hiệu nào tách được "AI click" khỏi "người click".

## 5. Bộ khung thư mục (theo `mindmap`)

```
apps/mini-browser/
├── Cargo.toml                 # app-space-sdk, tokio, axum(ws), rusqlite(bundled),
│                              # chromiumoxide, serde, anyhow, futures-util, tower-http
├── senclaw-manifest.json      # id="mini-browser", port 4360, mcp /api/mcp/sse, skills, personas
├── README.md
├── src/
│   ├── main.rs                # boot: dò web_dist (tránh static-dir collision), bind PORT, launch Chromium
│   ├── api.rs                 # AppState { session, db, mcp_tx }, REST routes, WS screencast+input
│   ├── mcp.rs                 # MCP JSON-RPC/SSE — tools/list + call_tool
│   ├── session.rs             # BrowserSession: chromiumoxide page, navigate/click/type/snapshot
│   ├── stealth.rs             # flags + JS payload inject (addScriptToEvaluateOnNewDocument)
│   ├── input.rs               # human-like mouse curve + typing jitter (CDP Input.*)
│   ├── llm.rs                 # browser_act / extract qua app-space-sdk (như mindmap/llm.rs)
│   └── db.rs                  # rusqlite: history, bookmarks, saved sessions
├── web/                       # React+Vite: LiveView(canvas/img) + AddressBar + Tabs + ChatPanel
│   └── src/{main.tsx,App.tsx,api.ts,components/{LiveView,ChatPanel}.tsx}
├── skills/
│   ├── browse-web/SKILL.md        # "mở trang / tìm / đọc giúp tôi …"
│   └── web-task/SKILL.md          # "đăng nhập / điền form / mua / thao tác nhiều bước"
├── personas/
│   └── web-operator.md            # persona điều khiển trình duyệt an toàn, người-hoá
└── release/web_dist/          # bản build web đóng gói kèm binary
```

## 6. Lớp stealth (cốt lõi "không bị nhận là bot")

Kết hợp CDP flag + JS inject sớm (kiểu `puppeteer-extra-plugin-stealth`), inject qua `Page.addScriptToEvaluateOnNewDocument` **trước khi script trang chạy**.

**6.1 Launch flags (`stealth.rs`)**
- `--disable-blink-features=AutomationControlled` (bỏ cờ automation)
- Dùng `--headless=new` (headless mới, khó phát hiện hơn old headless) **hoặc** headful offscreen; **không** dùng old headless.
- Tắt các cờ tự lộ: không thêm `--enable-automation`; xoá switch này khỏi argv Chromium.
- UA thật, thay `HeadlessChrome/…` → `Chrome/…`; set `Sec-CH-UA` client hints khớp.
- Persistent `--user-data-dir` (profile bền) → có cookie/lịch sử như người dùng thật.
- `--lang`, timezone, `--window-size` hợp lý; locale/geo khớp qua CDP `Emulation.setTimezoneOverride`, `Emulation.setLocaleOverride`.

**6.2 JS inject (chạy trước mọi trang)**
- Xoá `navigator.webdriver` (define undefined).
- Vá `navigator.plugins` / `mimeTypes` (mảng thật thay vì rỗng).
- `navigator.languages = ['vi-VN','vi','en-US','en']`.
- Giả `window.chrome = { runtime: {...} }`.
- Vá `Notification.permission`, `navigator.permissions.query` (không trả trạng thái mâu thuẫn).
- Spoof WebGL `UNMASKED_VENDOR/RENDERER` (ví dụ "Intel Inc." / "Intel Iris OpenGL").
- Che `iframe.contentWindow`, `toString` của các hàm bị vá (chống `fn.toString()` lộ `[native code]` giả).
- Hardware: `navigator.hardwareConcurrency`, `deviceMemory` giá trị đời thường.

**6.3 Hành vi người-hoá (`input.rs`)**
- Gõ phím: delay ngẫu nhiên 40–160 ms/ký tự (không paste tức thời).
- Chuột: di theo đường cong Bézier + vài bước trung gian trước khi click, không teleport toạ độ.
- Scroll từng nấc, có quán tính; chờ ngẫu nhiên giữa các action.
- Tôn trọng `robots`/rate-limit tuỳ chọn (đạo đức + tránh bị chặn).

> Lưu ý thực tế: stealth **giảm mạnh** khả năng bị nhận diện (qua được sannysoft/creepjs cơ bản, bot.incolumitas), nhưng anti-bot cao cấp (Cloudflare Turnstile, DataDome) vẫn có thể chặn. Mục tiêu là "trông như trình duyệt thật của người", không phải bảo chứng 100%.

## 7. Nhúng AI sâu

**7.1 MCP tools** (`mcp.rs`, JSON-RPC/SSE giống `mindmap`):
- Điều hướng/tab: `browser_navigate`, `browser_back`, `browser_forward`, `browser_reload`, `browser_new_tab`, `browser_list_tabs`, `browser_switch_tab`, `browser_close_tab`.
- Quan sát: `browser_snapshot` (accessibility/flat DOM tree — port từ `senclaw-extension-chrome/DomTreeBuilder.ts`), `browser_screenshot`, `browser_extract_text`, `browser_extract_links`, `browser_extract_table`.
- Thao tác (theo index từ snapshot): `browser_click`, `browser_type`, `browser_select_option`, `browser_scroll`, `browser_hover`, `browser_press_key`, `browser_fill_form`, `browser_upload_file`, `browser_execute_js`, `browser_wait`.
- **AI-native (gọi LLM qua `app-space-sdk`)**:
  - `browser_act { instruction }` — nhận lệnh ngôn ngữ tự nhiên ("đăng nhập bằng email X", "bấm nút Thanh toán"), tự chụp snapshot → LLM chọn element/hành động → thực thi (đóng vòng observe→decide→act tối đa N bước).
  - `browser_extract { schema | question }` — trích xuất có cấu trúc theo JSON schema hoặc trả lời câu hỏi về trang.
  - `browser_search { query, engine }` — tìm kiếm + parse kết quả (port từ `SearchEngine.ts`).

**7.2 Chat panel (UI)** — grounded on page: gửi snapshot/text trang hiện tại kèm câu hỏi → LLM (qua `app-space-sdk::SpaceClient::llm_request`, y hệt `apps/mindmap/src/llm.rs`). Cho phép "tóm tắt trang", "điền form giúp tôi", "trang này nói gì về …".

**7.3 Skills + personas** — đăng ký trong manifest với `triggers` (VI+EN), ví dụ `browse-web` ("mở giúp tôi…", "tìm trên web…"), `web-task` ("đăng nhập…", "điền form…", "đặt lịch trên trang…"); persona `web-operator` nêu nguyên tắc thao tác an toàn, người-hoá, xác nhận trước hành động nhạy cảm (thanh toán/gửi tiền/xoá).

## 8. Manifest (rút gọn)

```jsonc
{
  "id": "mini-browser",
  "name": "SenClaw Browser",
  "icon": "🕶️",
  "runtime": { "kind": "server", "start": "./mini-browser",
               "healthPath": "/api/status", "port": 4360 },
  "integration": { "type": "iframe", "url": "/" },
  "bridge": { "postMessage": true, "capabilities": ["space.rest", "llm.request"] },
  "mcp": { "name": "mini-browser-mcp", "transport": "http",
           "path": "/api/mcp/sse", "autoRegister": true },
  "skills":   [ { "name": "browse-web", "path": "skills/browse-web", "triggers": [...] },
                { "name": "web-task",  "path": "skills/web-task",  "triggers": [...] } ],
  "personas": [ { "name": "web-operator", "path": "personas/web-operator.md" } ]
}
```

## 9. Testing

**9.1 Unit / integration (`cargo test`)**
- Stealth JS: nạp payload vào page trắng, assert `navigator.webdriver === undefined`, `navigator.plugins.length > 0`, `navigator.languages` đúng, `window.chrome` tồn tại, WebGL vendor bị spoof.
- Input người-hoá: assert khoảng delay gõ phím nằm trong dải, chuột có >1 bước trung gian.
- MCP: `tools/list` trả đủ tool; `browser_navigate`→`browser_snapshot` cho ra tree hợp lệ.
- DB: history/bookmark round-trip.

**9.2 Bot-detection smoke test** (feature-gated, cần Chromium)
- Điều tới các trang test và assert "không bị gắn cờ": `bot.sannysoft.com`, `arh.antoinevastel.com/bots/areyouheadless`, `abrahamjuliot.github.io/creepjs`, `bot.incolumitas.com`. Chấm điểm pass/fail theo dấu hiệu webdriver/headless.
- Ghi report (JSON) vào `apps/mini-browser/test-report/`.

**9.3 E2E "người ≡ AI"**
- Kịch bản: AI `browser_act("điền form demo")` trên trang test nội bộ, đồng thời gửi input người qua WS; assert cùng một page state, server không lộ header/ánh xạ nào phân biệt nguồn.

**9.4 CI**
- `cargo test -p mini-browser` (unit, không cần Chromium).
- Job optional có Chromium chạy smoke bot-detection (nightly).

## 10. Lộ trình

1. **Scaffold**: copy khuôn `mindmap` → `apps/mini-browser`, đổi id/port/manifest, boot axum + static-dir dò-đa-ứng-viên.
2. **Session**: `chromiumoxide` launch + navigate + snapshot + screenshot; REST `/api/*` tối thiểu.
3. **Live UI**: `Page.startScreencast` → WS broadcast → `LiveView` React; input người → CDP `Input.*`.
4. **Stealth**: `stealth.rs` flags + JS inject; chạy 9.2, tinh chỉnh tới khi pass cơ bản.
5. **MCP**: port tool set + `browser_act`/`browser_extract` (LLM qua app-space-sdk).
6. **AI UX**: chat panel grounded, skills + personas + triggers.
7. **DB**: history/bookmarks/sessions; **Testing** 9.1–9.4; đóng gói `release/web_dist` + `pack.sh`.

## 11. Rủi ro / lưu ý

- **Phụ thuộc Chromium binary**: cần dò/bundle; tài liệu hoá cách cài. (Không thuần-Rust 100% về engine — không tránh được nếu muốn render thật.)
- **Anti-bot cao cấp** (Cloudflare/DataDome) vẫn có thể chặn — nêu rõ giới hạn, không hứa vượt 100%.
- **Đạo đức/pháp lý**: stealth + tự động hoá dễ bị lạm dụng (spam/scrape vi phạm ToS). Persona `web-operator` phải yêu cầu xác nhận với hành động nhạy cảm; mặc định tôn trọng rate-limit.
- **Tài nguyên**: mỗi Chromium tốn RAM; giới hạn số tab, GC tab như extension đã làm.
- **Trùng tên với `playwright-browser`**: cân nhắc thay thế hẳn app cũ hoặc đặt tên/icon khác để tránh nhầm trong danh sách App Space.
```
