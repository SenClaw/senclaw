# Auto Accept Tool Request — Thiết kế tính năng

> **Trạng thái:** Thiết kế gốc — phần lớn đã triển khai, xem §0 trước khi đọc tiếp  
> **Phạm vi:** `src/zen_core/permissions.rs`, `src/agent/permission_bridge/bridge.rs`, `src/mcp/`, Web UI  
> **Mục tiêu:** Cho phép cấu hình linh hoạt việc tự động chấp nhận / từ chối yêu cầu tool mà không cần tương tác thủ công mỗi lần.

---

## 0. Thực tế đã triển khai

Tài liệu từ §1 trở đi là **thiết kế gốc**; một số chi tiết không khớp code đang chạy.
Khác biệt cần biết trước:

| Thiết kế gốc | Thực tế |
|---|---|
| `tool-rules.json` tại `Config::data_dir()` | Bảng SQLite **`tool_rules`** (`id`, `rule_json`, `updated_at`) — xem [`src/db/tool_rules.rs`](../src/db/tool_rules.rs) |
| `src/zen_core/permission_rules.rs` (module mới) | Không tồn tại. Rule sống trong [`permission_bridge`](../src/agent/permission_bridge/) (`types.rs` + `bridge.rs::rule_matches`) |
| `RuleEngine` gọi trong `PermissionManager` | Rule được đánh giá ở **`PermissionBridge::should_auto_accept`**, tức *trước khi* thẻ permission được tạo — sớm hơn `PermissionManager` |

### 0.1 Hai kho quyền song song

Có **hai** nơi lưu "đã duyệt", phạm vi khác nhau, và cả hai đều còn hiệu lực:

| | `tool_rules` (toàn cục) | `groups.approved_tools` (theo chat) |
|---|---|---|
| Chặn ở | `PermissionBridge::should_auto_accept` | `PermissionManager::is_allowed` |
| Phạm vi | mọi chat | đúng một `group_jid` |
| Đặt từ | Plugins → Skills (*Auto Access*), Tool Rules panel, **và nút "never ask again"** | nút "never ask again" |

### 0.2 Nút "Confirm, never ask again" làm gì

Một lần bấm ghi **cả hai**: grant theo chat *và* một rule toàn cục. Rule được suy ra từ
`permission_key` nên chỉ cấp đúng thứ người dùng nhìn thấy trên thẻ, không rộng hơn nhãn nút:

| Tool | `permission_key` | Rule sinh ra |
|---|---|---|
| Skill | `Skill(<tên>)` | `skill-auto-access:<tên>` — `SkillExact` |
| Bash (lệnh) | `Bash(<lệnh>)` | `bash-auto-access:<lệnh>` — `BashRegex` `^<lệnh>$` |
| Bash (prefix) | `Bash(<prefix>:*)` | `BashRegex` `^<prefix>(\s\|$)` — `git` không nuốt `github-cli` |
| MCP | `mcp__<server>__<tool>` | `mcp:<server>:<tool>` — `McpServer` scope đúng 1 tool, không phải cả server |
| Edit/Write/NotebookEdit | tên tool | `tool-category:file-edit` — `ToolCategory::FileEdit` |
| Khác | tên tool | `tool-exact:<tên>` — `ToolExact` |

Id trùng convention của Web UI nên rule sinh ra **hiện và gỡ được** ở Plugins → Skills /
Tool Rules — khác `approved_tools` vốn vô hình.

### 0.3 Ba bẫy đã sửa, đừng tái lập

- **`permission_key` ≠ `tool_name`.** `tool_name` route response về engine (khoá của
  `ResponseRegistry`); `permission_key` là thứ đem đi lưu. Lưu nhầm `tool_name` khiến mọi
  duyệt Skill/Bash không bao giờ khớp lại khi engine mới dựng — thẻ permission quay lại mãi.
- **Pattern Bash khớp *câu lệnh*, không phải tên tool.** `BashGlob`/`BashRegex` từng
  `glob_match(pattern, tool_name)` với `tool_name` luôn là `"Bash"`, nên mọi rule Bash là code
  chết. Lệnh nằm ở `content["command"]`. `McpGlob` thì vẫn khớp tên tool — đừng gộp lại.
- **Nhánh file-edit phải đọc `allowed_tools`.** `global_edit_granted` sống theo engine, nên
  nếu không đọc grant đã lưu thì "never ask for file editing" chết sau mỗi lần restart.

---

## 1. Tổng quan hệ thống hiện tại

### 1.1 Kiến trúc permission hiện có

```
Agent gọi tool
     │
     ▼
run_tools.rs — is_read_only()?
     │ không (write tool)
     ▼
PermissionChecker::check()
     │
     ├─ File Edit  → skip_file_edit / global_edit_granted / request_permission()
     ├─ Bash       → skip_bash / SAFE_COMMANDS / allowed_tools / request_permission()
     ├─ Skill      → skip_skill / allowed_tools / request_permission()
     ├─ MCP        → skip_mcp / allowed_tools / request_permission()
     └─ Khác       → default allow (Ok(true))
          │
          ▼
     PermissionManager::request_permission()
          │ emit EngineEvent::ToolPermissionRequest
          ▼
     PermissionBridge — notify WS / Telegram
          │
          ▼
     User chọn: agree / allow / refuse
```

### 1.2 Cơ chế hiện tại

| Cơ chế | Vị trí | Mô tả |
|--------|--------|-------|
| `skip_file_edit/bash/skill/mcp` | `PermissionManager` | Bỏ qua hoàn toàn nhóm tool |
| `SAFE_COMMANDS` | `permissions.rs:30` | Whitelist lệnh bash luôn cho phép |
| `allowed_tools: HashSet<String>` | `permissions.rs:72` | Allowlist theo key (`Bash(cmd)`, `mcp__server__tool`) |
| `global_edit_granted` | session | "Never ask for file editing" cho cả session |
| `AllowAllPermissions` | `run_tools.rs:37` | No-op checker, dùng khi disable hoàn toàn |
| `VirtualWorkerPool.skip_perms` | `virtual_worker_pool.rs:531` | Auto-refuse nếu còn request; skip nếu cờ bật |

### 1.3 Hạn chế hiện tại

- Không có rule dạng pattern/glob (vd: `git *`, `npm run *`)
- Không có deny-list (force request dù skip flag bật)
- Không phân biệt auto-accept theo MCP server (chỉ per-tool)
- Không có cấu hình persistent (allowlist reset sau restart)
- Không có rule theo group/agent context

---

## 2. Thiết kế tính năng mới

### 2.1 Mô hình Rule

```rust
/// Một rule auto-accept/deny cho tool request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAutoAcceptRule {
    /// ID duy nhất của rule (uuid hoặc slug người dùng đặt)
    pub id: String,

    /// Điều kiện khớp tool
    pub matcher: RuleMatcher,

    /// Hành động khi khớp
    pub action: RuleAction,

    /// Chỉ áp dụng cho group/agent cụ thể (None = áp dụng toàn cục)
    pub scope: Option<RuleScope>,

    /// Rule có đang bật không
    pub enabled: bool,

    /// Mô tả do người dùng đặt
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuleMatcher {
    /// Bash command khớp glob pattern (vd: "git *", "npm run *", "docker *")
    BashGlob { pattern: String },

    /// Bash command khớp regex
    BashRegex { pattern: String },

    /// Tên tool khớp chính xác (vd: "Edit", "Write", "Skill")
    ToolExact { tool_name: String },

    /// MCP server khớp (vd: server = "filesystem", tool = None → tất cả tool của server)
    McpServer { server: String, tool: Option<String> },

    /// MCP tool khớp glob (vd: "mcp__memory__*")
    McpGlob { pattern: String },

    /// Nhóm tool theo category
    ToolCategory { category: ToolCategory },

    /// Luôn khớp (dùng cho rule "accept all")
    Always,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    FileEdit,
    Bash,
    Skill,
    Mcp,
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    /// Tự động chấp nhận (không hỏi user)
    AutoAccept,
    /// Tự động từ chối (không hỏi user)
    AutoDeny,
    /// Bắt buộc hỏi user dù skip flag đang bật
    ForceRequest,
    /// Tự động chấp nhận và lưu vào allowlist (persistent)
    AutoAcceptAndAllow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleScope {
    /// Áp dụng cho group JID cụ thể
    pub group_jid: Option<String>,
    /// Áp dụng cho agent ID cụ thể
    pub agent_id: Option<String>,
}
```

### 2.2 Rule Engine

```rust
pub struct RuleEngine {
    rules: RwLock<Vec<ToolAutoAcceptRule>>,
}

impl RuleEngine {
    /// Đánh giá rules theo thứ tự, trả về action đầu tiên khớp.
    /// Rules được sắp xếp: ForceRequest > AutoDeny > AutoAccept > không khớp.
    pub fn evaluate(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
        context: &RuleContext,
    ) -> Option<RuleAction>;

    /// Load rules từ file JSON (persistent storage).
    pub fn load_from_file(path: &Path) -> Result<Self>;

    /// Lưu rules ra file JSON.
    pub fn save_to_file(&self, path: &Path) -> Result<()>;

    /// Thêm rule mới (gọi từ command hoặc UI).
    pub fn add_rule(&self, rule: ToolAutoAcceptRule);

    /// Xóa rule theo ID.
    pub fn remove_rule(&self, id: &str) -> bool;
}

pub struct RuleContext<'a> {
    pub group_jid: Option<&'a str>,
    pub agent_id: &'a str,
}
```

### 2.3 Tích hợp vào `PermissionManager`

`PermissionManager` nhận thêm `rule_engine: Arc<RuleEngine>` và kiểm tra trước các skip flag:

```rust
// Thứ tự ưu tiên mới trong PermissionManager::check()
//
// 1. RuleEngine::evaluate() → ForceRequest, AutoDeny, AutoAccept
// 2. skip_* flags (existing)
// 3. SAFE_COMMANDS / global_edit / allowed_tools (existing)
// 4. request_permission() → hỏi user

async fn check(&self, tool, input, cancel, agent_id) -> Result<bool> {
    // Bước 0: Nếu tool là read-only → skip (run_tools.rs đã lọc)

    // Bước 1: Kiểm tra RuleEngine
    let ctx = RuleContext { group_jid: ..., agent_id };
    if let Some(action) = self.rule_engine.evaluate(tool.name(), input, &ctx) {
        match action {
            RuleAction::ForceRequest => {
                return self.request_permission(tool, input, None, cancel, agent_id).await;
            }
            RuleAction::AutoDeny => return Ok(false),
            RuleAction::AutoAccept => return Ok(true),
            RuleAction::AutoAcceptAndAllow => {
                let key = Self::get_permission_key(tool, input, None);
                self.add_allowed_tool(&key);
                return Ok(true);
            }
        }
    }

    // Bước 2–4: Logic hiện tại giữ nguyên
    // ...
}
```

---

## 3. Tính năng 1 — Auto rule theo command

### 3.1 Mô tả

Cho phép cấu hình các rule bash glob/regex để tự động chấp nhận lệnh khớp pattern mà không cần mở rộng `SAFE_COMMANDS` hardcode.

### 3.2 Ví dụ config (`~/.senclaw/tool-rules.json`)

```json
[
  {
    "id": "git-all",
    "matcher": { "type": "bash_glob", "pattern": "git *" },
    "action": "auto_accept",
    "enabled": true,
    "description": "Cho phép tất cả lệnh git"
  },
  {
    "id": "npm-scripts",
    "matcher": { "type": "bash_glob", "pattern": "npm run *" },
    "action": "auto_accept",
    "enabled": true,
    "description": "Cho phép npm run scripts"
  },
  {
    "id": "docker-block",
    "matcher": { "type": "bash_regex", "pattern": "^docker\\s+(rm|rmi|system\\s+prune)" },
    "action": "force_request",
    "enabled": true,
    "description": "Luôn hỏi trước khi xóa docker resources"
  }
]
```

### 3.3 Glob matching

Sử dụng crate [`glob`](https://docs.rs/glob/) hoặc implement đơn giản:

```rust
fn bash_glob_match(pattern: &str, command: &str) -> bool {
    // "git *" khớp "git status", "git commit -m 'foo'"
    // "npm run *" khớp "npm run build", "npm run test:watch"
    let cmd = command.trim();
    if let Some(prefix) = pattern.strip_suffix(" *") {
        return cmd == prefix || cmd.starts_with(&format!("{prefix} "));
    }
    if let Some(suffix) = pattern.strip_prefix("* ") {
        return cmd.ends_with(suffix);
    }
    // Full glob via `glob` crate nếu cần
    cmd == pattern
}
```

### 3.4 Tích hợp với lệnh `/allow`

Agent hoặc user có thể gọi lệnh slash command để thêm rule:

```
/allow bash git *          → thêm BashGlob rule "git *" với action auto_accept
/allow bash npm run *      → thêm BashGlob rule "npm run *"
/deny bash rm -rf *        → thêm BashGlob rule "rm -rf *" với action force_request
/rules list                → hiển thị tất cả rules
/rules remove git-all      → xóa rule theo ID
```

---

## 4. Tính năng 2 — Trigger force request theo rule command

### 4.1 Mô tả

Ngược lại với auto-accept: khi một command/tool khớp rule `ForceRequest`, hệ thống **bắt buộc hỏi user** dù `skip_*` flag đang bật. Đây là cơ chế **deny-list** / **audit trail** — luôn có xác nhận cho các thao tác nguy hiểm.

### 4.2 Ứng dụng

```json
[
  {
    "id": "force-rm",
    "matcher": { "type": "bash_glob", "pattern": "rm *" },
    "action": "force_request",
    "enabled": true,
    "description": "Luôn xác nhận trước khi xóa file"
  },
  {
    "id": "force-prod-deploy",
    "matcher": { "type": "bash_regex", "pattern": ".*--env\\s+prod.*" },
    "action": "force_request",
    "enabled": true,
    "description": "Luôn hỏi khi deploy lên production"
  },
  {
    "id": "force-db-write",
    "matcher": {
      "type": "mcp_glob",
      "pattern": "mcp__database__*"
    },
    "action": "force_request",
    "scope": { "group_jid": "production-group@g.us" },
    "enabled": true,
    "description": "Luôn hỏi với database tools trong group production"
  }
]
```

### 4.3 Thứ tự ưu tiên rule

`ForceRequest` có độ ưu tiên **cao nhất** — override mọi skip flag và allowlist:

```
ForceRequest > AutoDeny > AutoAccept > skip flags > SAFE_COMMANDS > allowlist > request
```

Rule engine duyệt theo thứ tự này (không phải thứ tự trong mảng config):

```rust
pub fn evaluate(&self, ...) -> Option<RuleAction> {
    let rules = self.rules.read().unwrap();
    let matched: Vec<_> = rules.iter()
        .filter(|r| r.enabled && r.matches(tool_name, input, context))
        .collect();

    // ForceRequest thắng tất cả
    if matched.iter().any(|r| r.action == RuleAction::ForceRequest) {
        return Some(RuleAction::ForceRequest);
    }
    // AutoDeny thắng AutoAccept
    if matched.iter().any(|r| r.action == RuleAction::AutoDeny) {
        return Some(RuleAction::AutoDeny);
    }
    // AutoAccept
    matched.first().map(|r| r.action.clone())
}
```

---

## 5. Tính năng 3 — Auto accept theo MCP

### 5.1 Mô tả

Tự động chấp nhận các MCP tool theo server name hoặc tool name pattern, cho phép cấu hình fine-grained hơn cờ `skip_mcp` hiện tại (skip toàn bộ MCP).

### 5.2 Granularity levels

| Level | Ví dụ | Rule matcher |
|-------|-------|-------------|
| **Toàn bộ MCP** | Mọi `mcp__*` | `ToolCategory::Mcp` + `AutoAccept` |
| **Theo MCP server** | Mọi tool của server `filesystem` | `McpServer { server: "filesystem", tool: None }` |
| **Theo MCP tool cụ thể** | `mcp__filesystem__read_file` | `McpServer { server: "filesystem", tool: Some("read_file") }` |
| **Theo glob** | `mcp__memory__*` | `McpGlob { pattern: "mcp__memory__*" }` |

### 5.3 Matching logic

MCP tool name có format: `mcp__{server}__{tool}` (hai dấu `__`).

```rust
impl RuleMatcher {
    pub fn matches_tool(&self, tool_name: &str, input: &serde_json::Value) -> bool {
        match self {
            RuleMatcher::McpServer { server, tool } => {
                // tool_name = "mcp__filesystem__read_file"
                let prefix = format!("mcp__{server}__");
                if !tool_name.starts_with(&prefix) { return false; }
                match tool {
                    None => true, // tất cả tool của server này
                    Some(t) => tool_name == format!("mcp__{server}__{t}"),
                }
            }
            RuleMatcher::McpGlob { pattern } => {
                glob_match(pattern, tool_name)
            }
            // ...
        }
    }
}
```

### 5.4 Ví dụ config — trusted MCP servers

```json
[
  {
    "id": "mcp-memory-auto",
    "matcher": { "type": "mcp_server", "server": "memory", "tool": null },
    "action": "auto_accept",
    "enabled": true,
    "description": "Tất cả tools của memory MCP đều tự động chấp nhận"
  },
  {
    "id": "mcp-filesystem-read",
    "matcher": { "type": "mcp_server", "server": "filesystem", "tool": "read_file" },
    "action": "auto_accept",
    "enabled": true,
    "description": "Chỉ auto accept read_file, các tool khác vẫn cần hỏi"
  },
  {
    "id": "mcp-filesystem-write-audit",
    "matcher": { "type": "mcp_server", "server": "filesystem", "tool": "write_file" },
    "action": "force_request",
    "enabled": true,
    "description": "Luôn hỏi khi ghi file qua MCP"
  }
]
```

### 5.5 Persistent MCP allowlist

Khác với `allowed_tools` hiện tại (chỉ trong session), MCP rule với `AutoAcceptAndAllow` sẽ persist vào `tool-rules.json`:

```
Lần đầu: agent yêu cầu mcp__filesystem__read_file
→ Hỏi user → user chọn "Allow, never ask again"
→ RuleEngine tạo rule McpServer { server: "filesystem", tool: "read_file" } + AutoAcceptAndAllow
→ Lưu vào tool-rules.json
→ Lần sau: tự động chấp nhận không hỏi
```

---

## 6. Tính năng 4 — Auto accept toàn bộ

### 6.1 Mô tả

Ba mode toàn bộ, từ ít rủi ro đến toàn quyền:

| Mode | Cơ chế | Rủi ro |
|------|--------|--------|
| `yolo_bash` | `skip_bash = true` | Bash tự do |
| `yolo_all` | `AllowAllPermissions` | Không có gating |
| `yolo_mcp` | `skip_mcp = true` | MCP tự do |
| **`dangerously_accept_all`** | Rule `Always + AutoAccept` | Tương đương `AllowAllPermissions` nhưng qua rule engine |

### 6.2 `ZenCoreOptions` bổ sung

```rust
pub struct ZenCoreOptions {
    // ... existing fields ...

    /// Bật auto-accept toàn bộ (no permission prompts)
    pub dangerously_accept_all: bool,

    /// Path đến file rules JSON
    pub tool_rules_path: Option<PathBuf>,

    /// Rules inline (ưu tiên hơn file)
    pub tool_rules: Vec<ToolAutoAcceptRule>,
}
```

### 6.3 Khởi tạo engine với rule

```rust
// Trong ZenCore::new() / engine init:
let rule_engine = if opts.dangerously_accept_all {
    Arc::new(RuleEngine::with_rule(ToolAutoAcceptRule {
        id: "accept-all".into(),
        matcher: RuleMatcher::Always,
        action: RuleAction::AutoAccept,
        scope: None,
        enabled: true,
        description: Some("dangerously_accept_all mode".into()),
    }))
} else {
    let engine = RuleEngine::new();
    // Load từ file nếu có
    if let Some(path) = &opts.tool_rules_path {
        engine.load_from_file(path)?;
    }
    // Thêm inline rules
    for rule in &opts.tool_rules {
        engine.add_rule(rule.clone());
    }
    Arc::new(engine)
};
```

### 6.4 Virtual Worker Pool

Virtual workers hiện auto-refuse nếu còn permission request. Với rule engine:

```rust
// virtual_worker_pool.rs — khi skip_perms = false nhưng có rules
let rule_opts = if skip_perms {
    vec![ToolAutoAcceptRule::accept_all()] // existing behavior
} else {
    parent_rules.clone() // thừa kế rules từ main agent
};

let opts = ZenCoreOptions {
    // ...
    tool_rules: rule_opts,
    ..
};
```

Nếu virtual agent vẫn nhận `ToolPermissionRequest` (không khớp bất kỳ rule nào), giữ nguyên hành vi **auto-refuse** với warning.

---

## 7. Giao diện người dùng

### 7.1 WebSocket events mới

```typescript
// permission:rule:added
{ type: "permission:rule:added", rule: ToolAutoAcceptRule }

// permission:rule:removed
{ type: "permission:rule:removed", ruleId: string }

// permission:rule:matched
// (debug/audit) tool matched một rule và được auto-accept/deny
{ type: "permission:rule:matched", toolName: string, ruleId: string, action: RuleAction }
```

### 7.2 Web UI — Panel quản lý Rules

Thêm tab **"Tool Rules"** trong **Settings** (`SettingsPage` → `ToolRulesSettings`):

```
┌─────────────────────────────────────────────────────────┐
│ Tool Rules                                    [+ Add Rule]│
├─────────────────────────────────────────────────────────┤
│ ● git-all    Bash: git *          [Auto Accept]  [✕]     │
│ ● npm-run    Bash: npm run *      [Auto Accept]  [✕]     │
│ ● rm-audit   Bash: rm *           [Force Ask]    [✕]     │
│ ○ mcp-mem    MCP: memory/*        [Auto Accept]  [✕]     │
├─────────────────────────────────────────────────────────┤
│ [☑ Dangerously Accept All — bypass all prompts]          │
└─────────────────────────────────────────────────────────┘
```

### 7.3 Slash commands

```
/rules list                          → liệt kê rules
/rules add bash "git *" accept       → thêm bash glob rule
/rules add bash "rm *" force         → force request rule
/rules add mcp memory accept         → auto accept MCP server
/rules add mcp filesystem read_file accept  → auto accept MCP tool cụ thể
/rules remove <id>                   → xóa rule
/rules enable <id>                   → bật rule
/rules disable <id>                  → tắt rule
/rules clear                         → xóa tất cả rules
/yolo                                → bật dangerously_accept_all cho session
```

---

## 8. Lưu trữ & persistence

### 8.1 File layout

```
~/.senclaw/
├── config.json              # cấu hình daemon
├── tool-rules.json          # rules persist (global)
└── groups/
    └── {group_jid}/
        └── tool-rules.json  # rules theo group (override global)
```

### 8.2 Sơ đồ load

```
Startup:
  1. Load global tool-rules.json → RuleEngine (global)
  2. Khi agent join group → merge group tool-rules.json
  3. Group rules có scope.group_jid → ưu tiên hơn global rules
```

### 8.3 Schema `tool-rules.json`

```json
{
  "version": 1,
  "rules": [
    {
      "id": "uuid-or-slug",
      "matcher": { "type": "bash_glob", "pattern": "git *" },
      "action": "auto_accept",
      "scope": null,
      "enabled": true,
      "description": "Allow all git commands"
    }
  ]
}
```

---

## 9. Kế hoạch triển khai

### Phase 1 — Rule engine cơ bản (Bash + MCP)

- [ ] Định nghĩa `ToolAutoAcceptRule`, `RuleMatcher`, `RuleAction` trong `src/zen_core/permission_rules.rs`
- [ ] Implement `RuleEngine` với `evaluate()`, `add_rule()`, `load_from_file()`, `save_to_file()`
- [ ] Tích hợp `RuleEngine` vào `PermissionManager` (thêm field, gọi trước skip flags)
- [ ] Thêm `tool_rules_path` + `dangerously_accept_all` vào `ZenCoreOptions`
- [x] Test unit: glob matching (khớp câu lệnh, xem §0.3) — rule priority / ForceRequest chưa làm

### Phase 2 — Persistence & commands

- [ ] Load/save `tool-rules.json` tại `Config::data_dir()`
- [ ] Parse slash commands `/rules *` trong `MessageRouter`
- [ ] MCP tool `tool_rules_add`, `tool_rules_list`, `tool_rules_remove` (optional, dùng trong dispatch)
- [x] Khi user chọn "Allow, never ask again" → lưu rule (vào bảng `tool_rules`, không phải file) — xem §0.2

### Phase 3 — Web UI & WS events

- [ ] Emit `permission:rule:matched` event qua `notify.rs`
- [ ] Thêm `permission:rule:added/removed` WS messages
- [ ] Web UI: Tab "Tool Rules" trong Settings (`ToolRulesSettings.tsx`)
- [ ] Form thêm/sửa/xóa rule
- [ ] Badge hiển thị "auto-accepted via rule" trong tool call history

### Phase 4 — Group scope & Virtual worker

- [ ] Load group-specific rules, merge với global
- [ ] VirtualWorkerPool thừa kế rules từ parent agent
- [ ] Scope matching theo `group_jid` / `agent_id`
- [ ] Audit log: ghi lại khi rule auto-accept (cho admin review)

---

## 10. Tham chiếu code

| File | Vai trò |
|------|---------|
| `src/zen_core/permissions.rs` | `PermissionManager` — tích hợp `RuleEngine` tại đây |
| `src/zen_core/run_tools.rs` | `PermissionChecker` trait + `AllowAllPermissions` |
| `src/zen_core/permission_rules.rs` | **[Mới]** `RuleEngine`, `ToolAutoAcceptRule`, matchers |
| `src/agent/permission_bridge/bridge.rs` | Hiển thị request lên UI/channel |
| `src/agent/virtual_worker_pool.rs` | Thừa kế rules, auto-refuse fallback |
| `src/mcp/dispatch_server.rs` | MCP tools quản lý rules (Phase 3) |
| `src/gateway/websocket_gateway/notify.rs` | WS events `permission:rule:*` |
| `web/src/types.ts` | Type `ToolAutoAcceptRule` cho UI |
| `web/src/pages/SettingsPage.tsx` | Tab Tool Rules |
| `web/src/components/settings/ToolRulesSettings.tsx` | Panel quản lý rules |
| `web/src/components/AgentConsole.tsx` | Console dispatch / permissions (không chứa rules) |
