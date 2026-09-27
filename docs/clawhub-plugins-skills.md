# ClawHub — Skills & Plugins: Thiết kế tích hợp

> Tài liệu nghiên cứu kỹ thuật — tháng 5/2026  
> Registry: [clawhub.ai/skills](https://clawhub.ai/skills) · [clawhub.ai/plugins](https://clawhub.ai/plugins)

---

## 1. Tổng quan

SenClaw sử dụng hai loại extension khác nhau:

| Loại | Nơi lưu | Scope | Định nghĩa bởi |
|---|---|---|---|
| **Skill** | `~/.senclaw/managed/skills/<slug>/` | Agent session (MCP server + system-prompt extension) | `SKILL.md` (frontmatter + body) |
| **Plugin** | `~/.senclaw/managed/plugins/<slug>/` | Toàn daemon (thêm route, service, hoặc channel adapter) | `PLUGIN.md` + optional binary |

ClawHub là marketplace quản lý cả hai. Một skill/plugin có thể:
- Được **cài từ ClawHub** (download zip → extract → wired vào agent pool hoặc daemon)
- Được **publish lên ClawHub** (zip + metadata → POST `/api/v1/skills`)
- Được **enable/disable** mà không cần uninstall

---

## 2. Skills

### 2.1 Kiến trúc hiện tại

```
~/.senclaw/
├── skills/                     ← global-compat (người dùng tự đặt)
│   └── <slug>/SKILL.md
├── managed/
│   ├── bundled-skills/         ← nhúng trong binary, tự giải nén ra đây
│   │   ├── .build              ← mốc SENCLAW_BUILD_EPOCH đã ghi thư mục này
│   │   └── <slug>/SKILL.md
│   └── skills/
│       ├── .clawhub/
│       │   └── lock.json       ← lockfile (version, installedAt)
│       └── <slug>/
│           ├── SKILL.md
│           └── .clawhub/
│               └── origin.json ← registry, slug, installedVersion
└── disabled-skills.json        ← danh sách slug bị tắt
```

**Scan priority** (cao → thấp):
1. Bundled (`~/.senclaw/managed/bundled-skills/`, xem 2.1b)
2. ClawHub-managed (`~/.senclaw/managed/skills/`)
3. Global-sema (`~/.sema/skills/`)
4. Global-compat (`~/.senclaw/skills/`)
5. Workspace-local (`.senclaw/skills/` trong working dir)

### 2.1b Skill builtin đi trong binary

18 skill trong `skills/` của repo (`pattern`, `web-research`, `agent-browser`,
`code`, `wiki`, `workflow`…) được build script gói thẳng vào binary bằng
`include_bytes!`, cùng cách `assets/patterns/` đã làm. Lúc chạy,
`skills::bundled::unpack` giải chúng ra `~/.senclaw/managed/bundled-skills/`
rồi scanner đọc như mọi nguồn thư mục khác.

**Vì sao không đọc thẳng từ `skills/` của repo.** `Paths::bundled_skills_dir`
phân giải theo `SENCLAW_BUNDLED_SKILLS_DIR`, nếu không thì
`CARGO_MANIFEST_DIR/skills` — một đường dẫn **nướng lúc biên dịch**, tức cây
nguồn của máy build. Nó đúng với người chạy từ checkout và sai với tất cả những
người còn lại: binary tải từ release đi tìm một thư mục không tồn tại trên đĩa
người dùng, không tìm thấy gì, và phục vụ **0 skill builtin** — im lặng, không
log một dòng nào.

Thư mục giải nén thuộc về binary chứ không thuộc người dùng: nó bị thay trọn
gói mỗi khi mốc `.build` khác `SENCLAW_BUILD_EPOCH` hiện hành, để một skill bị
bỏ ở bản mới không sống sót qua lần nâng cấp. Sửa tay thì sửa ở nguồn ghi được
(`~/.claude/skills`, clawhub-managed) — scanner đọc riêng và không bao giờ đụng
tới chúng.

`SENCLAW_BUNDLED_SKILLS_DIR` (hoặc `CARGO_MANIFEST_DIR/skills` khi tồn tại) vẫn
thắng: người phát triển sửa `skills/` trong checkout và muốn lần quét kế tiếp
thấy ngay, không phải ảnh chụp lúc build binary.

### 2.2 SKILL.md schema

```yaml
---
name: my-skill
description: "Mô tả ngắn"
version: "1.2.0"
mcp_servers:
  - senclaw-code
  - senclaw-wiki
personas:
  - name: code-agent
    file: personas/code-agent.md
tags: [code, productivity]
min_senclaw: "0.5.0"
---

# System prompt extension

Nội dung bổ sung vào system prompt của agent khi skill được load.
```

### 2.3 API REST (đã có)

| Method | Path | Mô tả |
|---|---|---|
| `GET` | `/api/skills` | Danh sách skills đã cài (local) |
| `GET` | `/api/skills/remote-search?q=` | Tìm kiếm trên ClawHub registry |
| `POST` | `/api/skills/install` | Cài skill từ ClawHub (`{ slug }`) |
| `GET` | `/api/skills/:name/readme` | Đọc nội dung SKILL.md |
| `PUT` | `/api/skills/:name/readme` | Ghi đè SKILL.md |
| `POST` | `/api/skills/:name/enable` | Enable skill |
| `POST` | `/api/skills/:name/disable` | Disable skill |

### 2.4 Database (cần thêm)

```sql
-- Theo dõi trạng thái cài đặt trong DB (bổ sung cho lockfile)
CREATE TABLE IF NOT EXISTS installed_skills (
    slug            TEXT PRIMARY KEY,
    display_name    TEXT,
    summary         TEXT,
    version         TEXT NOT NULL,
    registry        TEXT NOT NULL DEFAULT 'https://lightmake.site',
    source          TEXT NOT NULL DEFAULT 'clawhub',  -- 'clawhub' | 'local'
    enabled         INTEGER NOT NULL DEFAULT 1,
    installed_at    INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    manifest_json   TEXT                              -- cached SKILL.md frontmatter
);

CREATE INDEX IF NOT EXISTS idx_installed_skills_enabled
    ON installed_skills(enabled, source);
```

**Lý do cần DB song song với lockfile:** lockfile là file-based (tương thích CLI), DB cho phép query từ UI, phân trang, filter theo tag/enabled, và join với bảng khác.

---

## 3. Plugins

### 3.1 Khái niệm

Plugin là extension **cấp daemon** — không phải skill cấp agent. Một plugin có thể:

- Thêm **HTTP route** vào axum router (ví dụ: OAuth callback handler)
- Thêm **WebSocket handler** mới
- Cung cấp **channel adapter** (ví dụ: Discord, Slack, LINE)
- Expose **MCP server** mới được wired tự động
- Thêm **cron job** chạy trong daemon

### 3.2 Plugin structure

```
~/.senclaw/managed/plugins/<slug>/
├── PLUGIN.md          ← manifest (frontmatter + docs)
├── plugin.toml        ← cấu hình runtime
├── .clawhub/
│   └── origin.json
└── bin/
    └── senclaw-plugin-<slug>   ← optional binary (stdio MCP hoặc HTTP proxy)
```

**PLUGIN.md frontmatter:**

```yaml
---
name: discord-channel
display_name: "Discord Channel Adapter"
version: "2.1.0"
description: "Connect SenClaw agents to Discord servers"
plugin_type: channel_adapter    # channel_adapter | mcp_server | http_route | cron
entry_point: bin/senclaw-plugin-discord
env_vars:
  - DISCORD_BOT_TOKEN
  - DISCORD_GUILD_ID
routes:
  - POST /api/channels/discord/webhook
permissions:
  - send_message
  - receive_message
min_senclaw: "0.5.0"
tags: [messaging, discord]
---
```

**plugin_type values:**

| Type | Mô tả | Wiring |
|---|---|---|
| `channel_adapter` | Thêm channel vào `ChannelRegistry` | Spawn subprocess, giao tiếp qua stdio JSON-RPC |
| `mcp_server` | MCP server thêm tools cho agents | Register vào `McpManager` |
| `http_route` | Thêm HTTP route vào axum router | Plugin expose HTTP trên local port, daemon reverse-proxy |
| `cron` | Cron job chạy trong daemon | Đăng ký với `TaskScheduler` |

### 3.3 Database

```sql
CREATE TABLE IF NOT EXISTS installed_plugins (
    slug            TEXT PRIMARY KEY,
    display_name    TEXT,
    summary         TEXT,
    version         TEXT NOT NULL,
    plugin_type     TEXT NOT NULL,    -- channel_adapter | mcp_server | http_route | cron
    registry        TEXT NOT NULL DEFAULT 'https://lightmake.site',
    enabled         INTEGER NOT NULL DEFAULT 1,
    installed_at    INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    config_json     TEXT NOT NULL DEFAULT '{}',   -- user-supplied env/config
    manifest_json   TEXT                          -- cached PLUGIN.md frontmatter
);

CREATE INDEX IF NOT EXISTS idx_installed_plugins_type
    ON installed_plugins(plugin_type, enabled);

-- Runtime state (spawned process info)
CREATE TABLE IF NOT EXISTS plugin_runtime (
    slug        TEXT PRIMARY KEY REFERENCES installed_plugins(slug) ON DELETE CASCADE,
    status      TEXT NOT NULL DEFAULT 'stopped',  -- stopped | starting | running | error
    pid         INTEGER,
    port        INTEGER,                           -- local port nếu http_route
    started_at  INTEGER,
    error_msg   TEXT,
    last_ping   INTEGER
);
```

---

## 4. ClawHub Registry API

Base URL: `https://lightmake.site` (env: `CLAWHUB_REGISTRY`)  
Auth: Bearer token (lưu tại `~/Library/Application Support/clawhub/config.json`)

### 4.1 Skills endpoints

| Method | Path | Auth | Mô tả |
|---|---|---|---|
| `GET` | `/api/v1/search?q=&limit=&type=skill` | No | Full-text search skills |
| `GET` | `/api/v1/skills/:slug` | No | Metadata + versions |
| `GET` | `/api/v1/skills/:slug/download?version=` | No | Download zip |
| `POST` | `/api/v1/skills` | Yes | Publish (multipart: payload + files) |
| `GET` | `/api/v1/whoami` | Yes | Thông tin user đang đăng nhập |

### 4.2 Plugins endpoints (cần thêm)

| Method | Path | Auth | Mô tả |
|---|---|---|---|
| `GET` | `/api/v1/search?q=&type=plugin` | No | Tìm kiếm plugins |
| `GET` | `/api/v1/plugins/:slug` | No | Metadata plugin |
| `GET` | `/api/v1/plugins/:slug/download?version=` | No | Download zip |
| `POST` | `/api/v1/plugins` | Yes | Publish plugin |

### 4.3 Moderation

Mỗi package trả về `moderation: { isSuspicious, isMalwareBlocked }`.  
`isMalwareBlocked = true` → từ chối install (trả về `403 Forbidden`).

---

## 5. Install/Uninstall flow

### 5.1 Install skill

```
POST /api/skills/install { slug }
  │
  ├─ validate slug (alphanumeric + _ -)
  ├─ GET /api/v1/skills/:slug → check moderation
  ├─ GET /api/v1/skills/:slug/download?version=<latest>
  ├─ extract zip → ~/.senclaw/managed/skills/<slug>/
  ├─ write .clawhub/origin.json
  ├─ update lock.json (lockfile)
  ├─ INSERT OR REPLACE INTO installed_skills (...)
  ├─ agent_api.reload_all_skills()
  └─ emit_skills_refresh (WebSocket broadcast)
```

### 5.2 Uninstall skill (cần thêm)

```
DELETE /api/skills/:slug
  │
  ├─ kiểm tra slug tồn tại trong installed_skills
  ├─ tokio::fs::remove_dir_all(managed_skills_dir/<slug>)
  ├─ xoá khỏi lock.json
  ├─ DELETE FROM installed_skills WHERE slug = ?
  ├─ xoá khỏi disabled-skills.json nếu có
  ├─ agent_api.reload_all_skills()
  └─ emit_skills_refresh
```

### 5.3 Install plugin

```
POST /api/plugins/install { slug, config }
  │
  ├─ validate slug
  ├─ GET /api/v1/plugins/:slug → check moderation
  ├─ GET /api/v1/plugins/:slug/download?version=<latest>
  ├─ extract zip → ~/.senclaw/managed/plugins/<slug>/
  ├─ parse PLUGIN.md → plugin_type, entry_point, env_vars
  ├─ validate config keys match env_vars
  ├─ INSERT OR REPLACE INTO installed_plugins (...)
  ├─ INSERT INTO plugin_runtime (slug, status='stopped')
  ├─ spawn plugin subprocess (nếu enabled)
  └─ emit_plugins_refresh (WebSocket broadcast)
```

### 5.4 Uninstall plugin

```
DELETE /api/plugins/:slug
  │
  ├─ kill subprocess (SIGTERM, wait 5s, SIGKILL)
  ├─ remove_dir_all(managed_plugins_dir/<slug>)
  ├─ DELETE FROM installed_plugins WHERE slug = ?
  ├─ DELETE FROM plugin_runtime WHERE slug = ?
  └─ emit_plugins_refresh
```

---

## 6. REST API mở rộng (cần thêm vào core.rs)

### Skills

```
DELETE /api/skills/:slug              → uninstall
GET    /api/skills/:slug/versions     → danh sách versions từ registry
POST   /api/skills/:slug/update       → cập nhật lên version mới nhất
```

### Plugins

```
GET    /api/plugins                   → danh sách plugins đã cài
GET    /api/plugins/remote-search?q=  → tìm trên ClawHub
POST   /api/plugins/install           → cài { slug, config_json }
DELETE /api/plugins/:slug             → gỡ cài
GET    /api/plugins/:slug             → chi tiết + runtime status
POST   /api/plugins/:slug/enable      → bật plugin + spawn process
POST   /api/plugins/:slug/disable     → tắt + kill process
POST   /api/plugins/:slug/configure   → cập nhật config_json (env vars)
GET    /api/plugins/:slug/logs        → stdout/stderr gần nhất
```

---

## 7. Rust module plan

### 7.1 Skills (bổ sung)

```
src/skills/
├── mod.rs
├── disabled.rs    ← đã có
├── expand.rs      ← đã có (expand SKILL.md template vars)
├── scan.rs        ← đã có (load_all_local_skills)
└── db.rs          ← MỚI: installed_skills DB CRUD
```

`src/skills/db.rs`:
```rust
pub fn upsert_installed_skill(db: &Db, slug: &str, meta: &SkillDbRecord) -> Result<()>
pub fn delete_installed_skill(db: &Db, slug: &str) -> Result<()>
pub fn list_installed_skills(db: &Db) -> Result<Vec<SkillDbRecord>>
pub fn get_installed_skill(db: &Db, slug: &str) -> Result<Option<SkillDbRecord>>
```

### 7.2 Plugins (mới hoàn toàn)

```
src/plugins/
├── mod.rs
├── manifest.rs    ← parse PLUGIN.md frontmatter
├── manager.rs     ← PluginManager: spawn/kill subprocesses
├── registry.rs    ← scan managed/plugins/
└── db.rs          ← installed_plugins + plugin_runtime CRUD
```

`src/gateway/ui_server/plugins.rs` — HTTP handlers:
```rust
pub(crate) async fn plugins_list(...)
pub(crate) async fn plugins_remote_search(...)
pub(crate) async fn plugins_install(...)
pub(crate) async fn plugins_uninstall(...)
pub(crate) async fn plugins_get(...)
pub(crate) async fn plugins_toggle(...)
pub(crate) async fn plugins_configure(...)
pub(crate) async fn plugins_logs(...)
```

### 7.3 Schema additions

```rust
// src/db/schema.rs → apply_marketplace_tables()

pub(crate) fn apply_marketplace_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(r#"
        CREATE TABLE IF NOT EXISTS installed_skills ( ... );
        CREATE TABLE IF NOT EXISTS installed_plugins ( ... );
        CREATE TABLE IF NOT EXISTS plugin_runtime ( ... );
    "#)?;
    Ok(())
}
```

---

## 8. Frontend plan

### 8.1 Skills UI (SkillsPanel — đã có, cần mở rộng)

Thêm vào `SkillsPanel.tsx`:
- Nút **Uninstall** trên card đã cài (gọi `DELETE /api/skills/:slug`)
- Tab **Updates** — so sánh version cài vs latest registry
- Badge số lượng skills đang disabled

### 8.2 Plugins UI (mới)

```
web/src/components/plugins/
├── PluginsPanel.tsx        ← main panel (list + search)
├── PluginCard.tsx          ← card hiển thị một plugin
├── PluginConfigModal.tsx   ← form nhập env vars khi cài
└── PluginDetailDrawer.tsx  ← chi tiết + logs + status
```

**Luồng cài plugin từ UI:**
1. Search trên remote tab → click **Install**
2. `PluginConfigModal` hiện ra form các `env_vars` từ manifest
3. Submit → `POST /api/plugins/install { slug, config_json }`
4. Daemon spawn process → WebSocket broadcast `plugins_refresh`
5. UI cập nhật status chip: `stopped → starting → running`

---

## 9. Security

| Rủi ro | Biện pháp |
|---|---|
| Malicious package | `moderation.isMalwareBlocked` check trước khi extract |
| Path traversal trong zip | `extract_zip_to_dir` validate mọi entry path trước khi ghi |
| Plugin binary exec | Chỉ chạy binary trong `managed/plugins/<slug>/bin/` (canonical path check) |
| Env var injection | Whitelist keys từ `env_vars` trong manifest; reject keys không khai báo |
| Privilege escalation | Plugin subprocess chạy với UID của daemon, không có elevated perms |
| Registry spoofing | Chỉ trust `CLAWHUB_REGISTRY` env hoặc default hardcoded |

---

## 10. Compatibility

| Loại | Tương thích ngược |
|---|---|
| Lockfile (`lock.json`) | Giữ nguyên format (version 1), DB là bổ sung |
| `disabled-skills.json` | Giữ nguyên; DB `enabled` column sync khi read |
| Bundled skills | Không bao giờ bị uninstall (source = 'bundled', chỉ disable) |
| CLI `senclaw clawhub install <slug>` | Vẫn hoạt động, ghi lockfile + origin; DB được sync khi daemon start |
