# Sema Code Core — Kiến trúc & API Tham chiếu

> Phiên bản: `1.0.0` | NPM: `sema-core` | Repository: [midea-ai/sema-code-core](https://github.com/midea-ai/sema-code-core)
>
> Tài liệu phân tích từ `node_modules/sema-core/dist/` — mã đã biên dịch từ TypeScript.

## 1. Tổng quan

**Sema Code Core** là một engine AI coding assistant event-driven, cung cấp khả năng xử lý thông minh có thể cắm (pluggable) để xây dựng các công cụ AI programming. Hỗ trợ multi-agent collaboration, Skill extension, Plan mode task planning, và MCP protocol.

### Năng lực cốt lõi

| Năng lực | Mô tả |
|:---------|:------|
| **Multi-agent** | Điều phối sub-agent động theo loại task (Explore, Plan, general-purpose, v.v.) |
| **Skill Extension** | Kiến trúc plugin để mở rộng khả năng AI qua file Markdown có frontmatter |
| **Plan Mode** | Phân rã và lập kế hoạch thực thi cho tác vụ phức tạp |
| **MCP Protocol** | Tích hợp Model Context Protocol để mở rộng tool từ server ngoài |
| **Multi-Model** | Tương thích Anthropic SDK, OpenAI SDK và LLM API từ các nhà cung cấp lớn |
| **Permission Control** | Quản lý quyền chi tiết, an toàn và có thể kiểm soát |

---

## 2. Kiến trúc tổng thể

### 2.1 Component Diagram

```
                          ┌─────────────────────────────┐
                          │       Host Application       │
                          │  (VSCode, Web UI, CLI, ...)  │
                          └──────────────┬──────────────┘
                                         │ events + method calls
                                         ▼
┌──────────────────────────────────────────────────────────────────────────┐
│                           SemaCore (Public API)                           │
│                                                                          │
│  ┌────────────┐ ┌────────────┐ ┌──────────┐ ┌────────┐ ┌────────────┐  │
│  │  Session   │ │   Model    │ │   MCP    │ │ Skills │ │   Agents   │  │
│  │ create     │ │ add/del/   │ │ add/rm/  │ │ get/    │ │ get/add/   │  │
│  │ process    │ │ switch/    │ │ connect  │ │ reload  │ │ reload     │  │
│  │ interrupt  │ │ test/fetch │ │ update   │ │         │ │            │  │
│  └──────┬─────┘ └─────┬──────┘ └────┬─────┘ └────┬───┘ └─────┬──────┘  │
│         │             │             │             │            │         │
│         └─────────────┼─────────────┼─────────────┼────────────┘         │
│                       │             │             │                      │
│                       ▼             ▼             ▼                      │
│              ┌────────────────────────────────────────┐                  │
│              │           SemaEngine                   │                  │
│              │  ┌────────┐ ┌───────────┐ ┌────────┐  │                  │
│              │  │EventBus│ │StateMgr   │ │MCPMgr  │  │                  │
│              │  └────────┘ └───────────┘ └────────┘  │                  │
│              │  ┌──────────────┐ ┌───────────────┐   │                  │
│              │  │ Conversation │ │ Agent/Skill/  │   │                  │
│              │  │ Loop (query) │ │ Cmd Loaders   │   │                  │
│              │  └──────────────┘ └───────────────┘   │                  │
│              └────────────────┬───────────────────────┘                  │
└───────────────────────────────┼──────────────────────────────────────────┘
                                │
                    ┌───────────┴───────────┐
                    ▼                       ▼
        ┌──────────────────┐    ┌──────────────────────┐
        │  Anthropic API   │    │   OpenAI-compat API   │
        │  (Claude models) │    │  (GPT, local, etc.)   │
        └──────────────────┘    └──────────────────────┘
```

### 2.2 Multi-Tenant Isolation

```
 ┌───────────────────┐   ┌───────────────────┐   ┌───────────────────┐
 │  SemaCore #1       │   │  SemaCore #2       │   │  SemaCore #3       │
 │  workingDir: /projA│   │  workingDir: /projB│   │  workingDir: /projC│
 └────────┬──────────┘   └────────┬──────────┘   └────────┬──────────┘
          │                       │                       │
          ▼                       ▼                       ▼
 ┌────────────────────────────────────────────────────────────────────┐
 │                    AsyncLocalStorage (node:async_hooks)             │
 │                                                                    │
 │   ┌──────────────┐      ┌──────────────┐      ┌──────────────┐    │
 │   │ EngineStore 1│      │ EngineStore 2│      │ EngineStore 3│    │
 │   │ eventBus     │      │ eventBus     │      │ eventBus     │    │
 │   │ stateMgr     │      │ stateMgr     │      │ stateMgr     │    │
 │   │ mcpMgr       │      │ mcpMgr       │      │ mcpMgr       │    │
 │   │ coreConfig   │      │ coreConfig   │      │ coreConfig   │    │
 │   │ workingDir   │      │ workingDir   │      │ workingDir   │    │
 │   └──────────────┘      └──────────────┘      └──────────────┘    │
 │                                                                    │
 │   getEventBus() / getStateManager() / getMCPManager()             │
 │   → tự động trả về instance tương ứng với engine hiện tại          │
 └────────────────────────────────────────────────────────────────────┘
```

### 2.3 Module Dependency Graph

```
  SemaCore ──────────────────────────────────────────────┐
     │                                                    │
     ▼                                                    │
  SemaEngine ──────────────┐                             │
     │                      │                             │
     ├── ConfigManager      ├── Conversation (query)      │
     ├── ModelManager       │      │                      │
     ├── MCPManager ────────┤      ├── queryLLM ─────────┤
     ├── StateManager       │      │      ├── cache       │
     ├── EventBus           │      │      ├── anthropic   │
     │                      │      │      └── openai      │
     │                      │      ├── compact            │
     │                      │      ├── RunTools           │
     │                      │      │      ├── Bash        │
     │                      │      │      ├── Read        │
     │                      │      │      ├── Edit        │
     │                      │      │      ├── Write       │
     │                      │      │      ├── Glob        │
     │                      │      │      ├── Grep        │
     │                      │      │      ├── Task ───────┤── AgentsManager
     │                      │      │      ├── Skill ──────┤── SkillRegistry
     │                      │      │      ├── TodoWrite   │
     │                      │      │      ├── AskUserQ    │
     │                      │      │      ├── ExitPlan    │
     │                      │      │      ├── NoteEdit    │
     │                      │      │      └── MCP tools   │
     │                      │      └── PermissionMgr      │
     │                      │                             │
     │                      ├── genSystemPrompt           │
     │                      ├── SkillRegistry             │
     │                      └── Custom Commands           │
     │                                                    │
     └── EngineContext (AsyncLocalStorage) ◄──────────────┘
```

**Nguyên tắc cô lập**: Mỗi instance `SemaCore` sở hữu một `SemaEngine` riêng, sử dụng `AsyncLocalStorage` để tất cả lệnh gọi nội bộ tự động trả về đúng instance mà không cần truyền tham số.

---

## 3. Session Lifecycle

### 3.1 Sequence: createSession

```
 User/Host          SemaCore          SemaEngine        StateMgr     EventBus      ModelMgr    SkillRegistry
    │                   │                  │                 │            │             │              │
    │ createSession()   │                  │                 │            │             │              │
    ├──────────────────►│                  │                 │            │             │              │
    │                   │ runWithEngine()  │                 │            │             │              │
    │                   ├─────────────────►│                 │            │             │              │
    │                   │                  │ generate ID     │            │             │              │
    │                   │                  ├────────────────►│            │             │              │
    │                   │                  │ setSessionId()  │            │             │              │
    │                   │                  │                 │            │             │              │
    │                   │                  │ historyPath     │            │             │              │
    │                   │                  ├────────────────►│            │             │              │
    │                   │                  │ load history    │            │             │              │
    │                   │                  │                 │            │             │              │
    │                   │                  │ init plugins    │            │             │              │
    │                   │                  ├─────────────────┼────────────┼─────────────┼──────────────┤
    │                   │                  │                 │            │             │ init()       │
    │                   │                  │                 │            │             │              │
    │                   │                  │                 │            │ emit        │              │
    │                   │                  │                 │◄───────────┤             │              │
    │                   │                  │                 │ session:ready            │              │
    │                   │                  │                 │            │             │              │
    │◄──────────────────┤                  │                 │            │             │              │
    │ session:ready     │                  │                 │            │             │              │
```

### 3.2 Sequence: processUserInput

```
 User         SemaCore      SemaEngine     runCommand    Conversation    queryLLM      RunTools
  │               │              │              │               │             │            │
  │ processInput  │              │              │               │             │            │
  ├──────────────►│              │              │               │             │            │
  │               │ processInput │              │               │             │            │
  │               ├─────────────►│              │               │             │            │
  │               │              │ /command?    │               │             │            │
  │               │              ├─────────────►│               │             │            │
  │               │              │ handled      │               │             │            │
  │               │              │◄─────────────┤               │             │            │
  │               │              │              │               │             │            │
  │               │              │ state→processing             │             │            │
  │               │              ├──────────────────────────────┼─────────────┼────────────┤
  │               │              │              │               │             │            │
  │               │              │ processQuery │               │             │            │
  │               │              ├──────────────┼──────────────►│             │            │
  │               │              │              │               │ query()     │            │
  │               │              │              │               ├────────────►│            │
  │               │              │              │               │◄────────────┤            │
  │               │              │              │               │ AssistantMsg│            │
  │               │              │              │               │             │            │
  │               │              │              │               │ runTools()  │            │
  │               │              │              │               ├─────────────┼───────────►│
  │               │              │              │               │◄────────────┼────────────┤
  │               │              │              │               │ ToolResults │            │
  │               │              │              │               │             │            │
  │               │              │              │               │ recurse...  │            │
  │               │              │              │               │             │            │
  │  events:      │              │              │               │             │            │
  │  message:*    │              │              │               │             │            │
  │  tool:*       │◄─────────────┼──────────────┼───────────────┤             │            │
  │  state:update │              │              │               │             │            │
  │◄──────────────┤              │              │               │             │            │
```

---

## 4. Lớp công khai (Public API)

### 4.1 SemaCore

File: `dist/core/SemaCore.js`

Constructor: `new SemaCore(config?: SemaCoreConfig)`

#### Điều khiển session
| Method | Mô tả |
|:-------|:------|
| `createSession(sessionId?: string)` | Tạo session mới, khởi tạo history, system prompt |
| `processUserInput(input, originalInput?)` | Gửi input từ người dùng, bắt đầu conversation loop |
| `interruptSession()` | Dừng session hiện tại, trạng thái về `idle` |
| `pauseSession()` | Tạm dừng session, trạng thái về `paused`, giữ context |
| `hasSessionToolResults()` | Kiểm tra history đã có tool_use chưa (cho pause/resume) |

#### Quản lý model
| Method | Mô tả |
|:-------|:------|
| `addModel(config: ModelConfig, skipValidation?)` | Thêm model mới |
| `delModel(modelName)` | Xóa model |
| `switchModel(modelName)` | Chuyển model hiện tại |
| `applyTaskModel(config: TaskConfig)` | Cấu hình model main + quick |
| `getModelData()` | Lấy toàn bộ model config hiện tại |
| `fetchAvailableModels(params)` | Lấy danh sách model từ provider |
| `testApiConnection(params)` | Test kết nối API |

#### Cấu hình runtime
| Method | Mô tả |
|:-------|:------|
| `updateCoreConfByKey(key, value)` | Cập nhật một trường config |
| `updateCoreConfig(config)` | Cập nhật nhiều trường config |
| `updateUseTools(toolNames)` | Lọc danh sách tool được dùng |
| `updateAgentMode(mode)` | Chuyển Agent/Plan mode |
| `updateThinking(enabled)` | Bật/tắt thinking mode |
| `updateSkipPermissions(skip)` | Bật/tắt tất cả permission check (cho sub-agent) |
| `setWorkingDir(newDir)` | Đổi working directory (WorkspaceTool) |

#### MCP & Skills
| Method | Mô tả |
|:-------|:------|
| `addOrUpdateMCPServer(config, scope)` | Thêm/cập nhật MCP server |
| `removeMCPServer(name, scope)` | Xóa MCP server |
| `getMCPServerConfigs()` | Lấy tất cả MCP server info |
| `connectMCPServer(name)` | Kết nối một MCP server |
| `updateMCPUseTools(name, tools)` | Cập nhật tool filter cho MCP server |
| `getSkillsInfo()` | Lấy danh sách skill |
| `reloadSkills(disabledNames?)` | Hot-reload skill registry |

#### Agents & Custom Commands
| Method | Mô tả |
|:-------|:------|
| `getAgentsInfo()` | Lấy danh sách sub-agent |
| `addAgentConf(agentConf)` | Thêm custom agent |
| `getCustomCommands()` | Lấy danh sách slash command |
| `reloadCustomCommands()` | Reload custom commands |

#### Events & Permissions
| Method | Mô tả |
|:-------|:------|
| `on(event, listener)` | Đăng ký event listener |
| `once(event, listener)` | Đăng ký one-time listener |
| `off(event, listener)` | Hủy listener |
| `respondToToolPermission(response)` | Phản hồi permission request |
| `respondToAskQuestion(response)` | Phản hồi câu hỏi từ AskUserQuestion |
| `respondToPlanExit(response)` | Phản hồi Plan mode exit |
| `dispose()` | Dọn dẹp tài nguyên |

### 4.2 SemaEngine

File: `dist/core/SemaEngine.js`

Lớp nội bộ, mỗi `SemaCore` instance sở hữu một engine. Chịu trách nhiệm:
- Khởi tạo và quản lý EventBus, StateManager, MCPManager
- Tạo session: load history, generate system prompt, khởi tạo Skill registry và custom commands
- Process user input: xử lý system command → custom command → process query
- Interrupt/pause session với xử lý AbortController
- Phát hiện topic trong background (async, không block)

### 4.3 EngineContext (AsyncLocalStorage)

File: `dist/core/EngineContext.js`

```typescript
interface EngineStore {
    instanceId: string;
    workingDir: string;
    agentDataDir: string;
    coreConfig: SemaCoreConfig;
    eventBus: EventBus;
    stateManager: StateManager;
    mcpManager: MCPManager;
}
```

Các hàm `getEventBus()`, `getStateManager()`, `getMCPManager()` tự động trả về instance tương ứng với engine hiện tại nếu đang ở trong `runWithEngine()`, hoặc global singleton nếu không.

---

## 5. Hệ thống Event

File: `dist/events/EventSystem.js`, `dist/events/types.d.ts`

### EventBus

Kế thừa `EventEmitter` từ Node.js. Hỗ trợ: `on`, `once`, `off`, `emit`, `removeAllListeners`, `hasListeners`, `listenerCount`, `eventNames`.

### Các sự kiện chính

| Event | Data Type | Mô tả |
|:------|:----------|:------|
| `session:ready` | `SessionReadyData` | Session đã sẵn sàng, có history |
| `session:interrupted` | `SessionInterruptedData` | Session bị ngắt |
| `session:error` | `SessionErrorData` | Lỗi (api_error, fatal_error, context_length_exceeded, model_error) |
| `session:cleared` | `SessionClearedData` | Session bị xóa |
| `state:update` | `StateUpdateData` | Trạng thái thay đổi (idle/processing/paused) |
| `message:thinking:chunk` | `ThinkingChunkData` | Streaming thinking content |
| `message:text:chunk` | `TextChunkData` | Streaming text content |
| `message:complete` | `MessageCompleteData` | AI response hoàn tất (gồm toolCalls nếu có) |
| `tool:permission:request` | `ToolPermissionRequestData` | Yêu cầu quyền chạy tool |
| `tool:execution:complete` | `ToolExecutionCompleteData` | Tool chạy xong |
| `tool:execution:error` | `ToolExecutionErrorData` | Tool chạy lỗi |
| `todos:update` | `TodoItem[]` | Todo list thay đổi |
| `topic:update` | `TopicUpdateData` | Topic (tiêu đề hội thoại) cập nhật |
| `conversation:usage` | `Usage` | Token usage (mỗi lần AI response) |
| `compact:start` | `CompactStartData` | Bắt đầu compact context |
| `compact:exec` | `CompactExecData` | Kết quả compact |
| `file:reference` | `FileReferenceData` | File được agent tham chiếu |
| `ask:question:request` | `AskQuestionRequestData` | Agent hỏi người dùng |
| `plan:exit:request` | `PlanExitRequestData` | Plan mode yêu cầu exit |
| `plan:implement` | `PlanImplementData` | Bắt đầu thực thi plan |
| `task:agent:start` | `TaskAgentStartData` | Sub-agent bắt đầu |
| `task:agent:end` | `TaskAgentEndData` | Sub-agent kết thúc |

---

## 6. Managers

### 6.1 StateManager

File: `dist/manager/StateManager.js`

Quản lý trạng thái session, history, todos, read timestamps. Phân lập theo `agentId`:

**Trạng thái cô lập (per agentId)**:
- `statesMap` — trạng thái agent (`idle` / `processing` / `paused`)
- `messageHistoryMap` — lịch sử tin nhắn
- `readFileTimestampsMap` — timestamp đọc file
- `todosMap` — danh sách todo

**Trạng thái chia sẻ**:
- `sessionId` — ID của session hiện tại
- `globalEditPermissionGranted` — quyền edit toàn cục (sau khi user approve một lần)
- `planModeInfoSent` — đã gửi plan mode info chưa
- `currentAbortController` — controller để hủy request

Cung cấp `forAgent(agentId)` trả về `AgentStateAccessor` để thao tác với state của một agent cụ thể.

### 6.1.1 Session State Machine

```
                    ┌──────────────────────────────────────────────────┐
                    │                   SESSION STATES                  │
                    └──────────────────────────────────────────────────┘

    ┌─────────┐   createSession()   ┌─────────────┐   processUserInput()   ┌────────────┐
    │         │────────────────────►│             │───────────────────────►│            │
    │  (init) │                     │    IDLE     │                        │ PROCESSING │
    │         │◄────────────────────│             │◄───────────────────────│            │
    └─────────┘   clearSession()    └─────────────┘   sessionDone/Error    └─────┬──────┘
                                          │       ◄──────────────────────────────┘
                                          │                   interruptSession()
                                          │
                              pauseSession()         interruptSession()
                              (save context)         (discard context)
                                          │                   ▲
                                          ▼                   │
                                    ┌─────────────┐          │
                                    │             │──────────┘
                                    │   PAUSED    │
                                    │             │
                                    └──────┬──────┘
                                           │
                                           │ resumeSession()
                                           │ (restore context)
                                           ▼
                                    ┌─────────────┐
                                    │  PROCESSING │
                                    └─────────────┘
```

### 6.2 ModelManager

File: `dist/manager/ModelManager.js`

Lưu model config ra file `~/.sema/model.conf`. Cấu trúc:

```typescript
interface ModelConfiguration {
    modelProfiles: ModelProfile[];  // danh sách model đã cấu hình
    modelPointers: {
        main: string;   // tên model dùng cho main query
        quick: string;  // tên model dùng cho quick query (compact, topic, ...)
    };
}
```

### 6.3 ConfigManager

File: `dist/manager/ConfManager.js`

Quản lý config hệ thống và project. Lưu project config ra `~/.sema/projects.conf`.

```typescript
interface ProjectConfig {
    allowedTools: string[];
    history: string[];        // lịch sử input
    lastEditTime: string;
    rules: string[];          // rules từ AGENTS.md
}
```

Hỗ trợ multi-tenant: `registerProjectConfig()` chỉ đăng ký mà không set global CWD, các engine khác nhau dùng workingDir khác nhau qua EngineContext.

### 6.4 PermissionManager

File: `dist/manager/PermissionManager.js`

Hàm `hasPermissionsToUseTool()` kiểm tra quyền trước khi chạy tool. Hỗ trợ:
- `skipFileEditPermission`, `skipBashExecPermission`, `skipSkillPermission`, `skipMCPToolPermission` trong config
- Global edit permission (sau lần approve đầu tiên)
- Permission per-tool qua dialog xác nhận

### 6.4.1 Permission Check Flow

```
  RunTools                PermissionMgr            Config          EventBus        User/Host
     │                         │                     │                │                │
     │ checkPermission(tool)   │                     │                │                │
     ├────────────────────────►│                     │                │                │
     │                         │ check skip flags    │                │                │
     │                         ├────────────────────►│                │                │
     │                         │◄────────────────────┤                │                │
     │                         │                     │                │                │
     │                         │ [skip=true]─────────┼───────────────►│                │
     │◄──── allow ─────────────┤ return true         │ emit           │                │
     │                         │                     │ tool:exec:     │                │
     │                         │                     │ complete       │                │
     │                         │                     │                │                │
     │                         │ [skip=false]        │                │                │
     │                         │                     │ emit           │                │
     │                         ├─────────────────────┼───────────────►│                │
     │                         │                     │ tool:perm:     │                │
     │                         │                     │ request        │                │
     │                         │                     │                │                │
     │                         │                  ┌──┼────────────────┼─── Wait ───────┤
     │                         │                  │  │                │                │
     │                         │   respondToTool  │  │◄───────────────┼────────────────┤
     │                         │   Permission     │  │                │    selected    │
     │                         │◄─────────────────┘  │                │                │
     │                         │                     │                │                │
     │                         │ [approved]──────┼──┼───────────────►│                │
     │                         │ return true        │ emit           │                │
     │◄──── allow ─────────────┤                     │ tool:exec:     │                │
     │                         │                     │ complete       │                │
     │                         │                     │                │                │
     │                         │ [rejected]─────────┼───────────────►│                │
     │◄──── deny ──────────────┤ return false        │ tool:exec:     │                │
     │                         │                     │ error          │                │
```

---

## 7. Hệ thống Tool

### 7.1 Tool Interface

File: `dist/tools/base/Tool.d.ts`

```typescript
interface Tool<TInput, TOutput> {
    name: string;
    description?: string | (() => string);
    inputSchema: ZodObject;
    isReadOnly: () => boolean;
    validateInput?: (input, agentContext) => Promise<ValidationResult>;
    genResultForAssistant: (output) => string;
    genToolPermission?: (input) => { title, summary?, content };
    genToolResultMessage?: (output, input?) => { title, summary, content };
    getDisplayTitle?: (input?) => string;
    call: (input, agentContext) => AsyncGenerator<{type:'result', data, resultForAssistant?}>;
}
```

### 7.2 Built-in Tools

| Tool | File | Type | Mô tả |
|:-----|:-----|:-----|:------|
| **Bash** | `tools/Bash/` | R/W | Chạy shell command, timeout, sandbox |
| **Read** | `tools/Read/` | Read-only | Đọc file text/image/PDF/notebook |
| **Write** | `tools/Write/` | Write | Ghi file mới hoặc ghi đè |
| **Edit** | `tools/Edit/` | Write | String replacement chính xác, sinh diff |
| **Glob** | `tools/Glob/` | Read-only | Tìm file theo pattern |
| **Grep** | `tools/Grep/` | Read-only | Tìm kiếm nội dung qua ripgrep |
| **Task** | `tools/Task/` | R/W | Dispatch sub-agent |
| **Skill** | `tools/Skill/` | R/W | Gọi skill |
| **TodoWrite** | `tools/TodoWrite/` | Write | Tạo/cập nhật danh sách việc cần làm |
| **AskUserQuestion** | `tools/AskUserQuestion/` | Read-only | Hỏi người dùng câu hỏi |
| **ExitPlanMode** | `tools/ExitPlanMode/` | Write | Thoát Plan mode, chuyển sang thực thi |
| **NotebookEdit** | `tools/NotebookEdit/` | Write | Sửa Jupyter notebook (.ipynb) |

### 7.3 Tool Selection

Hàm `getTools(useTools?)` trong `dist/tools/base/tools.js`:
- Nếu `useTools` là `null` hoặc không truyền → tất cả built-in tools
- Nếu `useTools` là `string[]` → chỉ các tool có tên trong danh sách
- Hàm được memoize bằng lodash

---

## 8. MCP Integration

### 8.0 MCP Connection & Tool Flow

```
  SemaCore        MCPManager        MCPClient       MCPToolAdapter     External MCP Server
     │                │                 │                  │                  │
     │ addOrUpdate    │                 │                  │                  │
     │ MCPServer      │                 │                  │                  │
     ├───────────────►│                 │                  │                  │
     │                │ save config     │                  │                  │
     │                │ to file         │                  │                  │
     │                │                 │                  │                  │
     │                │ new MCPClient() │                  │                  │
     │                ├────────────────►│                  │                  │
     │                │                 │ connect()        │                  │
     │                │                 ├──────────────────┼─────────────────►│
     │                │                 │ createTransport  │                  │
     │                │                 │ (stdio/sse/http) │                  │
     │                │                 │◄─────────────────┼──────────────────┤
     │                │                 │ fetchCapabilities│                  │
     │                │                 ├──────────────────┼─────────────────►│
     │                │                 │◄─────────────────┼──────────────────┤
     │                │                 │ MCPToolDef[]     │                  │
     │                │                 │                  │                  │
     │                │ cache tools     │                  │                  │
     │                │◄────────────────┤                  │                  │
     │                │                 │                  │                  │
     │                │ createMCPTool   │                  │                  │
     │                │ Adapter × N     │                  │                  │
     │                ├─────────────────┼─────────────────►│                  │
     │                │                 │                  │ wrap MCP tool    │
     │                │                 │                  │ as Sema Tool     │
     │                │◄────────────────┼──────────────────┤                  │
     │◄───────────────┤                 │                  │                  │
     │                │                 │                  │                  │
     │  ── later, during query() ──     │                  │                  │
     │                │                 │                  │                  │
     │  getMCPTools() │ (cached)        │                  │                  │
     ├───────────────►│─────────────────┼──────────────────┤                  │
     │◄───────────────┤ Tool[]          │                  │                  │
     │                │                 │                  │                  │
     │  ── agent calls MCP tool ──      │                  │                  │
     │                │                 │                  │                  │
     │                │  callTool(name, │                  │                  │
     │                │  args)          │                  │                  │
     │                ├────────────────►│                  │                  │
     │                │                 │ callTool()       │                  │
     │                │                 ├──────────────────┼─────────────────►│
     │                │                 │ MCPToolResult    │                  │
     │                │◄────────────────┼──────────────────┤◄─────────────────┤
     │                │                 │                  │                  │
     │                │ emit MCP tool   │                  │                  │
     │                │ completion      │                  │                  │
```


### 8.1 MCPManager

File: `dist/services/mcp/MCPManager.js`

- Quản lý 2 scope: `project` và `user`
- Lưu config MCP server ra file JSON
- Cache tool list, tự động refresh khi file config thay đổi (theo mtime)
- Merge tools từ tất cả server, filter theo `useTools` của từng server

### 8.2 MCPClient

File: `dist/services/mcp/MCPClient.js`

- Wrap MCP SDK client, quản lý connection đến một MCP server
- Hỗ trợ 3 transport: `stdio`, `sse`, `http`
- Timeout: connect 10s, call 600s (hỗ trợ long-running tools như dispatch_task)
- Cache capabilities sau khi connect

### 8.3 MCPToolAdapter

File: `dist/services/mcp/MCPToolAdapter.js`

Chuyển đổi MCP tool definition thành SemaCore Tool interface:
- Tên tool MCP được prefix: `mcp__<server_name>__<tool_name>`
- `parseMCPToolName()` parse tên để lấy server và tool gốc
- `isMCPTool()` kiểm tra prefix MCP

---

## 9. Agent System

### 9.1 AgentsManager

File: `dist/services/agents/agentsManager.js`

Quản lý sub-agent với 3 mức ưu tiên: **project > user > builtin**

Built-in agents (từ `defaultBuiltInAgentsConfs`):
- **general-purpose**: Agent tổng quát, đầy đủ tools
- **Explore**: Agent chỉ đọc (Read/Glob/Grep), dùng cho khám phá codebase
- **Plan**: Agent thiết kế kiến trúc, chuyên lập kế hoạch

### 9.2 Agent Config

```typescript
interface AgentConfig {
    name: string;           // tên duy nhất
    description: string;    // mô tả cho Task tool
    tools?: string[] | '*'; // danh sách tool được phép, '*' = tất cả
    prompt: string;         // system prompt riêng
    model?: string;         // haiku/sonnet/opus/inherit
    locate?: 'user' | 'project' | 'builtin';
}
```

Cấu hình được định nghĩa trong file Markdown với YAML frontmatter:

```markdown
---
name: my-agent
description: "My custom agent"
tools: Glob, Grep, Read
model: haiku
---

Agent prompt content here...
```

### 9.3 Task Tool (Sub-agent dispatch)

Tool `Task` cho phép agent chính dispatch sub-agent. Input: `description`, `prompt`, `subagent_type`. Sub-agent chạy với context riêng:
- AbortController riêng (hủy được)
- `agentId = taskId` (không phải `main`)
- Không emit các event: `conversation:usage`, `message:chunk`, `state:update`, `todos:update`, `topic:update`
- Có emit: `task:agent:start`, `task:agent:end`, `message:complete`, `tool:*`

#### 9.3.1 Sub-agent Dispatch Flow

```
 Agent chính (main)       TaskTool        AgentsManager       SemaEngine        Sub-agent (taskId)
     │                       │                  │                  │                    │
     │ TaskTool.call()       │                  │                  │                    │
     ├──────────────────────►│                  │                  │                    │
     │                       │ getAgentConfig() │                  │                    │
     │                       ├─────────────────►│                  │                    │
     │                       │◄─────────────────┤                  │                    │
     │                       │ AgentConfig      │                  │                    │
     │                       │                  │                  │                    │
     │                       │ emit             │                  │                    │
     │                       │ task:agent:start │                  │                    │
     │                       │                  │                  │                    │
     │                       │ build tools cho  │                  │                    │
     │                       │ sub-agent (filter│                  │                    │
     │                       │ theo config)     │                  │                    │
     │                       │                  │                  │                    │
     │                       │ build system     │                  │                    │
     │                       │ prompt riêng     │                  │                    │
     │                       │                  │                  │                    │
     │                       │              ┌───┼──────────────────┤                    │
     │                       │              │   │ query()          │                    │
     │                       │              │   │ (recursive loop) │                    │
     │                       │              │   ├──────────────────┼───────────────────►│
     │                       │              │   │                  │  queryLLM()        │
     │                       │              │   │                  │◄───────────────────┤
     │                       │              │   │                  │  AssistantMessage  │
     │                       │              │   │                  │                    │
     │                       │              │   │                  │  runTools()        │
     │                       │              │   │                  │◄───────────────────┤
     │                       │              │   │                  │  ToolResult        │
     │                       │              │   │                  │                    │
     │                       │              │   │                  │  ...recurse...     │
     │                       │              │   │                  │                    │
     │                       │              └───┼──────────────────┤                    │
     │                       │                  │                  │                    │
     │                       │ emit             │                  │                    │
     │                       │ task:agent:end   │                  │                    │
     │                       │                  │                  │                    │
     │◄──── output ──────────┤                  │                  │                    │
     │  { agentType, result, │                  │                  │                    │
     │    durationMs }       │                  │                  │                    │
```

---

## 10. Skill System

### 10.1 Skill Metadata

```typescript
interface SkillMetadata {
    name: string;
    description: string;
    'allowed-tools'?: string[];      // soft constraint — tool được khuyến nghị
    'when-to-use'?: string;          // mô tả khi nào nên dùng
    model?: 'haiku' | 'sonnet' | 'opus' | 'inherit';
    'max-thinking-tokens'?: number;
    'disable-model-invocation'?: boolean;
    'argument-hint'?: string;
    version?: string;
}
```

### 10.2 Skill Registry

File: `dist/services/skill/skillRegistry.js`

- Load từ nhiều directory: builtin → global user (`~/.sema/skills/`) → project (`.sema/skills/`) → extra dirs
- Ưu tiên: load sau ghi đè load trước (project thắng global)
- Skill là file Markdown với YAML frontmatter
- `SkillTool` khi được gọi sẽ nạp nội dung skill vào context

### 10.3 Skill Loader

`loadAllSkills(workingDir, extraDirs?)` — memoized. Quét các thư mục, parse frontmatter, trả về danh sách `Skill[]`.

#### 10.3.1 Skill Loading Flow

```
  SemaEngine          SkillRegistry       SkillLoader         Filesystem
      │                     │                   │                  │
      │ initializePlugins() │                   │                  │
      ├────────────────────►│                   │                  │
      │                     │ loadAllSkills()   │                  │
      │                     ├──────────────────►│                  │
      │                     │                   │                  │
      │                     │           ┌───────┤ scan            │
      │                     │           │       │ ~/.sema/skills/ │
      │                     │           │       ├─────────────────►│
      │                     │           │       │◄─────────────────┤
      │                     │           │       │                  │
      │                     │           │       │ scan            │
      │                     │           │       │ .sema/skills/   │
      │                     │           │       ├─────────────────►│
      │                     │           │       │◄─────────────────┤
      │                     │           │       │                  │
      │                     │           │       │ scan extraDirs  │
      │                     │           │       ├─────────────────►│
      │                     │           │       │◄─────────────────┤
      │                     │           └───────┤                  │
      │                     │                   │                  │
      │                     │         ┌─────────┤ parse từng file │
      │                     │         │ gray-matter               │
      │                     │         │ tách frontmatter + body   │
      │                     │         │ validate metadata         │
      │                     │         └─────────┤                  │
      │                     │                   │                  │
      │                     │  Skill[]          │                  │
      │                     │◄──────────────────┤                  │
      │                     │                   │                  │
      │                     │ Ghi đè theo       │                  │
      │                     │ priority:         │                  │
      │                     │ project > global  │                  │
      │                     │                   │                  │
      │  SkillRegistry      │                   │                  │
      │◄────────────────────┤                   │                  │
```

### 10.4 Skill Call Flow

```
  Agent                 SkillTool           SkillRegistry
    │                       │                     │
    │ SkillTool.call()      │                     │
    ├──────────────────────►│                     │
    │                       │ findSkill(name)     │
    │                       ├────────────────────►│
    │                       │◄────────────────────┤
    │                       │ Skill (content,     │
    │                       │  allowed-tools,     │
    │                       │  baseDir)           │
    │                       │                     │
    │◄──── load skill ──────┤                     │
    │  content into context │                     │
    │                       │                     │
    │  Skill content now    │                     │
    │  guides agent behavior│                     │
    │  with soft tool       │                     │
    │  constraints          │                     │
```

---

## 11. Custom Commands (Slash Commands)

File: `dist/services/plugins/customCommands.js`

```typescript
interface CustomCommand {
    name: string;           // "optimize" hoặc "frontend:test"
    displayName: string;    // "/optimize" hoặc "/frontend:test"
    description: string;
    argumentHint?: string;  // "<file-path>"
    filePath: string;
    scope: 'user' | 'project';
    content: string;        // nội dung Markdown (không có frontmatter)
}
```

- Load từ `~/.sema/commands/` (user) và `.sema/commands/` (project)
- File Markdown với YAML frontmatter: `description`, `argument-hint`
- Hỗ trợ `$ARGUMENTS` placeholder trong nội dung
- Xử lý trong `tryHandleCustomCommand()` trước khi vào conversation loop

---

## 12. API Layer

### 12.1 queryLLM

File: `dist/services/api/queryLLM.js`

```typescript
queryLLM(
    messages: Message[],
    systemPromptContent: TextBlock[],
    signal: AbortSignal,
    tools: Tool[],
    modelPointer?: 'main' | 'quick',
    disableChunkEvents?: boolean,
    suppressErrorEvent?: boolean
): Promise<AssistantMessage>
```

Quy trình:
1. Lấy model profile từ ModelManager
2. Resolve adapter (Anthropic hoặc OpenAI) qua `resolveAdapter()`
3. Thử cache LLM → nếu cache hit, trả về ngay
4. Gọi adapter tương ứng (`queryAnthropic` hoặc `queryOpenAI`)
5. Lưu cache nếu response không có tool_use
6. Trả về `AssistantMessage`

Hàm `queryQuick()` dùng model `quick` cho các tác vụ nhẹ (compact, topic detection).

#### 12.1.1 queryLLM Internal Flow

```
  Conversation         queryLLM          LLMCache        resolveAdapter    Anthropic/OpenAI
      │                    │                 │                 │                 │
      │ queryLLM()         │                 │                 │                 │
      ├───────────────────►│                 │                 │                 │
      │                    │ getModel()     │                 │                 │
      │                    │ from ModelMgr  │                 │                 │
      │                    │                 │                 │                 │
      │                    │ tryGetCached() │                 │                 │
      │                    ├────────────────►│                 │                 │
      │                    │ [HIT]           │                 │                 │
      │                    │◄────────────────┤                 │                 │
      │◄───────────────────┤ cached response │                 │                 │
      │                    │                 │                 │                 │
      │                    │ [MISS]          │                 │                 │
      │                    │◄────────────────┤                 │                 │
      │                    │                 │                 │                 │
      │                    │ resolveAdapter(provider, model)   │                 │
      │                    ├─────────────────┼────────────────►│                 │
      │                    │◄────────────────┼─────────────────┤                 │
      │                    │                 │                 │ 'anthropic'|'openai'
      │                    │                 │                 │                 │
      │                    │ ┌── anthropic ──┤                 │                 │
      │                    │ │ queryAnthropic│                 │                 │
      │                    │ ├───────────────┼─────────────────┼────────────────►│
      │                    │ │               │                 │    /v1/messages │
      │                    │ │               │                 │◄────────────────┤
      │                    │ │               │                 │  stream chunks  │
      │                    │ │               │                 │                 │
      │                    │ │ emit chunk     │                 │                 │
      │                    │ │ events (text+  │                 │                 │
      │                    │ │ thinking)      │                 │                 │
      │                    │ └───────────────┤                 │                 │
      │                    │                 │                 │                 │
      │                    │ ┌── openai ────┤                 │                 │
      │                    │ │ queryOpenAI   │                 │                 │
      │                    │ ├───────────────┼─────────────────┼────────────────►│
      │                    │ │               │                 │ /v1/chat/comp   │
      │                    │ │ convert Anthropic ←→ OpenAI      │◄───────────────┤
      │                    │ │ format        │                 │  stream chunks  │
      │                    │ └───────────────┤                 │                 │
      │                    │                 │                 │                 │
      │                    │ [no tool_use]   │                 │                 │
      │                    │ setCached()     │                 │                 │
      │                    ├────────────────►│                 │                 │
      │                    │◄────────────────┤                 │                 │
      │                    │                 │                 │                 │
      │◄───────────────────┤ AssistantMessage│                 │                 │
```

### 12.2 Anthropic Adapter

File: `dist/services/api/adapt/anthropic.js`

- Dùng `@anthropic-ai/sdk`
- Hỗ trợ streaming (text + thinking chunks) qua `emitChunkEvent()`
- Temperature: `MAIN_QUERY_TEMPERATURE = 0.7`
- Có cơ chế retry khi gặp `ContextLengthError` — tự động giảm `max_tokens` cho lần retry

### 12.3 OpenAI Adapter

File: `dist/services/api/adapt/openai.js`

- Dùng `openai` SDK
- Parse Anthropic-style content blocks sang OpenAI format và ngược lại
- Hỗ trợ streaming
- Hỗ trợ `max_completion_tokens` cho các model yêu cầu tham số này

### 12.4 LLM Cache

File: `dist/services/api/cache.js`

- Cache response LLM dựa trên: messages + system prompt + model + stream/thinking flags
- Chỉ cache response không có tool_use
- Dùng để tránh gọi LLM trùng lặp

---

## 13. Conversation Loop

File: `dist/core/Conversation.js`

Hàm `query()` là một async generator, thực hiện vòng lặp:

```
 ┌─────────────────────────────────────────────────┐
 │  1. Auto-compact check (chỉ main agent)         │
 │  2. queryLLM() → AssistantMessage               │
 │  3. Nếu bị abort → interrupt, return            │
 │  4. Yield AssistantMessage                      │
 │  5. Nếu không có tool_use → finalize, return    │
 │  6. Phân loại tool: read-only concurrent / khác │
 │  7. runToolsConcurrently() | runToolsSerially() │
 │  8. Nếu bị abort → interrupt, return            │
 │  9. Kiểm tra control signal (mode switch)       │
 │ 10. Đệ quy: query(newMessages, ...)            │
 └─────────────────────────────────────────────────┘
```

### Điểm kiểm tra abort (checkpoints):
- **Checkpoint 1**: Sau AI response, trước khi chạy tools
- **Checkpoint 2**: Sau khi tất cả tools chạy xong, trước recursive query
- **Checkpoint 3**: Trước mỗi tool execution
- **Checkpoint 4**: Sau mỗi tool execution

Khi bị abort, các tool_use chưa chạy được gán `tool_result` với nội dung ngắt để tránh lỗi API khi resume.

### Tool execution strategies:
- **Concurrent**: Khi tất cả tool_use là read-only → chạy song song
- **Serial**: Khi có ít nhất một tool write → chạy tuần tự

### Control Signal:
- `ExitPlanMode` tool có thể gửi `ToolControlSignal` với `rebuildContext`
- Khi nhận được, engine rebuild context: lấy lại tools, system prompt, và messages mới
- Dùng để chuyển từ Plan mode sang Agent mode (và ngược lại)

### 13.1 Detailed Conversation Loop Sequence

```
 ┌──────────────────────────────────────────────────────────────────────────────┐
 │                        CONVERSATION LOOP (query)                              │
 └──────────────────────────────────────────────────────────────────────────────┘

  messages ─────► ┌─────────────────────────────────────────────────────┐
                  │  [Main Agent Only]                                  │
                  │  checkAutoCompact(messages, abortController)         │
                  │  ├── tính token count                               │
                  │  ├── nếu vượt ngưỡng → compactMessages()            │
                  │  │   ├── dùng quick model tóm tắt history           │
                  │  │   ├── emit compact:start / compact:exec          │
                  │  │   └── fallback: cắt bớt message cũ nếu lỗi      │
                  │  └── trả về { messages, wasCompacted }              │
                  └──────────────┬──────────────────────────────────────┘
                                 │
                  ┌──────────────▼──────────────────────────────────────┐
                  │  queryLLM(messages, systemPrompt, signal, tools)     │
                  │  ├── resolveAdapter(provider, model)                 │
                  │  ├── tryGetCachedResponse()                          │
                  │  ├── queryAnthropic() | queryOpenAI()                │
                  │  │   ├── streaming chunks → emit chunk events        │
                  │  │   └── retry nếu ContextLengthError                │
                  │  └── setCachedResponse() nếu không có tool_use       │
                  └──────────────┬──────────────────────────────────────┘
                                 │ AssistantMessage
                  ┌──────────────▼──────────────────────────────────────┐
                  │  [Checkpoint 1]                                     │
                  │  if (aborted) → emit session:interrupted            │
                  │     → gen tool_result stop cho pending tool_use     │
                  │     → finalizeMessages() → return                   │
                  └──────────────┬──────────────────────────────────────┘
                                 │
                  ┌──────────────▼──────────────────────────────────────┐
                  │  yield AssistantMessage                             │
                  │  emit message:complete (text + toolCalls)           │
                  │  emit conversation:usage                            │
                  └──────────────┬──────────────────────────────────────┘
                                 │
                    ┌────────────▼────────────┐
                    │  Có tool_use không?     │
                    └──────┬─────────┬────────┘
                           │ YES     │ NO
                           ▼         ▼
              ┌──────────────────┐  ┌──────────────────────────┐
              │ Tất cả read-only?│  │ finalizeMessages()       │
              └────┬─────────┬───┘  │ state → idle             │
                   │ YES     │ NO    │ return                   │
                   ▼         ▼       └──────────────────────────┘
         ┌────────────┐ ┌──────────────┐
         │ CONCURRENT │ │   SERIAL     │
         │ (song song)│ │ (tuần tự)    │
         └──────┬─────┘ └──────┬───────┘
                │               │
                ▼               ▼
    ┌──────────────────────────────────────────┐
    │  For each tool_use:                      │
    │  ├── [Checkpoint 3] if aborted → break   │
    │  ├── checkPermissionsAndCallTool()        │
    │  │   ├── PermissionMgr kiểm tra           │
    │  │   ├── nếu cần → emit permission req    │
    │  │   │   → wait user response             │
    │  │   ├── tool.call() → AsyncGenerator     │
    │  │   └── yield UserMessage (tool_result)  │
    │  ├── emit tool:execution:complete/error   │
    │  └── [Checkpoint 4] if aborted → break   │
    └──────────────┬───────────────────────────┘
                   │
    ┌──────────────▼───────────────────────────┐
    │  [Checkpoint 2]                          │
    │  if (aborted) → emit session:interrupted │
    │     → gen stop cho pending tools         │
    │     → finalizeMessages() → return        │
    └──────────────┬───────────────────────────┘
                   │
    ┌──────────────▼───────────────────────────┐
    │  handleControlSignalRebuild()            │
    │  ├── kiểm tra ToolControlSignal          │
    │  ├── nếu rebuildContext:                 │
    │  │   ├── updateAgentMode()               │
    │  │   ├── getTools() lại                  │
    │  │   ├── formatSystemPrompt() lại        │
    │  │   └── build messages mới              │
    │  └── nếu không: append tool_results      │
    └──────────────┬───────────────────────────┘
                   │
                   ▼
    ┌──────────────────────────────────────────┐
    │  RECURSE: query(nextMessages,            │
    │                  nextSystemPrompt,        │
    │                  nextAgentContext)        │
    └──────────────────────────────────────────┘
```

---

## 14. Context Management

### 14.1 Auto Compact

File: `dist/util/compact.js`

`checkAutoCompact()` được gọi trước mỗi `query()` cho main agent:
- Kiểm tra nếu context vượt ngưỡng → gọi `compactMessages()`
- Compact dùng LLM (quick model) để tóm tắt lịch sử hội thoại
- Fallback: cắt bớt message cũ nếu compact thất bại
- Emit events: `compact:start`, `compact:exec`

### 14.2 Token Counting

File: `dist/util/tokens.js`

`getTokens(messages)` tính tổng token usage, trả về `Usage { useTokens, maxTokens, promptTokens }`.

---

## 15. Plan Mode

Khi `agentMode = 'Plan'`:
- System prompt thay đổi để hướng dẫn agent lập kế hoạch thay vì thực thi
- `TodoWrite` tool bị loại bỏ khỏi danh sách tools
- `ExitPlanMode` tool được thêm vào
- Khi user chọn "Start editing", `ExitPlanMode` emit `rebuildContext` signal
- Context được rebuild: tools mới, system prompt mới, messages mới

### 15.1 Plan → Agent Mode Transition

```
  User             EventBus        ExitPlanMode      Conversation        ConfManager
   │                  │                 │                  │                  │
   │                  │ plan:exit:      │                  │                  │
   │                  │ request         │                  │                  │
   │◄─────────────────┤                 │                  │                  │
   │                  │                 │                  │                  │
   │ respondToPlan    │                 │                  │                  │
   │ Exit("clearCtx") │                 │                  │                  │
   ├─────────────────►│                 │                  │                  │
   │                  │ plan:exit:      │                  │                  │
   │                  │ response        │                  │                  │
   │                  ├────────────────►│                  │                  │
   │                  │                 │                  │                  │
   │                  │                 │ yield ToolResult │                  │
   │                  │                 │ with             │                  │
   │                  │                 │ controlSignal:   │                  │
   │                  │                 │   rebuildContext │                  │
   │                  │                 ├─────────────────►│                  │
   │                  │                 │                  │                  │
   │                  │                 │          ┌───────┤ handleControl    │
   │                  │                 │          │       │ SignalRebuild()  │
   │                  │                 │          │       │                  │
   │                  │                 │          │       │ updateAgentMode  │
   │                  │                 │          │       │ ('Agent')        │
   │                  │                 │          │       ├─────────────────►│
   │                  │                 │          │       │                  │
   │                  │                 │          │       │ getTools(useTools)
   │                  │                 │          │       │ + MCP tools      │
   │                  │                 │          │       │                  │
   │                  │                 │          │       │ genSystemPrompt()│
   │                  │                 │          │       │ (Agent mode)     │
   │                  │                 │          │       │                  │
   │                  │                 │          │       │ build messages   │
   │                  │                 │          │       │ from rebuildMsg  │
   │                  │                 │          └───────┤                  │
   │                  │                 │                  │                  │
   │                  │ emit            │                  │                  │
   │                  │ plan:implement  │                  │                  │
   │◄─────────────────┤                 │                  │                  │
   │                  │                 │                  │                  │
   │                  │                 │       ┌──────────┤ recurse query()  │
   │                  │                 │       │ Agent mode│ with new context │
   │                  │                 │       │ tiếp tục │                  │
```

---

## 16. Cấu trúc thư mục (dist)

```
dist/
├── constants/          # config paths, message strings, product info
│   ├── config.js       # SEMA_ROOT, các đường dẫn & giới hạn
│   ├── message.js      # INTERRUPT_MESSAGE, CANCEL_MESSAGE, ...
│   └── product.js      # PRODUCT_NAME, GROUP, PROJECT_FILE (AGENTS.md)
├── core/               # Lõi engine
│   ├── SemaCore.js     # Public API
│   ├── SemaEngine.js   # Engine nội bộ
│   ├── EngineContext.js # AsyncLocalStorage isolation
│   ├── Conversation.js # Hàm query() — vòng lặp hội thoại
│   └── RunTools.js     # runToolsConcurrently / runToolsSerially
├── events/             # Hệ thống event
│   ├── EventSystem.js  # EventBus, getEventBus()
│   └── types.js        # Type event data
├── manager/            # Managers
│   ├── StateManager.js
│   ├── ModelManager.js
│   ├── ConfManager.js
│   └── PermissionManager.js
├── services/
│   ├── agents/         # AgentsManager, genSystemPrompt, prompt
│   ├── api/            # queryLLM, adapters (Anthropic, OpenAI), cache
│   ├── command/        # System commands (/clear, /compact, ...)
│   ├── mcp/            # MCPClient, MCPManager, MCPToolAdapter
│   ├── plugins/        # Custom slash commands
│   └── skill/          # SkillLoader, SkillParser, SkillRegistry
├── tools/              # Tool implementations
│   ├── base/           # Tool interface, getTools, buildTools
│   ├── Bash/
│   ├── Read/
│   ├── Write/
│   ├── Edit/
│   ├── Glob/
│   ├── Grep/
│   ├── Task/
│   ├── Skill/
│   ├── TodoWrite/
│   ├── AskUserQuestion/
│   ├── ExitPlanMode/
│   └── NotebookEdit/
├── types/              # TypeScript type definitions
│   ├── index.ts        # SemaCoreConfig, ModelConfig, ...
│   ├── agent.ts
│   ├── command.ts
│   ├── config.ts
│   ├── errors.ts       # ContextLengthError, InterruptedException
│   ├── mcp.ts
│   ├── message.ts      # Message, UserMessage, AssistantMessage
│   ├── model.ts
│   ├── notebook.ts
│   ├── skill.ts
│   └── uuid.ts
└── util/               # Utilities
    ├── adapter.js      # resolveAdapter (Anthropic/OpenAI)
    ├── compact.js      # checkAutoCompact, compactMessages
    ├── diff.js
    ├── exec.js
    ├── file.js
    ├── git.js
    ├── history.js
    ├── message.js      # createUserMessage, normalizeMessagesForAPI
    ├── model.js
    ├── ripgrep.js      # @vscode/ripgrep wrapper
    ├── rules.js        # AGENTS.md rules loading
    ├── session.js      # session ID generation
    ├── shell.js        # shell-quote + spawn-rx
    ├── tokens.js
    └── ...
```

---

## 17. Cấu hình & Constants

### 17.1 SemaCoreConfig

```typescript
interface SemaCoreConfig {
    instanceId?: string;
    workingDir?: string;
    agentDataDir?: string;
    skillsExtraDirs?: Array<{ dir: string; locate: SkillLocate }>;
    logLevel?: 'debug' | 'info' | 'warn' | 'error' | 'none';
    stream?: boolean;           // default: false
    thinking?: boolean;         // default: false
    systemPrompt?: string;
    customRules?: string;
    skipFileEditPermission?: boolean;  // default: false
    skipBashExecPermission?: boolean;  // default: false
    skipSkillPermission?: boolean;     // default: false
    skipMCPToolPermission?: boolean;   // default: false
    enableLLMCache?: boolean;          // default: false
    useTools?: string[] | null;
    agentMode?: 'Agent' | 'Plan';
    skipMCPInit?: boolean;
}
```

### 17.2 Đường dẫn mặc định

Base: `~/.sema/`
- Model config: `~/.sema/model.conf`
- Project config: `~/.sema/projects.conf`
- History: `~/.sema/history/`
- Logs: `~/.sema/logs/`
- LLM logs: `~/.sema/llm_logs/`
- Event logs: `~/.sema/event/`
- Skills (user): `~/.sema/skills/`
- Skills (project): `.sema/skills/`
- Commands (user): `~/.sema/commands/`
- Commands (project): `.sema/commands/`
- Agents (user): `~/.sema/agents/`
- Agents (project): `.sema/agents/`

### 17.3 Dependencies chính

| Package | Vai trò |
|:--------|:-------|
| `@anthropic-ai/sdk` | Anthropic Claude API |
| `openai` | OpenAI API & compatible providers |
| `@modelcontextprotocol/sdk` | MCP client implementation |
| `zod` + `zod-to-json-schema` | Tool input validation & schema gen |
| `@vscode/ripgrep` | Fast file search (Grep tool) |
| `diff` | Diff generation (Edit/Write tools) |
| `glob` | File pattern matching (Glob tool) |
| `gray-matter` | YAML frontmatter parsing (Skills, Agents, Commands)|
| `shell-quote` + `spawn-rx` | Shell command execution (Bash tool) |
| `lru-cache` | LLM response cache |
| `nanoid` | Session ID generation |
| `lodash-es` | Memoize, utility functions |
