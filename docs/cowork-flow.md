# Cowork Chat Flow — Message → Task Decomposition → Agent Execution

## 1. High-level Architecture

```mermaid
sequenceDiagram
    actor User
    participant UI as Cowork UI (Web)
    participant WS as WebSocket Gateway (WS)
    participant REST as REST /messages
    participant CM as CoworkManager
    participant DB as SQLite
    participant AP as AgentPool
    participant ZE as ZenEngine (LLM)

    User->>UI: Send message in workspace
    UI->>WS: send:cowork:message
    WS->>CM: process_user_message(db, ws_id, from, content, now, agent_api?, self_arc)

    Note over CM: 1. Save message to DB
    CM->>DB: insert_cowork_message()

    Note over CM: 2. Find lead agent
    CM->>DB: list_cowork_members(ws_id)

    Note over CM: 3. Create tasks
    CM->>DB: create_task(title=content, assignee=lead, status=todo)

    Note over CM: 4. Check member triggers
    CM->>DB: create_task(assignee=triggered_member)

    alt agent_api is Some
        Note over CM: 5. Dispatch tasks to agents
        loop For each created task
            CM->>CM: send_to_cowork_agent()
            CM->>DB: update_cowork_task(status=in_progress)
            CM->>AP: process_and_wait(jid, group, prompt)
            AP->>ZE: Run agent with prompt
            ZE-->>AP: Agent output
            AP-->>CM: Result (ok/err)
            CM->>DB: update_cowork_task(status=done|todo)
            CM->>CM: fire_changed() → WS push
        end
    else agent_api is None
        Note over CM: Tasks created as todo, no execution
        CM->>CM: fire_changed() → WS push to UI
    end

    WS-->>UI: cowork:tasks + cowork:message:sent
    UI-->>User: Tasks appear in board
```

## 2. Entry Points

Messages enter the Cowork system through two paths:

| Path | Protocol | Handler | Orchestration |
|------|----------|---------|---------------|
| WebSocket | `send:cowork:message` | `handle_cowork_message_send` (cowork_handlers.rs) | If workspace has members: `process_user_message` with agent dispatch. Otherwise: simple `send_message` (DB insert only) |
| REST | `POST /api/cowork/{id}/messages` | `cowork_messages_send` (ui_server/cowork.rs) | `process_user_message` with agent dispatch from UiState |

## 3. Task Lifecycle

```mermaid
stateDiagram-v2
    [*] --> todo: User sends message → create_task()
    todo --> in_progress: send_to_cowork_agent() dispatches
    in_progress --> done: Agent process_and_wait() returns Ok
    in_progress --> todo: Agent process_and_wait() returns Err
    done --> in_progress: Handoff from dependent task (future)
    done --> [*]
```

Status transitions are persisted to DB via `update_cowork_task()` and pushed to WebSocket clients through `fire_changed()`.

## 4. Multi-Agent Pipeline (Software Development Template)

```mermaid
sequenceDiagram
    actor User
    participant CM as CoworkManager
    participant Code as code-agent (worker)
    participant Review as review-agent (reviewer)
    participant Qa as test-agent (worker)

    User->>CM: "Build user auth with JWT"
    CM->>CM: Create task T1 → assignee=code-agent
    CM->>CM: Process triggers → T2=review-agent, T3=test-agent

    par Lead agent first
        CM->>Code: process_and_wait(prompt + board)
        Code->>Code: Implement auth endpoints
        Code-->>CM: Ok → T1 status=done
    end

    Note over CM: T2 depends on T1

    CM->>Review: process_and_wait(prompt + T1 result)
    Review->>Review: Review diff, record decisions
    Review-->>CM: Ok → T2 status=done

    Note over CM: T3 triggered by T1 completion

    CM->>Qa: process_and_wait(prompt + review output)
    Qa->>Qa: Write integration tests, run coverage
    Qa-->>CM: Ok → T3 status=done

    CM-->>User: All tasks done, board updated
```

## 5. Agent Prompt Structure

Each dispatched task receives a rich prompt built by `cowork::prompt::build_cowork_task_prompt()`:

```
## Task: {title}
**Description:** {description}
**Priority:** {priority}
**Status:** todo

## Context
### Workspace: {workspace_name}
{workspace_description}

### Board
{board_entries formatted by section}

### Your Role: {member.role}
**Persona:** {member.persona}
**Responsibilities:** {member.responsibilities}
**Acceptance Criteria:** {member.acceptance_criteria}
**Output Format:** {member.output_format}
**SLA:** {member.sla}
**Limits:** {member.limits}

### Dependency Results
{dependent_results formatted with agent output}

## Instructions
Complete the task described above. Follow your persona, responsibilities,
and acceptance criteria. Use the board for shared context.
```

## 6. Key Data Flow

```mermaid
flowchart TD
    A[User Message] --> B{Has Members?}
    B -->|No| C[send_message → DB insert only]
    B -->|Yes| D[process_user_message]
    D --> E[Save Message to DB]
    E --> F[Find Lead Agent]
    F --> G[Create Lead Task]
    G --> H[Check Member Triggers]
    H --> I[Create Trigger Tasks]
    I --> J{agent_api?}
    J -->|Some| K[send_to_cowork_agent]
    J -->|None| L[fire_changed → WS push]
    K --> M[Build Cowork Prompt]
    M --> N[Mark in_progress]
    N --> O[process_and_wait]
    O --> P{Result?}
    P -->|Ok| Q[Mark done]
    P -->|Err| R[Mark todo]
    Q --> S[fire_changed → WS push]
    R --> S
    S --> T[UI updates Tasks tab]
```

## 7. GroupBinding Construction

`sent_to_cowork_agent` creates a synthetic `GroupBinding` per agent:

| Field | Value |
|-------|-------|
| `jid` | `cowork:{workspace_id}:{member_id}` |
| `folder` | `member.member_id` |
| `channel` | `"web"` |
| `group_type` | `"cowork"` |
| `allowed_work_dirs` | `member.subdir` |

This JID is used by AgentPool to look up/create the ZenEngine session, route replies through broadcast channels, and track per-agent state.
