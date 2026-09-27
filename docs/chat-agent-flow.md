# Luồng chat agent

Tài liệu này mô tả end-to-end luồng chat agent của SenClaw: từ Web UI hoặc channel adapter,
qua WebSocket/MessageRouter/GroupQueue/AgentPool, vào ZenCore, rồi quay lại UI bằng các event realtime.
Tài liệu tập trung vào luồng chat thường; phần dispatch/DAG team được nhắc ở các điểm giao nhau.

## Bức Tranh Tổng Quan

```mermaid
flowchart LR
    subgraph UI["Web UI"]
        ChatPage["ChatPage"]
        ChatView["ChatView"]
        WsHook["useWebSocket"]
        MsgBubble["MessageBubble"]
    end

    subgraph Gateway["Gateway"]
        WSG["WebSocketGateway"]
        WsHandlers["websocket handlers"]
        Router["MessageRouter"]
    end

    subgraph AgentRuntime["Agent runtime"]
        Queue["GroupQueue<br/>FIFO theo JID"]
        Pool["AgentPool"]
        Core["ZenCore / ZenEngine"]
        Sink["WsAgentEventSink"]
    end

    subgraph Persistence["Persistence"]
        DB["SQLite messages/groups"]
        Memory["Memory / daily log"]
    end

    ChatView --> WsHook
    WsHook --> WSG
    WSG --> WsHandlers
    WsHandlers --> DB
    WsHandlers --> Queue
    Router --> DB
    Router --> Queue
    Queue --> Pool
    Pool --> Memory
    Pool --> Core
    Core --> Pool
    Pool --> Sink
    Sink --> WSG
    WSG --> WsHook
    WsHook --> ChatPage
    ChatPage --> ChatView
    ChatView --> MsgBubble
```

Các file chính:

- UI chat: `web/src/pages/ChatPage.tsx`, `web/src/components/ChatView.tsx`,
  `web/src/components/MessageBubble.tsx`.
- WebSocket hook: `web/src/hooks/useWebSocket.ts`.
- WebSocket server: `src/gateway/websocket_gateway/handlers.rs`,
  `src/gateway/websocket_gateway/notify.rs`, `src/gateway/websocket_gateway/gateway.rs`.
- Channel router: `src/gateway/message_router.rs`.
- Agent lifecycle: `src/agent/agent_pool/pool.rs`.
- Daemon wiring: `src/lib.rs`.

## Khởi Tạo UI Và Subscribe

Khi Web UI mount, `useWebSocket()` mở kết nối tới `ws://127.0.0.1:<wsPort>`.
Sau `auth:ok`, hook lấy danh sách groups/channels/agents/bindings. Khi có groups, UI tự subscribe
admin group để nhận snapshot admin và các event broadcast admin.

```mermaid
sequenceDiagram
    participant UI as ChatPage/AppLayout
    participant Hook as useWebSocket
    participant WS as WebSocketGateway
    participant API as WsApi

    UI->>Hook: mount
    Hook->>WS: connect(token)
    WS-->>Hook: auth:ok
    Hook->>WS: list:groups
    Hook->>WS: list:channels
    Hook->>WS: list:agents
    Hook->>WS: list:bindings
    WS-->>Hook: groups
    Hook->>WS: subscribe(admin group)
    WS->>API: get_dispatch_parents()
    WS-->>Hook: dispatch:update snapshot
    WS->>API: get_agent_todos()
    WS-->>Hook: agent:todos snapshot(s)
    WS-->>Hook: agent:tools snapshot(s)
```

`ChatPage` chọn group đầu tiên, ưu tiên admin group. Khi user chọn group trong sidebar,
`ChatPage` gọi `ws.subscribe(jid)` nếu chưa subscribe. Khi có dispatch active/queued,
`ChatPage` gọi `subscribeAll()` để UI cũng nhận chat transcript của các child agents.

## Luồng Gửi Tin Nhắn Từ Web UI

Khi agent idle, nút action trong `ChatView` là Send. User gửi bằng Enter hoặc nút send:

1. `ChatView` gọi `onSend(text)`.
2. `ChatPage` truyền thành `ws.sendMessage(selectedJid, text)`.
3. `useWebSocket` thêm optimistic message local vào `messages[jid]`.
4. Hook gửi frame `{ type: "message", groupJid, text }`.
5. Backend `handle_message_send()` validate auth, group, command, rồi persist message vào DB.
6. Backend gọi `state.api.enqueue_and_process(&group_jid, &group, &text)`.
7. Daemon implementation đưa job vào `GroupQueue`.
8. Job gọi `AgentPool::process_and_wait()`.

```mermaid
sequenceDiagram
    participant User
    participant ChatView
    participant Hook as useWebSocket
    participant WSH as handle_message_send
    participant DB as SQLite
    participant Queue as GroupQueue
    participant Pool as AgentPool
    participant Core as ZenCore

    User->>ChatView: nhập text + Enter/Send
    ChatView->>Hook: sendMessage(jid, text)
    Hook->>Hook: add optimistic user message
    Hook->>WSH: WS frame type=message, groupJid, text
    WSH->>WSH: require_auth + validate group
    WSH->>WSH: admin command interception
    WSH->>DB: insert_group_message()
    WSH->>Queue: enqueue_and_process()
    Queue->>Pool: process_and_wait(jid, prompt)
    Pool->>Core: process_user_input(fullPrompt)
```

Lưu ý:

- Web message được persist trong `handle_message_send()` để lần sau `history:load` vẫn có user message.
- Nếu client là admin và text là command (`/reset`, v.v.), `dispatch_command()` xử lý trước và có thể
  trả `agent:reply` ngay, không đi qua AgentPool.
- Pending binding (`:pending:`) không cho chat từ UI cho tới khi channel thật hoàn tất binding.

## Luồng Tin Nhắn Từ Channel Adapter

Tin nhắn từ Telegram/Feishu/QQ/WeChat/App không đi qua `handle_message_send()`.
Nó đi vào `MessageRouter::handle_message()` dưới dạng `IncomingMessage`.

```mermaid
sequenceDiagram
    participant Channel as Channel adapter
    participant Router as MessageRouter
    participant GM as GroupManager
    participant DB as SQLite
    participant WSG as WebSocketGateway
    participant UI as Web UI
    participant Queue as GroupQueue
    participant Pool as AgentPool

    Channel->>Router: IncomingMessage
    Router->>GM: resolve group/binding
    Router->>GM: complete pending binding nếu cần
    Router->>DB: store_message()
    Router->>WSG: notify_incoming()
    WSG-->>UI: incoming
    Router->>Router: should_trigger()
    alt Không trigger
        Router-->>Channel: dừng
    else Trigger
        Router->>Router: command_dispatcher nếu admin command
        Router->>GM: touch_active()
        Router->>Queue: enqueue(jid, run_agent)
        Queue->>Pool: process_and_wait(jid, prompt từ DB history)
    end
```

Điểm khác với Web UI:

- Router luôn persist incoming message trước, rồi mới trigger check.
- `notify_incoming()` broadcast tới các UI đang subscribe group để thấy message realtime.
- Nếu `requires_trigger` không thỏa, agent không chạy nhưng message vẫn lưu trong history.

## Build Prompt Và FIFO Theo Group

`GroupQueue` đảm bảo mỗi `jid` chỉ chạy một job chat tại một thời điểm. Điều này tránh hai message
cùng group làm race session/context.

```mermaid
flowchart TD
    A["MessageRouter / WebSocket handler"] --> B["GroupQueue.enqueue(jid, job)"]
    B --> C{"JID đang có job chạy?"}
    C -- Có --> D["Đợi trong queue của JID"]
    C -- Không --> E["Chạy job"]
    D --> E
    E --> F["build_prompt_for_group(DB, jid)"]
    F --> G["AgentPool::process_and_wait()"]
    G --> H["Khi xong: chạy job tiếp theo của cùng JID"]
```

Với channel message, `run_agent()` gọi `build_prompt_for_group()` để lấy prompt từ DB history.
Sau khi `process_and_wait()` hoàn tất, router ghi `last_agent_timestamp` để đánh dấu cursor xử lý.

Với Web UI message, `handle_message_send()` truyền text mới vào `enqueue_and_process()`.
Implementation daemon vẫn đưa về AgentPool path, còn history đã được persist trước đó.

## AgentPool Và ZenCore

`AgentPool::process_and_wait_inner()` là trung tâm của vòng đời một lượt agent:

1. `get_or_create(group)` tạo hoặc lấy core/session hiện có cho JID.
2. Có thể inject memory pre-retrieval vào prompt nếu config bật.
3. Ghi daily log role user.
4. Đăng ký event bridge cho `state:update`, `session:error`, abort và reset timer.
5. Bật typing indicator.
6. Gọi `core_api.process_user_input(jid, full_prompt)` non-blocking.
7. Chờ event `Idle`, `Error`, `Reset`, `Abort`, hoặc timeout.
8. Cleanup registrations, tắt typing indicator.

```mermaid
stateDiagram-v2
    [*] --> GetOrCreate
    GetOrCreate --> MemoryInjection
    MemoryInjection --> RegisterEventBridge
    RegisterEventBridge --> Processing: process_user_input()
    Processing --> Processing: state:update active -> Reset timeout
    Processing --> Paused: state:update paused
    Paused --> Processing: resume_agent()
    Processing --> Success: state:update idle
    Processing --> Retry: transient session:error
    Retry --> Processing: retries_left > 0
    Processing --> NetworkPreserved: NETWORK_ERROR
    Processing --> FatalReset: fatal session:error
    Processing --> Timeout: inactivity timeout
    Processing --> Aborted: stop/destroy
    Success --> [*]
    NetworkPreserved --> [*]
    FatalReset --> [*]
    Timeout --> [*]
    Aborted --> [*]
```

Event loop chính:

| Event | Nguồn | Hành động |
| --- | --- | --- |
| `Idle` | `state:update` từ core | resolve lượt chat, cleanup |
| `Reset` | `state:update` active hoặc permission/activity | reset inactivity timer |
| `Error(data)` | `session:error` | retry/network/fatal theo phân loại |
| `Abort` | `stop_agent()` / destroy | dừng lượt đang chờ |
| `Timeout` | timer `AGENT_TIMEOUT_MS` | destroy, notify dispatch error nếu có |

## Core Events Đi Ngược Lên UI

Khi `ZenCore` phát event, `AgentPool::bind_events()` đã đăng ký các handler persistent theo JID.
Các handler này vừa cập nhật state nội bộ, vừa forward qua `WsAgentEventSink` tới WebSocket.

```mermaid
flowchart LR
    Core["ZenCore events"] --> MC["message:complete"]
    Core --> SU["state:update"]
    Core --> TU["todos:update"]
    Core --> CS["compact:start / compact:exec"]
    Core --> SE["session:error"]
    Core --> PR["permission/question request"]

    MC --> Reply["broadcast reply<br/>agent:reply"]
    SU --> State["agent:state"]
    TU --> Todos["agent:todos"]
    CS --> Compact["agent:compacting"]
    SE --> PAW["process_and_wait loop<br/>Error(data)"]
    PR --> Cards["permission:request<br/>question:request"]

    Reply --> WS["WebSocketGateway"]
    State --> WS
    Todos --> WS
    Compact --> WS
    Cards --> WS
    WS --> Hook["useWebSocket"]
    Hook --> UI["ChatView / AgentConsole"]
```

Các event UI nhận trong `useWebSocket.ts`:

- `incoming`: thêm message `other` nếu không phải `isFromMe`.
- `history:load`: thay messages của group bằng history từ backend.
- `agent:reply`: thêm message role `agent`.
- `agent:state`: cập nhật `agentStates[groupJid]`; `ChatView` đổi Ready/Thinking/Paused.
- `agent:compacting`: disable pause khi compacting.
- `agent:usage`: cập nhật token usage trên header chat.
- `permission:request` và `question:request`: thêm card tương tác vào message list.
- `permission:resolved` và `question:resolved`: cập nhật card đã xử lý.
- `agent:todos`: cập nhật Agent Console, không nằm trực tiếp trong transcript chat.

## Render Trên ChatView

`ChatView` là component render transcript và input state.

```mermaid
flowchart TD
    A["ws.messages[selectedJid]"] --> B["ChatView.visibleMessages"]
    B --> C["MessageBubble"]
    D["ws.agentStates[selectedJid]"] --> E{"agentState"}
    E -- idle --> F["Ready + Send"]
    E -- processing --> G["Thinking + TypingIndicator + Pause"]
    E -- paused --> H["Paused + Resume"]
    I["ws.agentCompacting[selectedJid]"] --> J["Disable Pause<br/>show Compacting"]
    K["ws.agentUsage[selectedJid]"] --> L["Token usage progress"]
```

Input rules:

- Idle: Enter gửi message; Shift+Enter xuống dòng.
- Processing: textarea disabled, nút action là Pause.
- Paused: textarea cho phép nhập follow-up instruction, nút action là Resume.
- IME/bộ gõ đang composing thì không gửi để tránh Enter bị bắt nhầm.
- Reset session luôn hiển thị ở header; nếu agent active, reset sẽ terminate lượt đang chạy.

## Pause, Resume, Stop

UI gửi frame `agent:control` cho pause/resume/stop. Backend yêu cầu client đã subscribe group đó trước khi
được control agent.

```mermaid
sequenceDiagram
    participant ChatView
    participant Hook as useWebSocket
    participant WSH as handle_agent_control
    participant Pool as AgentPool
    participant Core as ZenCore
    participant WSG as WebSocketGateway

    ChatView->>Hook: pauseAgent(jid)
    Hook->>WSH: agent:control pause
    WSH->>Pool: pause_agent(jid)
    Pool->>Core: pause_session(jid)
    Core-->>Pool: state:update paused
    Pool->>WSG: agent:state paused
    WSG-->>Hook: agent:state

    ChatView->>Hook: resumeAgent(jid, query?)
    Hook->>WSH: agent:control resume, query?
    WSH->>Pool: resume_agent(jid, query?)
    Pool->>Core: process_user_input("Go on." hoặc query)

    ChatView->>Hook: stopAgent(jid)
    Hook->>WSH: agent:control stop
    WSH->>Pool: stop_agent(jid)
    Pool->>Core: create_session(jid)
    Pool->>WSG: agent:state idle + agent:todos []
```

Pause/resume có thêm nhánh dispatch:

- Nếu admin agent đang điều phối DAG, pause sẽ gọi `DispatchBridge::pause_admin(folder)` và pause các
  child JID đang có active abort.
- Resume sẽ gọi `DispatchBridge::resume_admin(folder)` và gửi `"Go on."` cho các child JID đã pause.
- Stop admin sẽ cancel dispatch parents và đệ quy stop child subagents.

## Permission Và Question Cards

Tool permission và AskUserQuestion đi qua card trong chat transcript.

```mermaid
sequenceDiagram
    participant Core as ZenCore
    participant Pool as AgentPool
    participant Bridge as PermissionBridge
    participant WSG as WebSocketGateway
    participant Hook as useWebSocket
    participant UI as MessageBubble

    Core-->>Pool: tool:permission:request / ask-question
    Pool->>Bridge: handle request
    Bridge->>WSG: permission:request / question:request
    WSG-->>Hook: websocket event
    Hook->>UI: append permission/question message
    UI->>Hook: user chooses option/answers
    Hook->>WSG: permission:response / question:response
    WSG->>Pool: resolve_permission / resolve_ask_question
    Pool->>Bridge: resolve pending request
    Bridge->>WSG: permission:resolved / question:resolved
    WSG-->>Hook: mark card resolved
```

Những cards này nằm trong `messages[jid]`, vì vậy `AgentConsole` cũng có thể scan toàn bộ messages để hiện
`Pending Permissions` tập trung.

## Compacting Và Usage

Khi core compact context:

- `compact:start` -> `agent:compacting { isCompacting: true }`.
- `compact:exec` -> `agent:compacting { isCompacting: false }`.
- UI disable pause trong lúc compacting để tránh dừng giữa quá trình ghi lại context.

`agent:usage` được broadcast tới all authenticated clients, keyed bằng `agentJid`.
`ChatView` hiển thị token usage nếu `ws.agentUsage[selectedJid]` có dữ liệu.

## Dispatch Child Task Đi Qua Luồng Chat

Khi một DAG task target persistent agent, `DispatchBridge` không gọi core trực tiếp. Nó dùng lại luồng chat:

1. Bridge build augmented prompt.
2. Daemon set workspace override, map `jid -> task_id`, mark `dispatch_executing`.
3. Job được enqueue vào `GroupQueue`.
4. AgentPool chạy `process_and_wait()` như chat thường.
5. Khi core `message:complete`, reply được lưu vào `last_dispatch_replies`.
6. Khi core `state:update idle`, AgentPool gọi `DispatchBridge::notify_task_done(task_id, reply)`.
7. DispatchBridge mutate state và phát `dispatch:update`.

```mermaid
flowchart TD
    A["DispatchBridge start_task"] --> B["build_augmented_prompt"]
    B --> C["set_current_dispatch_task_id<br/>mark_dispatch_executing"]
    C --> D["GroupQueue.enqueue(child_jid)"]
    D --> E["AgentPool::process_and_wait"]
    E --> F["ZenCore message:complete"]
    F --> G["last_dispatch_replies[jid]"]
    E --> H["ZenCore state:update idle"]
    H --> I["notify_task_done(task_id, reply)"]
    I --> J["DispatchBridge modify_state"]
    J --> K["dispatch:update"]
    K --> L["Agent Console DAG"]
```

Điều này giải thích vì sao child agent vẫn có transcript riêng trong Chat UI nếu UI đã subscribe group/JID đó.

## Các Event WebSocket Liên Quan Đến Chat

| Event | Backend phát từ | UI xử lý ở | Ý nghĩa |
| --- | --- | --- | --- |
| `incoming` | `MessageRouter -> notify_incoming()` | `useWebSocket` | Message từ channel adapter |
| `history:load` | subscribe/history handlers | `useWebSocket` | Load transcript persisted |
| `agent:reply` | `AgentPool -> WsAgentEventSink` | `useWebSocket` | Reply của agent |
| `agent:state` | `state:update` core event | `useWebSocket` | `idle`, `processing`, `paused`, ... |
| `agent:compacting` | compact events | `useWebSocket` | Context compacting đang chạy |
| `agent:usage` | usage callback | `useWebSocket` | Token usage |
| `permission:request` | PermissionBridge | `useWebSocket` | Card approve/deny |
| `question:request` | AskQuestion bridge | `useWebSocket` | Card trả lời câu hỏi |
| `permission:resolved` | PermissionBridge | `useWebSocket` | Mark permission card resolved |
| `question:resolved` | AskQuestion bridge | `useWebSocket` | Mark question card resolved |
| `agent:todos` | `todos:update` core event | `useWebSocket` | Agent Console todos |
| `dispatch:update` | DispatchBridge | `useWebSocket` | DAG workflow state |

## Debug Checklist

Khi chat không chạy hoặc UI không thấy reply:

1. Web UI có `auth:ok` và `groups` chưa.
2. Group đã được subscribe chưa (`subscribed` event).
3. Với Web UI send: backend có vào `handle_message_send()` và persist DB không.
4. Với channel send: `MessageRouter` có log `Triggering agent` hay bị `Trigger check failed`.
5. `GroupQueue` có đang bị job trước cùng JID giữ không.
6. `AgentPool` có log `process_user_input start jid=...` không.
7. Core có phát `message:complete` và `state:update idle` không.
8. `WsAgentEventSink` có gọi `notify_agent_reply` / `notify_agent_state` không.
9. UI có nhận `agent:reply` / `agent:state` trong browser console không.
10. Nếu là dispatch child task, kiểm tra thêm `dispatch_task_map`, `dispatch_executing`,
    và `dispatch:update`.

## Ranh Giới Với Tài Liệu Dispatch

Tài liệu này mô tả luồng chat agent thường và các điểm giao với dispatch child task.
Chi tiết sâu về `dispatch:update`, `agent:todos`, `task:backlog`, MCP dispatch server và Agent Console debug
nằm trong `docs/notify-dispatch-flow.md`.
