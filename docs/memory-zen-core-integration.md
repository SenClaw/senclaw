# Memory qua SemaCore (cũ) vs ZenCore (mới) — Nghiên cứu & Đề xuất

> Phân tích cách Memory tích hợp với agent engine: mẫu cũ (sema-core TypeScript) vs mẫu mới (zen-core Rust).
> Xác định khoảng trống và đề xuất phương án tích hợp đầy đủ.

---

## 1. Tổng quan hai mẫu tích hợp

```
┌──────────────────────────────────────────────────────────────────────────┐
│                   OLD: sema-core (TypeScript)                             │
│                                                                          │
│  ┌─────────┐    ┌──────────────┐    ┌───────────────────────────────┐   │
│  │ SOUL.md │    │ SemaEngine   │    │ Conversation Loop             │   │
│  │ MEMORY.md│───►│ formatSystem │───►│                               │   │
│  │ AGENTS.md│    │ Prompt()     │    │ Turn 1: <system-reminder>     │   │
│  └─────────┘    │              │    │   ├── SOUL.md (persona)       │   │
│                 │ generateRules│    │   ├── MEMORY.md (persistent)  │   │
│  ┌─────────┐    │ Reminders()  │    │   └── AGENTS.md (project)     │   │
│  │ FTS5    │    └──────────────┘    │                               │   │
│  │ Vector  │───► AgentPool ────────►│ Every Turn: <memory>          │   │
│  │ Index   │    preRetrieval        │   ├── search(query)           │   │
│  └─────────┘                        │   └── top 5 results           │   │
│                                     └───────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────────────┐
│                   NEW: zen-core (Rust)                                    │
│                                                                          │
│  ┌─────────┐    ┌──────────────┐    ┌───────────────────────────────┐   │
│  │ SOUL.md │    │ ZenEngine    │    │ Conversation Loop             │   │
│  │ MEMORY.md│───►│ ❌ NOT READ  │    │                               │   │
│  │ AGENTS.md│    │              │    │ System prompt:                │   │
│  └─────────┘    │ system_prompt│    │ "You are a helpful AI         │   │
│                 │ = "" (empty) │    │  assistant." ← DEFAULT        │   │
│  ┌─────────┐    └──────────────┘    │                               │   │
│  │ FTS5    │                        │ Every Turn: <memory>          │   │
│  │ Vector  │───► AgentPool ────────►│   ├── search(query) ✅        │   │
│  │ Index   │    preRetrieval        │   └── top 5 results ✅        │   │
│  └─────────┘                        └───────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────────────┘
```

### Kết luận chính

| Kênh Memory | Old (sema-core) | New (zen-core) | Status |
|---|---|---|---|
| **Static context** (SOUL.md + MEMORY.md + project context) | ✅ Injected as `<system-reminder>` in first user message | ❌ **Dropped entirely** | **REGRESSION** |
| **Dynamic retrieval** (FTS5 + vector search) | ✅ Prepended as `<memory>` every turn | ✅ Same logic preserved | OK |
| **Daily logging** | ✅ AgentPool logs User + Assistant | ✅ Same | OK |
| **Memory index init** | ✅ MemoryManager.initAgent() | ✅ Same | OK |
| **Dirty file re-index** | ✅ mark_dirty after edits | ✅ Same | OK |

---

## 2. Phân tích chi tiết: Mẫu cũ (sema-core)

### 2.1. Static Context Injection — `generateRulesReminders()`

**File:** `code-old/sema-core/dist/util/rules.js` (lines 118-147)

Đây là cơ chế inject context tĩnh vào system prompt của old sema-core:

```javascript
function generateRulesReminders() {
    const persona = readPersonaFile();   // SOUL.md
    const memory = readMemoryFile();     // MEMORY.md
    const project = readProjectConfigFile(); // AGENT.md / CLAUDE.md

    const sections = [];
    if (hasPersona) {
        sections.push(`## ${path.basename(persona.filePath)}
        — your agent persona & long-term instructions
        Path: ${persona.filePath}
        ${persona.content.trim()}`);
    }
    if (hasMemory) {
        sections.push(`## ${path.basename(memory.filePath)}
        — persistent cross-session memory
        Path: ${memory.filePath}
        ${memory.content.trim()}`);
    }
    if (hasProject) {
        sections.push(`## ${path.basename(project.filePath)}
        — current project context
        Path: ${project.filePath}
        ${project.content.trim()}`);
    }

    return `<system-reminder>
Context loaded from user and project files is shown below.
Treat persona and memory directives as authoritative;
apply project context when relevant to the current task.
${sections.join('\n\n')}
</system-reminder>`;
}
```

**Cách hoạt động:**

1. **Đọc 3 file từ disk** tại thời điểm build system prompt:
   - `SOUL.md` — agent persona, long-term instructions
   - `MEMORY.md` — persistent cross-session memory (do agent tự ghi)
   - `AGENTS.md` hoặc `CLAUDE.md` — project-level context

2. **Inject vào first user message** (KHÔNG phải system prompt):
   - `SemaEngine.processUserInput()` → `buildAdditionalReminders()` → `generateRulesReminders()`
   - Chỉ inject khi `messageHistory.length === 0` (lần đầu tiên trong session)
   - Format: `<system-reminder>...</system-reminder>` XML block trong body của user message đầu tiên

3. **Lý do inject vào user message thay vì system prompt:**
   - Anthropic API cache system prompt — nếu thay đổi MEMORY.md giữa các turn, cache bị invalidate
   - Inject vào first user message cho phép system prompt giữ nguyên (cacheable) trong khi context files vẫn được đưa vào

### 2.2. Dynamic Retrieval — Pre-Retrieval Search

**File:** `code-old/SenClaw/src/agent/AgentPool.ts` (lines 710-734)

```typescript
if (config.memory.preRetrieval) {
    const mm = MemoryManager.getInstance();
    const results = await mm.search(binding.folder, prompt, {
        maxResults: config.memory.searchMaxResults,
        minScore: config.memory.searchMinScore,
    });
    const todayFile = new Date().toISOString().slice(0, 10) + '.md';
    const filtered = results
        .filter(r => r.score >= config.memory.searchMinScore)
        .filter(r => !r.path.endsWith(todayFile))
        .slice(0, config.memory.searchMaxResults);
    const memContext = formatSearchResults(filtered);
    if (memContext) {
        fullPrompt = `<memory>\n${memContext}\n</memory>\n\n${prompt}`;
    }
}
```

**Đặc điểm:**
- Dùng chính user prompt làm search query (không query expansion)
- Search FTS5 + vector (nếu có embedding provider)
- Lọc: min_score threshold + exclude today's daily log
- Format kết quả thành `<memory>...</memory>` block prepend vào prompt

### 2.3. Tổng flow old sema-core

```
Session Start
  │
  ├── SemaEngine.formatSystemPrompt()
  │     └── Build system prompt từ template + tools + agent mode
  │
  ├── User sends message
  │
  ├── SemaEngine.processUserInput()
  │     │
  │     ├── [First turn only] buildAdditionalReminders()
  │     │     └── generateRulesReminders()
  │     │           ├── readPersonaFile() → SOUL.md
  │     │           ├── readMemoryFile() → MEMORY.md
  │     │           └── readProjectConfigFile() → AGENTS.md
  │     │     → Inject <system-reminder> into first user message
  │     │
  │     └── Conversation Loop
  │           ├── queryLLM(system_prompt, messages_with_reminders, tools)
  │           ├── Run tools
  │           └── Recurse...
  │
  └── AgentPool (outer wrapper)
        ├── Pre-retrieval: search FTS5/vector → prepend <memory> to prompt
        └── processUserInput(modified_prompt)
```

---

## 3. Phân tích chi tiết: Mẫu mới (zen-core)

### 3.1. ZenCoreOptions — System Prompt

**File:** `src/zen_core/mod.rs` (lines 480-522)

```rust
pub struct ZenCoreOptions {
    pub system_prompt: String,     // ← CÓ field này
    pub custom_rules: String,      // ← CÓ field này
    // ... 17 fields total
}

impl Default for ZenCoreOptions {
    fn default() -> Self {
        Self {
            system_prompt: String::new(),    // ← EMPTY
            custom_rules: String::new(),     // ← EMPTY, never used
            // ...
        }
    }
}
```

### 3.2. ZenEngine — Cách dùng system_prompt

**File:** `src/zen_core/engine.rs` (lines 464-469)

```rust
// Build system prompt
let system_prompt = if opts.system_prompt.is_empty() {
    "You are a helpful AI assistant.".to_string()
} else {
    opts.system_prompt.clone()
};
```

**Vấn đề:** Nếu `system_prompt` rỗng (mặc định), zen-core dùng fallback cứng `"You are a helpful AI assistant."`. Không có bất kỳ logic nào để:
- Đọc SOUL.md / MEMORY.md
- Inject persona
- Inject project context
- Gọi callback/hook để customizer system prompt

### 3.3. AgentPool — Cách tạo ZenEngine

**File:** `src/agent/agent_pool.rs` (lines 408-427)

```rust
fn ensure_engine(&self, jid: &str) -> Arc<ZenEngine> {
    let mut engines = self.engines.lock().unwrap();
    if let Some(engine) = engines.get(jid) {
        return engine.clone();
    }
    let opts = ZenCoreOptions {
        instance_id: jid.to_string(),
        ..Default::default()   // ← system_prompt = "" (empty!)
    };
    let engine = ZenEngine::new(opts, self.mcp_manager.clone());
    // ...
}
```

**Vấn đề:** `system_prompt` luôn là empty string. ZenEngine fallback về `"You are a helpful AI assistant."`. Agent không có persona, không có MEMORY.md context, không có project rules.

### 3.4. ZenCoreHandlers — Không có memory hooks

**File:** `src/zen_core/mod.rs` (lines 580-599)

```rust
pub struct ZenCoreHandlers {
    pub on_session_ready: Option<...>,
    pub on_message_complete: Option<...>,
    pub on_state_update: Option<...>,
    pub on_session_error: Option<...>,
    pub on_todos_update: Option<...>,
    pub on_conversation_usage: Option<...>,
    pub on_compact_start: Option<...>,
    pub on_compact_exec: Option<...>,
    pub on_tool_permission_request: Option<...>,
    pub on_tool_execution_complete: Option<...>,
    pub on_tool_execution_error: Option<...>,
    pub on_ask_question_request: Option<...>,
    pub on_plan_exit_request: Option<...>,
    pub on_task_agent_start: Option<...>,
    pub on_task_agent_end: Option<...>,
    pub on_text_chunk: Option<...>,
    pub on_thinking_chunk: Option<...>,
    // 17 handlers total
    // ❌ ZERO memory-related handlers
}
```

### 3.5. Conversation Loop — Không có hook points

**File:** `src/zen_core/conversation.rs`

`QueryConfig` (line 68) nhận `system_prompt: String` — static string, không có callback để dynamic generate. Không có pre-turn hook, post-turn hook, hay context injection point nào trong conversation loop.

### 3.6. Điều gì được bảo tồn từ mẫu cũ

| Thành phần | File (Rust) | Status |
|---|---|---|
| MemoryManager.init_agent() | `agent_pool.rs:1654` | ✅ Ported |
| Pre-retrieval search | `agent_pool.rs:1732-1773` | ✅ Ported |
| Daily logging | `agent_pool.rs:1776-1782` | ✅ Ported |
| format_search_results | `memory/manager.rs` | ✅ Ported |
| Dirty file re-index | `agent_pool.rs:2694` | ✅ Ported |
| Memory cleanup on destroy | `agent_pool.rs:2404` | ✅ Ported |

### 3.7. Điều bị mất từ mẫu cũ

| Thành phần | Old location | Status |
|---|---|---|
| readPersonaFile() → SOUL.md | `sema-core/util/rules.js` | ❌ **MISSING** |
| readMemoryFile() → MEMORY.md | `sema-core/util/rules.js` | ❌ **MISSING** |
| readProjectConfigFile() → AGENTS.md/CLAUDE.md | `sema-core/util/rules.js` | ❌ **MISSING** |
| generateRulesReminders() | `sema-core/util/rules.js` | ❌ **MISSING** |
| buildAdditionalReminders() | `sema-core/SemaEngine.js` | ❌ **MISSING** |
| <system-reminder> injection | `sema-core/SemaEngine.js` | ❌ **MISSING** |
| Dynamic system_prompt via hook | `sema-core/formatSystemPrompt` | ❌ **MISSING** |

---

## 4. Vấn đề & Tác động

### 4.1. Agent mất persona

**Hiện tại:** Mọi agent trong SenClaw đều nhận system prompt `"You are a helpful AI assistant."` — giống hệt nhau, không phân biệt.

**Đúng ra phải là:** Mỗi agent có persona riêng từ `SOUL.md`:
- `code-agent/SOUL.md`: "Bạn là senior Rust engineer. Ưu tiên correctness, không dùng unwrap()..."
- `review-agent/SOUL.md`: "Bạn là code reviewer. Tập trung vào security, performance..."
- `test-agent/SOUL.md`: "Bạn là QA engineer. Viết test toàn diện, edge cases..."

### 4.2. Agent mất persistent memory

**Hiện tại:** Agent không thấy `MEMORY.md` — file mà chính agent đã ghi vào để nhớ thông tin xuyên session.

**Đúng ra phải là:** Agent thấy nội dung `MEMORY.md` trong `<system-reminder>` block ở turn đầu tiên, cho phép nó "nhớ" decisions, preferences, context đã tích lũy.

### 4.3. Agent mất project context

**Hiện tại:** `AGENTS.md` / `CLAUDE.md` trong project directory không được đọc.

**Đúng ra phải là:** Project-level rules, conventions, guidelines được inject vào context.

### 4.4. Không có hook để Cowork inject context

**Hiện tại:** Cowork context (board, task board, channel messages, member spec) không có chỗ để inject vào system prompt.

**Đúng ra phải là:** Có hook point để các layer bên ngoài (CoworkManager, MemoryManager) inject context vào prompt.

---

## 5. Đề xuất sửa đổi

### 5.1. Strategy: Inject tại AgentPool (không sửa zen-core)

**Lý do:** zen-core là engine thuần túy — không nên có knowledge về file system, memory, hay cowork. Việc đọc SOUL.md/MEMORY.md/AGENTS.md và inject context nên ở layer AgentPool (giống cách pre-retrieval đã làm).

```
┌──────────────────────────────────────────────────────────────────┐
│                      AgentPool Layer                              │
│                                                                   │
│  build_system_prompt(jid, group) → String                        │
│  ┌─────────────────────────────────────────────────────────────┐ │
│  │ 1. Đọc SOUL.md từ agent folder                               │ │
│  │ 2. Đọc MEMORY.md từ agent folder                             │ │
│  │ 3. Đọc AGENTS.md/CLAUDE.md từ working directory              │ │
│  │ 4. Nếu Cowork: inject Member Spec persona + responsibilities │ │
│  │ 5. Format: <agent_persona> + <persistent_memory> +            │ │
│  │            <project_context> + <cowork_context>               │ │
│  │ 6. Append base system prompt (optional, từ config)            │ │
│  └─────────────────────────────────────────────────────────────┘ │
│                          │                                        │
│                          ▼                                        │
│  ZenCoreOptions { system_prompt: built_prompt }                   │
│                          │                                        │
│                          ▼                                        │
│                  ZenEngine::new(opts)                              │
└──────────────────────────────────────────────────────────────────┘
```

### 5.2. Implementation Plan

#### Step 1 — SystemPromptBuilder (file mới)

```rust
// src/agent/system_prompt.rs

pub struct SystemPromptBuilder {
    agent_folder: String,
    working_dir: String,
}

impl SystemPromptBuilder {
    /// Build the full system prompt for an agent, layering:
    ///   1. SOUL.md (agent persona)
    ///   2. MEMORY.md (persistent cross-session memory)
    ///   3. AGENTS.md / CLAUDE.md (project context)
    ///   4. Cowork member spec (nếu có)
    ///   5. Cowork board context (nếu có)
    ///   6. Base system prompt (fallback nếu không có gì)
    pub fn build(&self, cowork_ctx: Option<&CoworkPromptContext>) -> String {
        let mut parts = Vec::new();

        // 1. SOUL.md — agent persona
        if let Some(soul) = self.read_agent_file("SOUL.md") {
            parts.push(format!(
                "<agent_persona>\n{}\n</agent_persona>",
                soul
            ));
        }

        // 2. MEMORY.md — persistent memory
        if let Some(mem) = self.read_agent_file("MEMORY.md") {
            parts.push(format!(
                "<persistent_memory>\n{}\n</persistent_memory>",
                mem
            ));
        }

        // 3. Project context
        if let Some(rules) = self.read_project_rules() {
            parts.push(format!(
                "<project_context>\n{}\n</project_context>",
                rules
            ));
        }

        // 4-5. Cowork context (nếu có)
        if let Some(ctx) = cowork_ctx {
            if let Some(spec) = &ctx.member_spec_prompt {
                parts.push(spec.clone());
            }
            if let Some(board) = &ctx.board_context {
                parts.push(board.clone());
            }
        }

        // 6. Fallback
        if parts.is_empty() {
            parts.push("You are a helpful AI assistant.".to_string());
        }

        parts.join("\n\n")
    }

    fn read_agent_file(&self, filename: &str) -> Option<String> {
        let path = Path::new(&self.agent_folder).join(filename);
        if path.exists() {
            std::fs::read_to_string(&path).ok()
        } else {
            None
        }
    }

    fn read_project_rules(&self) -> Option<String> {
        // Thử AGENTS.md trước, CLAUDE.md sau
        for name in &["AGENTS.md", "CLAUDE.md"] {
            let path = Path::new(&self.working_dir).join(name);
            if path.exists() {
                return std::fs::read_to_string(&path).ok();
            }
        }
        None
    }
}
```

#### Step 2 — Sửa AgentPool::ensure_engine()

```rust
// src/agent/agent_pool.rs — ensure_engine()

fn ensure_engine(&self, jid: &str, group: &GroupBinding) -> Arc<ZenEngine> {
    let mut engines = self.engines.lock().unwrap();
    if let Some(engine) = engines.get(jid) {
        return engine.clone();
    }

    // Build system prompt with full context layers
    let builder = SystemPromptBuilder {
        agent_folder: self.resolve_agent_folder(&group.folder),
        working_dir: group.allowed_work_dirs.first()
            .cloned()
            .unwrap_or_else(|| ".".to_string()),
    };

    let cowork_ctx = self.build_cowork_prompt_context(jid); // nếu có
    let system_prompt = builder.build(cowork_ctx.as_ref());

    let opts = ZenCoreOptions {
        instance_id: jid.to_string(),
        system_prompt,                         // ← NOW POPULATED
        agent_data_dir: self.resolve_agent_folder(&group.folder),
        working_dir: group.allowed_work_dirs.first()
            .cloned()
            .unwrap_or_else(|| ".".to_string()),
        use_tools: group.allowed_tools.clone(),
        ..Default::default()
    };

    let engine = ZenEngine::new(opts, self.mcp_manager.clone());
    // ...
}
```

#### Step 3 — System prompt caching strategy

Để tránh làm mất prompt cache của Anthropic API khi system prompt thay đổi:

```
Prompt Caching Strategy:
═══════════════════════════════════════════════════════════════

Layer                              Cacheable?    Thay đổi khi?
─────                              ──────────    ────────────
SOUL.md (persona)                  ✅ STATIC     User sửa file
MEMORY.md (persistent)             ⚠️ DYNAMIC   Agent tự ghi
AGENTS.md (project context)        ✅ STATIC     User sửa file
Cowork member spec (persona)       ✅ STATIC     User cập nhật
Cowork board (brief, guidelines)   ⚠️ DYNAMIC   Agent/User cập nhật
Cowork task board snapshot         ❌ DYNAMIC    Mỗi turn thay đổi
Cowork channel messages            ❌ DYNAMIC    Mỗi turn thay đổi
Current task                       ❌ DYNAMIC    Mỗi task khác nhau

→ STATIC layers: inject vào system_prompt (cacheable)
→ DYNAMIC layers: inject vào user message body (không phá cache)

Anthropic prompt cache behavior:
  - System prompt được cache riêng
  - Nếu system prompt thay đổi → cache miss → full price
  - Static layers nên ở system prompt
  - Dynamic layers nên ở first user message (như old pattern)
```

#### Step 4 — Cập nhật ZenCoreOptions

```rust
// Thêm field để hỗ trợ dynamic context injection
pub struct ZenCoreOptions {
    // ... existing fields ...
    pub system_prompt: String,         // ← static layers (populated)
    pub custom_rules: String,          // ← project rules (populated)
    pub first_turn_context: String,    // ← NEW: inject vào first user message
}
```

#### Step 5 — Cập nhật ZenEngine.process_user_input()

```rust
// src/zen_core/engine.rs

fn process_user_input(&self, prompt: &str, _original_input: Option<&str>) -> Result<()> {
    // ...

    let user_msg = {
        let blocks = {
            let state = self.state.lock().unwrap();
            let history_len = state.message_history(MAIN_AGENT_ID).len();

            if history_len == 0 && !opts.first_turn_context.is_empty() {
                // Inject dynamic context vào first user message
                // (giống <system-reminder> pattern của old sema-core)
                vec![
                    ContentBlock::Text {
                        text: format!(
                            "<system-reminder>\n{}\n</system-reminder>",
                            opts.first_turn_context
                        ),
                    },
                    ContentBlock::Text {
                        text: prompt.to_string(),
                    },
                ]
            } else {
                vec![ContentBlock::Text {
                    text: prompt.to_string(),
                }]
            }
        };
        create_user_message(blocks)
    };
    // ...
}
```

### 5.3. Cấu trúc prompt hoàn chỉnh (sau khi sửa)

```
┌──────────────────────────────────────────────────────────────────┐
│ SYSTEM PROMPT (cacheable — Anthropic prompt cache)                │
│                                                                   │
│ <agent_persona>                                                   │
│ Bạn là senior Rust engineer trong workspace project-alpha.        │
│ Chuyên về axum + SQLite. Không dùng unwrap() trong production.    │
│ </agent_persona>                                                  │
│                                                                   │
│ <project_context>                                                 │
│ ## AGENTS.md                                                      │
│ - Dùng Rust + axum cho tất cả API mới                             │
│ - Test coverage phải >= 80%                                       │
│ - Không dùng unwrap() trong production code                       │
│ </project_context>                                                │
└──────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────┐
│ FIRST USER MESSAGE (dynamic — injected mỗi session)              │
│                                                                   │
│ <system-reminder>                                                 │
│                                                                   │
│ <persistent_memory>                                               │
│ ## MEMORY.md                                                      │
│ - Q1/2026: Chọn SQLite thay PostgreSQL để đơn giản hóa deploy    │
│ - 2026-04-15: Quyết định dùng JWT RSA-256 cho auth               │
│ - User preference: luôn dùng `thiserror` cho error types         │
│ </persistent_memory>                                              │
│                                                                   │
│ <cowork_board>                                                    │
│ ## Brief: Xây dựng REST API cho inventory management             │
│ ## Guidelines: Dùng Rust + axum, không unwrap()                  │
│ ## Recent Decisions: Chọn SQLite, JWT RSA-256                    │
│ </cowork_board>                                                   │
│                                                                   │
│ <cowork_member_spec>                                              │
│ ## Responsibilities:                                              │
│ - Implement task có tag "backend"                                 │
│ - Viết unit test cho mọi hàm public                               │
│ ## Handoff: Khi complete → gửi review_request cho review-agent   │
│ </cowork_member_spec>                                             │
│                                                                   │
│ </system-reminder>                                                │
│                                                                   │
│ Implement auth module with JWT                                    │
└──────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────┐
│ EVERY USER MESSAGE (pre-retrieval)                                │
│                                                                   │
│ <memory>                                                          │
│ - [src/auth/jwt.rs] JWT validation with RSA-256 (score: 0.89)    │
│ - [docs/decisions.md] Auth flow decision log (score: 0.82)       │
│ - [src/auth/mod.rs] Auth module structure (score: 0.78)          │
│ </memory>                                                         │
│                                                                   │
│ [User message content...]                                         │
└──────────────────────────────────────────────────────────────────┘
```

---

## 6. Zen-core: Những hook points cần thêm

Ngoài việc build system prompt ở AgentPool, zen-core cần thêm một số hook points để các layer ngoài có thể inject context mà không phải sửa engine:

### 6.1. Proposed hooks (ZenCoreHandlers mở rộng)

```rust
pub struct ZenCoreHandlers {
    // ... existing 17 handlers ...

    /// Called before building system prompt. Returns additional
    /// system prompt content to append.
    pub on_build_system_prompt:
        Option<Box<dyn Fn() -> String + Send + Sync>>,

    /// Called before each turn. Returns content to inject
    /// into the current user message (e.g. memory pre-retrieval).
    /// Receives the raw user prompt, returns (prefix, suffix).
    pub on_pre_turn:
        Option<Box<dyn Fn(&str) -> (String, String) + Send + Sync>>,

    /// Called after tool execution completes. Receives tool name
    /// and result. Can be used to trigger memory indexing.
    pub on_post_tool:
        Option<Box<dyn Fn(&str, &ToolResult) + Send + Sync>>,

    /// Called after conversation turn completes. Receives the
    /// final messages array. Can be used for daily logging.
    pub on_post_turn:
        Option<Box<dyn Fn(&[Message]) + Send + Sync>>,
}
```

### 6.2. Tích hợp hooks vào Conversation Loop

```rust
// src/zen_core/conversation.rs — query()

pub async fn query(
    mut messages: Vec<Message>,
    config: &QueryConfig<'_>,
    cancel: &CancellationToken,
) -> Result<Vec<Message>> {
    loop {
        // === Pre-turn hook ===
        if let Some(ref hook) = config.hooks.on_pre_turn {
            if let Some(last_user_msg) = messages.last_mut() {
                if let Some(text) = last_user_msg.get_text() {
                    let (prefix, suffix) = hook(text);
                    // Inject prefix before user text, suffix after
                }
            }
        }

        // ... existing: queryLLM, run tools, etc. ...

        // === Post-tool hook ===
        if let Some(ref hook) = config.hooks.on_post_tool {
            for result in &tool_results {
                hook(&result.tool_name, &result);
            }
        }

        // ... existing: recurse or return ...
    }
}
```

---

## 7. So sánh tổng kết

| Chiều | sema-core (old TS) | zen-core (current Rust) | zen-core (proposed) |
|---|---|---|---|
| **SOUL.md injection** | ✅ system-reminder, first turn | ❌ | ✅ system_prompt (static) |
| **MEMORY.md injection** | ✅ system-reminder, first turn | ❌ | ✅ first_turn_context (dynamic) |
| **AGENTS.md injection** | ✅ system-reminder, first turn | ❌ | ✅ system_prompt (static) |
| **Pre-retrieval** | ✅ every turn via AgentPool | ✅ every turn via AgentPool | ✅ every turn via AgentPool |
| **Cowork persona** | N/A (chưa có Cowork) | ❌ | ✅ system_prompt |
| **Cowork board** | N/A | ❌ | ✅ first_turn_context |
| **Cowork task board** | N/A | ❌ | ✅ every turn (pre-retrieval) |
| **System prompt hook** | ✅ formatSystemPrompt() | ❌ | ✅ on_build_system_prompt |
| **Pre-turn hook** | ✅ buildAdditionalReminders() | ❌ | ✅ on_pre_turn |
| **Post-tool hook** | ❌ | ❌ | ✅ on_post_tool |
| **Prompt cache aware** | ✅ (static in system, dynamic in user) | ❌ | ✅ (same strategy) |
| **Daily logging** | ✅ | ✅ | ✅ |
| **Memory index** | ✅ | ✅ | ✅ |

---

## 8. File index

| File | Vai trò |
|---|---|
| `src/zen_core/mod.rs` | ZenCoreOptions, ZenCoreHandlers, AgentMode |
| `src/zen_core/engine.rs` | ZenEngine: process_user_input, system_prompt building |
| `src/zen_core/conversation.rs` | Conversation loop: query(), QueryConfig |
| `src/zen_core/events.rs` | EngineEvent enum |
| `src/agent/agent_pool.rs` | AgentPool: ensure_engine, process_and_wait_inner, pre-retrieval |
| `src/agent/session_bridge.rs` | build_prompt_for_group (message history only) |
| `src/memory/manager.rs` | MemoryManager: init_agent, search, format_search_results |
| `src/memory/fts_search.rs` | Hybrid search: hybrid_search, fts_search, vec_search |
| `code-old/sema-core/dist/util/rules.js` | generateRulesReminders: SOUL.md + MEMORY.md + AGENTS.md |
| `code-old/sema-core/dist/core/SemaEngine.js` | buildAdditionalReminders, formatSystemPrompt |
| `code-old/SenClaw/src/agent/AgentPool.ts` | Old TS AgentPool: pre-retrieval, memory integration |

---

*Nghiên cứu từ source code — ngày 2026-05-01.*
