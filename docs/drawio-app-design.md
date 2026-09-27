# Draw.io Space App — thiết kế tích hợp editor + LLM tự vẽ (`apps/drawio`)

> Nghiên cứu 2026-07-27. Trạng thái: **BUILT & VERIFIED 2026-07-27** — cả 4 pha
> xong, đã register-local vào daemon (`drawio-mcp` connected), zip 2.8MB.
> Port: **4610**. MCP: `drawio-mcp` → `mcp__drawio-mcp__drawio_*` (10 tools).
> Verify runtime: editor tải + sha256 khớp (`05907c7d…f85c3`), Mermaid convert
> CÓ trong bản war tự host (rủi ro #1 đóng), XML mode + edit_ai chạy tốt qua
> `ag/gemini-pro-agent`. Gotcha phát hiện thêm: sau `load` XML editor giữ
> viewport cũ → DrawioFrame luôn gửi action `fit` khi nhận event `load`.

## 1. Mục tiêu

Một Space App nhúng **editor draw.io đầy đủ** (diagrams.net) vào SenClaw, và nối
LLM của SenClaw (bridge `llm.request`) để **tự vẽ sơ đồ từ mô tả tiếng
Việt/Anh**: flowchart, sequence, kiến trúc hệ thống, ER, BPMN, org chart…
Người dùng có thể:

- Vẽ tay trong editor draw.io như bình thường (đủ shape libraries, stencils).
- Bấm ✨ "AI vẽ" → mô tả → LLM sinh sơ đồ đổ thẳng vào canvas, **chỉnh sửa được**
  (không phải ảnh).
- Chat với AI để sửa sơ đồ hiện tại ("đổi hướng flow sang ngang", "thêm bước
  xác thực OTP sau login").
- Agent/persona khác gọi qua MCP: `drawio_generate("kiến trúc microservice X")`
  → trả về link mở sơ đồ + SVG.

## 2. Kết quả nghiên cứu draw.io

### 2.1 Embed protocol (postMessage JSON) — nền tảng tích hợp

draw.io hỗ trợ chế độ nhúng chính thức qua iframe với `?embed=1&proto=json`
([doc](https://www.drawio.com/doc/faq/embed-mode)). Giao tiếp 2 chiều:

- **Editor → host (events)**: `init` (editor sẵn sàng, host phải trả `load`),
  `save`, `autosave` (kèm `xml`), `exit`, `export` (kèm `data` data-URI + `xml`),
  `load` (kèm `bounds`).
- **Host → editor (actions)**:
  - `load` — nạp XML / PNG / SVG, hoặc **descriptor** `{format:'mermaid'|'csv', data}`
    (editor tự convert Mermaid → shapes chỉnh sửa được). Tham số: `autosave:1`,
    `dark`, `title`, `layout`…
  - `merge` — trộn XML vào sơ đồ đang mở (dùng cho AI-edit từng phần).
  - `export` — xuất `svg|png|xml|html` (trả về qua event `export`).
  - `layout` — chạy auto-layout (`verticalFlow`, `horizontalTree`, `organic`…).
  - `configure`, `dialog`, `status`, `spinner`, `fit`, `invokeAction`…
- URL params hữu ích: `spin=1`, `libraries=1`, `noSaveBtn=1`, `configure=1`,
  `modified=0`, `ui=dark`.

Wrapper React có sẵn: [`react-drawio`](https://github.com/marcveens/react-drawio)
(`DrawIoEmbed`, hỗ trợ `baseUrl` trỏ instance tự host). Protocol đơn giản nên
**khuyến nghị tự viết wrapper mỏng (~150 dòng)** — chủ động kiểm soát, không thêm dep,
khớp phong cách mindmap/clock (dependency-free React).

### 2.2 Self-host editor — bài toán kích thước

- Webapp draw.io là **static site thuần** (`src/main/webapp`): `app.min.js`,
  `shapes/`, `stencils/`, `templates/`, `resources/`. Serve bằng bất kỳ static
  server nào; pattern "drawio-local" đặt `offline=1&local=1` trong `PreConfig.js`
  để cắt mọi kết nối ra ngoài (Google Drive, OneDrive…).
- **`draw.war` release mới nhất (v31.1.2) = ~52.7 MB nén** (war = zip của webapp).
  → **KHÔNG thể bundle vào zip app**: local-install limit 50 MB
  (`space.rs:974`), hub-publish limit **20 MB** (`publish.rs:27`).
- Embed mode chạy được trên bản self-host (`embed=1` — chính là cách drawio-local
  và GitLab dùng).

**Quyết định: pattern "composite download" (tiền lệ VieNeu-TTS).**
Zip app chỉ chứa binary Rust + React UI (~4-6 MB). Lần chạy đầu, backend tải
`draw.war` (pin version + sha256) từ GitHub release → giải nén vào
`~/.senclaw/space-apps/drawio/editor/` → axum serve tại `/drawio/` (same-origin
với UI). Sau đó **hoàn toàn offline**. Khi editor chưa tải xong, UI hiển thị
màn hình tải tiến độ; tùy chọn fallback iframe `https://embed.diagrams.net`
(config, mặc định TẮT — giữ nguyên tắc self-contained).

Lý do không dùng npm-bundle như code-ide/Monaco: không tồn tại npm package
maintained cho editor drawio đầy đủ (`mxgraph` đã archive; `@maxgraph/core` là
rewrite thiếu UI/stencils). Iframe same-origin là cách chính thống duy nhất.

### 2.3 LLM sinh sơ đồ — chuẩn chính thức đã có sẵn

jgraph phát hành [`drawio-mcp`](https://github.com/jgraph/drawio-mcp)
(Apache-2.0) + [tài liệu chính thức dạy LLM sinh XML](https://www.drawio.com/docs/reference/diagram-generation/).
Ta **tái sử dụng bộ quy tắc**, không dùng server của họ (nó mở app.diagrams.net
tab rời / render inline chat — không nhúng được vào Space App, và không đi qua
bridge LLM của SenClaw).

Cấu trúc XML LLM cần sinh (dạng rút gọn — draw.io tự bọc `mxfile`):

```xml
<mxGraphModel>
  <root>
    <mxCell id="0"/>
    <mxCell id="1" parent="0"/>
    <mxCell id="n1" value="Login" style="rounded=1;fillColor=#DAE8FC;" vertex="1" parent="1">
      <mxGeometry x="40" y="40" width="120" height="60" as="geometry"/>
    </mxCell>
    <mxCell id="e1" style="edgeStyle=orthogonalEdgeStyle;" edge="1" source="n1" target="n2" parent="1">
      <mxGeometry relative="1" as="geometry"/>
    </mxCell>
  </root>
</mxGraphModel>
```

10 quy tắc chính thức nhét vào system prompt: bắt buộc cell id `0`/`1`; **không
nén, không XML comment**; id duy nhất; `vertex="1"` xor `edge="1"`; style
`key=value;` phân cách chấm phẩy; perimeter khớp shape (ellipse →
`ellipsePerimeter`); gốc tọa độ trên-trái; escape `&lt; &gt; &amp; &quot;`
trong `value`; tọa độ con relative theo parent group.

### 2.4 Hai mode sinh — Mermaid (nhanh) và mxGraph XML (chính xác)

| | **Mermaid mode** (mặc định) | **XML mode** |
|---|---|---|
| LLM sinh | Mermaid text (~10× ít token) | mxGraphModel XML đầy đủ |
| Đưa vào editor | `load`/`merge` descriptor `{format:'mermaid'}` — drawio convert thành shapes chỉnh sửa được | `load`/`merge` XML trực tiếp |
| Ưu | Rẻ, gần như không lỗi cú pháp, hợp flowchart/sequence/class/ER/gantt | Kiểm soát vị trí, màu, swimlane, stencil AWS/Azure, group lồng nhau |
| Nhược | Không kiểm soát style/tọa độ; giới hạn loại sơ đồ Mermaid hỗ trợ | Đắt token; dễ truncate → cần validate + repair |
| Dùng khi | User chọn "Nhanh" hoặc loại sơ đồ thuộc họ Mermaid | User chọn "Chi tiết", sơ đồ kiến trúc/BPMN/custom style |

Sau khi load ở cả 2 mode, có thể gửi action `layout` để chỉnh bố cục
(`horizontalFlow`, `verticalTree`…).

**Lưu ý Mermaid convert chạy trong editor** (client-side) — webapp đầy đủ có
bundle sẵn mermaid; cần verify sau khi giải nén war (nếu bản trim thiếu thì
XML mode vẫn là đường chính).

## 3. Kiến trúc app (theo chuẩn Space App hiện hành)

```
apps/drawio/
  Cargo.toml                  # pins y hệt mindmap: axum 0.7, rusqlite 0.32, tower-http 0.5,
                              # reqwest 0.12 rustls, app-space-sdk (path) — KHÔNG thêm feature dep chung
  senclaw-manifest.json       # id "drawio", port 4610, mcp "drawio-mcp"
  senclaw-hub.json            # version 1.0.0, category "productivity"
  scripts/pack.sh             # theo biến thể zeach (guard skills/personas, copy hub.json)
  src/
    main.rs                   # 5-candidate static-dir của mindmap (nguyên văn) + serve /drawio/ từ editor dir
    api.rs                    # AppState { db, mcp_tx: broadcast } + REST
    db.rs                     # ~/.senclaw/space-apps/drawio/drawio.db
    editor.rs                 # composite download: tải draw.war pin-version + sha256, giải nén, verify
    llm.rs                    # SpaceClient wrapper + generate/edit pipeline + XML repair
    mcp.rs                    # JSON-RPC + SSE hand-rolled (copy skeleton mindmap, KHÔNG rmcp)
  skills/
    drawio-generate/SKILL.md  # "vẽ sơ đồ/flowchart/kiến trúc…", "draw a diagram…"
    drawio-edit/SKILL.md      # "sửa sơ đồ…", "update the diagram…"
  personas/diagram-architect.md
  web/                        # React 19 + Vite 8, base './' (một trang, không router)
    src/App.tsx               # sidebar danh sách sơ đồ + DrawioFrame + AI panel
    src/DrawioFrame.tsx       # wrapper postMessage tự viết
```

### 3.1 Luồng dữ liệu

```
User/Agent
  │  "vẽ flowchart đăng ký tài khoản"
  ▼
[skill drawio-generate] ──► mcp__drawio-mcp__drawio_generate
  │                            │
  ▼                            ▼
Web UI ✨ panel ──► POST /api/diagrams/:id/generate {prompt, mode, kind}
                               │
                               ▼
                    llm.rs: bridge llm_request_full(system, prompt, 16000)
                        (POST {SENCLAW_BASE_URL}/api/space/apps/drawio/bridge
                         action "llm.request" — BẮT BUỘC check finish=="length")
                               │
                               ▼
                    validate + repair (mermaid parse / XML tag-stack repair)
                               │
                               ▼
                    DB save (xml, name) ──► broadcast mcp_tx {type:"diagram:update", id}
                               │
                               ▼
        Web UI nhận SSE/WS ──► postMessage {action:'load'|'merge', xml|descriptor} vào iframe
                               │
        iframe /drawio/?embed=1&proto=json&spin=1&libraries=1
                               │
        editor events: autosave {xml} ──► PUT /api/diagrams/:id  (+ export SVG snapshot cache)
```

### 3.2 DB schema

```sql
CREATE TABLE diagrams (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  xml  TEXT NOT NULL DEFAULT '',      -- mxfile/mxGraphModel uncompressed
  svg  TEXT NOT NULL DEFAULT '',      -- snapshot cache từ event export (cho MCP export + widget)
  kind TEXT NOT NULL DEFAULT 'flowchart',
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE TABLE ai_log ( id, diagram_id, prompt, mode, model, finish, ok, created_at );
```

SVG snapshot: editor là nơi duy nhất render được → sau mỗi `save`/`autosave`
(debounce), UI gửi action `export {format:'svg'}` rồi PUT kết quả về server.
MCP `drawio_export` trả SVG cache này (kèm cảnh báo staleness nếu UI chưa mở
từ lần sửa XML cuối).

### 3.3 senclaw-manifest.json (rút gọn)

```json
{
  "id": "drawio",
  "name": "SenClaw Diagrams",
  "icon": "📐",
  "runtime":     { "kind": "server", "start": "./drawio", "healthPath": "/api/status", "port": 4610 },
  "integration": { "type": "iframe", "url": "/" },
  "bridge":      { "postMessage": true, "capabilities": ["space.rest", "llm.request"] },
  "mcp": { "name": "drawio-mcp", "transport": "http", "path": "/api/mcp/sse", "autoRegister": true },
  "skills": [
    { "name": "drawio-generate", "path": "skills/drawio-generate",
      "triggers": ["vẽ sơ đồ", "vẽ flowchart", "vẽ lưu đồ", "sơ đồ kiến trúc", "draw a diagram", "create a flowchart", "architecture diagram"] },
    { "name": "drawio-edit", "path": "skills/drawio-edit",
      "triggers": ["sửa sơ đồ", "cập nhật sơ đồ", "update the diagram", "edit the flowchart"] }
  ],
  "personas": [{ "name": "diagram-architect", "path": "personas/diagram-architect.md",
                 "description": "Turns descriptions into clear, well-laid-out draw.io diagrams" }]
}
```

### 3.4 MCP tools (`drawio-mcp`)

| Tool | Mô tả |
|---|---|
| `drawio_list` / `drawio_get` / `drawio_create` / `drawio_rename` / `drawio_delete` | CRUD sơ đồ (get trả XML + meta) |
| `drawio_generate` | `{prompt, kind?, mode?: "mermaid"\|"xml", diagram_id?}` → AI sinh, lưu, trả `{id, url, summary}`; `diagram_id` = merge vào sơ đồ có sẵn |
| `drawio_edit_ai` | `{diagram_id, instruction}` → gửi XML hiện tại + yêu cầu → LLM trả XML sửa → validate → save + broadcast |
| `drawio_set_xml` / `drawio_get_xml` | đọc/ghi XML trực tiếp (cho agent tự soạn) |
| `drawio_export` | trả SVG snapshot cache (+ `stale: true/false`) |

### 3.5 Pipeline LLM — chi tiết chống lỗi

1. **System prompt XML mode**: 10 quy tắc chính thức + palette style mặc định
   (rounded, fillColor pastel, `edgeStyle=orthogonalEdgeStyle`) + **giới hạn ≤ 40
   node/lần** (trần output bridge — không có `temperature`, `maxTokens` ≤ 32000,
   model reasoning đốt budget vào trace trước → sơ đồ to sẽ đứt giữa chừng).
   Sơ đồ lớn hơn: sinh theo cụm rồi `merge` + `layout` (giống mindmap expand từng nhánh).
2. **`finish == "length"` = LỖI** → retry 1 lần với yêu cầu rút gọn (giảm node,
   bỏ style rườm), vẫn fail → trả lỗi rõ ràng, không lưu XML cụt.
3. **Validate server-side** (quick-xml hoặc scan tay, KHÔNG thêm dep nặng):
   well-formed; có cell `0`,`1`; id không trùng; mỗi cell là vertex xor edge;
   edge có source/target tồn tại; strip ```` ```xml ```` fences.
4. **Repair ladder** (phỏng theo `parse_gen` của mindmap, đổi sang XML):
   direct parse → strip fences → cắt lấy block `<mxGraphModel>…` cân bằng đầu tiên
   → nếu truncate: cắt tại thẻ đóng hợp lệ cuối, dựng tag-stack phần giữ lại,
   append thẻ đóng còn thiếu.
5. **Truncate label an toàn UTF-8**: dùng `chars().take(n)` — tên node tiếng Việt
   sẽ panic với `&s[..N]`.
6. **AI-edit sơ đồ lớn**: nếu XML hiện tại > ~8k token, gửi bản rút gọn
   (id + value + source/target, bỏ geometry/style) để LLM quyết định thay đổi,
   rồi apply dạng patch qua `merge` thay vì bắt LLM chép lại toàn bộ.

### 3.6 UI & editor frame

- `DrawioFrame.tsx`: iframe `src="/drawio/?embed=1&proto=json&spin=1&libraries=1&modified=unsavedChanges"`;
  lắng nghe `init` → gửi `load {xml, autosave:1, dark}`; `autosave`/`save` → PUT;
  expose `loadXml/mergeXml/runLayout/exportSvg` cho App.
- Theme: handshake `senclaw:ready` → `senclaw:init {theme}` + `senclaw:theme`
  từ host SenClaw → truyền `dark` vào action `load` (hoặc reload iframe với `ui=dark`).
- **Sandbox host không có `allow-downloads`** → chặn nút File→Download của editor
  (embed mode mặc định đã ẩn); mọi export đi qua `/api/diagrams/:id/export` để
  server trả file.
- Vite `base: './'` (một trang, không router) — an toàn cả khi bị serve qua proxy
  `/api/space/apps/drawio/proxy/`.

### 3.7 editor.rs — composite download

- Pin: `DRAWIO_VERSION = "31.1.2"`, URL
  `https://github.com/jgraph/drawio/releases/download/v31.1.2/draw.war`, sha256
  hardcode. Override: `SENCLAW_DRAWIO_EDITOR_DIR` (dev trỏ bản giải nén sẵn),
  `SENCLAW_DRAWIO_WAR_URL` (mirror).
- Tải về `editor/download.tmp` → verify sha256 → unzip (war = zip) →
  `editor/webapp/` → viết `editor/VERSION`. `/api/status` trả
  `{editor: "ready"|"downloading"|"missing", progress}` để UI hiển thị.
- Sau giải nén, patch `PreConfig.js`: `offline=1`, `local=1` (cắt external calls
  — pattern drawio-local). Serve `ServeDir` tại `/drawio/`.
- `senclaw-hub.json.permissions.network`: `["127.0.0.1", "github.com", "objects.githubusercontent.com"]`.

## 4. Kế hoạch triển khai theo pha

| Pha | Nội dung | DoD |
|---|---|---|
| **P1 — Skeleton + editor** | crate + workspace member, manifest, db, static-dir 5-candidate, editor.rs download/serve, DrawioFrame load/save round-trip | Vẽ tay, lưu, mở lại được; offline sau lần tải đầu |
| **P2 — AI generate** | llm.rs (bridge + finish check), mermaid mode + xml mode, validate/repair, ✨ panel, layout action | "vẽ flowchart đăng ký" ra sơ đồ chỉnh sửa được |
| **P3 — MCP + skills + AI edit** | mcp.rs 10 tools, drawio_edit_ai (merge/patch), skills + persona, SVG snapshot cache, ai_log | Chat channel "vẽ sơ đồ X" → link mở app có sơ đồ |
| **P4 — Đóng gói** | pack.sh, hub.json, widget "sơ đồ gần đây", test register-local + install từ zip, verify hub 20MB | zip < 20MB, install sạch trên máy trắng |

## 5. Rủi ro & điểm cần verify khi build

1. **Mermaid convert trong bản war**: verify sau P1; nếu thiếu → XML mode là mặc định.
2. **Embed mode trên bản self-host cần đúng entry** (`/drawio/?embed=1` — bản war
   dùng `index.html`/`app.html`; drawio-local xác nhận chạy được, cần thử đường dẫn thật).
3. **Trần output bridge** ([memory: bridge output ceiling]) — đo thực tế số node
   tối đa/lần sinh với model đang active, chỉnh cap 40 nếu cần.
4. Hub catalog dynamic có thể rỗng — không block: register-local là đường dev chính.
5. `draw.war` đổi layout giữa version — pin version, chỉ nâng có chủ đích.

## Nguồn

- [Embed mode — draw.io](https://www.drawio.com/doc/faq/embed-mode)
- [Generate and validate draw.io diagrams with AI](https://www.drawio.com/docs/reference/diagram-generation/)
- [jgraph/drawio-mcp](https://github.com/jgraph/drawio-mcp) (Apache-2.0)
- [jgraph/drawio releases — draw.war v31.1.2 ≈ 52.7 MB](https://github.com/jgraph/drawio/releases)
- [tobyqin/drawio-local — pattern self-host offline](https://github.com/tobyqin/drawio-local)
- [marcveens/react-drawio](https://github.com/marcveens/react-drawio)
- [Supported URL parameters](https://www.drawio.com/docs/reference/supported-url-parameters/)
