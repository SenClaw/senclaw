# Tra cứu tên MCP tool & skill (Tool/Skill Name Lookup)

Tài liệu chuẩn để tìm **đúng** tên tool MCP và skill trong SenClaw — cho cả người viết skill lẫn agent đang chạy. Viết sai tên (rút gọn, đoán mò) là nguyên nhân số 1 của lỗi "No such tool available".

## 1. Hai họ tên MCP server

### a) Server bundled trong daemon (`senclaw-*`)

```
mcp__senclaw-<domain>__<prefix>_<verb>[_<modifier>]
```

Ví dụ: `mcp__senclaw-browser__browser_navigate`, `mcp__senclaw-memory__memory_search`, `mcp__senclaw-cognitive__cog_search` (cognitive là ngoại lệ duy nhất: prefix `cog_`).

- **Nguồn sự thật tên server:** các builder `*_mcp_config()` trong `src/mcp/helper.rs`.
- **Nguồn sự thật tên tool:** các `#[rmcp::tool] async fn <name>` trong `src/mcp/<domain>_server.rs`.
- Bảng registry đầy đủ nằm trong `CLAUDE.md` (mục "SenClaw MCP naming convention").
- **Trong agent SenClaw (mặc định `mcp.bundled`) model thấy `mcp__core__<tool>`**
  (`mcp__core__browser_search`); với `SENCLAW_MCP_BUNDLED=false` là `mcp__<domain>__<verb>`
  (`mcp__browser__search`). Resolver (`resolve_across_layouts` trong
  `src/tools/tool_search.rs`) nối cả ba cách viết — `mcp__senclaw-browser__browser_search`,
  `mcp__browser__search`, `mcp__core__browser_search` — cho cả `select:` lẫn lời gọi trực tiếp.
  Tên **trần** (`browser_navigate`) thì mơ hồ: `mini-browser-mcp` dùng lại 18 tên `browser_*`.

### b) Server của Space App (`<app>-mcp`)

```
mcp__<mcp.name trong senclaw-manifest.json>__<tool>
```

Tên server **không** suy ra từ id app — nó là giá trị `mcp.name` khai báo trong manifest. Ví dụ app `ssh-manager`:

```json
"mcp": { "name": "ssh-manager-mcp", "transport": "http", "path": "/api/mcp/sse", "autoRegister": true }
```

→ tool đầy đủ: `mcp__ssh-manager-mcp__ssh_list_hosts`, `mcp__ssh-manager-mcp__ssh_execute_command`, …

- **Nguồn sự thật tên server:** `apps/<app>/senclaw-manifest.json` → trường `mcp.name`.
- **Nguồn sự thật tên tool:** danh sách `tools/list` trong `apps/<app>/src/mcp.rs` (grep `"name": "` trong JSON tools).
- Một số tên server không theo mẫu `<id>-mcp` (vd. luna-calendar → `luna-mcp`) — luôn đọc manifest, đừng đoán.

### c) Server của plugin marketplace (`mkt__<plugin>__<server>`)

Một plugin khai báo MCP server trong `.mcp.json` ở thư mục gốc của nó. Daemon
đọc **cả hai dạng** file: dạng lồng `{"mcpServers": {…}}` của SenClaw và dạng
**phẳng** `{"<tên>": {command,args,env}}` của plugin Claude Code. Đọc thiếu một
dạng làm file kia trông như rỗng — không server nào chạy, và trình quét bảo mật
cũng không thấy lệnh nào để soi.

Tên server tới model được tiền tố theo plugin để hai plugin không giẫm tên nhau:

```
mcp__mkt__<tên plugin>__<tên server trong .mcp.json>__<tool>
```

- `${CLAUDE_PLUGIN_ROOT}` trong `command`, `args`, `url` và mọi giá trị `env`
  được thay bằng thư mục plugin; daemon còn tiêm sẵn `CLAUDE_PLUGIN_ROOT` và
  `SENCLAW_PLUGIN_ROOT` vào env.
- Server sống trên **`McpManager` dùng chung**, không phải spawn riêng từng
  phiên chat — nên nó hiện ở `GET /api/mcp-servers` như mọi server ngoài khác,
  và một plugin chỉ có một tiến trình.
- Đăng ký **không ghi vào `~/.senclaw/mcp.json`**: server thuộc về plugin. Bật
  / tắt / gỡ plugin là dựng lại tập server ngay, không cần restart.
- Lệnh mang động từ phá huỷ (`rm`, `sudo`, chuỗi `;`/`&&`) **không** được đăng
  ký chỉ vì người dùng bật plugin. `GET /api/marketplace/mcp-status` liệt kê
  từng server đã khai báo kèm `registered` và `reason` nói vì sao bị bỏ.

## 2. Tra cứu lúc runtime

### Server nào đang đăng ký & trạng thái

```bash
curl -s http://127.0.0.1:18788/api/mcp-servers
```

Trả về từng server với `status: connected | error` và `url`. Nếu server của app không có mặt: app chưa chạy hoặc `autoRegister` thất bại — mở Space Apps UI hoặc restart daemon.

### Nạp tool trong phiên agent (ToolSearch)

- **Nạp đích danh, MỘT lệnh:** `ToolSearch` query
  `select:mcp__ssh-manager-mcp__ssh_list_hosts,mcp__ssh-manager-mcp__ssh_execute_command`
- **Tìm theo từ khóa:** `ToolSearch` query `ssh connect` — kết quả gồm cả tool (deferred) lẫn **skill** (resolver with_skills).
- ToolSearch **không phân biệt `-` và `_`** ở tên server: `mcp__ssh_manager_mcp__ssh_list_hosts` vẫn resolve về server `ssh-manager-mcp` (xem `src/tools/tool_search.rs::canonicalize`).
- **Không tồn tại dạng rút gọn**: `mcp__browser__*`, `mcp__ssh__*`, `mcp__ssh-manager__*` đều KHÔNG resolve.

### Khi ToolSearch trả 0 kết quả

Đọc kỹ output: nếu `deferred_total: 0` thì phiên này **không có tool MCP nào cả** — vấn đề không phải sai tên. Kiểm tra theo thứ tự:

1. **Whitelist `allowed_tools` của group** (bẫy phổ biến nhất): nếu cột `groups.allowed_tools` khác rỗng, phiên chỉ thấy đúng các tool trong danh sách đó.
   ```bash
   sqlite3 ~/.senclaw/senclaw.db "SELECT jid, allowed_tools FROM groups WHERE allowed_tools IS NOT NULL AND allowed_tools != '';"
   ```
   Lịch sử: trước bản vá tháng 7/2026, bấm "Always allow" ở permission prompt sẽ append tên tool vào chính cột này → session sau chỉ còn đúng tool đó (vd. `["Skill"]` tước sạch mọi MCP tool của phiên schedule SSH). Từ bản vá, lựa chọn "Always allow" lưu vào cột riêng `approved_tools`; `allowed_tools` chỉ còn là whitelist do người dùng chủ đích cấu hình. Log daemon in `set_use_tools: [...]` khi whitelist được áp.
2. **App chưa chạy / MCP chưa đăng ký:** kiểm tra `/api/mcp-servers` như trên.
3. **Daemon chưa restart** sau khi đăng ký/cài mới.

Agent gặp tình huống này phải **báo người dùng và dừng** — không thay thế bằng Bash/ssh cục bộ, không đoán tên khác.

## 3. Tra cứu skill

- **Skill đã cài (bản thật được nạp vào phiên):** `~/.senclaw/managed/skills/<name>/SKILL.md`.
- **Skill nguồn của Space App:** `apps/<app>/skills/<name>/SKILL.md` + khai báo trong `senclaw-manifest.json` → `skills[]` (name, path, triggers). Sau khi sửa nguồn phải đồng bộ sang bản đã cài (và bản deploy trong `<workspace>/space-apps/<app>/skills/` nếu có).
- **Tắt/bật:** `~/.senclaw/disabled-skills.json`.
- Trong phiên agent, ToolSearch theo từ khóa cũng trả về skill (trường `skills` trong kết quả) kèm cách invoke: `Skill { "skill": "<name>" }`.

## 4. Quy tắc khi viết/sửa SKILL.md có nhắc tool MCP

1. Ghi **tên đầy đủ** `mcp__<server>__<tool>` — copy nguyên văn từ nguồn sự thật (mục 1), không gõ tay theo trí nhớ.
2. Thêm mục "Tool names & availability" đầu skill. Khi skill được nạp, engine **tự nạp sẵn** mọi tool
   skill ghi tên đầy đủ (`apply_skill_activation`) — nên bước `ToolSearch select:...` phải ghi rõ là
   *chỉ khi tool chưa có trong danh sách*; bắt buộc gọi trước là tốn một vòng LLM vô ích. Một `select:`
   trượt giờ trả về các tool gần nhất của đúng server đó (đã nạp) hoặc báo server không có tool nào.
3. Không trộn server: skill của SenClaw/Space App không được trỏ sang Playwright hay MCP browser khác.
4. Kiểm chứng nhanh trước khi commit:
   ```bash
   grep -o 'mcp__[a-z0-9_-]*__[a-z0-9_]*' SKILL.md | sort -u   # tên nhắc trong skill
   curl -s http://127.0.0.1:18788/api/mcp-servers               # server có thật + connected?
   ```
