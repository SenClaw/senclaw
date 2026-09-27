# Hướng dẫn chi tiết tạo Widget cho SenClaw

Widget là **card hiển thị inline trong ô chat** (Web + Desktop) và trên Dashboard — cách app/plugin đưa UI trực quan (đếm ngược, biểu đồ, phễu bán hàng, bảng số liệu…) vào thẳng cuộc hội thoại, do **agent chèn** bằng tool `emit_widget` đúng lúc ngữ cảnh cần.

Tài liệu này là hướng dẫn thực hành đầy đủ: chọn loại widget, khai manifest, viết trang HTML, viết skill dạy agent, test và debug. Hợp đồng payload gốc: [`WIDGET_CONTRACT.md`](../WIDGET_CONTRACT.md). Code đối chiếu: registry [`src/widgets/mod.rs`](../src/widgets/mod.rs), tool [`src/tools/emit_widget.rs`](../src/tools/emit_widget.rs), renderer web [`web/src/components/WidgetCard.tsx`](../web/src/components/WidgetCard.tsx), app mẫu [`apps/widget-pack/`](../apps/widget-pack/). Hướng dẫn tổng quan về Space App: [space-app-sdk-publish-guide.md](space-app-sdk-publish-guide.md).

## 1. Kiến trúc — ai làm gì

```
        khai báo                      emit                        render
┌─────────────────────┐   ┌────────────────────────┐   ┌───────────────────────────┐
│ 3 NGUỒN             │   │ AGENT                  │   │ 3 RENDERER                │
│ • builtin (6 kind)  │──►│ widget_list (khám phá) │──►│ • Web: WidgetCard.tsx     │
│ • app: manifest     │   │ emit_widget (chèn)     │   │ • Desktop: widget_card.dart│
│   widgets[]         │   │        │               │   │ • Kênh text (TG/Zalo…):   │
│ • plugin:           │   │        ▼               │   │   1 dòng textFallback     │
│   widgets/widgets.json │ │ daemon resolve registry │  └───────────────────────────┘
└─────────────────────┘   │ + validate params       │
                          │ + build entry URL       │
                          │ → WS chat:widget        │
                          │ → persist chat_widgets  │
                          └────────────────────────┘
```

Điểm mấu chốt phải hiểu trước khi viết widget:

1. **Widget là display-only** — một chiều, user không tương tác trả lời qua nó (khác FormUI). Tool emit xong trả về ngay.
2. **Agent là người chèn**, không phải app. App chỉ *khai báo* widget + *phục vụ* trang HTML; muốn agent thực sự dùng thì phải **kèm skill dạy** (mục 5).
3. **Registry tính lại mỗi lần gọi** (quét bảng `space_apps` enabled + plugin enabled) — cài/update app là widget có mặt ngay, không cần restart daemon.
4. Kênh chat chỉ có text không render được card — daemon tự gửi **một dòng text fallback** thay thế. Viết widget mà quên đường này là user Telegram/Zalo nhận im lặng.

## 2. Chọn đúng loại widget

| Loại | Bản chất | Khi nào dùng | Không dùng khi |
|---|---|---|---|
| **Builtin kind** (`chart`, `image`, `clock`, `weather`, `video`, `audio`) | Client render native từ `data` | Số liệu/media thuần: biểu đồ, ảnh, video, đồng hồ. Nhẹ, đẹp theo theme, không tốn iframe | Cần logic/JS riêng, cần dữ liệu sống tự cập nhật |
| **App widget** (kind `app`, manifest `widgets[]`) | Iframe trỏ vào trang HTML app phục vụ | UI riêng của app: phễu CRM, đếm ngược, bảng tuỳ biến, khung có dữ liệu sống (tự fetch API app) | Chỉ để vẽ một biểu đồ tĩnh — builtin `chart` nhẹ hơn nhiều |
| **Plugin widget** (`widgets/widgets.json`) | Iframe HTML tĩnh, daemon serve hộ | Widget thuần client không cần server (plugin không có process) | Cần API động phía sau |

Kinh nghiệm từ chính widget-pack (skill của nó tự khuyên): trong **chat** ưu tiên builtin kind; bản iframe dành cho **Dashboard** hoặc khi cần khung cố định có logic riêng.

## 3. Builtin kinds — cấp data đúng shape

Agent emit trực tiếp; app tham gia bằng cách **cấp dữ liệu/URL đúng** (qua tool MCP của app, kết quả trả về dặn agent emit). Data schema từng kind (contract §2):

| kind | data |
|---|---|
| `chart` | `{ chartType: bar\|line\|area\|pie\|scatter, series: [{name, color?, points: [{x,y}]}], xLabel?, yLabel?, stacked? }` — tool còn nhận **shortcut**: `rows` (mảng object phẳng — MỌI cột số thành series, cột x tự dò hoặc khai `x`), `labels` + `values`, points dạng `[x,y]`/số trần; chuỗi số `"37"`, `"33,5"` (phẩy thập phân) đều parse được. Daemon chuẩn hoá về canonical (`src/widgets/chart_data.rs`) |
| `image` | `{ url? \| dataUrl?, caption?, alt? }` — bắt buộc 1 trong url/dataUrl |
| `clock` | `{ tz?, label?, showSeconds?, showDate?, format24h? }` |
| `weather` | `{ location, unit: "C"\|"F", current: {temp, condition, icon, humidity?, wind?}, daily?: [{day, hi, lo, icon}] }` — icon ∈ sunny/partly_cloudy/cloudy/rain/thunderstorm/snow/fog/wind |
| `video` | `{ url, poster?, caption?, mime?, autoplay? }` — url **bắt buộc http(s)**; đường dẫn file local bị tool từ chối thẳng (card chết) |
| `audio` | `{ url, caption?, mime? }` — như video |

Hai pattern app-cấp-liệu đang chạy thật:

- **drawio**: tool `export` trả `svg_path` = URL same-origin qua `/api/space/apps/drawio/proxy/...`, mô tả tool dặn agent đưa vào `emit_widget` kind `image` → sơ đồ hiện inline trong chat.
- **tiktok-dl**: mỗi download xong trả `file_urls` http(s) → agent emit kind `video` phát ngay trong chat.

Ngoài tool, agent chèn chart/media **inline giữa câu trả lời** bằng fence markdown ` ```chart ` / ` ```weather ` / ` ```image `… (cùng shortcut data). Kind `app` thì tool là đường chính (xem mục 8).

## 4. Tạo App widget từng bước

Ví dụ xuyên suốt: widget `countdown` của app `widget-pack` (code thật, đọc được toàn bộ tại `apps/widget-pack/`).

### Bước 1 — Khai `widgets[]` trong `senclaw-manifest.json`

```jsonc
"widgets": [
  {
    "id": "countdown",                     // BẮT BUỘC — id đầy đủ = "<app-id>.countdown"
    "name": "Đếm ngược",
    "description": "Đồng hồ đếm ngược sống tới một thời điểm (deadline, sự kiện). Param `to` là ngày YYYY-MM-DD hoặc ISO datetime; `label` là tên sự kiện.",
    "entryUrl": "/widget/countdown.html",  // đường dẫn app phục vụ
    "size": "small",                       // small | medium | large | tall
    "surfaces": ["chat", "dashboard"],     // MẶC ĐỊNH ["dashboard"] — thiếu "chat" là KHÔNG chèn được vào chat
    "params": {
      "type": "object",
      "properties": {
        "to":    { "type": "string", "description": "Thời điểm đích: \"2026-12-31\" hoặc \"2026-12-31T09:00\"" },
        "label": { "type": "string", "description": "Tên sự kiện hiển thị phía trên" }
      },
      "required": ["to"]
    },
    "textFallback": "⏳ Đếm ngược {label} tới {to} — xem trên SenClaw Web/Desktop"
  }
]
```

Chi tiết từng trường (nguồn: `parse_manifest_widgets` trong `src/widgets/mod.rs`):

| Trường | Bắt buộc | Ghi chú |
|---|---|---|
| `id` | ✔ | Thiếu/rỗng → entry bị bỏ qua im lặng. Id đầy đủ trong registry = `<app-id>.<id>` |
| `name` | – | Mặc định = id. Thành **title mặc định** của card khi agent không truyền title |
| `description` | – | **Quan trọng nhất với agent** — `widget_list` đưa nguyên văn cho agent quyết định khi nào chèn và điền param gì. Viết như viết mô tả tool: nói rõ dùng khi nào, từng param nghĩa gì |
| `entryUrl` | – | App-relative (`/widget/foo.html`). Entry cuối = origin app đang chạy (daemon stamp `runtime.url` khi spawn) hoặc fallback `/api/space/apps/<id>/proxy` + entryUrl — proxy **tự boot app** ở hit đầu nên app chưa chạy vẫn render được |
| `size` | – | Chiều cao khung trên web: `small` 180px, `medium` 320px (mặc định), `large` 480px, `tall` 560px |
| `surfaces` | – | `"chat"` và/hoặc `"dashboard"`. **Vắng mặt = `["dashboard"]`** (giữ tương thích manifest cũ) → emit vào chat bị từ chối với lỗi "does not support the chat surface" |
| `params` | – | JSON Schema `type: object`. Daemon validate lúc emit — xem Bước 3 |
| `refreshMs` | – | Client reload iframe theo chu kỳ (setInterval). **< 1000 bị lờ đi**. Dùng cho widget dữ liệu sống (phễu CRM 30000) |
| `textFallback` | – | Template cho kênh text, placeholder `{param}` — xem Bước 4 |
| `intents` | – | Khai widget làm ứng viên default-handler cho flow (`media`…) — cấu hình ở `GET/PUT /api/defaults`, hiếm khi cần |

### Bước 2 — Viết trang HTML widget

Nguyên tắc: **một file HTML tự chứa** (CSS + JS inline), đọc params từ **query string**, render gọn trong khung size đã khai. Đây là `countdown.html` thật, rút gọn phần lặp — dùng làm khuôn:

```html
<!doctype html>
<html lang="vi">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Đếm ngược</title>
<style>
  /* Theme: iframe KHÔNG nhận message theme từ host — tự theo hệ bằng
     color-scheme + prefers-color-scheme, nền transparent để hoà vào card. */
  :root { color-scheme: light dark;
    --fg: #1f2430; --muted: #7a8194; --accent: #5b8ff9; --bg: transparent; }
  @media (prefers-color-scheme: dark) {
    :root { --fg: #e8eaf0; --muted: #8a90a3; }
  }
  html, body { margin: 0; height: 100%; background: var(--bg);
    font-family: -apple-system, "Segoe UI", Roboto, sans-serif; color: var(--fg); }
  .wrap { height: 100%; display: flex; flex-direction: column;
    align-items: center; justify-content: center; gap: 8px; padding: 12px; box-sizing: border-box; }
  /* … style số/nhãn … */
</style>
</head>
<body>
<div class="wrap">
  <div class="label" id="label"></div>
  <div class="digits" id="digits" hidden><!-- ngày/giờ/phút/giây --></div>
  <div class="msg" id="msg"></div>
</div>
<script>
  // 1. Params tới qua query string — emit_widget ghép từ `params` của agent.
  const q = new URLSearchParams(location.search);
  const label = q.get('label') || '';
  const rawTo = (q.get('to') || '').trim();

  // 2. Validate trong trang: param thiếu/sai → thông báo thân thiện TRONG khung,
  //    không bao giờ để khung trắng (schema daemon chỉ chặn thiếu required,
  //    không chặn giá trị rác).
  const iso = /^\d{4}-\d{2}-\d{2}$/.test(rawTo) ? rawTo + 'T00:00:00' : rawTo;
  const target = new Date(iso);
  if (!rawTo || isNaN(target.getTime())) {
    document.getElementById('msg').textContent = rawTo
      ? 'Thời điểm không hợp lệ: "' + rawTo + '"'
      : 'Thiếu tham số `to` (YYYY-MM-DD hoặc ISO datetime).';
  } else {
    // 3. Logic sống: setInterval 1s cập nhật số — widget "động" mà không cần refreshMs.
    /* … tick() đếm ngược … */
  }
</script>
</body>
</html>
```

Checklist cho trang widget:

- **Tự chứa** — không CDN ngoài (widget phải chạy offline/loopback); cần thư viện thì inline hoặc serve cùng app.
- **Theme**: `color-scheme: light dark` + `prefers-color-scheme` + nền `transparent`. Đừng hardcode nền trắng — dark mode sẽ chói.
- **Vừa khung**: thiết kế cho đúng chiều cao size đã khai (small 180px…); tránh scroll dọc trong card.
- **Chịu lỗi param**: schema daemon lenient (Bước 3) nên trang phải tự validate và hiện lỗi đọc được.
- **Dữ liệu sống**: hoặc JS tự tick (đồng hồ), hoặc fetch API của chính app (same-origin qua proxy), hoặc khai `refreshMs` để client reload cả iframe.

### Bước 2b — Phục vụ trang

Hai cách, đều đang chạy thật:

- **HTML tĩnh thuần (widget-pack)**: đặt `web/widget/*.html`, **không cần build** — `pack.sh` copy nguyên `web/` thành `web_dist/` cạnh binary, server app serve static. Đơn giản nhất cho widget.
- **App có Vite (crm…)**: đặt file vào thư mục public của Vite (được copy nguyên vào `dist/`) để giữ đường dẫn `/widget/foo.html` cố định — entry path nằm trong manifest, **đừng** để Vite hash tên file.

Kiểm tra nhanh: mở thẳng `http://127.0.0.1:<port-app>/widget/countdown.html?to=2026-12-31&label=Test` trên trình duyệt — trang phải render đúng trước khi nghĩ tới agent.

### Bước 3 — Hiểu validate params (để khai schema cho khéo)

Validator daemon (`validate_params`) **cố tình tối giản**:

- Check `required` có mặt.
- Check `type` nguyên thuỷ từng property đã khai: `string` / `number` / `integer` / `boolean` / `array` / `object`.
- **Param không khai trong schema vẫn cho qua** (app có thể nhận nhiều hơn nó công bố). Không check format/enum/min-max — việc của trang HTML.

Hệ quả thiết kế:

- Params đi qua **query string** → giá trị phức tạp (mảng, object) truyền dạng **chuỗi JSON** rồi trang tự `JSON.parse`. Pattern thật — widget `table` của widget-pack: `"rows": "[[\"Coffee\",25000],[\"Milk coffee\",30000]]"` (schema khai `type: string`, description ghi rõ "JSON string").
- Ghi `description` từng param thật kỹ — agent điền param dựa vào đó, validator không đỡ được giá trị sai nghĩa.

### Bước 4 — `textFallback` cho kênh chỉ có text

Khi widget bắn ra jid không phải `web:`/`app:` (Telegram, Zalo, QQ…), WS `chat:widget` không tới được — daemon gửi **một tin nhắn text** thay thế:

- Có `textFallback`: render template, `{param}` thay bằng giá trị emit; **param không tồn tại → chuỗi rỗng** (không bao giờ lộ ngoặc `{}` thô); ngoặc không đóng giữ nguyên literal.
- Không có: dòng mặc định `"<title> — mở SenClaw → /space/app/<app-id> để xem chi tiết"`.

Viết fallback như một câu tin nhắn hoàn chỉnh có emoji + giá trị chính (`"⏳ Đếm ngược {label} tới {to} — xem trên SenClaw Web/Desktop"`) — với user kênh ngoài, dòng này **là** widget.

### Bước 5 — Viết skill dạy agent dùng widget

Không có skill, agent gần như không tự biết app có widget để chèn. Pattern chuẩn (nguyên văn cấu trúc `apps/widget-pack/skills/widget-pack/SKILL.md`):

```markdown
---
name: my-app-widgets
description: Insert My App widgets into the chat — pipeline funnel, KPI card — via emit_widget kind "app"
version: 1.0.0
when-to-use: When the user wants to see the sales pipeline or a KPI snapshot right inside the chat
triggers:
  - phễu bán hàng
  - pipeline
  - kpi
allowed-tools:
  - emit_widget
  - widget_list
---

# My App — widgets in the chat box

Insert via `emit_widget` with `kind: "app"` (do NOT pass `data` — pass
`widget` + `params`). Call `widget_list` if you need the catalog/params.

**Pipeline** — sales funnel by stage:
​```
emit_widget { "kind": "app", "widget": "my-app.pipeline",
  "params": { "stage": "won" } }
​```

## Notes
- Messaging channels only receive a one-line text fallback — the full widget
  shows on the SenClaw Web/Desktop UI.
- Params travel as a query string; compute values yourself and fill them in.
```

Ba điều bắt buộc có trong skill:

1. `allowed-tools: [emit_widget, widget_list]`.
2. Nhắc rõ **"kind app: truyền `widget` + `params`, KHÔNG truyền `data`"** — lỗi agent hay mắc nhất.
3. Ví dụ emit **từng widget với params thật** — agent bắt chước ví dụ tốt hơn đọc schema.

Skill khai trong manifest `skills[]` như mọi skill app khác; đặt `triggers` trúng từ khoá user hay gõ.

### Bước 6 — Test end-to-end & debug

```bash
# 1. Registry đã thấy widget chưa? (id, enabled, entry, surfaces)
curl -s http://127.0.0.1:18788/api/widgets | python3 -m json.tool | grep -A2 '"my-app.'

# 2. Trang render đúng chưa? — mở thẳng entry + query params trên trình duyệt
open "http://127.0.0.1:18788/api/space/apps/my-app/proxy/widget/pipeline.html?stage=won"

# 3. Nhờ agent chèn thật (Web UI chat):
#    "dùng widget_list xem có widget nào của my-app rồi chèn thử phễu giai đoạn won"
```

Bảng lỗi emit thường gặp (message thật từ `parse_app_spec`):

| Lỗi tool trả về | Nguyên nhân | Cách xử |
|---|---|---|
| `unknown widget "x.y" — call widget_list…` | Sai id (nhớ **id đầy đủ** `<app-id>.<short-id>`), app **disabled**, hoặc manifest entry thiếu `id` | Check `GET /api/widgets`; bật app trong Space |
| `widget "x.y" is disabled in Plugins → Widget settings` | User tắt widget này | Bật lại ở Plugins → Widget (lưu `defaults.disabledWidgets` trong `~/.senclaw/config.json`) |
| `widget "x.y" does not support the chat surface` | Manifest thiếu `"chat"` trong `surfaces` (mặc định chỉ dashboard) | Thêm `"surfaces": ["chat", "dashboard"]` |
| `missing required param "…"` / `param "…" must be of type …` | Agent điền thiếu/sai kiểu | Sửa description param + ví dụ trong skill |
| `kind "app" requires "widget"…` / `params must be an object` | Agent truyền `data` thay vì `widget`+`params` | Skill phải có dòng nhắc "do NOT pass data" |
| `the widget registry is not available in this runtime` | Tool chạy ngoài daemon (MCP subprocess/test standalone) | Kind app chỉ hoạt động trong daemon; builtin kinds vẫn chạy |
| Iframe trắng | Entry 404 (file không vào `web_dist/`), hoặc JS crash | Mở entry trực tiếp + DevTools; check `pack.sh` copy đủ `widget/` |

Sửa manifest xong: registry đọc từ bảng `space_apps`, nên cần **cài lại/refresh app** (re-install zip hoặc restart app từ Space) để manifest mới vào DB — sau đó widget có mặt ngay, không cần restart daemon.

## 5. Hành vi runtime cần biết

- **Emit → persist + broadcast**: mỗi widget có `id` (`widget-<uuid>`), lưu bảng `chat_widgets` (FIFO theo jid), đẩy WS `{type: "chat:widget", groupJid, id, widget, ts}`; `history:load` trả lại như message `role: "widget"` — widget **sống qua reload** trang chat.
- **Iframe sandbox** đúng bằng SpaceAppFrame: `allow-forms allow-modals allow-popups allow-same-origin allow-scripts` — widget chung mức tin cậy với app của nó.
- **Chiều cao** theo `size` (small 180 / medium 320 / large 480 / tall 560, mặc định medium); dưới card luôn có link "Mở app ↗" deep-link `/space/app/<app-id>`.
- **Entry hỏng** (app gỡ, registry không resolve được): card hiện fallback + link mở app — không bao giờ khung vỡ.
- **`chat_jid`**: tool nhận param này để bắn widget sang chat khác (mặc định chat hiện tại) — dùng khi agent chạy nền muốn đẩy card về một group.
- **Title**: agent truyền `title` thì dùng, không thì lấy `name` trong manifest.
- Client **cũ** gặp kind lạ render chip lỗi, không crash — cứ thêm kind/field mới yên tâm về backward-compat.

## 6. Widget cho Plugin (không có app server)

Plugin marketplace khai widget y hệt schema trên, trong file `<pluginDir>/widgets/widgets.json` (mảng entries):

```json
[
  { "id": "hello", "name": "Hello", "description": "…", "entryUrl": "/hello.html",
    "surfaces": ["chat"], "params": { "type": "object", "properties": {} } }
]
```

Khác biệt so với app widget:

- **Không có server** — daemon serve file tĩnh hộ tại `/api/marketplace/plugins/<plugin>/widget-static/<entryUrl>`. HTML phải thuần client (fetch được API daemon same-origin nếu cần).
- Surface mặc định là `["chat"]` (plugin sinh ra cho ô chat), ngược với app widget mặc định `["dashboard"]`.
- Id đầy đủ = `<plugin-name>.<short-id>`; nguồn hiện là `plugin:<name>` trong catalog.
- Chỉ load khi plugin **enabled**; `widgets.json` hỏng → bỏ qua có log warn, không chết daemon.

## 7. Quản trị: bật/tắt & catalog

- `GET /api/widgets` — toàn bộ catalog (builtin + app + plugin) kèm cờ `enabled`.
- `PUT /api/widgets/:id` body `{"enabled": false}` — tắt một widget (ghi `defaults.disabledWidgets`); widget tắt thì emit fail với message rõ ràng.
- UI người dùng: **Plugins → Widget** (catalog + toggle + defaults).
- `GET/PUT /api/defaults` — default flow handlers (open link / media / search / note); widget khai `intents` là ứng viên tại đây.

## 8. Fence ` ```widget ` — đường chèn không qua tool

Trong text trả lời, agent viết fence là client render widget tại đúng vị trí đó:

- ` ```chart `, ` ```weather `, ` ```clock `, ` ```video `, ` ```audio `, ` ```image ` — chở thẳng object `data` (chart nhận đủ shortcut như tool).
- ` ```widget ` — chở full spec `{kind, title, data}`, kể cả kind `app`. **Nhưng** spec từ fence không qua daemon: web client tự resolve `entry` từ `GET /api/widgets` (path cố định theo manifest, params vẫn chỉ vào query string), còn **validate params, textFallback và fallback kênh text đều KHÔNG chạy**. Vì vậy: fence hợp cho chart/media inline; kind `app` cứ đi qua tool.
- JSON hỏng/chưa stream xong → hiện code block thường (an toàn khi streaming).

## 9. Checklist tạo một app widget

```
[ ] Manifest widgets[]: id + name + description viết cho agent đọc
[ ] surfaces có "chat" (mặc định chỉ dashboard!)
[ ] size hợp nội dung (small 180 / medium 320 / large 480 / tall 560)
[ ] params: JSON Schema object, description từng param kỹ; giá trị phức tạp = JSON string
[ ] textFallback là một câu tin nhắn hoàn chỉnh có {param}
[ ] Trang HTML tự chứa tại entryUrl: đọc URLSearchParams, theme light/dark
    (color-scheme + prefers-color-scheme, nền transparent), tự validate param,
    vừa khung không scroll
[ ] pack.sh copy trang vào web_dist/ (đường dẫn cố định, không hash)
[ ] refreshMs (≥1000) nếu là dữ liệu sống cần reload cả frame
[ ] Skill: allowed-tools [emit_widget, widget_list], nhắc "widget+params, KHÔNG data",
    ví dụ emit từng widget với params thật
[ ] Test: mở entry trực tiếp → GET /api/widgets thấy id + enabled → nhờ agent chèn
[ ] Thử một kênh text (nếu có) xem dòng textFallback đọc có ổn không
```
