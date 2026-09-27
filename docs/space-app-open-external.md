# Mở link ngoài từ Space App — luồng `openExternal`

**Vấn đề.** Trong desktop SenClaw, Space App chạy trong webview nhúng
(WKWebView/WebView2 qua `flutter_inappwebview`). Một thẻ `<a href>` bình
thường sẽ điều hướng *chính webview đó* sang trang ngoài — người dùng "mất"
UI của app (báo cáo Zeach, dashboard CRM, …) và không có nút back tử tế.
Mọi link ra ngoài app phải mở trên **trình duyệt hệ thống**.

## Luồng 3 lớp

```
Click link ngoài trong UI Space App
│
├─ Lớp 1 — Hook JS trong app (chủ động, nên có)
│   installExternalLinkHook(): click <a> http(s) khác origin
│   → preventDefault → openExternal(url):
│        1. Desktop webview → flutter_inappwebview.callHandler('senclawOpenExternal', url)
│        2. Trình duyệt thật (standalone/iframe) → window.open(url,'_blank','noopener')
│
├─ Lớp 2 — Lưới an toàn trong webview desktop (tự động, mọi app được hưởng)
│   desktop_app/lib/widgets/embedded_web_stub.dart:
│   • onCreateWindow: window.open / target=_blank → browser hệ thống
│   • shouldOverrideUrlLoading: điều hướng main-frame sang origin ≠ origin app
│     → CANCEL + mở browser hệ thống (iframe con không bị đụng)
│
└─ Lớp 3 — API trên daemon (programmatic)
    POST http://127.0.0.1:18788/api/ui/open-url  {"url": "https://…"}
    → validate http/https → mở bằng open/xdg-open/FileProtocolHandler trên máy host
```

Lớp 2 nghĩa là **mọi app đều đã được bảo vệ trên desktop mà không cần sửa
code**: link thường không thể kéo webview rời khỏi app nữa. Lớp 1 vẫn nên
thêm khi chạm vào UI của app, vì nó cho hành vi đúng cả khi app chạy
standalone trong trình duyệt thật (mở tab mới thay vì rời trang) và không
phụ thuộc phiên bản desktop.

## Lớp 1 — thêm hook vào một app (checklist)

1. **Copy helper** — mẫu chuẩn: [`apps/zeach/web/src/openExternal.ts`](../apps/zeach/web/src/openExternal.ts)
   (bản gốc rút gọn: `apps/facebook-pro/web/src/api.ts` `openExternal`).
2. **Gắn hook một lần** trong `web/src/main.tsx`:

   ```ts
   import { installExternalLinkHook } from './openExternal'
   installExternalLinkHook()
   ```

   Hook bắt ở capture phase: click trái, không phím bổ trợ, `<a href>`
   http(s) khác `location.origin` → `openExternal`. Các component **không
   cần sửa** gì.
3. **Markdown renderer** (nếu app render markdown từ LLM/web): thêm
   `target="_blank" rel="noreferrer noopener"` cho thẻ `a` — ví dụ
   [`apps/zeach/web/src/Md.tsx`](../apps/zeach/web/src/Md.tsx). Nhờ đó
   middle-click/cmd-click và ngữ cảnh standalone vẫn đúng.
4. **Gọi trực tiếp khi cần** (nút "Mở trang", OAuth, docs):
   `openExternal(url)` thay cho `window.open`.
5. Rebuild web (`npm run build`), chạy lại `scripts/pack.sh`, sync
   `web_dist` bản đã cài nếu muốn áp dụng ngay.

Trường hợp bắt buộc dùng `openExternal` chứ không phải `window.open`:
**OAuth** (Facebook/Google từ chối webview nhúng — `disallowed_useragent`),
link tải file, và mọi link trong nội dung do LLM/web sinh ra.

## Lớp 2 — hành vi webview desktop (tham chiếu)

`embedded_web_stub.dart` giữ webview ghim vào origin của `app.url`:

- Điều hướng main-frame cùng scheme+host+port → cho phép (SPA, trang nội bộ).
- Khác origin http(s), `mailto:`, `tel:`, scheme lạ → `launchUrl` ra ngoài,
  CANCEL trong webview.
- `about:`/`data:`/`blob:`/`javascript:` và mọi điều hướng **sub-frame**
  (iframe — drawio, preview…) → cho phép, không đụng.
- `window.open`/`target=_blank` → `onCreateWindow` → browser hệ thống.

Giới hạn đã biết: bản **web build** của desktop (iframe trong trình duyệt
thật) không có lớp này — link tuân theo trình duyệt; vì vậy app vẫn nên có
Lớp 1.

## Lớp 3 — `POST /api/ui/open-url`

Handler: [`src/gateway/ui_server/open_url.rs`](../src/gateway/ui_server/open_url.rs).

```bash
curl -s -X POST http://127.0.0.1:18788/api/ui/open-url \
  -H 'content-type: application/json' \
  -d '{"url":"https://example.com/docs"}'
# → {"ok":true}
```

- Chỉ nhận URL http/https tuyệt đối, có host, không ký tự control — chặn
  `file://`, `javascript:`, scheme app tùy ý.
- URL được truyền thành **một** argv cho opener của OS (`open` /
  `xdg-open` / `rundll32 url.dll,FileProtocolHandler`), không qua shell.
- UI server bind `127.0.0.1` → chỉ tiến trình trên cùng máy gọi được.
- Dùng cho: backend Space App, agent/MCP, hoặc UI muốn nhờ daemon mở trên
  máy host. Lưu ý khi client ở máy khác (relay/mobile): API mở trên máy
  *daemon*, không phải máy client — với web UI từ xa hãy dùng
  `window.open` phía trình duyệt.

## Trạng thái áp dụng

- `zeach` — Lớp 1 đầy đủ (hook + markdown `target=_blank`). Mẫu chuẩn.
- `google-workspace` — Lớp 1 đầy đủ (hook + `openExternal`) **và Lớp 3 làm
  đường chính cho OAuth**: nút OAuth gọi `POST /api/auth/open` trên backend
  app → daemon `/api/ui/open-url` → browser hệ thống (không phụ thuộc bridge
  webview hay popup policy; với OAuth browser buộc phải ở máy daemon vì
  redirect URI là `127.0.0.1:4310`). Client-side `openExternal` chỉ là
  fallback khi daemon không mở được; callback trả trang "đóng tab này".
- `facebook-pro` — có `openExternal` cho OAuth (chưa có hook click toàn cục).
- Các app còn lại — được Lớp 2 bảo vệ trên desktop; thêm Lớp 1 theo
  checklist ở trên khi chạm vào UI của app đó.
