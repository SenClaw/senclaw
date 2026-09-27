# Nghiên cứu đối chiếu: Sema Code VS Code Extension vs JCode

> Phân tích hai AI coding agent hàng đầu từ **source code thực tế**, đối chiếu với SenClaw để xác định cơ hội phát triển tính năng Chat, Cowork và Code.
>
> **Cập nhật:** 2026-05-01 | **Trạng thái:** Hoàn chỉnh

---

## 1. Tổng quan hai dự án

### 1.1. Sema Code — VSCode Extension (midea-ai)

| Thuộc tính | Giá trị |
|---|---|
| **Repo** | [midea-ai/sema-code-vscode-extension](https://github.com/midea-ai/sema-code-vscode-extension) |
| **Version** | 2.0.3 (April 17, 2026) |
| **Ngôn ngữ** | TypeScript 84.8% + CSS 14.6% |
| **Engine** | [sema-code-core](https://github.com/midea-ai/sema-code-core) (NPM: `sema-core`) |
| **License** | MIT |
| **VS Code API** | ^1.75.0 |
| **Mô hình** | Agent side-panel trong VS Code (React webview) |
| **Stars** | 12 stars, 3 releases, 83 commits |

**Cấu trúc source:**
```
src/
  extension.ts              ← Entry point — khởi tạo webview, đăng ký commands
  core/
    semaCoreWrapper.ts      ← 39KB — Wrapper event-driven quanh sema-core
    semaSidebarProvider.ts  ← Provider điều phối 3 webview panels
  managers/                 ← Các managers (config, session, history)
  utils/                    ← Tiện ích
  webview/
    chat/
      App.tsx               ← 32KB — Main chat state management + render
      MessageItem.tsx       ← 7.8KB — Render từng message (text, thinking, tool)
      TaskDetailModal.tsx   ← Sub-agent task detail popup
      chatWebview.ts        ← 14KB — postMessage bridge extension↔webview
      types.ts
      blocks/               ← Reusable UI blocks
      components/           ← Shared components
    config/                 ← Config panel webview
    sessionHistory/         ← Session history panel
```

### 1.2. JCode (1jehuang)

| Thuộc tính | Giá trị |
|---|---|
| **Repo** | [1jehuang/jcode](https://github.com/1jehuang/jcode) |
| **Version** | 0.11.6 (56 releases) |
| **Ngôn ngữ** | Rust 94.1%, Python 3%, Swift 1.3%, Shell 0.9% |
| **Engine** | Tự xây dựng (Rust native, 42 crates workspace) |
| **License** | MIT |
| **Stars** | ~1,900 |
| **Mô hình** | CLI/TUI native + TCP :7643 gateway server/client |

**Cấu trúc source chính:**
```
src/
  agent.rs         ← 27KB — Core agent runtime
  session.rs       ← 62KB — Persistence: snapshot+journal, PID tracking
  gateway.rs       ← TCP :7643 WebSocket gateway + auth
  server.rs        ← 79KB — Multi-client, swarm coordination
  protocol.rs      ← pub use jcode_protocol::* (re-export)
  compaction.rs    ← 70KB — Context window management
  background.rs    ← 44KB — Long-running background tasks
  memory.rs        ← Semantic memory with embedding
  memory_graph.rs  ← Graph: nodes, edges, clusters
  memory_agent.rs  ← Memory extraction agent
  browser.rs       ← 26KB — Firefox Agent Bridge

crates/
  jcode-protocol/      ← Request/Response enum cho toàn bộ protocol
  jcode-core/          ← Core runtime library
  jcode-agent-runtime/ ← Agent execution runtime
  jcode-embedding/     ← Local embedding
  jcode-tui-*/         ← TUI rendering (markdown, mermaid, workspace)
  jcode-provider-*/    ← LLM providers (gemini, openrouter, azure, ...)
  jcode-plan/          ← Plan graph DAG
  jcode-memory-types/  ← Memory type definitions
  jcode-gateway-types/ ← Gateway type definitions
  ... (42 crates total)
```

### 1.3. Vị trí so với SenClaw

```
┌─────────────────────────────────────────────────────────────────┐
│                        AI Coding Agents                         │
│                                                                 │
│  ┌──────────────────┐  ┌──────────────┐  ┌──────────────────┐  │
│  │ Sema Code (VSCode)│  │    JCode     │  │    SenClaw      │  │
│  │                  │  │              │  │                  │  │
│  │ VS Code side-    │  │ Terminal TUI │  │ Multi-channel    │  │
│  │ panel webview    │  │ native Rust  │  │ gateway (TG,     │  │
│  │ React + sema-core│  │ 42 crates    │  │ Feishu, QQ, WX)  │  │
│  │                  │  │ TCP gateway  │  │ + Web UI + API   │  │
│  └────────┬─────────┘  └──────┬───────┘  └────────┬─────────┘  │
│           │                   │                    │            │
│           ▼                   ▼                    ▼            │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │              Shared Concepts                               │ │
│  │  • Multi-agent orchestration   • Tool calling              │ │
│  │  • Plan/thinking mode          • Permission system         │ │
│  │  • Memory/RAG                  • Skill/plugin extension    │ │
│  │  • MCP protocol                • Streaming responses       │ │
│  └────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

---

## 2. Bảng so sánh tổng hợp

### 2.1. So sánh tính năng cốt lõi

| Tính năng | Sema Code VSCode | JCode | SenClaw (hiện tại) | SenClaw (target) |
|---|---|---|---|---|
| **Chat interface** | Webview side panel (React) | TUI ratatui | Web UI + Messaging apps | Web UI + channels |
| **Multi-model** | Anthropic + OpenAI + more | 15+ providers | SemaCore (Anthropic/OpenAI) | SemaCore |
| **Agent orchestration** | Sub-agent via Task tool | Swarm coordinator + DAG | DAG Dispatch + Group | Cowork Workspace |
| **Plan mode** | ✅ (sema-core) | ❌ | ✅ (qua SemaCore) | ✅ |
| **Memory** | ❌ (không có trong extension) | ✅ Semantic graph + embedding | ✅ FTS5 + Vector (chưa wired Rust) | ✅ Shared memory pool |
| **Skill system** | ✅ (sema-core) | ✅ Custom skills | ✅ (qua SemaCore) | ✅ |
| **MCP protocol** | ✅ (sema-core) | ✅ | ✅ (nhiều MCP servers) | ✅ |
| **Permission control** | ✅ (per-tool skip flags) | ✅ | ✅ (human-in-the-loop) | ✅ |
| **Streaming** | ✅ (text + thinking + tools) | ✅ | ✅ (qua SemaCore) | ✅ |
| **Multi-agent chat** | ❌ (sub-agent only) | ✅ (Swarm DM+Broadcast+pubsub) | ❌ (1 agent/group) | ✅ (Cowork channels) |
| **Session resume** | ✅ (history panel) | ✅ (snapshot+journal, PID) | ⚠️ (DB messages) | ✅ |
| **Session replay** | ❌ | ✅ (full replay→mp4) | ❌ | ✅ (Recording) |
| **Self-dev mode** | ❌ | ✅ (agents modify jcode) | ❌ | ❌ |
| **Browser automation** | ❌ | ✅ (Firefox Agent Bridge) | ❌ | Cân nhắc |
| **Task board** | ❌ | ✅ (PlanGraphStatus DAG) | ⚠️ (DAG Dispatch) | ✅ (Cowork task board) |
| **Git worktree isolation** | ❌ | ✅ | ❌ | ✅ (Cowork) |
| **File change notification** | ❌ | ✅ (cross-agent) | ❌ | ✅ (Cowork WS event) |
| **Crash detection** | ❌ | ✅ (PID probe) | ❌ | ✅ |
| **Mermaid rendering** | ❌ | ✅ (mermaid-rs, 1800x faster) | ❌ | Cân nhắc |

### 2.2. So sánh kiến trúc

| Chiều | Sema Code VSCode | JCode | SenClaw |
|---|---|---|---|
| **Engine** | sema-core (Node.js, subprocess) | Rust native | sema-core (subprocess) + Rust daemon |
| **UI** | Webview (React) | TUI (ratatui) | Web UI (React) + mobile channels |
| **Process model** | 1 process (VS Code extension host) | Server + multiple clients | Daemon (Rust monolith) |
| **Gateway** | VS Code API (no TCP) | TCP :7643 WebSocket + Unix socket pair | WebSocket axum :18789 |
| **Concurrency** | Sub-agent spawn (sema-core) | Tokio async, headless agents | Tokio + GroupQueue per JID |
| **Persistence** | JSON files | Snapshot + journal JSONL | SQLite (WAL) |
| **IPC** | VS Code postMessage API | TCP WebSocket (newline JSON) | WebSocket (JSON) |
| **Auth** | Không có (local) | Token header / `?token=` + `/pair` | Chưa có |
| **Plugin system** | MCP + Skills (sema-core) | Skills + MCP | MCP + Skills |
| **Config** | `~/.sema/` | `~/.jcode/` | `~/.senclaw/` |

---

## 3. Phân tích chi tiết: Chat

### 3.1. Sema Code VSCode — Chat Architecture

```
Extension Host                         Webview (React)
┌─────────────────────────────┐        ┌─────────────────────────────┐
│ extension.ts                │        │ App.tsx (32KB)              │
│   └→ SemaSidebarProvider    │        │   ├── MessageItem.tsx        │
│        ├── chatWebview      │        │   ├── blocks/ (UI blocks)   │
│        ├── configWebview    │◄──────►│   ├── components/ (shared)  │
│        └── historyWebview   │        │   └── TaskDetailModal.tsx   │
│                             │        └─────────────────────────────┘
│ SemaCoreWrapper (39KB)      │
│   ├── sema-core instance    │
│   └── EventEmitter handlers │
└─────────────────────────────┘
```

**Event system (`semaCoreWrapper.ts`):**

| Event | Direction | Payload | Xử lý |
|---|---|---|---|
| `session:ready` | Core→Wrapper | — | Enable input, init token info |
| `session:interrupted` | Core→Wrapper | — | Disable input, show interrupted |
| `session:error` | Core→Wrapper | message | Show error |
| `message:thinking:chunk` | Core→Wrapper | delta | streamingAssistantMap[id].reasoningDelta |
| `message:text:chunk` | Core→Wrapper | delta | streamingAssistantMap[id].contentDelta |
| `message:complete` | Core→Wrapper | message | Finalize, remove from map |
| `tool:execution:chunk` | Core→Wrapper | chunk | streamingToolMap[id].contentDelta |
| `tool:execution:complete` | Core→Wrapper | result | Update tool message with result |
| `tool:execution:error` | Core→Wrapper | error | Mark tool as failed |
| `task:agent:start` | Core→Wrapper | taskId | taskAgentMap[taskId] = new stream |
| `task:agent:end` | Core→Wrapper | taskId | Finalize sub-agent stream |
| `topic:update` | Core→Wrapper | topic | Update conversation topic |
| `compact:exec` | Core→Wrapper | — | Context compacted |
| `plan:implement` | Core→Wrapper | — | Plan implementation started |

**Sub-agent throttling (3 giây):**
```typescript
// semaCoreWrapper.ts
// 节流：3s 内多次更新只发一次
private taskAgentThrottleTimer: any;
private pendingTaskAgentContent: TaskMessageContent;

sendTaskAgentUpdate(content: TaskMessageContent): void {
  this.pendingTaskAgentContent = content;
  if (!this.taskAgentThrottleTimer) {
    this.taskAgentThrottleTimer = setTimeout(() => {
      this.callbacks.onMessage({ type: 'taskAgentUpdate', ... });
      this.taskAgentThrottleTimer = null;
    }, 3000);
  }
}
```

**Message protocol Webview↔Extension (đầy đủ):**

*Frontend → Extension:*

| Type | Parameters | Mô tả |
|---|---|---|
| `frontendReady` | — | Khởi tạo session |
| `sendInput` | text, files | Gửi input + file context |
| `interrupt` | — | Hủy operation đang chạy |
| `openFile` | filePath, line, endLine | Mở file tại dòng cụ thể |
| `requestWorkspaceFiles` | — | Lấy file tree |
| `searchWorkspaceFiles` | query | Tìm file theo tên |
| `requestModelInfo` | — | Lấy model đang dùng |
| `switchModel` | modelName | Đổi AI model |
| `restoreFromSnapshot` | filePath | Revert file về trước khi edit |
| `restoreFromSnapshots` | filePaths[] | Revert nhiều files |
| `showFileDiff` | filePath, minLine | Hiển thị diff |
| `showPermissionDiff` | filePath, diffContent | Hiển thị permission diff |
| `getFileChangeStats` | filePath | Lấy số lines thay đổi |
| `searchContentInFiles` | content | Tìm text trong workspace |
| `toolPermissionResponse` | response | Phản hồi permission request |
| `askQuestionResponse` | response | Phản hồi câu hỏi của agent |
| `planExitResponse` | response | Phản hồi plan exit |
| `verifyFilePath` | filePath, tempId, originalCode, lineInfo? | Xác minh file tồn tại |
| `insertPermissionRequest` | permissionData | Yêu cầu cấp quyền |
| `updateAgentMode` | mode | Đổi mode (code/chat/etc.) |
| `requestCommands` | — | Lấy custom commands |
| `openBashOutput` | content, command, toolId | Hiển thị output terminal |
| `transferAgentToBackground` | taskId | Chạy task nền |

*Extension → Frontend:*

| Type | Payload | Trigger |
|---|---|---|
| `appendMessages` | messages[] | Session messages loaded |
| `chunkUpdate` | msgId, delta | Streaming in progress |
| `completeUpdate` | msgId, message | Streaming complete |
| `updateMessage` | msgId, message | Tool result arrived |
| `toolPermissionRequest` | data | Core permission event |
| `askQuestionRequest` | data | Agent asking user |
| `planExitRequest` | data | Plan exit needed |
| `updateTokenInfo` | tokenInfo | Usage stats |
| `todosUpdate` | todos | Task list changed |
| `taskStart` / `taskEnd` | taskId | Sub-agent lifecycle |
| `openAgentDetail` | taskId | Navigate to task |
| `stateUpdate` | idle/processing | Agent state |
| `workspaceFiles` | files[] | File tree result |
| `updateModelInfo` | modelName, availableModels | Model data |
| `fileChangeStats` | fullPath, stats | Change metrics |
| `contentSearchResult` | result | Search result |
| `customCommandsLoaded` | commands | Commands list |
| `agentModeUpdate` | mode | Mode changed |
| `error` | message | Error occurred |
| `clearSessionPanels` | — | New session started |
| `resetTokenInfo` | — | Reset token counter |
| `disableInput` / `enableInput` | — | Input state |
| `clearTodos` / `clearFileChanges` | — | Reset UI state |

### 3.2. JCode — Chat Architecture

```
TCP :7643
  ↓
WebSocket upgrade (gateway.rs)
  ↓ auth token (header / ?token=)
  ↓
Unix socket pair → handle_client()
  ↓
Server (79KB) → Agent (27KB)
  ↓
Provider (15+) → stream()
  ↓
TUI (ratatui) render
```

**Gateway protocol (jcode gateway.rs):**
- TCP :7643 → WebSocket upgrade → Unix socket pair
- Auth: `Authorization: Bearer <token>` hoặc `?token=` (deprecated)
- Text format: newline-delimited JSON (`\n` mandatory)
- Keepalive: ping mỗi 20 giây
- HTTP endpoints:
  - `GET /health` — server status + version
  - `POST /pair` — exchange pairing code → auth token
  - `OPTIONS *` — CORS preflight

**Session persistence (session.rs, 62KB):**
```rust
Session {
    id: String,                          // session UUID
    friendly_name: Option<String>,       // e.g., "fox", "oak"
    messages: Vec<StoredMessage>,        // full transcript
    compaction: Option<StoredCompactionState>, // compacted view
    provider_messages_cache: Vec<Message>,    // native provider format
    provider_messages_hash: Option<String>,   // invalidation key
    pid: Option<u32>,                    // crash detection
    status: SessionStatus,               // Active, Closed, Crashed, Error
    created_at, last_active_at, ...
}

// Save strategy
metadata change → full snapshot JSON
new message     → append journal JSONL line
journal > 512KB → checkpoint snapshot, delete journal

// Load strategy
1. Load snapshot JSON
2. Replay journal lines
3. Rebuild provider_messages_cache
```

**Streaming flow:**
```
Agent::turn_execution()
  → provider.stream()
  → keepalive ticker (STREAM_KEEPALIVE_PONG_ID)
  → text chunks → TUI render (ratatui)
  → thinking chunks → collapsible thinking block
  → tool calls → repair_missing_tool_outputs()
  → token usage → CacheTracker (prefix hash tracking)
  → compaction check → hard/soft compact if needed
```

### 3.3. Đối chiếu Chat — Cơ hội cho SenClaw

| Tính năng | Sema Code VSCode | JCode | Trạng thái SenClaw | Khuyến nghị |
|---|---|---|---|---|
| **Streaming delta UI** | Webview chunkUpdate | TUI ratatui inline | ✅ WS chunkUpdate | OK |
| **Sub-agent visibility** | TaskDetailModal (throttled 3s) | Swarm status graph | ❌ | Implement Cowork dashboard |
| **Sub-agent throttle** | ✅ 3s window | ❌ | ❌ | Thêm 3s throttle cho cowork WS events |
| **Session persistence** | JSON files | Snapshot+journal JSONL | SQLite | OK, cân nhắc journal |
| **Crash recovery** | ❌ | ✅ PID tracking + probe | ❌ | Thêm PID tracking |
| **Mermaid rendering** | ❌ | ✅ 1800x faster Rust impl | ❌ | Có thể thêm vào Web UI |
| **Context compaction** | sema-core auto | Custom 70KB module | sema-core auto | OK |
| **Multi-panel** | 3 panels (chat, config, history) | Side panels | 1 panel (chat) | Cân nhắc config + history panel |

---

## 4. Phân tích chi tiết: Cowork (Multi-Agent Collaboration)

### 4.1. Sema Code VSCode — Sub-Agent (không có Cowork thực sự)

Sema Code VSCode **không có Cowork workspace**. Multi-agent chỉ qua DAG dispatch nội bộ:

```
Admin agent (main)
  → gọi Task tool: { subagent_type, prompt, description, isolation? }
  → Sub-agent chạy với:
    - agentId = taskId (không phải "main")
    - System prompt riêng từ AgentConfig
    - Tools bị filter theo agent config
    - Không emit chunk/state/topic events
    - Chỉ emit task:agent:start, task:agent:end
  → Kết quả: string trả về cho admin agent
```

**Hạn chế so với JCode:**
- Không có inter-agent messaging (DM, broadcast, pub/sub)
- Không có shared context giữa sub-agents
- Task board chỉ tồn tại trong 1 turn (không persistent)
- User không thấy được sub-agent activity (ẩn trong TaskDetailModal)
- Không có agent capabilities registry

### 4.2. JCode — Swarm Architecture

```
┌──────────────────────────────────────────────────────────┐
│                     JCode Swarm                          │
│                                                          │
│  Coordinator Agent                                       │
│       │                                                  │
│       ├── CommSpawn → spawn new headless agent           │
│       ├── CommProposePlan → PlanGraphStatus DAG          │
│       ├── CommApprovePlan / CommRejectPlan                │
│       ├── CommAssignTask → gán cho agent cụ thể          │
│       └── CommAwaitMembers → block đến khi done          │
│                                                          │
│  Per Agent:                                              │
│       ├── AgentRegister { capabilities }                 │
│       ├── CommMessage (DM hoặc broadcast)                │
│       ├── CommShare / CommRead (KV store shared)         │
│       ├── CommSubscribeChannel / CommUnsubscribeChannel  │
│       ├── CommStatus (non-blocking snapshot)             │
│       ├── CommReport (completion)                        │
│       ├── CommReadContext (đọc transcript agent khác)    │
│       ├── Split (fork session)                           │
│       └── CommStop                                       │
└──────────────────────────────────────────────────────────┘
```

**Toàn bộ CommRequests (từ jcode-protocol):**

| Request | Mô tả |
|---|---|
| `CommSpawn` | Coordinator spawn agent mới |
| `AgentRegister { capabilities }` | Agent tự đăng ký capabilities |
| `CommProposePlan { items: Vec<PlanItem> }` | Đề xuất DAG plan |
| `CommApprovePlan` / `CommRejectPlan` | Duyệt/từ chối plan |
| `CommAssignTask { task_id, target }` | Gán task cho agent |
| `CommAssignNext { prefer_spawn }` | Gán task tiếp theo |
| `CommTaskControl { action }` | pause/resume/cancel task |
| `CommMessage { to?, channel?, content }` | DM hoặc broadcast |
| `CommShare { key, value, append }` | Ghi vào KV store chung |
| `CommRead { key }` | Đọc từ KV store chung |
| `CommSubscribeChannel { channel }` | Subscribe pub/sub channel |
| `CommUnsubscribeChannel { channel }` | Unsubscribe channel |
| `CommReport { task_id, result }` | Báo cáo completion |
| `CommStatus { status, detail }` | Non-blocking status snapshot |
| `CommSummary` | Tool call summary |
| `CommReadContext { session_id }` | Đọc transcript của agent khác |
| `CommAssignRole { role }` | coordinator / agent / worktree_manager |
| `CommAwaitMembers { session_ids, target_status, timeout }` | Block cho đến khi ready |
| `CommStop` | Dừng agent |
| `Split` | Fork session (clone) |

**PlanGraphStatus (DAG):**
```rust
struct PlanGraphStatus {
    swarm_id: Option<String>,
    version: u64,
    item_count: usize,
    ready_ids: Vec<String>,                 // sẵn sàng assign
    blocked_ids: Vec<String>,               // chờ dependency
    active_ids: Vec<String>,                // đang chạy
    completed_ids: Vec<String>,             // đã xong
    cycle_ids: Vec<String>,                 // circular dependency
    unresolved_dependency_ids: Vec<String>,
    next_ready_ids: Vec<String>,            // sẽ ready tiếp
    newly_ready_ids: Vec<String>,           // vừa ready (dependency done)
}
```

**AgentInfo (per-agent real-time status):**
```rust
struct AgentInfo {
    session_id: String,
    friendly_name: Option<String>,          // human-readable name
    files_touched: Vec<String>,             // files đã chạm
    status: Option<String>,                 // "idle", "processing", ...
    detail: Option<String>,                 // mô tả chi tiết
    role: Option<String>,                   // coordinator / agent / worktree_manager
    is_headless: Option<bool>,              // không có TUI attachment
    report_back_to_session_id: Option<String>,
    latest_completion_report: Option<String>,
    live_attachments: Option<usize>,        // số TUI clients đang attach
    status_age_secs: Option<u64>,
}
```

**Shared KV context:**
```
CommShare { key: "database_schema", value: "...", append: false }
CommShare { key: "review_notes", value: "issue 1: ...", append: true }
CommRead  { key: "database_schema" } → trả về value hiện tại
```

**Pub/Sub channels:**
```
CommSubscribeChannel { channel: "#backend" }
CommSubscribeChannel { channel: "#reviews" }
// Agent nhận mọi message broadcast vào channel đó
CommMessage { to: None, channel: Some("#reviews"), content: "PR ready" }
```

### 4.3. SenClaw CoworkManager (hiện tại)

SenClaw đã implement `src/gateway/cowork_manager.rs` với:

```
CoworkManager {
    on_changed: Mutex<Option<Box<dyn Fn() + Send + 'static>>>,
}

APIs:
  process_user_message()     ← route message → create tasks for agents
  create_workspace()         ← tạo workspace dir structure
  add_member()               ← thêm agent + tạo symlink workspace
  update_member_spec()       ← persona, triggers, handoff_rules, SLA, limits
  create_task() / update_task() / list_tasks()
  add_task_comment()
  send_message() / list_messages() / mark_message_read()
  upsert_board_entry() / get_board() / update_board_entry()
  ensure_builtin_templates() ← 6 built-in templates:
    - Software Development (code + review + test agents)
    - Research (researcher + synthesizer + critic)
    - Content Pipeline (writer + editor + fact-checker)
    - Data Analysis (analyst + visualizer)
    - API Backend (backend-dev + api-tester)
    - Blank
  create_workspace_with_template()
  list_templates() / get_template()
```

**CoworkWorkspace filesystem layout:**
```
~/.senclaw/workspace/cowork/{id}/
  board/        ← Shared knowledge board
  tasks/        ← Task files
  memory/       ← Shared vector + FTS5 index
  shared/       ← Shared files
  agents/       ← Per-agent subdirs
    {member_id}/
  recordings/   ← Session recordings
```

**CoworkMember spec fields:**
```rust
CoworkMember {
    workspace_id, member_id, role,
    jid,                  // channel binding (Telegram, Feishu, etc.)
    subdir,               // agent working subdir
    persona,              // system prompt persona
    responsibilities,     // JSON array of responsibility strings
    triggers,             // JSON: [{type, from, condition}, ...]
    handoff_rules,        // JSON: [{when, to, type}, ...]
    acceptance_criteria,  // JSON array
    output_format,        // JSON: {format, requiredSections, attachDiff}
    sla,                  // JSON: {maxDurationPerTaskMinutes, maxTokenPerTask}
    limits,               // JSON: {allowedBashCommands, deniedTools}
}
```

### 4.4. Đối chiếu Cowork — Khoảng cách cần implement

| Tính năng | JCode Swarm | SenClaw (đã có) | Khoảng cách |
|---|---|---|---|
| **Workspace & members** | ✅ per-session | ✅ CoworkManager | Đã có — OK |
| **Task board** | ✅ PlanGraphStatus DAG | ✅ CoworkTask | Đã có — cần wiring |
| **Shared board** | ✅ CommShare KV | ✅ CoworkBoardEntry | Đã có — cần WS events |
| **Message channel** | ✅ DM + broadcast | ✅ CoworkMessage | Đã có — cần WS fan-out |
| **Templates** | ❌ | ✅ 6 built-in | SenClaw tốt hơn |
| **Agent capabilities registry** | ✅ AgentRegister | ❌ | Cần thêm |
| **Pub/Sub channels** | ✅ | ❌ | Nên thêm |
| **Non-blocking status** | ✅ CommStatus | ❌ | Cần WS event |
| **CommReadContext** | ✅ | ❌ | Cân nhắc |
| **CommAwaitMembers** | ✅ | ❌ | Hữu ích cho coordinator |
| **Session fork (Split)** | ✅ | ❌ | Thêm sau |
| **Git worktree isolation** | ✅ | ❌ | P1 |
| **File change cross-agent** | ✅ | ❌ | P1 — planned |
| **WS fan-out broadcast** | ✅ | ❌ | **P0 — cần ngay** |
| **Agent presence tracking** | ✅ live_attachments | ❌ | P1 |
| **Recording** | ✅ session→mp4 | ✅ recordings/ dir | Structure có, engine chưa |

**Thiếu quan trọng nhất:** CoworkManager hiện chỉ write/read DB — **chưa có WS fan-out**. Khi agent gửi message hay update task, không có cơ chế push real-time tới clients đang xem workspace.

---

## 5. Phân tích chi tiết: Code

### 5.1. Sema Code VSCode — Code Editing

**Tools từ sema-core:**

| Tool | Chức năng | Integration |
|---|---|---|
| `Read` | Đọc file (text, image, PDF, notebook) | VS Code workspace |
| `Write` | Ghi file mới hoặc overwrite | Ghi trực tiếp → VS Code refresh |
| `Edit` | String replacement + sinh diff | `diff` package |
| `Glob` | Tìm file theo pattern | `glob` package |
| `Grep` | Tìm nội dung trong file | `@vscode/ripgrep` |
| `Bash` | Chạy shell command | Sandbox + permission |
| `NotebookEdit` | Sửa Jupyter `.ipynb` | VS Code notebook API |

**Edit tool flow (precision string replacement):**
```
1. Đọc file
2. Tìm old_string (phải UNIQUE trong file — error nếu không unique)
3. Replace bằng new_string
4. Sinh unified diff
5. Ghi file
6. Emit tool:execution:complete { diff }
7. chatWebview postMessage → UI render diff panel
```

**Permission model:**
```typescript
// semaCoreWrapper.ts
hasPermissionsToUseTool(tool):
  if tool.isReadOnly → always allowed
  if tool.name == 'Write'|'Edit' && skipFileEditPermission → allowed
  if tool.name == 'Bash' && skipBashExecPermission → allowed
  if globalPermissionGranted[tool.name] → allowed
  else → emit permission_request event → user prompt
```

**File snapshot system:**
```
Before Edit:
  1. snapshot[filePath] = {before: original, after: null}

After Edit:
  2. snapshot[filePath].after = new_content

Restore:
  restoreFromSnapshot(filePath):
    fs.writeFileSync(filePath, snapshot[filePath].before)
    delete snapshot[filePath]
```

**Webview code features:**
```
requestWorkspaceFiles    → scan working dir → return file tree
searchWorkspaceFiles     → ripgrep file names
searchContentInFiles     → ripgrep content
openFileAtLine           → VS Code showTextDocument + reveal range
showFileDiff             → VS Code diff editor
getFileChangeStats       → count lines added/removed
verifyFilePath           → check file exists (for inline code blocks)
```

### 5.2. JCode — Code Editing

**Key differentiators:**

| Feature | File | Mô tả |
|---|---|---|
| **Git worktree isolation** | server.rs | Mỗi agent spawn → riêng worktree `~/.jcode/worktrees/{session_id}/` |
| **File change notification** | server.rs | "Agent A edits file X → Agent B (đã đọc X) nhận notification" |
| **Background tasks** | background.rs (44KB) | Long-running tasks, non-blocking |
| **Session resume** | session.rs | Resume từ codex, claude code, opencode, pi |
| **Self-dev mode** | agent.rs | Agent sửa source code của chính jcode |
| **Browser automation** | browser.rs (26KB) | Firefox Bridge: open, click, fill, screenshot, eval, scroll |

**Git worktree isolation flow:**
```
Coordinator: CommSpawn { capabilities }
  → Server tạo git worktree: git worktree add ~/.jcode/worktrees/{id} HEAD
  → Agent A làm việc trong worktree riêng
  → Agent B làm việc trong worktree riêng
  → Không conflict khi cùng sửa file
  → CommReport → server merge worktree changes
  → git worktree remove {id}
```

**File change cross-agent notification:**
```
Agent A: Edit tool → server.notify_file_change(session_id_A, path)
Server: for each session that has read(path):
  send notification to that session
Agent B: receives "File {path} was modified by {A}"
Agent B: re-reads file, continues with updated content
```

**Browser automation (Firefox Bridge):**
```rust
// browser.rs
BrowserTool {
  open(url),
  click(selector),
  fill(selector, value),
  screenshot() → base64 PNG,
  evaluate(js_code) → JSON result,
  scroll(direction, amount),
  find_element(selector),
  get_text(selector),
}
```

### 5.3. SenClaw — Code mode (hiện tại)

`GroupBinding.group_type == "code"` đã khai báo trong `src/types.rs:63` nhưng **chưa có logic phân biệt** với `chat` mode. Cần implement:

**WS messages cần thêm cho `code` mode:**

```
Client → Server:
  { type: 'code:file-tree', groupJid }          ← list workspace files
  { type: 'code:search-files', groupJid, query } ← search file names  
  { type: 'code:search-content', groupJid, query } ← ripgrep content
  { type: 'code:show-diff', groupJid, filePath }   ← show unified diff
  { type: 'code:restore-file', groupJid, filePath } ← restore snapshot
  { type: 'code:file-stats', groupJid, filePath }   ← lines added/removed

Server → Client:
  { type: 'code:file-tree', files[] }
  { type: 'code:search-result', query, matches[] }
  { type: 'code:file-diff', filePath, before, after, diff }
  { type: 'code:file-stats', filePath, added, removed }
  { type: 'code:snapshot', filePath, restored: true }
```

**File snapshot lifecycle:**
```
Agent bắt đầu Edit tool:
  1. Đọc file trước khi edit → lưu snapshot
  2. Execute edit
  3. Emit WS event: { type: 'code:file-diff', filePath, diff }

User click "Restore":
  { type: 'code:restore-file', filePath }
  → Server: write snapshot.before → file
  → Emit: { type: 'code:snapshot', filePath, restored: true }
```

### 5.4. Đối chiếu Code — Cơ hội cho SenClaw

| Tính năng | Sema Code VSCode | JCode | SenClaw hiện tại | Khuyến nghị |
|---|---|---|---|---|
| **File Edit tool** | ✅ (string replace + diff) | ✅ | ✅ (sema-core) | OK |
| **Diff visualization** | ✅ (webview diff panel) | ✅ (TUI side panel) | ⚠️ (Web UI có) | Thêm WS diff event |
| **File snapshot/restore** | ✅ | ❌ | ❌ | **P0** |
| **File tree** | ✅ (requestWorkspaceFiles) | ✅ | ❌ | P1 |
| **Content search** | ✅ (ripgrep) | ✅ | ❌ | P1 |
| **Git worktree isolation** | ❌ | ✅ | ❌ | P1 (Cowork) |
| **File change cross-agent** | ❌ | ✅ | ❌ | P1 (Cowork) |
| **Permission model** | ✅ (per-tool flags) | ✅ | ✅ (human-in-the-loop) | OK |
| **Background tasks** | ⚠️ sub-agent | ✅ 44KB module | ✅ Scheduler | OK |
| **Browser automation** | ❌ | ✅ Firefox Bridge | ❌ | Cân nhắc MCP tool |

---

## 6. Phân tích sâu: Patterns đáng học hỏi

### 6.1. JCode — Memory System (đáng học nhất)

```
Memory Architecture:
┌─────────────────────────────────────────────┐
│              Memory Graph                    │
│  ┌─────────┐  ┌─────────┐  ┌─────────┐     │
│  │Memories │  │  Tags   │  │Clusters │     │
│  │ HashMap │  │ Index   │  │ Groups  │     │
│  └────┬────┘  └────┬────┘  └────┬────┘     │
│       │            │            │           │
│       └────────────┼────────────┘           │
│                    │                        │
│            ┌───────┴───────┐                │
│            │  Weighted     │                │
│            │    Edges      │                │
│            └───────────────┘                │
└─────────────────────────────────────────────┘
```

**Retrieval pipeline:**
```
1. Embedding search (cosine ≥ 0.5, top 10 results)
2. Injection filtering (loại bỏ những đã inject rồi)
3. Sidecar verification (Haiku model batch 5, kiểm tra relevance thực)
4. Gap filtering (loại trailing entries có score gap > 25%)
5. Composite ranking

Deduplication:
  cosine ≥ 0.85 → reinforce existing memory (tăng strength + access_count)
  thay vì tạo memory mới

Composite score:
  score = recency(24h half-life)
        × access(sqrt(access_count))
        × category_importance
        × trust_multiplier
        × consolidation_strength(ln(strength))
```

**Khuyến nghị cho SenClaw memory:**
- Thêm **weighted edges** giữa memories (hiện chỉ có isolated entries)
- Thêm **sidecar verification** dùng quick model (Haiku) để filter relevance trước khi inject
- Thêm **reinforce** mechanism (cosine ≥ 0.85 → merge thay vì duplicate)
- Thêm **composite scoring** (hiện chỉ dùng similarity score)
- Thêm **clustering** theo topic

### 6.2. JCode — Session Persistence (Snapshot + Journal)

**Ưu điểm so với SQLite thuần:**
```
Snapshot + Journal vs SQLite:
  ✅ Human-readable (debug dễ — cat session.json)
  ✅ Atomic append (không cần transaction)
  ✅ Easy to export/share session với người khác
  ✅ Journal replay cho crash recovery
  ✅ Không cần lock — writer append, reader load snapshot
  ❌ Không có query (không dùng SELECT WHERE)
  ❌ No cross-session search

SQLite:
  ✅ Full query (SELECT, JOIN, FTS5)
  ✅ Transactions, ACID
  ✅ Cross-session search
  ❌ Write amplification khi thêm từng message
  ❌ WAL có thể grow lớn
```

**Khuyến nghị cho SenClaw:** Giữ SQLite cho structured/queryable data (groups, tasks, memory index). Thêm journal JSONL cho message streaming (append-only, giảm write amplification, crash recovery tốt hơn).

### 6.3. JCode — Swarm Architecture Lessons

```
Điểm mạnh cần học:
1. Protocol-driven: Mọi thứ qua Request/Response enum → dễ test, extend, mock
2. Centralized coordinator: Rõ ai điều phối → tránh conflict assignment
3. Plan graph DAG: Immutable snapshot → coordinator query an toàn
4. Non-blocking status: CommStatus không block agent → luôn responsive
5. Headless mode: Agent không cần UI attachment → scale horizontal
6. Capabilities registry: Agent tự khai báo → smart task assignment
7. CommAwaitMembers: Coordinator block cho đến khi có member ready
8. CommReadContext: Coordinator đọc transcript của bất kỳ agent
```

### 6.4. Sema Code VSCode — Sub-Agent Throttling

```typescript
// semaCoreWrapper.ts
// 节流：3s 内多次更新只发一次
// (Throttle: within 3s multiple updates → send only once)

private taskAgentThrottleTimer: ReturnType<typeof setTimeout> | null = null;
private pendingTaskAgentContent: TaskMessageContent | null = null;

sendTaskAgentUpdate(content: TaskMessageContent): void {
  this.pendingTaskAgentContent = content;
  if (!this.taskAgentThrottleTimer) {
    this.taskAgentThrottleTimer = setTimeout(() => {
      if (this.pendingTaskAgentContent) {
        this.callbacks.onMessage({
          type: 'chunkUpdate',
          msgId: this.getTaskAgentMsgId(),
          ...this.pendingTaskAgentContent,
        });
      }
      this.taskAgentThrottleTimer = null;
      this.pendingTaskAgentContent = null;
    }, 3000);
  }
}
```

**Khuyến nghị cho SenClaw:** Áp dụng throttle tương tự cho WS events từ agent khi đang chạy `cowork` mode — chỉ push 1 update/3s cho sub-task progress.

### 6.5. JCode — Crash Detection & Recovery

```rust
// session.rs
fn detect_crash(session: &Session) -> bool {
    if let Some(pid) = session.pid {
        // probe: kill(pid, 0) → no signal, just check existence
        match nix::sys::signal::kill(Pid::from_raw(pid as i32), None) {
            Ok(()) => false,  // process alive → not crashed
            Err(_) => true,   // process dead → crashed
        }
    } else {
        // fallback: if last_active > 120s → assume crashed
        session.last_active_at
            .map(|t| Utc::now() - t > Duration::seconds(120))
            .unwrap_or(false)
    }
}

fn mark_crashed(session: &mut Session, message: &str) {
    session.status = SessionStatus::Crashed;
    session.crash_message = Some(message.to_string());
    session.pid = None;
    session.save();
}
```

**Khuyến nghị cho SenClaw:** Thêm `pid: Option<u32>` vào group/session state. Khi daemon restart, kiểm tra các session "active" xem process còn chạy không.

---

## 7. Khuyến nghị phát triển cho SenClaw

### 7.1. Priority Matrix

| Priority | Tính năng | Học từ | File mục tiêu | Impact | Effort |
|---|---|---|---|---|---|
| **P0** | WS fan-out broadcast (cowork) | JCode Swarm | `websocket_gateway.rs` | Cao | Trung bình |
| **P0** | File snapshot trước Edit | Sema VSCode | `agent/` + `gateway/` | Cao | Trung bình |
| **P0** | `code` mode WS handlers | Sema VSCode | `websocket_gateway.rs` | Cao | Trung bình |
| **P1** | Agent capabilities registry | JCode AgentRegister | `cowork_manager.rs` | Cao | Thấp |
| **P1** | File change cross-agent notify | JCode file tracking | `cowork_manager.rs` | Trung bình | Trung bình |
| **P1** | Git worktree isolation | JCode worktree | `agent/` | Cao | Cao |
| **P1** | Pub/Sub channels trong Cowork | JCode CommSubscribeChannel | `cowork_manager.rs` | Trung bình | Trung bình |
| **P1** | Keepalive ping WS | JCode gateway.rs | `websocket_gateway.rs` | Thấp | Thấp |
| **P2** | Memory graph (edges, clusters) | JCode memory_graph | `memory/` | Trung bình | Cao |
| **P2** | Sidecar verification (retrieval) | JCode memory.rs | `memory/` | Trung bình | Trung bình |
| **P2** | Session crash detection | JCode PID tracking | `lib.rs` + DB | Trung bình | Thấp |
| **P2** | Non-blocking agent status WS | JCode CommStatus | `gateway/` | Trung bình | Thấp |
| **P2** | CommAwaitMembers equivalent | JCode | `cowork_manager.rs` | Trung bình | Trung bình |
| **P3** | Message journal (append JSONL) | JCode session.rs | `db/` | Thấp | Trung bình |
| **P3** | Session fork | JCode Split | `agent/` | Thấp | Cao |
| **P3** | Browser automation MCP tool | JCode browser.rs | `mcp/` | Thấp | Cao |
| **P3** | Mermaid rendering Web UI | JCode tui-mermaid | `web/` | Thấp | Thấp |

### 7.2. Protocol Design cho Cowork (học từ JCode, adapted cho SenClaw)

```rust
// Đề xuất: extend WS protocol cho cowork mode
// Thêm vào websocket_gateway.rs

// Client → Server
enum CoworkClientMsg {
    // Agent registration
    AgentRegister { workspace_id: String, capabilities: Vec<String> },
    AgentUnregister { workspace_id: String },

    // Messaging
    SendMessage { workspace_id: String, to: Option<String>, channel: Option<String>, content: String },

    // Shared context (học từ CommShare)
    ShareContext { workspace_id: String, key: String, value: String, append: bool },
    ReadContext { workspace_id: String, key: String },

    // Channels (học từ CommSubscribeChannel)
    SubscribeChannel { workspace_id: String, channel: String },
    UnsubscribeChannel { workspace_id: String, channel: String },

    // Status (học từ CommStatus — non-blocking)
    ReportStatus { workspace_id: String, status: String, detail: Option<String> },
    ReportCompletion { workspace_id: String, task_id: String, result: String },

    // Observation (học từ CommReadContext)
    GetAgentStatus { workspace_id: String, agent_id: String },
}

// Server → Client (broadcast tới tất cả subscribers)
enum CoworkServerEvent {
    MessageReceived { from: String, content: String, channel: Option<String>, ts: String },
    ContextUpdated { from: String, key: String, value: String },
    AgentJoined { agent_id: String, capabilities: Vec<String> },
    AgentLeft { agent_id: String },
    AgentStatusChanged { agent_id: String, status: String, detail: Option<String> },
    FileChanged { agent_id: String, file_path: String, change_type: String }, // created/modified/deleted
    TaskUpdated { task: CoworkTask },
    BoardUpdated { entry: CoworkBoardEntry },
    ChannelMessage { channel: String, from: String, content: String },
}
```

### 7.3. WS Fan-out (thiếu quan trọng nhất hiện tại)

CoworkManager hiện chỉ write DB — **không push WS**. Cần thêm:

```rust
// websocket_gateway.rs: thêm CoworkPresence

pub struct CoworkPresence {
    // workspace_id → set of (client_id, sender)
    watchers: Arc<Mutex<HashMap<String, HashMap<String, UnboundedSender<Message>>>>>,
}

impl CoworkPresence {
    pub async fn broadcast(&self, workspace_id: &str, event: CoworkServerEvent) {
        let msg = serde_json::to_string(&event).unwrap();
        if let Ok(map) = self.watchers.lock() {
            if let Some(clients) = map.get(workspace_id) {
                for (_, tx) in clients {
                    let _ = tx.send(Message::Text(msg.clone()));
                }
            }
        }
    }
}

// CoworkManager nhận CoworkPresence trong constructor
// Mỗi khi fire_changed() → gọi presence.broadcast()
```

### 7.4. Lộ trình đề xuất

```
Q2 2026: Foundation (P0 + P1)
  ├── WS fan-out broadcast cho Cowork (CoworkPresence)
  ├── File snapshot trước Edit (hook vào tool execution)
  ├── code mode WS handlers (file-tree, search, diff, restore)
  ├── Agent capabilities registry
  ├── Keepalive ping WS (20s như JCode)
  └── File change cross-agent notification

Q3 2026: Advanced Cowork (P1 + P2)
  ├── Git worktree isolation cho agent spawn
  ├── Pub/Sub channels trong Cowork
  ├── Non-blocking agent status (CommStatus-like)
  ├── CommAwaitMembers equivalent
  └── Session crash detection (PID tracking)

Q4 2026: Memory & Polish (P2 + P3)
  ├── Memory graph (edges, weighted, clusters)
  ├── Sidecar verification cho retrieval
  ├── Memory composite scoring
  ├── Message journal JSONL
  └── Cowork recording & replay
```

---

## 8. Tổng kết

### 8.1. Điểm mạnh từng dự án

**Sema Code VSCode:**
- Tích hợp VS Code sâu (diff editor, file tree, keybindings)
- Kế thừa toàn bộ sema-core engine (tested, plan mode, MCP)
- UI webview React với nhiều UX patterns tốt
- Permission system fine-grained với skip flags
- Sub-agent throttling (3s) — pattern đáng học

**JCode:**
- Performance cực cao (Rust native, 27.8MB RAM, 14ms startup)
- **Swarm architecture hoàn chỉnh nhất** — DM, broadcast, pub/sub, shared KV, DAG plan
- **Memory system tiên tiến** — graph, sidecar verification, composite scoring, deduplication
- **Session persistence mạnh** — snapshot+journal, PID crash detection
- 15+ providers với OAuth headless
- Git worktree isolation cho parallel agents
- File change cross-agent notification
- Browser automation (Firefox Bridge)

### 8.2. SenClaw có lợi thế gì?

1. **Multi-channel gateway** — Telegram, Feishu, QQ, WeChat (không ai có)
2. **Web UI self-hosted** — không phụ thuộc VS Code hay terminal
3. **Scheduler tích hợp** — cron/interval/once tasks
4. **Wiki + Memory** — git-backed knowledge base + FTS5/vector
5. **MCP servers đa dạng** — admin, dispatch, memory, schedule, send, workspace, Feishu wiki
6. **Cowork templates** — 6 built-in templates với full member specs (code/review/test agents)
7. **Cowork design** đã hoàn chỉnh — CoworkWorkspace, CoworkMember, CoworkTask, CoworkBoardEntry, CoworkMessage đều đã implemented trong DB + Manager

### 8.3. Nhận định tổng quát

SenClaw đã xây dựng được **data layer và manager layer** cho Cowork rất tốt — templates phong phú hơn cả JCode. Điểm thiếu chủ yếu là **real-time layer** (WS fan-out) và **code mode** specific handlers. Một khi có WS fan-out, toàn bộ Cowork features sẽ hoạt động end-to-end ngay lập tức vì DB/manager đã sẵn sàng.

---

*Tài liệu được tổng hợp từ source code thực tế (không chỉ README): semaCoreWrapper.ts 39KB, chatWebview.ts 14KB, gateway.rs, session.rs 62KB, cowork_manager.rs SenClaw, jcode-protocol crate.*
