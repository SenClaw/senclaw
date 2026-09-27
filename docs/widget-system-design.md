# Widget System & Default Flows — hợp nhất widget chat + widget app, registry, skill, và cài đặt luồng mặc định

> Trạng thái: **ĐÃ BUILD & VERIFY — 2026-08-02** (P0→P3 cùng ngày thiết kế).
> cargo test toàn suite xanh (1267 lib + mọi integration target), `npm run
> build` (web) OK, `flutter analyze` desktop_app + channel_app sạch. Chưa
> e2e với daemon sống (daemon đang chạy là bản cũ — cần build + khởi động lại
> để có `/api/widgets`). Hợp đồng payload: `WIDGET_CONTRACT.md` (repo root).
> Phạm vi: (1) widget dựng UI đặc biệt trong ô chat, app cung cấp được widget;
> (2) widget registry + skill hướng dẫn; (3) file định nghĩa widget trong app;
> (4) bộ quản lý widget trong trang **Plugins → mục "Widget"**; (5) cài đặt
> widget mặc định + các **luồng mặc định**: mở link ở đâu, media dùng gì,
> search dùng gì, note ghi vào đâu.
>
> **Khác biệt so với thiết kế ban đầu (quyết định lúc build):**
> 1. `textFallback` chỉ hỗ trợ thay thế `{param}` phẳng (không có cú pháp điều
>    kiện `{p? …}` — không đáng độ phức tạp).
> 2. `openLink` server-side: `POST /api/ui/open-url` **giữ nguyên** system
>    browser (UiState không có kênh WS broadcast; đường `ui:open` để sau).
>    Default `mini-browser` được tôn trọng ở: click link web UI, click link
>    desktop (đồng thời sửa luôn bug click thường không làm gì), và prompt
>    inject cho agent.
> 3. Static route widget plugin là `/api/marketplace/plugins/:name/widget-static/*`
>    (KHÔNG phải `/api/plugins/...` — namespace đó thuộc hệ plugin clawhub cũ).
> 4. channel_app (mobile, remote qua relay): video/audio/app render thành card
>    thông tin (caption + URL + textFallback) vì URL `127.0.0.1` của daemon
>    không truy cập được từ điện thoại.
> 5. `widget_list` là native tool `should_defer=true` (ToolSearch + skill dẫn
>    tới), không phình prompt mặc định.
>
> **Bổ sung cùng ngày (theo yêu cầu sau khi dùng thử):**
> 6. **Luồng data→widget** (`src/widgets/chart_data.rs`): `emit_widget` kind
>    `chart` nhận dạng thô — `rows` (bảng: mỗi cột số thành một series, cột x
>    tự nhận diện `x/date/day/label/name/ngày…` hoặc chỉ định `"x"`),
>    `labels`+`values`, points `[x,y]`/số trần, chuỗi số kể cả "33,5" — daemon
>    chuẩn hoá về canonical trước khi persist. Lý do: agent từng viết
>    `/tmp/weather_data.js` + node chỉ để reshape data (tốn 1 lượt permission).
>    Description + skill giờ CẤM file tạm/bash/node cho việc chuẩn bị data widget.
> 7. **Fence là đường chính để chèn chart trong câu trả lời**: renderer 3 client
>    tự chuẩn hoá cùng các shape trên (`deriveChartSeries` web,
>    `_seriesFromRows` 2 bản Flutter) nên ```` ```chart ```` trong text nhận
>    `rows` y như tool; skill dạy đặt fence đúng vị trí trong mạch văn bản và
>    "đã nói 'dưới đây là biểu đồ' thì phải kèm fence". Kind `app` vẫn bắt buộc
>    qua tool (cần registry).
> 8. **App `apps/widget-pack` (port 4750)**: Space App mẫu chứa widget custom
>    iframe cho chat — **6 widget**: `countdown` / `progress` / `table` +
>    `chart` (SVG bar/line/pie tự chứa) / `image` / `video` (params schema +
>    textFallback đầy đủ). Kèm skill `widget-pack` (body tiếng Anh, trigger
>    Việt+Anh — quy ước skill mới). Axum static thuần, không MCP, web/ không
>    cần build. Zip cài đặt: `scripts/pack.sh` → `widget-pack-app.zip` (604K,
>    manifest zip-root + binary + web_dist + skills). Workspace member +
>    launch.json entry.
>
> **E2E TRONG CHAT — VERIFIED 2026-08-02** (daemon test cách ly: HOME riêng,
> port 19788/19789, config chỉ chép llmConfigs — không channel token):
> register-local app → `GET /api/widgets` trả đủ 6 widget đúng surfaces → gửi
> tin nhắn thật → skill trigger → `widget_list` → `emit_widget kind "app"` ×2
> + `chart` (normalize `rows`) → 3 frame `chat:widget` → web UI render sống:
> countdown iframe, progress bar 68% trong iframe, bar chart native. KHÔNG có
> file tạm/bash nào trong cả lượt. Bẫy e2e ghi lại trong memory (frame
> `connect` để lấy admin WS; process 4750 stale của daemon thật chiếm port).
>
> **Desktop bổ sung cùng ngày:** dialog thông tin Space App hiện thêm mục
> **Widgets** (từ manifest widgets[], tooltip mô tả, chip surface) và **Alias**
> (mcp.toolAliases, ghi chú "nhập ở trạng thái tắt"); 3 tab Skills / Subagents
> / MCP có ô **search** (match tên/mô tả/tool, giữ query khi đổi tab).

---

## 1. Yêu cầu gốc

Từ yêu cầu người dùng (02-08-2026):

1. Widget để build các UI đặc biệt trên ô chat box.
2. App (Space App) có thể cung cấp widget.
3. Có **widget registry**; có **skill** hướng dẫn agent sử dụng các widget.
4. Có **file widget định nghĩa trong app**.
5. **Bộ quản lý widget nằm trong trang Plugins**, mục "Widget" (bổ sung: "thêm quản lý widget vào trong plugins → widget").
6. Cài đặt widget default; cài đặt các luồng mặc định:
   - Mở link default ở đâu?
   - Media default là gì?
   - Search default dùng gì?
   - Note default ghi vào đâu?

---

## 2. Hiện trạng — repo đã có gì (khảo sát 2026-08-02)

### 2.1 Chat widget (đã có, display-only, 5 kind hardcode)

Pipeline một chiều `emit_widget` → UI, tồn tại từ 2026-07-14:

- Tool native `emit_widget` — `src/tools/emit_widget.rs:21` `KINDS = ["chart","image","clock","weather","video"]` (5 kind, không phải 4 như doc comment cũ). Enum đóng: check imperative `:53` + JSON-Schema `enum` `:100`. `data` là JSON **opaque với backend** (`WidgetSpec.data: serde_json::Value`, `src/zen_core/mod.rs:779`) — chỉ client validate. Mô tả cho agent nằm duy nhất ở `DESCRIPTION` (`emit_widget.rs:23-41`). Tool luôn nằm trong roster (không deferred).
- Đường sự kiện: `EmitWidgetTool::call` → `EngineEvent::WidgetEmit` (`src/zen_core/events.rs:53`) → AgentPool loop (`src/agent/agent_pool/engine.rs:364-372`, callback `on_widget_emit`) → bridge `src/lib.rs:2383-2421`: persist `db.insert_chat_widget` **rồi mới** `gw.notify_widget` → WS frame `{"type":"chat:widget", groupJid, id, widget, ts}` (`src/gateway/websocket_gateway/notify.rs:567-582`).
- Persist: bảng `chat_widgets(id, chat_jid, widget_json, created_at)` (`src/db/schema.rs:199-206`), FIFO theo `groups.max_messages`; `history:load` merge row thành `{role:"widget"}` (`src/gateway/websocket_gateway/handlers.rs:424-478`).
- Render **3 bản copy**: web `web/src/components/WidgetCard.tsx` (recharts, dispatch `:418-425`), desktop `desktop_app/lib/features/chat/widgets/widget_card.dart` (fl_chart), mobile `channel_app/lib/widgets/widget_card.dart`. Fence fallback ```` ```widget/chart/weather/clock/video ```` ở cả 3 (`MessageBubble.tsx:97`, `app_markdown.dart:44`, `channel_app .../widget_card.dart:30`) — **cả 3 bộ đều thiếu `image`**, đã lệch với `KINDS`.

**Ba lỗ hổng hiện tại:**

1. **Kênh nhắn tin bị bỏ rơi im lặng.** `notify_widget` chỉ broadcast tới WS client subscribe jid (+ relay `app:`). Không có dòng nào dưới `src/channels/` biết đến widget; `Channel::send_message` chỉ nhận text (`src/channels/mod.rs:37`). Emit với `chat_jid="telegram:42"` → row vẫn ghi DB, tool vẫn trả "Rendered a chart widget…" → **agent tưởng thành công, người dùng Telegram không thấy gì**.
2. **Thêm 1 kind = sửa 6 chỗ hardcode** (KINDS + DESCRIPTION + schema enum; web types + WidgetCard + fence set; desktop widget_card + fence set; channel_app copy). Không có registry.
3. `WIDGET_CONTRACT.md` được trích dẫn ở 3 nơi (`emit_widget.rs:8`…) nhưng **không tồn tại trong repo**.

### 2.2 Widget của Space App (đã có — nhưng chỉ cho Dashboard, agent không biết)

Manifest đã có mục `widgets[]` từ trước: **10 app khai báo, 14 widget**, fields quan sát được: `id`, `name`, `description`, `size` (`small|medium`), `refreshMs`, `render:"client"`, `entryUrl` (vd `/widget/clock-analog.html`; các app CRM/email/mindmap/luna-calendar có hẳn entry riêng `web/src/widget-*-entry.tsx`).

- Tiêu thụ: web `web/src/pages/SpacePage.tsx:74-97` (resolve baseUrl = `runtime.url` trực tiếp `127.0.0.1:<port>` hoặc proxy `/api/space/apps/<id>/proxy/`) → render **iframe từng widget** ở `web/src/components/space/dashboard/Dashboard.tsx:60-108`; desktop `AppWidgetDef` (`space_providers.dart:174-187`) + `dashboard_screen.dart:853`.
- Manifest **không có Rust struct** — toàn bộ đọc bằng `serde_json::Value` + `.get()` chains, lưu raw TEXT trong bảng `space_apps` (`src/db/schema.rs:643-649`) → **thêm field mới không đụng migration nào**.
- Bridge postMessage host↔app đã chạy: `senclaw:init/ready/theme/request/response` (`web/src/components/space/SpaceAppFrame.tsx:38-100`; desktop inject ở `embedded_web_stub.dart:37-52`), bridge REST `POST /api/space/apps/<id>/bridge`.
- Deep-link nội bộ đã có chuẩn: `space_events.link` chỉ nhận `/space/app/<id>[?…]`, validator `sanitize_event_link` (`src/mcp/space_server.rs:31-64`), client re-validate (`desktop_app/.../event_link.dart`).
- Lưu ý hạ tầng: proxy reqwest **không tunnel được WebSocket** (`space_providers.dart:290-297`) → widget cần live data phải dùng direct URL hoặc polling `refreshMs`.

**Khoảng trống**: hai hệ widget (2.1 chat, 2.2 dashboard) không biết nhau. Agent không thể đưa widget app vào chat; dashboard widget không có params/schema; không có registry chung, không có skill nào dạy agent về chúng.

### 2.3 Plugin system — hiện là metadata thuần

- `plugin.json` chỉ parse `name/description/version/author/keywords` (`src/marketplace/manager.rs:62-69`). Capability theo **quy ước thư mục**: `skills/`, `subagents/`, `mcp/`, `hooks/` (`manager.rs:965-1030`).
- **Cả 4 đều chưa nối runtime**: marketplace MCP loading log `"not yet implemented"` (`src/agent/agent_pool/pool.rs:1148`); `load_all_skills_with_marketplace` / `get_source_defs_with_marketplace` (`src/skills/scan.rs:203/:75`) **không có caller**; hooks loader (`hook_config_loader.rs:481`) không có caller; subagents không có consumer. → Bật plugin hôm nay chỉ ghi state file.
- Install có **security scan gate** (`manager.rs:896`, `InstallOutcome::Blocked`); enable mặc định OFF; `install --force` chỉ người gõ được (`plugin_command.rs:75-77`).
- **Bẫy**: có **2 instance MarketplaceManager không chia sẻ bộ nhớ** — `Arc<MarketplaceManager>` riêng cho AgentPool (`lib.rs:1766`) và `Arc<Mutex<…>>` shared cho router/WS/REST (`lib.rs:1961`). REST enable/disable không tự đến AgentPool.
- UI: trang Plugins là **sidebar-nav + panel** (không phải antd Tabs). Thêm 1 mục = sửa 3 file + 1 entry JSX: `PluginsSidebar.tsx:25` (union `PluginsNavItem`), `PluginsPage.tsx:7` (`NAV_ITEMS`, deep-link `?nav=`), `PluginsView.tsx:21+41-49` (`NAV_LABEL` + switch), sidebar JSX `PluginsSidebar.tsx:353-430`.

### 2.4 Skills — cách dạy agent hiện nay

- Nguồn: `skills/` bundled → `~/.claude/skills` → `~/.sema/skills` → managed (sau đè trước, `src/skills/scan.rs:45,179`). Skill của Space App được cài lúc register (`space.rs:2541`, nhãn `app:<id>`).
- Frontmatter parse thật: `name/description/triggers/allowed-tools/when-to-use/use-mode/…` (`src/skills/metadata.rs:67`). **`mcp_servers:` trong frontmatter KHÔNG được parse** (vd `skills/note/SKILL.md:18` — chỉ là prose). Skill trỏ tool qua `allowed-tools` → `SkillTool` inject thành chỉ dẫn văn bản + nudge ToolSearch (`src/tools/skill.rs:275-288`).
- ToolSearch xếp hạng skill: name +100, triggers +40, when-to-use +25, description +10 (`src/tools/tool_search.rs:246`).
- Hot-reload **chỉ** watch `managed_skills_dir` (`pool.rs:3010`); sửa `skills/` bundled cần restart daemon.
- `pre_trigger_skill` mặc định **false** (`group_manager/llm.rs:50-55`) → trigger keyword chỉ là *hint* mềm trong message, model vẫn tự chọn.

### 2.5 Settings — pattern chuẩn để theo

Pattern thống trị = **`~/.senclaw/config.json`** (`GlobalConfig`, `src/gateway/group_manager/types.rs:173`, mọi field `Option + skip_serializing_if` → round-trip an toàn):

1. Thêm field vào `GlobalConfig`.
2. Getter/setter 4 dòng read-modify-write cạnh `save_ocr_settings`/`save_tts_settings` (`group_manager/llm.rs:228-254`).
3. Handler module mới trong `src/gateway/ui_server/` (mẫu: `agent_behavior_config.rs:45/63` partial-update, hoặc `ocr.rs` whole-blob PUT).
4. Route `GET/PUT` trong `core.rs`.
5. Nếu daemon cần đọc lúc boot → nối `apply_persisted_overrides` (`src/config.rs:673`) — comment ở đó cảnh báo đúng cái bẫy "Settings page ghi file mà daemon không bao giờ đọc".

Không dùng SQLite cho user settings (không có precedent; `router_state` không phải settings store).

### 2.6 Bốn luồng mặc định — hành vi hôm nay và các seam

| Luồng | Hôm nay | Seam (chỗ cắm quyết định) |
|---|---|---|
| **Mở link** | Web: hardcode `target="_blank"` (`MessageBubble.tsx:133`, `MarkdownBody.tsx:146`). Desktop: shift+click → `openExternal` (url_launcher); **click thường chưa nối gì** — `onLinkTap` không được truyền (`message_widgets.dart:123`). Daemon `POST /api/ui/open-url` → cứng OS browser (`open_url.rs:27,60-84`). Space App: `openExternal.ts` → JS handler → system browser. | `open_url.rs:27` (server-side duy nhất); `MessageBubble.tsx:133` + `MarkdownBody.tsx:146` (web); `app_markdown.dart:30` + `onLinkTap` sẵn-mà-chưa-wire (desktop); `openExternal.ts` (app). Lựa chọn thay thế trong-SenClaw: app `mini-browser` (4360, CDP screencast, MCP `mini-browser-mcp`). |
| **Media** | Chỉ ảnh (attachment). **Không có kind `audio`**, không có audio renderer — audio duy nhất là TTS pipeline bấm tay (`ttsPipeline.ts:96`; desktop `audioplayers`). Video **chỉ là widget kind** `video`: web `<video>` thật; desktop *không có player native* — mượn embedded webview (`widget_card.dart:735-830`, `instanceKey: 'chat-video'`). | Thêm kind vào `emit_widget.rs:21` + 3 bộ fence + 3 client. Chưa có bất kỳ khái niệm playlist/queue/seek. |
| **Search** | Không có native `web_search`. `browser_search` (senclaw-browser, engine mặc định **hardcode "google"** — `browser_server.rs:307-309`, URL build trong Chrome ext `SearchEngine.ts:51-61`). App `search` (4530, `search-mcp`, autoRegister, federated RRF) **có đăng ký nhưng thua** vì `skills/agent-browser/SKILL.md:7-13` giữ trigger `tìm`, `tìm kiếm`, `search`… và dạy `browser_search`; không skill nào surface `search_query`. | Roster builtin hardcode `pool.rs:1039-1133`; engine default `browser_server.rs:307`; trigger collision giữa `skills/agent-browser` và `skills/web-research`. |
| **Note** | 4 kho không có router: (1) **space notes** = default de-facto (`space_note_create` MCP `space_server.rs:364`, REST `space.rs:253`) qua 2 skill giẫm trigger nhau (`skills/note` + `skills/space` đều ăn `ghi chú`, `note`, `ý tưởng`…); (2) quicknotes — file `.md`, UI-only, **agent không gọi được**; (3) `wiki_write`; (4) `memory_save`. Bảng routing duy nhất là prose cuối `skills/note/SKILL.md`. | Prompt-level: skill + system prompt. Không có config nào. |

**Khái niệm "preferences/defaults" chưa tồn tại trong daemon** (`default_app` = 0 hit). Pattern gần nhất để copy: `pre_trigger_skill: Option<bool>` — GlobalConfig field → getter/setter → đọc lúc tạo agent (`pool.rs:1439-1440`) → đẩy vào engine (`agent_pool/engine.rs:466-468` → `zen_core/engine.rs:2116-2118`).

---

## 3. Thiết kế

### 3.0 Nguyên tắc

1. **Hợp nhất, không phát minh thêm hệ thứ ba.** Một `WidgetRegistry` phục vụ cả chat lẫn dashboard; manifest `widgets[]` sẵn có là *file định nghĩa widget trong app* — chỉ mở rộng field, không đổi format (manifest là `Value` untyped nên field mới tự round-trip).
2. **Hai loại render**: `template` (client render native từ data — chart/clock/… hiện tại) và `url` (iframe/webview từ `entryUrl` — dashboard widget hiện tại). App widget mặc định là `url`; built-in là `template`.
3. **Widget là display + điều hướng**, không phải form. Vòng phản hồi (action → agent) để pha cuối; FormUI vẫn là đường round-trip.
4. **Mọi quyết định "default" đều đọc từ một chỗ**: section `defaults` trong `~/.senclaw/config.json`, sửa qua Plugins → Widget.
5. **Kênh text-only không bao giờ bị bỏ rơi im lặng nữa** — mọi widget có text fallback.

### 3.1 Định nghĩa widget trong app — mở rộng manifest `widgets[]`

```jsonc
// apps/<app>/senclaw-manifest.json — mục widgets[] (fields mới đánh dấu +)
{
  "widgets": [
    {
      "id": "pipeline-mini",                    // đã có; id toàn cục = "<app_id>.<id>"
      "name": "Phễu bán hàng (mini)",           // đã có
      "description": "Hiển thị phễu deal theo giai đoạn, có tổng giá trị", // đã có — AI đọc để chọn
      "entryUrl": "/widget/pipeline.html",      // đã có (kind=url)
      "size": "medium",                         // đã có: small|medium (+ "large", "tall")
      "refreshMs": 30000,                       // đã có
      "render": "client",                       // đã có
   +  "surfaces": ["dashboard", "chat"],        // MẶC ĐỊNH ["dashboard"] → 14 widget cũ giữ nguyên hành vi
   +  "params": {                               // JSON Schema — agent điền khi emit vào chat
        "type": "object",
        "properties": { "stage": { "type": "string", "description": "Lọc theo giai đoạn" } }
      },
   +  "textFallback": "Phễu bán hàng giai đoạn {stage} — mở CRM để xem chi tiết",  // {param} thay thế phẳng; param thiếu → chuỗi rỗng
   +  "intents": ["media"]                      // tùy chọn: widget ứng cử làm handler cho luồng mặc định
    }
  ]
}
```

- **Không cần file mới**: `widgets[]` trong manifest *chính là* "file widget định nghĩa trong app". App nào khai dài có thể tách `"widgets": { "$file": "senclaw-widgets.json" }` — loader hỗ trợ nhưng không khuyến khích (thêm 1 đường code, 0 lợi ích khi manifest untyped).
- `params` interpolate vào `entryUrl` **chỉ dưới dạng query-string encode** (`?stage=...`), không bao giờ đổi path/origin — cùng triết lý `sanitize_event_link`.
- 14 widget hiện có không khai `surfaces` → mặc định `["dashboard"]`, không đổi hành vi.

### 3.2 WidgetRegistry (daemon) — `src/widgets/`

Module mới `src/widgets/{mod.rs, registry.rs, defaults.rs}`:

- **Nguồn catalog** (đọc lười, cache, invalidate khi app register/enable/disable):
  1. **Built-in**: 6 template kind (`chart`, `image`, `clock`, `weather`, `video`, + mới `audio`) — mô tả + data-schema chuyển từ `DESCRIPTION` hardcode vào bảng registry (một chỗ, hết cảnh sửa 6 nơi phía backend).
  2. **Space App**: quét `space_apps WHERE enabled=1`, đọc `manifest.widgets[]` (cùng chỗ `autoregister_installed` đã quét — `space_mcp.rs:186`).
  3. **Plugin** (pha P3): `<pluginDir>/widgets/widgets.json` — capability thư mục thứ 5, xem 3.7.
- **State** (enable/disable từng widget, defaults): **không thêm bảng DB** — section `widgets` + `defaults` trong `GlobalConfig` (pattern 2.5). Registry đọc config qua GroupManager giống `pre_trigger_skill`.
- **REST** (handler mới `src/gateway/ui_server/widgets.rs`):
  - `GET /api/widgets` — catalog đầy đủ: `{id, source: builtin|app:<id>|plugin:<name>, kind: template|url, name, description, surfaces, params, enabled, entry}` (entry đã resolve baseUrl như `SpacePage.tsx:57-69`: direct `runtime.url` khi có, fallback proxy).
  - `PUT /api/widgets/:id` — `{enabled}`.
  - `GET/PUT /api/defaults` — xem 3.6.
- Web/desktop client fetch `GET /api/widgets` một lần (cache theo session) để render widget `kind=url` từ fence/history mà không cần daemon nhúng URL vào từng row.

### 3.3 Đưa widget app vào chat — mở rộng `emit_widget`, thêm `widget_list`

**Giữ một tool emit duy nhất** (agent đã quen, description đã ship mọi prompt):

```jsonc
// emit_widget — thêm kind thứ 7: "app"
{ "kind": "app", "widget": "crm.pipeline-mini", "params": { "stage": "won" }, "title": "Phễu Q3", "chat_jid": "..." }
```

- `parse_spec` (`emit_widget.rs:47`) thêm nhánh `kind=="app"`: tra registry theo `widget` id → không tồn tại/không enable/không có surface `chat` → lỗi tool rõ ràng (agent thấy ngay, không silent). Validate `params` sơ bộ theo JSON Schema đã khai (required + type — dùng validator nhẹ, không kéo crate nặng).
- `WidgetSpec` phát ra giữ nguyên shape `{kind:"app", title?, data}` với `data = {app_id, widget_id, params, entry, height, refreshMs, textFallback đã render}` — client cũ gặp kind lạ đã có sẵn nhánh `ErrorChip "Unknown widget kind"` → **degrade an toàn** trên client chưa update.
- **Tool mới `widget_list`** (native, `should_defer() = true` — không phình prompt; skill + ToolSearch dẫn tới): trả catalog rút gọn `{id, name, description, params}` của widget `surfaces⊇chat` đang enabled. Đây là cách agent "khám phá" widget của app mới cài mà không cần sửa prompt tĩnh nào.
- **Fence fallback đồng bộ hóa** (sửa luôn lệch chuẩn hiện tại): cả 3 bộ fence set thêm `image` (đang thiếu) và nhận ```` ```widget ```` với `kind:"app"`; client resolve entry qua catalog cache 3.2.
- Cập nhật `DESCRIPTION` của `emit_widget`: thêm kind `app` + `audio`, câu "gọi `widget_list` để biết widget app khả dụng", và sửa doc comment 4-kind cũ (`zen_core/mod.rs:774`, `db/schema.rs:195`). Tạo `WIDGET_CONTRACT.md` thật (đang là dead reference ở 3 chỗ) hoặc gỡ các trích dẫn.

**Render phía client:**

- **Web**: `WidgetCard.tsx` case `app` → component `AppWidgetFrame` — iframe giống `Dashboard.tsx:60-108` (đã có sẵn logic baseUrl/sandbox), `sandbox` như `SpaceAppFrame.tsx:142-155`, chiều cao theo `size`.
- **Desktop**: case `app` → `embeddedWebView(entry, instanceKey: 'chat-widget-<id>')` — đúng precedent `_VideoBody` (`widget_card.dart:802-806`). Lưu ý mỗi instanceKey là một webview; giới hạn đồng thời (ví dụ chỉ mount khi visible trong viewport, còn lại poster + nút "Mở") để không mở N webview cho một lịch sử chat dài.
- **channel_app**: không nhúng iframe — render card `textFallback` + nút deep-link mở app (`/space/app/<id>?...`).

### 3.4 Text fallback cho kênh nhắn tin — vá luôn lỗ hổng silent-drop

Tại bridge `lib.rs:2383-2421` (chỗ duy nhất mọi widget đi qua):

1. Nếu `chat_jid` thuộc kênh text-only (Telegram/QQ/Feishu/WeChat/Zalo… — nhận diện qua GroupManager/channel prefix, cùng cách router phân loại jid):
   - Render text fallback: widget app → `textFallback` (template `{param}`); built-in → dòng tóm tắt sinh sẵn (`chart` → "Biểu đồ <title>: <n> series"; `clock/weather` → giá trị chính; `video/audio/image` → title + URL).
   - Gửi qua `Channel::send_message` như một tin nhắn thường (route sẵn có của sendReply).
2. `result_for_assistant` của `emit_widget` nói thật: *"Widget chỉ hiển thị trên Web/Desktop UI; đã gửi bản text fallback tới kênh <x>"* — chấm dứt việc agent tưởng đã render được chart trên Telegram.

Điều này áp dụng cho **cả 5 kind cũ** (sửa bug hiện hữu), không riêng widget app.

### 3.5 Skill hướng dẫn — `skills/widget/SKILL.md` (bundled)

- Frontmatter: `name: widget`, `triggers: [widget, biểu đồ, chart, đồ thị, hiển thị, dashboard, phát video, phát nhạc, xem ảnh, đồng hồ, thời tiết]`, `allowed-tools: [emit_widget, widget_list]`, `when-to-use` rõ ("khi câu trả lời hiển thị trực quan tốt hơn văn bản").
- Nội dung: (1) bảng 7 kind + shape `data` từng loại (nguồn chân lý chuyển từ `DESCRIPTION` sang đây, DESCRIPTION giữ bản ngắn); (2) quy trình dùng widget app: `widget_list` → chọn theo `description` → `emit_widget kind=app` với `params` đúng schema; (3) quy tắc: kênh text-only sẽ nhận fallback — đừng emit widget nặng cho Telegram; (4) tôn trọng `defaults` (mục 3.6) khi người dùng nói "phát", "mở", "tìm", "ghi chú".
- App tự dạy widget của mình trong skill của app (precedent: `apps/drawio/skills/drawio-generate/SKILL.md:40` đã dạy `emit_widget kind=image`) — `install_app_skills` sẵn có lo phần cài.
- Nhớ: sửa `skills/` bundled **cần restart daemon** (hot-reload chỉ watch managed dir).

### 3.6 Cài đặt luồng mặc định — section `defaults` trong config.json

```jsonc
// ~/.senclaw/config.json
{
  "defaults": {
    "openLink":  "system-browser",   // system-browser | mini-browser | new-tab
    "media":     "inline-widget",    // inline-widget | mini-browser | system-browser
    "search":    "browser",          // browser | search-app ; kèm "searchEngine": "google|bing"
    "note":      "space-notes",      // space-notes | wiki | memory
    "widgets":   { "disabled": ["clock.analog"] }   // enable/disable từ 3.2
  }
}
```

Triển khai đúng pattern 2.5: field `defaults: Option<DefaultsConfig>` trong `GlobalConfig` (`types.rs:173`) → getter/setter cạnh `llm.rs:228-254` → handler `src/gateway/ui_server/defaults_config.rs` → route `GET/PUT /api/defaults` (`core.rs` cạnh `:293`) → **nối `apply_persisted_overrides`** cho phần daemon cần lúc boot (search engine).

**Cách mỗi default được TIÊU THỤ** (điểm mấu chốt — config mà không ai đọc là config chết):

| Default | Cơ chế tiêu thụ | Chỗ sửa |
|---|---|---|
| `openLink` | **(a) Server**: `open_url_handler` branch — `system-browser` giữ nguyên; `mini-browser` → broadcast WS `ui:open {route:"/space/app/mini-browser?url=<enc>"}` cho UI client (không có client online → fallback OS browser). **(b) Web**: renderer `a` trong `MessageBubble.tsx:133` + `MarkdownBody.tsx:146` đọc default (context từ `GET /api/defaults`): `new-tab` giữ `_blank`; `mini-browser` → SPA navigate `/space/app/mini-browser?url=…`; `system-browser` → `POST /api/ui/open-url`. **(c) Desktop**: truyền `onLinkTap` tại `message_widgets.dart:123` (hook có sẵn, đang bỏ trống) → theo default: `openExternal` hoặc `RunningAppsController.openAt(mini-browser, query)` (precedent `event_link.dart:40-59`). | `open_url.rs:27`; `MessageBubble.tsx:133`; `MarkdownBody.tsx:146`; `message_widgets.dart:123`; `app_markdown.dart:74-85` |
| `media` | Thêm kind **`audio`** built-in (web `<audio>`; desktop `audioplayers` đã có dep — dựng `_AudioBody` native thay vì webview). Skill widget dạy: "được yêu cầu phát media → `emit_widget` `video`/`audio`" khi default = `inline-widget`; default khác → trả link + `open_url`. | `emit_widget.rs:21` + 3 client + 3 fence set; `skills/widget` |
| `search` | **(a)** Engine: `default_search_engine2()` (`browser_server.rs:307-309`) đọc từ config thay vì hardcode `"google"`. **(b)** Chọn tool: **inject 1 dòng vào system prompt lúc tạo agent** — cùng đường ống `pre_trigger_skill` (`pool.rs:1439` → `agent_pool/engine.rs:466` → `zen_core/engine.rs:2116`): `Default search: dùng mcp__search-mcp__search_query (app Search)` khi default = `search-app` **và app đang healthy** (check registry; app chết → không inject, khỏi dụ agent gọi tool chết). **(c)** Hạ nhiệt trigger collision: `skills/agent-browser` thêm câu "nếu Defaults chỉ định search-app thì dùng search_query". | `browser_server.rs:307`; `pool.rs:1439` vùng; `skills/agent-browser/SKILL.md` |
| `note` | Cùng cơ chế inject: `Default note store: space-notes` (hoặc wiki/memory) vào system prompt; `skills/note` + `skills/space` thêm câu tôn trọng default (bảng routing cuối `skills/note` thành bảng theo-default). Không đổi tool nào — cả 4 kho đã có tool, chỉ thiếu kim chỉ nam. | prompt inject + 2 SKILL.md |

Đoạn inject gộp thành một block nhỏ `## User defaults` (2–4 dòng) sinh từ `DefaultsConfig` — rẻ, deterministic, một chỗ.

### 3.7 Bộ quản lý widget — trang **Plugins → mục "Widget"**

Theo yêu cầu bổ sung ("thêm quản lý widget vào trong plugins → widget"):

- Nav: thêm `'widgets'` vào `PluginsNavItem` (`PluginsSidebar.tsx:25`), `NAV_ITEMS` (`PluginsPage.tsx:7`), `NAV_LABEL` + switch (`PluginsView.tsx:21,41-49`), entry `<StaticNavItem>` trong sidebar (`PluginsSidebar.tsx:353-430`, đặt trên "Marketplace", nhãn "Widget"). Deep-link `?nav=widgets` tự chạy.
- Panel mới `web/src/components/plugins/WidgetsPanel.tsx`, 2 phần:
  1. **Danh mục widget** — bảng antd (fetch `GET /api/widgets`): cột Tên/Mô tả · Nguồn (`builtin` | `app:<id>` | `plugin:<name>`) · Surface (chat/dashboard) · Toggle enabled (`PUT /api/widgets/:id`) · nút Preview (mở popover render thử `WidgetCard` với data mẫu / iframe entry).
  2. **Luồng mặc định** — 4 dropdown (Mở link · Media · Search · Note) + select engine, đọc/ghi `GET/PUT /api/defaults`, pattern fetch thô như `AgentBehaviorSettings.tsx:40-59`. Option `mini-browser`/`search-app` chỉ hiện khi app tương ứng đã cài & enabled (đọc từ `/api/space/apps`).
- Desktop Flutter: **đã làm cùng ngày** — `desktop_app/lib/features/plugins/widgets_panel.dart`
  (`WidgetsManagePanel`), mục "Widget" trong rail PluginsScreen (sau Space Apps),
  cùng REST; lưu default xong tự `ChatLinkFlow.prefetch(force)` nên hành vi click
  link đổi ngay không cần restart app.

### 3.8 Widget từ plugin (P3) — capability đầu tiên được nối thật

- Quy ước: `<pluginDir>/widgets/widgets.json` (mảng cùng schema 3.1) + file tĩnh cạnh đó; discovery thêm `discover_widgets` cạnh `manager.rs:965-1030`.
- Serve tĩnh: route mới `GET /api/plugins/:name/widget-static/*path` — copy hàng rào của `space_apps_static` (`space.rs:1457-1489`): canonicalize + containment, chặn `..`/backslash. Plugin **không có server riêng** nên chỉ hỗ trợ widget `url` tĩnh hoặc `template`.
- Registry đọc qua **instance shared** `Arc<Mutex<MarketplaceManager>>` (không phải bản Arc riêng của AgentPool — bẫy 2-instance ở 2.3); enable plugin → invalidate catalog cache.
- Nhờ security scan gate lúc install (đã có) — widget plugin là HTML/JS bên thứ ba chạy trong iframe sandbox, cùng mức tin cậy app UI.

### 3.9 An ninh & giới hạn

1. **Interpolation `params` → URL**: chỉ query-encode vào `entryUrl` gốc từ manifest; từ chối param chứa `://`, path traversal vô nghĩa vì không ghép path. Deep-link từ fallback/nút mở chỉ dạng `/space/app/<id>[?…]` — tái dùng `sanitize_event_link`.
2. **Iframe sandbox** như SpaceAppFrame; widget chat *không* nhận bridge `senclaw:request` ở P0–P2 (chỉ hiển thị + `senclaw:init` theme/env) — thu nhỏ bề mặt so với app pane đầy đủ.
3. **Action từ widget → agent (P4)**: qua postMessage → host → **hiển thị như tin nhắn người dùng** trong chat (thấy được, audit được), không bao giờ auto-execute tool. Đây là ranh giới chống prompt-injection từ nội dung web bên trong widget.
4. **Bẫy `allowed_tools`**: `widget_list` (tool mới) phải vào danh sách approved_tools, không thì group nào bật whitelist sẽ mất tool (memory: senclaw-allowed-tools-trap).
5. **Proxy không tunnel WS**: widget live-data qua proxy phải dùng `refreshMs` polling; direct URL (`runtime.url`) mới có WS.
6. **Webview desktop đắt**: giới hạn số webview mount đồng thời trong chat (lazy mount + poster), như đã ghi ở 3.3.
7. **Kind `app` trên client cũ**: rơi vào nhánh `Unknown widget kind` sẵn có — degrade thành chip lỗi, không crash.

### 3.10 Lộ trình build

| Pha | Nội dung | Đầu ra kiểm chứng |
|---|---|---|
| **P0 — Registry + emit app-widget** | `src/widgets/` registry (builtin + app manifest); mở rộng `emit_widget` kind `app` + validate params; `widget_list` (deferred); text-fallback kênh text-only (3.4, vá luôn 5 kind cũ); đồng bộ 3 bộ fence (+`image`, +`app`); `GET /api/widgets` | cargo test: registry resolve/enable/params-validate/fallback; test hiện có của emit_widget vẫn xanh |
| **P1 — Render + quản lý** | Web `AppWidgetFrame` trong WidgetCard + catalog cache; Plugins → Widget panel (danh mục + toggle); desktop case `app` qua embeddedWebView lazy-mount; channel_app card fallback | `npm run build:web`; flutter analyze; emit thử `crm.pipeline-mini` vào chat web thấy iframe, Telegram thấy text |
| **P2 — Defaults/luồng mặc định** | `DefaultsConfig` + REST `GET/PUT /api/defaults` + panel Mặc định; kind `audio`; open-link wiring 4 seam (gồm nối `onLinkTap` desktop đang bỏ trống); search engine từ config; prompt-inject `## User defaults`; cập nhật `skills/agent-browser`/`note`/`space`; `skills/widget` mới | đổi default search → search-app rồi hỏi "tìm X" thấy `search_query` được gọi; click link desktop mở đúng nơi theo default |
| **P3 — Plugin widgets** | `discover_widgets` + widgets.json + route static + nguồn `plugin:` trong registry/panel | cài plugin mẫu có widget → hiện trong panel, emit được vào chat |
| **P4 — Widget actions** | postMessage action → host → tin nhắn user trong chat; mở rộng bridge có kiểm soát | bấm nút trong widget → thấy message + agent phản hồi |

Việc nhỏ kèm theo (dọn nợ phát hiện khi khảo sát): tạo/gỡ `WIDGET_CONTRACT.md` dead-ref; sửa doc comment "4 kinds"; cân nhắc di trú dần Dashboard sang đọc `GET /api/widgets` thay vì tự parse manifest (một nguồn chân lý).

---

## 4. Trả lời trực tiếp 4 câu hỏi default

| Câu hỏi | Hiện trạng thực tế | Đề xuất default ban đầu | Các lựa chọn trong panel |
|---|---|---|---|
| **Mở link default ở đâu?** | Web: tab mới; Desktop: click thường không làm gì (bug), shift+click = system browser; agent/app backend: system browser | `system-browser` (giữ hành vi ít bất ngờ nhất; sửa bug desktop click) | `system-browser` · `mini-browser` (in-SenClaw) · `new-tab` (web) |
| **Media default là gì?** | Không có player audio; video chỉ qua widget kind `video` | `inline-widget` (thêm kind `audio`; video/audio phát ngay trong chat) | `inline-widget` · `mini-browser` · `system-browser` |
| **Search default dùng gì?** | `browser_search` engine hardcode Google, thắng nhờ trigger skill; `search-mcp` bị bỏ quên dù đã đăng ký | `browser` + engine `google` (khi chưa cài app Search); người dùng đã cài app Search nên chọn `search-app` để được federated + RRF | `browser` (+engine google/bing) · `search-app` |
| **Note default?** | space-notes de-facto qua 2 skill giẫm trigger; quicknotes agent không gọi được; wiki/memory tùy hứng model | `space-notes` (chuẩn hóa hiện trạng, có FTS + UI đủ 3 client) | `space-notes` · `wiki` · `memory` |

---

## 5. Liên quan

- `docs/space-app-open-external.md` — 3 lớp mở link ra ngoài (nền của `openLink` default)
- `docs/tool-skill-name-lookup.md` — quy ước tên MCP/kill khi viết `skills/widget`
- Memory: `chat-widgets-feature`, `formui-port` (round-trip precedent), `hub-store-marketplace`, `senclaw-allowed-tools-trap`, `study-app` (space_events.link), `search-app`, `mini-browser-app`
