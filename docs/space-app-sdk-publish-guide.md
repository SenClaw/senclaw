# Xây Space App bằng `app-space-sdk` trong repo riêng & publish lên senclaw.bacnd.com

Tài liệu này hướng dẫn trọn vòng đời một Space App **nằm ngoài monorepo SenClaw**:

1. Tạo repo riêng cho app, phụ thuộc SDK `app-space-sdk`.
2. **Toàn bộ API daemon cung cấp cho app** (LLM, agent, knowledge, MCP, config, sqlite, calendar, wiki…) — kèm app thật nào đang dùng gì.
3. Build + đóng gói thành `<id>-app.zip` đúng chuẩn daemon cài được.
4. Publish lên hub **https://senclaw.bacnd.com/** bằng `senclaw hub publish`.
5. Phát hành bản cập nhật (bump version) và cách người dùng nhận update.

> Đối chiếu code khi cần: bridge + REST cho app [`src/gateway/ui_server/space.rs`](../src/gateway/ui_server/space.rs),
> spawn app + MCP [`src/gateway/ui_server/space_mcp.rs`](../src/gateway/ui_server/space_mcp.rs),
> CLI [`src/cli/commands/hub.rs`](../src/cli/commands/hub.rs),
> client publish [`src/marketplace/publish.rs`](../src/marketplace/publish.rs),
> registry/install [`src/marketplace/registry.rs`](../src/marketplace/registry.rs),
> kiểm tra update [`src/marketplace/app_update.rs`](../src/marketplace/app_update.rs),
> SDK [`app-space-sdk/src/`](../app-space-sdk/src/),
> hợp đồng widget [`WIDGET_CONTRACT.md`](../WIDGET_CONTRACT.md).

## 0. Bức tranh chung

```
repo riêng (my-app)                    hub (senclaw.bacnd.com)          máy người dùng
┌──────────────────────┐   publish    ┌───────────────────────┐  cài/update  ┌──────────────────┐
│ src/  web/  skills/  │ ───────────► │ /api/v1/publish       │ ───────────► │ daemon SenClaw   │
│ senclaw-manifest.json│  (CLI hub)   │ /api/v1/packages/...  │  (CLI hub /  │ /api/space/apps/*│
│ senclaw-hub.json     │              │ /dl/... (artifact)    │   badge web) │  chạy app :port  │
│ scripts/pack.sh ─►zip│              └───────────────────────┘              └──────────────────┘
└──────────────────────┘
```

- **App = một HTTP server độc lập** (binary Rust) + UI tĩnh + manifest. Daemon tải zip về, giải nén, chạy `runtime.start`, health-check `runtime.healthPath`, nhúng UI qua iframe, đăng ký MCP server của app.
- **Mọi dịch vụ AI đi qua daemon** (bridge `llm.request`, `agent.run`, `knowledge.*`) — app **không bao giờ** cầm API key của provider.
- **Hub** là registry bất biến: mỗi `name@version` publish rồi là không đổi được; update = publish version mới.

Hai hostname `senclaw.bacnd.com` (chính thức) và `hub-store.bacnd.com` (tên cũ, giữ cho link cũ khỏi 404) **là cùng một server**. Hằng `DEFAULT_HUB_URL` đã là `https://senclaw.bacnd.com` (đổi 2026-08-05; source đã seed bằng tên cũ được daemon tự migrate khi khởi động — xem `MarketplaceManager::migrate_legacy_hub_url`). Tài liệu này vẫn truyền tường minh `--hub https://senclaw.bacnd.com` cho chắc trên các bản CLI cũ.

## 1. Chuẩn bị

| Thứ cần có | Ghi chú |
|---|---|
| Binary `senclaw` trên PATH | Từ bản cài Desktop (Resources) hoặc `cargo build --release` trong repo SenClaw |
| Rust ≥ 1.85 | `app-space-sdk` dùng edition 2024 |
| Node ≥ 18 | Build web UI (Vite) |
| Tài khoản trên https://senclaw.bacnd.com | Đăng nhập tại `/login`, **phải đặt username (handle)** — chưa có handle thì publish bị 403 `no_handle` |
| Publish token `snc_pat_…` | Tạo tại https://senclaw.bacnd.com/settings/tokens, chọn scope **publish** |
| Daemon SenClaw đang chạy (để test cài) | UI server mặc định `http://127.0.0.1:18788` (`SENCLAW_UI_PORT` nếu khác) |

Lưu token vào máy (đọc từ stdin, không bao giờ nằm trên command line / shell history):

```bash
senclaw hub login
```

Token lưu ở `~/.senclaw/hub-token` (chmod 600). CI thì dùng biến môi trường `SENCLAW_HUB_TOKEN` thay cho file. Kiểm tra:

```bash
senclaw hub whoami
```

## 2. Tạo repo riêng

```bash
mkdir my-app && cd my-app && git init
```

Cấu trúc chuẩn (bám theo layout các app trong `apps/*` của monorepo — `apps/mindmap` là template chuẩn để đối chiếu):

```
my-app/
├── Cargo.toml
├── src/
│   ├── main.rs              # axum server: REST + MCP + serve web_dist
│   └── mcp.rs               # tools/list + tools/call
├── web/                     # React + Vite (UI nhúng iframe)
├── skills/
│   └── my-app-manager/SKILL.md
├── personas/
│   └── my-app-keeper.md
├── senclaw-manifest.json    # manifest RUNTIME — daemon đọc khi cài/chạy
├── senclaw-hub.json         # metadata HUB — sinh bằng `senclaw hub init`
├── scripts/pack.sh          # build + đóng zip
├── README.md                # được upload làm trang giới thiệu gói trên hub
└── .gitignore
```

`.gitignore` tối thiểu:

```
/target
/release
*.zip
web/node_modules
web/dist
```

Tạo repo GitHub riêng và push (URL này sau sẽ khai vào `senclaw-hub.json` → `repo_url` để hiện trên trang gói):

```bash
gh repo create <bạn>/my-app --private --source . --push
```

### 2.1 Khai báo dependency `app-space-sdk`

SDK sống trong repo SenClaw ([`app-space-sdk/`](../app-space-sdk/)) và là workspace member, nên repo ngoài có 2 cách trỏ tới:

**Cách A — git dependency (khuyến nghị cho repo độc lập):**

```toml
[package]
name = "my-app"
version = "0.1.0"
edition = "2021"

[dependencies]
app-space-sdk = { git = "https://github.com/NortonBen/SenClaw.git", branch = "main" }
tokio = { version = "1", features = ["full"] }
axum = { version = "0.7", features = ["json"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
rusqlite = { version = "0.32", features = ["bundled"] }
anyhow = "1"
```

Cargo tự tìm package `app-space-sdk` trong workspace của repo git. Repo private thì máy build cần quyền truy cập (SSH agent / `gh auth`). Nhược điểm: lần đầu clone cả repo SenClaw (to); nên **pin commit** bằng `rev = "<sha>"` để build tái lập được.

**Cách B — path dependency (khi có clone SenClaw cạnh bên, tiện dev):**

```toml
app-space-sdk = { path = "../SemaClaw/app-space-sdk" }
```

Dev hằng ngày dùng B cho nhanh, trước khi publish đổi sang A (hoặc giữ B và chấp nhận yêu cầu clone cạnh bên — miễn `pack.sh` build được).

### 2.2 `src/main.rs` — khung tối thiểu

Quy tắc **bắt buộc** (xem mục "Space App network binding" trong [CLAUDE.md](../CLAUDE.md)): app không có auth riêng, ranh giới tin cậy là loopback — **không bao giờ** hardcode `0.0.0.0`:

```rust
use axum::{routing::get, Router};

#[tokio::main]
async fn main() {
    // Daemon gán PORT khi spawn app — LUÔN ưu tiên nó, fallback về port manifest.
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(4800);

    let app = Router::new()
        .route("/api/status", get(|| async { axum::Json(serde_json::json!({ "ok": true })) }))
        // .route("/api/mcp/sse", ...)      // MCP endpoint — xem mục 3.4
        // .route("/api/mcp/message", ...)  // JSON-RPC POST sibling
        // .nest_service("/", ServeDir::new(web_dist))  // UI tĩnh cạnh binary
        ;

    // Loopback mặc định. SENCLAW_BIND_HOST=0.0.0.0 là opt-in tường minh.
    let host = std::env::var("SENCLAW_BIND_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}")).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

- **`/api/status`** phải trả 200 — daemon health-check qua `runtime.healthPath`.
- **UI tĩnh**: serve thư mục `web_dist/` nằm **cạnh binary** (trong zip nó nằm cùng cấp), đừng trỏ đường dẫn tuyệt đối lúc dev.
- **Port**: chọn cố định một port chưa app nào dùng (các app hiện có chiếm dải 4310–4760; daemon dùng 18788/18789). Khai đúng port đó trong manifest.

**Biến môi trường daemon inject khi spawn app** ([`space_mcp.rs`](../src/gateway/ui_server/space_mcp.rs)):

| Env | Giá trị | Dùng để |
|---|---|---|
| `PORT` | port daemon gán cho app | bind server (ưu tiên trên port manifest) |
| `SENCLAW_BASE_URL` | `http://127.0.0.1:18788` | gọi ngược daemon (bridge/REST) |
| `SENCLAW_SPACE_APP_ID` | id của app | xây URL bridge `/api/space/apps/{id}/…` |
| `SENCLAW_BIND_HOST` | *không inject* — đọc từ env user | mặc định `127.0.0.1` |

### 2.3 `senclaw-manifest.json`

Manifest **runtime** — daemon đọc nó để cài, chạy, nhúng UI, đăng ký MCP/skill/persona. Trường `id` là định danh toàn cục: nó thành **tên gói trên hub** (`<scope>/<id>`) và tên thư mục cài. Mẫu đầy đủ:

```json
{
  "id": "my-app",
  "name": "My App",
  "description": "Mô tả ngắn gọn, BẮT BUỘC — hub từ chối gói không có description.",
  "icon": "🧩",
  "runtime": {
    "kind": "server",
    "mode": "session",
    "start": "./my-app",
    "healthPath": "/api/status",
    "port": 4800
  },
  "requires": { "bin": ["ffmpeg"] },
  "sandbox": { "enabled": true, "network": "hosts", "hosts": ["api.example.com"] },
  "integration": { "type": "iframe", "url": "/" },
  "bridge": {
    "postMessage": true,
    "capabilities": ["space.rest", "llm.request"]
  },
  "mcp": {
    "name": "my-app-mcp",
    "transport": "http",
    "path": "/api/mcp/sse",
    "description": "Mô tả để agent biết chọn tool — liệt kê nhóm tool chính.",
    "autoRegister": true
  },
  "skills": [
    { "name": "my-app-manager", "path": "skills/my-app-manager", "triggers": ["từ khoá 1", "keyword 2"] }
  ],
  "personas": [
    { "name": "my-app-keeper", "path": "personas/my-app-keeper.md", "description": "…" }
  ]
}
```

Lưu ý:

- `runtime.start` là đường dẫn **tương đối trong zip** (`./my-app` = binary ở gốc zip).
- `runtime.mode` — **`session` là mặc định**: daemon khởi động app khi người dùng
  mở nó hoặc khi agent gọi một tool MCP, rồi dừng lại sau `idleTimeoutSecs` giây
  rảnh (60s). Chỉ khai `background` khi app **tự làm việc** lúc không ai dùng
  (poll kênh chat, chạy lịch, giữ WebSocket cho extension) — app `background`
  chạy suốt đời daemon. Viết sai giá trị **không báo lỗi ở đâu**, nó rơi về
  `session`.
- `requires` — máy phải có gì (`node` / `python` với range version, `bin`, `env`,
  `os`). Kiểm lúc cài **và** trước mỗi lần chạy; thiếu thứ bắt buộc thì app
  không chạy và người dùng đọc được lý do, thay vì `exit 127` trong log.
- `sandbox` — mức giam app tự khai, áp dụng ngay lúc cài. `"force": true` khiến
  người dùng không tắt được sandbox của app.
- `runtime.runner` (`binary` | `node` | `python` | `shell`, đoán được từ `start`
  thì bỏ) — app Node/Python được cài phụ thuộc một lần sau cài/update; Python
  thêm `.venv` riêng trong thư mục app. **`binary` và `shell` không có bước
  install nào cả**: `runtime.install` khai ở đó bị bỏ qua im lặng, nên app Go
  hoặc ship binary đã build, hoặc biên dịch ngay trong `start` (`go run .`,
  trong hạn 30 giây health-check).
- Chi tiết cả bốn mục trên: [docs/space-app-lifecycle.md](space-app-lifecycle.md).
  SDK cho ngôn ngữ khác Rust: [Node](../senclaw-sdk/senclaw-app-sdk) ·
  [Python](../senclaw-sdk/senclaw-app-sdk-python) ·
  [Go](../senclaw-sdk/senclaw-app-sdk-go), mỗi bản kèm app mẫu chạy được trong
  `examples/` — xem bảng đối chiếu ở [senclaw-sdk/README.md](../senclaw-sdk/README.md).
- `description` viết thật kỹ — nó vừa là mô tả gói trên hub, vừa là ngữ cảnh để agent hiểu app.
- Trường tuỳ chọn `widgets[]` cho phép app đưa UI mini vào thẳng ô chat — xem mục 3.5.
- Link mở ra ngoài trong UI phải đi qua flow `openExternal` → `POST /api/ui/open-url` (xem [docs/space-app-open-external.md](space-app-open-external.md), helper chuẩn `apps/zeach/web/src/openExternal.ts`), đừng để navigate webview nhúng.

## 3. Toàn bộ API SenClaw cung cấp cho App

Đây là **mặt tiếp xúc chính thức** giữa app và daemon. Gốc URL luôn là `SENCLAW_BASE_URL` (mặc định `http://127.0.0.1:18788`).

### 3.1 Bridge — một endpoint, nhiều action

```
POST /api/space/apps/{id}/bridge
Body: { "action": "<tên action>", "payload": { … } }
```

Response luôn HTTP 200 dạng `{ "appId", "status": "ok" | "error", ...}` (400 chỉ khi thiếu trường bắt buộc). Handler: [`space.rs::space_apps_bridge`](../src/gateway/ui_server/space.rs).

| Action | Payload | Trả về | App thật đang dùng |
|---|---|---|---|
| `capabilities` | — | danh sách action khả dụng | (probe) |
| `llm.request` | `prompt`*, `system?`, `maxTokens?`, `profile?` | `text, model, finish, usage\|null` | ~40 app (mọi app có AI) |
| `agent.run` | `prompt`*, `system?`, `tools?[]`, `model?`, `space?`, `workspace?`, `timeoutSeconds?` | `text, durationMs, usage` | ai-chat, ai-office, discuss, rule-engine, search, video-flow, zeach |
| `knowledge.save` | `text`*, `space?`, `tags?[]`, `source?` | `chunksAdded, entitiesAdded` | ai-chat, ai-office, crm, lakehouse, moltbook, rule-engine, search, tiktok-activity, youtube, zeach |
| `knowledge.search` | `query`*, `space?`, `mode?`, `limit?` (1–30, mặc định 6) | `hits[{id,kind,name,summary,score}]` | (như trên) |
| `knowledge.recall` | `query`*, `space?`, `mode?`, `limit?`, `hops?` (1–6, mặc định 2) | `answer, grounded, sources[]` | (như trên) |
| `usage.report` | `model`*, `inputTokens`/`outputTokens`* (≥1 khác 0), `provider?`, `cacheReadTokens?`, `cacheCreationTokens?`, `latencyMs?`, `estimated?` | `ok` | chưa app nào — xem ghi chú |
| `mcp.call` | — | `status: "pending"` — **chưa bật** | — |

#### `llm.request` — completion một phát trên LLM của SenClaw

```rust
use app_space_sdk::SpaceClient;

let sc = SpaceClient::from_env();   // đọc SENCLAW_BASE_URL + SENCLAW_SPACE_APP_ID

// Bản gọn: (text, model)
let (text, model) = sc.llm_request("Bạn là trợ lý…", "Xin chào", 4000).await?;

// Bản đủ: text + model + finish + usage (Option) — nên dùng bản này
let reply = sc.llm_request_usage("system", "prompt", 4000, None).await?;
if reply.finish == "length" { /* bị cắt — tăng maxTokens hoặc chia nhỏ input */ }

// Chạy trên một LLM profile riêng (id hoặc label trong /api/llm-config)
// → app có model riêng mà không đổi model active của cả daemon
let r = sc.llm_request_on("system", "prompt", 4000, Some("llm_abc123")).await?;
```

Tương đương curl (cho app không dùng Rust SDK — 18 app hiện gọi raw kiểu này):

```bash
curl -s http://127.0.0.1:18788/api/space/apps/my-app/bridge \
  -H 'Content-Type: application/json' \
  -d '{"action":"llm.request","payload":{"system":"Bạn là trợ lý","prompt":"Xin chào","maxTokens":4000}}'
```

Điều phải biết (đúc từ các app thật):

- **Không có `temperature`** — payload chỉ nhận system/prompt/maxTokens/profile.
- `maxTokens` để rộng (nhiều app dùng tới 32000); `finish == "length"` phải xử lý như lỗi, đừng lặng lẽ nhận text cụt. Input quá to có thể bị tóm tắt ngầm — tự chia chunk trước.
- `usage` là `null` khi provider không báo (một số model local) — lúc đó app tự ước lượng nếu cần.
- **Daemon tự ghi usage** của call này vào jid `app:{id}` (hiện trên trang `/usage`) — **đừng** gọi `usage.report` thêm cho cùng call, sẽ đếm đôi.
- Timeout phía SDK là 125s.

#### `agent.run` — chạy agent đầy đủ tool, headless

Khác `llm.request` (một phát, không tool), `agent.run` chạy **một agent hoàn chỉnh**: có tool mặc định + **MCP của chính app** + browser/web-search, tự lặp cho tới khi xong, trả text cuối cùng.

```bash
curl -s http://127.0.0.1:18788/api/space/apps/my-app/bridge \
  -H 'Content-Type: application/json' \
  -d '{
    "action": "agent.run",
    "payload": {
      "prompt": "Tra giá vàng SJC hôm nay rồi lưu vào sổ bằng tool myapp_add",
      "system": "Bạn là trợ lý của app My App. Dùng tool khi hữu ích.",
      "tools": ["mcp__my-app-mcp__myapp_add", "WebSearch"],
      "model": "llm_abc123",
      "timeoutSeconds": 300
    }
  }'
```

- `tools` (tuỳ chọn) = **allowlist chính xác** — có mặt thì agent chỉ nhận đúng các tool này; vắng mặt thì nhận cả pool. Đây là cách app enforce chính sách per-bot (ai-chat cho mỗi bot CSKH một bộ tool/model riêng).
- `model` (tuỳ chọn) — model hint cho riêng lần chạy.
- `space` (tuỳ chọn) — folder bộ nhớ của agent, mặc định `space-app-<id>`.
- `timeoutSeconds` clamp 10–1800. Chạy đồng thời tối đa **4** phiên/app — gọi quá sẽ xếp hàng, thiết kế job dài cho phù hợp.
- Usage đã được ghi từng call LLM bên dưới (luật anti-double-count) — totals chỉ trả về cho app tham khảo.
- SDK **chưa có wrapper** cho `agent.run` — gọi raw JSON như trên (mọi app hiện làm vậy).

#### `knowledge.*` — bộ nhớ dài hạn có phân vùng

Mỗi app có **một knowledge space riêng theo id** (cô lập hoàn toàn giữa các app; `space` tuỳ chọn cho phép app chia nhỏ hơn nữa — ví dụ mỗi bot nội bộ một space, pattern của ai-chat/discuss).

```rust
// Ghi nhớ — text được cognify thành chunks + entities
sc.knowledge_save("Khách A thích giao hàng buổi sáng", None, Some("crm-note")).await?;

// Tìm thô: Vec<(name, summary, score)>
let hits = sc.knowledge_search("khách A giao hàng", None, 6).await?;

// Recall: LLM tổng hợp câu trả lời có trích dẫn [n] trên đúng space đó
let answer = sc.knowledge_recall("khách A muốn giao lúc nào?", None).await?;
```

- `knowledge.save` payload có thêm `tags` (mỗi tag thành một NodeSet global — dùng để gom nhóm xuyên app) và `source`.
- `knowledge.recall` degrade thành snippet ghép khi user chưa cấu hình cognitive LLM; `answer` rỗng khi space chưa có gì liên quan.
- Lỗi `"cognitive system is not initialized"` = user chưa bật Knowledge trong SenClaw — app phải chịu được (tính năng AI-memory là tăng cường, không phải core path).

#### `usage.report` — app gọi provider trực tiếp thì tự khai token

Chỉ dành cho app **cầm key riêng gọi thẳng provider** (kiểu video-cloner gọi Gemini) — báo về để trang `/usage` của SenClaw đủ số:

```rust
sc.usage_report("gemini-2.5-pro", "google", in_tokens, out_tokens, latency_ms, /*estimated*/ false).await?;
```

`estimated: true` khi con số là ước lượng chars/4 thay vì số provider trả. Fire-and-forget được. **Hiện chưa app nào gọi** — nếu app của bạn gọi provider trực tiếp, hãy là app đầu tiên làm đúng.

### 3.2 REST ngoài bridge

| Nhóm | Endpoint | Ghi chú | App dùng |
|---|---|---|---|
| **Models** | `GET /api/llm-config` | `{activeId, configs[{id,modelName,provider}]}` — SDK `list_models()` | mini-browser (model picker) |
| | `POST /api/llm-config/active` `{id}` | đổi model active toàn cục — SDK `set_active_model()`; cân nhắc, vì ảnh hưởng mọi người dùng khác | |
| **Config per-app** | `GET /api/space/apps/{id}/config` | list toàn bộ key | nhiều app (settings user nhập) |
| | `GET\|PUT\|DELETE /api/space/apps/{id}/config/{key}` | PUT body `{value: <json bất kỳ>}`; lưu bảng `space_app_config` trong DB daemon → **sống qua reinstall app** | |
| **SQLite hosted** | `POST /api/space/apps/{id}/sqlite/query` | `{sql, params?}` chạy trên `<appDir>/app.sqlite`; `select/with/pragma` → `{rows}`, còn lại → `{rowsAffected, lastInsertRowId}`. Dành cho app **UI-only không có server**; app server tự quản DB riêng như thường | |
| **Static / proxy** | `GET /api/space/apps/{id}/static/*` | file tĩnh trong app dir | |
| | `ANY /api/space/apps/{id}/proxy/*` | daemon forward vào port app — chính là origin mà iframe UI load; cũng là cách tạo **URL same-origin** đưa cho agent nhúng ảnh vào chat qua tool `emit_widget` (pattern drawio: export SVG → `svg_path` → emit_widget kind `image`) | drawio |
| **Logs** | `GET\|DELETE /api/space/apps/{id}/logs` | stdout/stderr app — debug khi app chết | |
| **Env discovery** | `GET /api/space/apps/{id}/env` | trả appDir + mọi endpoint ở trên (UI khỏi hardcode) | |
| **Calendar** | `GET\|POST /api/space/calendar/events` | tạo/list sự kiện lịch SenClaw | study, google-workspace |
| | `GET /api/space/calendar/events/search` | | |
| | `GET\|PATCH\|DELETE /api/space/calendar/events/{id}` | idempotent-sync: app nhớ `event_id` để update thay vì insert đôi (xem `apps/study/src/calendar.rs`) | |
| | `POST /api/space/calendar/events/{id}/reminder` | đặt nhắc | |
| **Wiki** | `GET /api/wiki/tree` · `GET\|PUT\|DELETE /api/wiki/file` · `GET /api/wiki/search\|stats\|history\|tags` · `POST /api/wiki/mkdir\|upload` (12 MB) · `DELETE /api/wiki/dir` | knowledge base git-backed của SenClaw | ai-chat, ai-office, crm, moltbook, search, video-cloner, zeach |
| **Mở link ngoài** | `POST /api/ui/open-url` | bắt buộc cho mọi link ngoài từ UI app ([docs/space-app-open-external.md](space-app-open-external.md)) | mọi app có link ngoài |
| **Quản trị app** | `GET /api/space/apps` · `POST /api/space/apps/install-zip` (body ≤ 64 MB) · `POST .../register-local` · `POST .../{id}/update\|restart` · `DELETE .../{id}` · `GET .../updates` | phần cài/update — xem mục 5–6 | CLI + web |

Trường `link` của sự kiện calendar **chỉ nhận route nội bộ** dạng `/space/app/<id>?...` — bấm sự kiện (hoặc reminder) mở thẳng đúng màn hình trong app (study deep-link vào bài học `?session=<id>`).

### 3.3 MCP — hai chiều

**Chiều 1 — app expose tool cho agent** (chiều chính):

- Khai trong manifest (`mcp.autoRegister: true`) → daemon tự đăng ký khi app chạy. App phục vụ:
  - `GET /api/mcp/sse` — kênh SSE (path khai trong manifest);
  - `POST /api/mcp/message` — sibling JSON-RPC thực sự chở `initialize` / `tools/list` / `tools/call`.
- Agent (mọi kênh chat + Claude Code) gọi tool qua tên `mcp__<mcp.name>__<tool>` — quy ước đặt tên **bắt buộc** (xem CLAUDE.md): `mcp.name` thường là `<app-id>-mcp`, tool snake_case có prefix thống nhất (`myapp_list`, `myapp_add`…). **Không bịa tên rút gọn.**
- Kiểm tra runtime: `GET /api/mcp-servers` (mọi server + trạng thái), `GET /api/space/apps/{id}/mcp` (block manifest + live status + tools).
- `mcp.toolAliases` trong manifest cho phép đổi tên/override tool — nhập ở trạng thái **disabled**, user phải bật ở Plugins → Alias ([docs/mcp-tool-alias.md](mcp-tool-alias.md)).
- Cách nhanh nhất để có MCP endpoint chuẩn: copy `apps/mindmap/src/mcp.rs` (template chuẩn) rồi thay danh sách tool.

**Chiều 2 — đăng ký MCP động** (khi không dùng autoRegister, hoặc app UI-only muốn thêm server):

```
POST /api/space/apps/{id}/mcp/register
{ "name": "my-app-mcp", "transport": "http" | "sse" | "stdio",
  "url": "...", "command": "...", "args": [], "env": {}, "use_tools": [], "enabled": true }
```

Daemon tự thêm `SENCLAW_SPACE_APP_ID` vào env của server đăng ký.

**App-to-app**: app này gọi tool của app kia bằng cách POST thẳng JSON-RPC vào `http://127.0.0.1:<port-app-kia>/api/mcp/message` (pattern của app search — federated search sang các app khác; xem `apps/search/src/transport/app_mcp.rs`). Nhớ: tin cậy dựa trên loopback, nên chỉ hoạt động cùng máy.

**Lưu ý agent.run + MCP**: agent do `agent.run` sinh ra được nhận sẵn MCP của chính app — app có thể "tự nói chuyện với tool của mình" qua agent mà không cần gọi tool trực tiếp.

### 3.4 Bridge postMessage cho UI iframe

UI của app load qua proxy nên **same-origin với SenClaw web** → cách đơn giản nhất là `fetch` thẳng các REST/bridge endpoint ở trên, không cần postMessage. Protocol postMessage tồn tại cho app muốn nhận theme/env có cấu trúc:

```
iframe → host : { type: 'senclaw:ready' }
host  → iframe: { type: 'senclaw:init', appId, theme: 'dark'|'light',
                  env: { apiBase, coreBase, staticBase, bridgeEndpoint,
                         configEndpoint, sqliteEndpoint, mcpRegisterEndpoint },
                  capabilities: ['llm.request','mcp.call','space.rest'] }
host  → iframe: { type: 'senclaw:theme', theme }          // mỗi lần user đổi theme
iframe → host : { type: 'senclaw:request', requestId, action, payload }   // forward vào bridge
host  → iframe: { type: 'senclaw:response', requestId, ok, payload | error }
```

(Nguồn: `web/src/components/space/SpaceAppFrame.tsx`.) Query string bên ngoài (`/space/app/my-app?d=3`) được forward nguyên vẹn vào iframe — dùng cho deep-link từ chat/calendar; app lờ param không biết.

### 3.5 Widgets — đưa UI của app vào thẳng ô chat

Widget là card hiển thị **inline trong ô chat** (Web + Desktop) và trên dashboard. Hợp đồng đầy đủ: [`WIDGET_CONTRACT.md`](../WIDGET_CONTRACT.md); **hướng dẫn tạo widget chi tiết từng bước** (manifest → trang HTML → skill → test/debug): [widget-authoring-guide.md](widget-authoring-guide.md). App có 2 đường tham gia:

**Đường 1 — kind built-in** (`chart`, `image`, `clock`, `weather`, `video`, `audio`): agent tự emit, app chỉ cần **cấp dữ liệu hoặc URL same-origin**. Pattern drawio: tool MCP của app trả `svg_path` (URL qua `/api/space/apps/{id}/proxy/...`) kèm mô tả dặn agent đưa URL đó vào `emit_widget` kind `image` — sơ đồ hiện thẳng trong chat mà app không phải làm gì thêm.

**Đường 2 — kind `app`**: widget iframe do chính app phục vụ, khai trong manifest:

```jsonc
// senclaw-manifest.json
"widgets": [
  {
    "id": "pipeline",                       // bắt buộc — id đầy đủ thành "<app-id>.pipeline"
    "name": "Phễu bán hàng",
    "description": "Mô tả KỸ — agent đọc qua widget_list để quyết định khi nào chèn",
    "entryUrl": "/widget/pipeline.html",    // trang HTML app phục vụ
    "size": "medium",                       // small | medium | large | tall
    "refreshMs": 30000,                     // gợi ý client tự reload
    "surfaces": ["dashboard", "chat"],      // MẶC ĐỊNH ["dashboard"] — PHẢI thêm "chat" mới hiện trong chat
    "params": {                             // JSON Schema — daemon validate khi emit
      "type": "object",
      "properties": { "stage": { "type": "string" } },
      "required": ["stage"]
    },
    "textFallback": "Phễu giai đoạn {stage} — mở CRM để xem"   // template {param} cho kênh text
  }
]
```

Luồng chạy:

1. Agent gọi tool `emit_widget { kind: "app", widget: "<app-id>.<widget-id>", params: {...} }` (truyền `widget` + `params`, **không** truyền `data`). Tool `widget_list` cho agent xem catalog + schema params.
2. Daemon resolve id trong **widget registry** (gom `widgets[]` của mọi app *enabled* + widget của plugin), validate `params` theo schema, rồi build entry: origin runtime của app (fallback `/api/space/apps/<id>/proxy`) + params gắn thành **query string** — params không bao giờ đổi được path.
3. Widget được persist (bảng `chat_widgets`, FIFO theo jid; `history:load` trả về `role: "widget"`) và đẩy WS frame `{ "type": "chat:widget", ... }` → Web/Desktop render **iframe sandboxed**.
4. **Kênh chat chỉ có text** (Telegram/Zalo… — jid không phải `web:`/`app:`): WS không tới được, daemon gửi `textFallback` đã điền `{param}` như tin nhắn thường kèm deep link mở app — vì vậy textFallback là bắt buộc-trên-thực-tế nếu user dùng kênh ngoài.

Phía app cần làm:

- Phục vụ **trang HTML nhỏ tự chứa** tại `entryUrl` (widget-pack đặt ở `web/widget/*.html`, build vào `web_dist`), đọc params từ query string, tự render gọn trong khung size đã khai.
- **Kèm một skill dạy agent dùng widget** — pattern chuẩn của widget-pack (`apps/widget-pack/skills/widget-pack/SKILL.md`): frontmatter `allowed-tools: [emit_widget, widget_list]`, thân skill ghi ví dụ emit từng widget với params thật. Không có skill thì agent hiếm khi tự biết mà chèn.
- Ví dụ gọi (agent-side):

```
emit_widget { "kind": "app", "widget": "widget-pack.countdown",
  "params": { "to": "2026-12-31", "label": "Tết Dương lịch" } }
```

Quản trị & giới hạn:

- Catalog: `GET /api/widgets` (kèm cờ `enabled`), bật/tắt từng widget `PUT /api/widgets/:id` — user quản ở **Plugins → Widget**. Widget của app tắt là emit fail, app phải chịu được.
- Trong text trả lời, agent cũng chèn được widget bằng fence ` ```chart `/` ```widget ` — nhưng **kind `app` bắt buộc đi qua tool** (resolve registry nằm daemon-side).
- App thật đang khai `widgets[]` (11 app): ai-office, clock, crm, email, hub, luna-calendar, mindmap, moltbook, predict, rule-engine, widget-pack — trong đó `widget-pack` là app mẫu thuần widget (countdown, progress, bảng dữ liệu).

### 3.6 Bảng tra nhanh: app thật nào đang dùng gì (khảo sát `apps/*`, 2026-08)

| Surface | App đang dùng |
|---|---|
| SDK `SpaceClient` (Rust) | autotest, cafe, capital, code-ide, crm, docx-editor, drawio, facebook-pro, ipscout, luna-calendar, mindmap, mini-browser, news, ontology, predict, search, secscan, sentinel, shopee, skill-builder, thinking, warehouse, youtube, zeach |
| Bridge gọi raw (không SDK) | ai-chat, ai-office, ba, deepwiki, discuss, kaen, lakehouse, moltbook, rewrite-story, rule-engine, study, tiktok-activity, video-cloner, video-flow |
| `agent.run` | ai-chat (per-bot tools/model), ai-office (personas), discuss (phòng thảo luận nhiều agent), rule-engine (node AI), search, video-flow, zeach (deep research) |
| `knowledge.*` | ai-chat, ai-office, crm, lakehouse, moltbook, rule-engine, search, tiktok-activity, youtube, zeach |
| Wiki REST | ai-chat, ai-office, crm, moltbook, search, video-cloner, zeach |
| Calendar REST | study (lịch học + deep-link bài học), google-workspace |
| Widget kind `app` (manifest `widgets[]`) | ai-office, clock, crm, email, hub, luna-calendar, mindmap, moltbook, predict, rule-engine, widget-pack |
| Widget built-in qua URL same-origin | drawio (SVG → `emit_widget` kind image) |
| App-to-app MCP | search (gọi `/api/mcp/message` các app khác) |
| `usage.report` | chưa app nào (API sẵn sàng — dùng khi gọi provider trực tiếp) |
| Gọi provider trực tiếp (ngoại lệ) | video-cloner (Gemini video — bridge không chở video được) |

Muốn xem ví dụ sống cho pattern nào, mở đúng app trong cột phải.

## 4. Đóng gói: `scripts/pack.sh`

Zip cài được có **layout phẳng**: binary + manifest + skills + personas + web_dist ở gốc zip. Bản cho repo độc lập (khác monorepo ở chỗ build tại chỗ, không `-p <crate>` từ ROOT):

```bash
#!/usr/bin/env bash
# Build my-app và đóng gói thành my-app-app.zip cài được trong SenClaw.
#   release/            <- staging phẳng
#     my-app            (binary release; manifest runtime.start = ./my-app)
#     senclaw-manifest.json
#     skills/ personas/
#     web_dist/         (UI đã build — server serve web_dist cạnh binary)
#   my-app-app.zip      <- artifact để cài / publish
# Usage: scripts/pack.sh [--skip-build]
set -euo pipefail

APP_DIR="$(cd "$(dirname "$0")/.." && pwd)"
REL="$APP_DIR/release"
ZIP="$APP_DIR/my-app-app.zip"
BIN="$APP_DIR/target/release/my-app"

if [[ "${1:-}" != "--skip-build" ]]; then
  echo "==> building web UI"
  ( cd "$APP_DIR/web" && npm install --silent && npm run build )
  echo "==> building release binary"
  ( cd "$APP_DIR" && cargo build --release )
fi

[[ -f "$BIN" ]] || { echo "missing $BIN"; exit 1; }
[[ -d "$APP_DIR/web/dist" ]] || { echo "missing web/dist"; exit 1; }

echo "==> staging release/"
rm -rf "$REL" "$ZIP"
mkdir -p "$REL"
cp "$BIN" "$REL/my-app" && chmod +x "$REL/my-app"
cp "$APP_DIR/senclaw-manifest.json" "$REL/"
cp -R "$APP_DIR/skills" "$REL/skills"
cp -R "$APP_DIR/personas" "$REL/personas"
cp -R "$APP_DIR/web/dist" "$REL/web_dist"

echo "==> zipping -> $ZIP"
( cd "$REL" && zip -rq "$ZIP" . -x '*.DS_Store' )
echo "done: $ZIP ($(du -h "$ZIP" | cut -f1))"
```

Ràng buộc quan trọng:

- **Tên zip mặc định là `<id>-app.zip`** đặt ở gốc app dir — `senclaw hub publish` tìm đúng tên đó (đổi được qua trường `artifact` trong `senclaw-hub.json`).
- **Giới hạn upload hub là 20 MB** (`MAX_UPLOAD_BYTES`, khớp server; cài local qua daemon nhận tới ~50 MB). Zip app thực tế ~3–4 MB; nếu phình, kiểm tra có lỡ đóng cả `node_modules`/asset thừa, và bật `strip = true`, `lto = true` trong `[profile.release]`.
- Binary build cho máy nào thì chỉ chạy trên máy đó — platform ghi trong `senclaw-hub.json` (mặc định máy đang build, vd `darwin-arm64`).

### 4.1 Test cục bộ trước khi publish

```bash
scripts/pack.sh
curl -F "file=@my-app-app.zip" http://127.0.0.1:18788/api/space/apps/install-zip
```

Daemon giải nén, chạy binary, health-check rồi app xuất hiện trong Space của Web UI. Kiểm nhanh:

```bash
curl -s http://127.0.0.1:18788/api/mcp-servers | grep my-app-mcp       # MCP đã đăng ký?
curl -s http://127.0.0.1:18788/api/space/apps/my-app/logs              # app có kêu ca gì không
```

Cài tay kiểu này **không có tem hub** (xem mục 6.3) nên chỉ dùng để thử.

## 5. Publish lần đầu lên senclaw.bacnd.com

### 5.1 Sinh `senclaw-hub.json`

```bash
senclaw hub init . --version 1.0.0
```

File này là metadata **phía hub** (tách khỏi manifest runtime một cách có chủ đích — hai file hai đối tượng đọc, gộp là drift). `init` scaffold sẵn permissions **hẹp** từ manifest:

```json
{
  "version": "1.0.0",
  "permissions": {
    "network": ["127.0.0.1"],
    "exec": ["./my-app"]
  },
  "updater": "none",
  "platform": "darwin-arm64"
}
```

Trước khi publish, tự tay bổ sung:

- `category`, `keywords` — để gói dễ tìm trên `/store`.
- `repo_url`, `homepage_url` — hiện trên trang gói.
- **`permissions` phải khai đúng thực tế** (app gọi mạng ra ngoài thì khai domain). Đây là bản khai bảo mật hiển thị cho người dùng **trước khi cài** — khai sai tệ hơn không khai.

### 5.2 Dry-run rồi publish

```bash
# Kiểm tra mọi thứ (semver, description, artifact, size, version đã tồn tại chưa) — KHÔNG upload
senclaw hub publish . --pack --dry-run --hub https://senclaw.bacnd.com

# Ổn rồi thì publish thật
senclaw hub publish . --pack --hub https://senclaw.bacnd.com
```

- `--pack` chạy `scripts/pack.sh` trước khi upload (bỏ nếu vừa pack tay).
- `senclaw hub status .` xem nhanh: version local, artifact, integrity, các version đã có trên hub.
- Publish gửi `kind=app`, `name=<id trong manifest>`, `description` lấy từ manifest, `README.md` làm trang gói. **Scope bị server ép = handle của chủ token** — không ai publish được vào namespace người khác.

Thành công sẽ in slug + URL dạng `https://senclaw.bacnd.com/p/<scope>/my-app` và integrity SHA-512 do **hub tự tính** từ bytes nó lưu (con số client tính chỉ để đối chiếu).

Kiểm tra sau publish:

```bash
senclaw hub info <scope>/my-app --hub https://senclaw.bacnd.com
senclaw hub install <scope>/my-app --dry-run --hub https://senclaw.bacnd.com   # tải + verify, không cài
```

### 5.3 Lỗi thường gặp

| Lỗi | Nghĩa là | Cách xử |
|---|---|---|
| HTTP 409 `version_exists` | `name@version` đã có — version publish rồi là **bất biến** | `senclaw hub bump . patch` rồi publish lại |
| HTTP 401 | Token sai/hết hạn | `senclaw hub login` với token mới |
| HTTP 403 `insufficient_scope` | Token thiếu scope publish | Tạo token mới có scope publish |
| HTTP 403 `no_handle` | Tài khoản chưa đặt username | Vào hub đặt username trước |
| HTTP 403 khác | Không phải maintainer của gói | Gói tên đó thuộc người khác trong scope của bạn |
| HTTP 413 / "vượt giới hạn 20 MB" | Artifact quá to | Bóp zip (strip, bỏ asset thừa) |
| "chưa có artifact …" | Chưa pack | Chạy với `--pack` hoặc `scripts/pack.sh` |
| "thiếu `description`" | Manifest trống description | Hub bắt buộc có — bổ sung vào `senclaw-manifest.json` |

Chú ý: preflight của CLI kiểm tra trùng version dưới scope `senclaw`; nếu handle của bạn khác `senclaw` thì phán quyết cuối cùng vẫn là server (409) — cứ bump là thoát.

## 6. Phát hành bản cập nhật

### 6.1 Phía người phát hành

```bash
# 1. Sửa code xong, tăng version trong senclaw-hub.json (patch | minor | major)
senclaw hub bump . patch        # 1.0.0 → 1.0.1

# 2. Đóng gói + publish
senclaw hub publish . --pack --hub https://senclaw.bacnd.com
```

Quy tắc: **mỗi thay đổi đã ship = một version mới**. Không có "publish đè". Version hỏng thì yank trên hub (registry từ chối cài version yanked) rồi publish bản vá.

### 6.2 Phía người dùng

```bash
senclaw hub install <scope>/my-app --hub https://senclaw.bacnd.com   # cài lần đầu
senclaw hub outdated                                                  # app nào có bản mới
senclaw hub update my-app                                             # update một app
senclaw hub update --all                                              # update mọi app có bản mới
```

Web UI cũng hiện badge update (daemon expose `GET /api/space/apps/updates`, `POST /api/space/apps/{id}/update`). Mọi download đều được verify SHA-512 + size với registry trước khi cài — lệch là từ chối.

Lưu ý scope: gõ tên trần (`senclaw hub install my-app`) mặc định hiểu là `senclaw/my-app`. Gói publish dưới handle khác phải gõ đủ `<handle>/my-app`.

### 6.3 Tem xuất xứ (để update tự động hoạt động)

Khi cài qua `senclaw hub install`, CLI gửi kèm `slug/version/hub/integrity` để daemon **đóng tem `manifest.hub`** vào manifest đã cài — nhờ đó `outdated` biết so version nào với gói nào. App cài tay bằng `install-zip` không tem: nếu `id` trùng gói trên hub thì luôn bị chào bản latest; app dev local (`install.type = "local"`) và id dạng `space-app-<uuid>` được bỏ qua.

## 7. Giới hạn hiện tại (tính đến 2026-08)

- **Một version = một artifact** qua CLI/API publish. Server đã có đường `addArtifact` (thêm platform thứ hai vào version đã publish, cùng version cho darwin/linux) nhưng `POST /api/v1/publish` chưa expose và CLI chưa gọi — đa nền tảng tạm thời là publish từ máy target với version riêng, hoặc chờ CLI hỗ trợ.
- Upload trần 20 MB (giới hạn Worker giữ file trong RAM để hash). Đường presigned-R2 cho artifact lớn nằm trong PLAN của hub, chưa dùng cho app zip.
- Bridge `mcp.call` chưa bật (trả `pending`) — app cần gọi tool thì dùng `agent.run` hoặc app-to-app `/api/mcp/message`.
- `DEFAULT_HUB_URL` của CLI là `https://senclaw.bacnd.com` từ 2026-08-05 (trước đó `hub-store.bacnd.com` — cùng server, còn sống cho link cũ). Daemon phía cài đọc `SENCLAW_HUB_URL` (mặc định cũng senclaw.bacnd.com) cho marketplace/update; source seed bằng hostname cũ được migrate tự động lúc daemon khởi động.

## 8. Checklist tóm tắt

```
[ ] Repo riêng: Cargo.toml dep app-space-sdk (git, pin rev) + .gitignore target/release/zip
[ ] main.rs: đọc PORT daemon gán, bind SENCLAW_BIND_HOST (mặc định 127.0.0.1),
    /api/status trả 200, serve web_dist cạnh binary
[ ] Port cố định chưa ai dùng, khai đúng trong manifest
[ ] AI qua bridge: llm.request (xử lý finish=length, không temperature),
    agent.run cho việc cần tool, knowledge.* cho bộ nhớ, usage.report nếu gọi provider trực tiếp
[ ] MCP: name `<id>-mcp`, path /api/mcp/sse + POST /api/mcp/message, tool prefix thống nhất
[ ] (tuỳ chọn) widgets[]: surfaces có "chat", params schema, textFallback,
    trang HTML tại entryUrl + skill dạy agent emit_widget kind "app"
[ ] Link ngoài đi qua POST /api/ui/open-url (openExternal)
[ ] senclaw-manifest.json: id/description/runtime/integration/bridge/mcp/skills/personas
[ ] scripts/pack.sh → <id>-app.zip layout phẳng, ≤ 20 MB
[ ] Test cục bộ: install-zip vào daemon, /api/mcp-servers thấy MCP, logs sạch
[ ] Hub: đăng ký + đặt username + token publish → senclaw hub login
[ ] senclaw hub init . → sửa permissions/category/keywords/repo_url
[ ] senclaw hub publish . --pack --dry-run --hub https://senclaw.bacnd.com → publish thật
[ ] Bản mới: bump → publish; người dùng: outdated → update
```
