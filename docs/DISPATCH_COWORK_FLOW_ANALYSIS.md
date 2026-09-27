# Phân tích chiều chạy Dispatch & Cowork - Quá trình lập kế hoạch và thực thi task

> Tài liệu phân tích chi tiết luồng chạy Dispatch Bridge và Cowork Space trong SenClaw, tập trung vào quy trình tạo checklist, chạy task, điều phối, đánh dấu và kiểm tra hoàn thành.

---

## Tổng quan

Hệ thống SenClaw có hai cơ chế điều phối task chính:

1. **DAG Dispatch** - Điều phối tự động từ admin agent, sử dụng dependency graph
2. **Cowork Space** - Không gian cộng tác đa agent với task board, shared memory, và inter-agent messaging

Hai cơ chế này có thể hoạt động độc lập hoặc tích hợp với nhau thông qua DispatchBridge.

---

## 1. Kiến trúc tổng thể

```
┌─────────────────────────────────────────────────────────────────┐
│                         User / UI Layer                          │
│  (WebSocket Gateway / UI Server)                                │
└────────────────────┬────────────────────────────────────────────┘
                     │
                     ▼
┌─────────────────────────────────────────────────────────────────┐
│                      CoworkManager                                │
│  - Workspace lifecycle                                          │
│  - Task board management                                         │
│  - Member spec management                                        │
│  - Trigger & handoff rule processing                             │
└──────┬──────────────────────────────────────────────┬───────────┘
       │                                              │
       ▼                                              ▼
┌──────────────────────┐                  ┌──────────────────────┐
│  DispatchBridge      │                  │  AgentPool           │
│  - DAG scheduling    │                  │  - Agent instances   │
│  - Task lifecycle   │                  │  - process_and_wait  │
│  - Timeout handling │                  │  - Virtual workers   │
└──────────┬───────────┘                  └──────────┬───────────┘
           │                                          │
           └──────────────┬───────────────────────────┘
                          ▼
                 ┌──────────────────────┐
                 │  ZenCore / SemaCore  │
                 │  - LLM interaction   │
                 │  - Tool execution    │
                 └──────────────────────┘
```

---

## 2. Chiều chạy khi User gửi message vào Cowork Workspace

### 2.1 Entry Points

Có 2 cách user tương tác với Cowork workspace:

1. **WebSocket** - Qua `cowork_handlers.rs::handle_cowork_message_send`
2. **HTTP API** - Qua `cowork.rs::cowork_messages_send`

Cả hai đều gọi đến `CoworkManager::process_user_message`.

### 2.2 Luồng xử lý message

**File:** `src/cowork/mod.rs` - `process_user_message` (line 1348-1461)

```rust
pub fn process_user_message(
    &self,
    db: &Db,
    workspace_id: &str,
    from_user: &str,
    content: &str,
    incoming_message_type: Option<&str>,
    now: &str,
    agent_api: Option<(Arc<dyn AgentApi>, Arc<Db>)>,
    self_arc: Arc<CoworkManager>,
) -> Result<(CoworkMessage, Vec<CoworkTask>)>
```

#### Bước 1: Lưu message (line 1362-1394)

```rust
let msg = CoworkMessage {
    id: msg_id,
    workspace_id: workspace_id.to_string(),
    from_member: from_user.to_string(),
    to_member: None,
    message_type: resolved_type.clone(),  // "status" hoặc "handoff"
    content: content.to_string(),
    attachments: None,
    task_id: None,
    is_read: false,
    created_at: now.to_string(),
};
db.insert_cowork_message(&msg)?;
```

#### Bước 2: Lấy danh sách members (line 1396-1403)

```rust
let members = db.list_cowork_members(workspace_id)?;

if members.is_empty() {
    self.fire_changed();
    return Ok((msg, created_tasks));
}
```

#### Bước 3: Gán task cho lead agent (line 1405-1432)

```rust
let lead = members
    .iter()
    .find(|m| m.role == "lead")
    .or_else(|| members.first());

if let Some(agent) = lead {
    let task = self.create_task(
        db,
        workspace_id,
        &task_title,
        Some(content),          // full message as description
        Some(&agent.member_id), // assignee
        None,
        Some("high"),
        None,
        from_user,
        None,
        now,
    )?;
    created_tasks.push(task);
}
```

**Quan trọng:** Task được tạo với status `"todo"` (xem `create_task` line 1809-1855).

#### Bước 4: Xử lý triggers để tạo thêm tasks (line 1434-1447)

```rust
let mut triggered = Vec::new();
self.collect_triggered_tasks(
    db,
    workspace_id,
    &members,
    from_user,
    &resolved_type,
    content,
    &created_tasks,
    now,
    &mut triggered,
)?;
created_tasks.extend(triggered);
```

**File:** `src/cowork/mod.rs` - `collect_triggered_tasks` (line 833-907)

Hàm này kiểm tra triggers của từng member:

```rust
for member in members {
    if let Some(ref triggers_json) = member.triggers {
        if let Ok(triggers) = serde_json::from_str::<Vec<serde_json::Value>>(triggers_json) {
            for trigger in &triggers {
                let trigger_type = trigger["type"].as_str().unwrap_or("");
                let should_fire = match trigger_type {
                    "message_received" => {
                        let from_filter = trigger["from"].as_str();
                        let from_ok = from_filter.map_or(true, |f| f == from_user);
                        let type_filter = trigger["messageType"].as_str();
                        let type_ok = match type_filter {
                            None => true,
                            Some(mt) => resolved_message_type == mt,
                        };
                        from_ok && type_ok
                    }
                    "on_mention" => {
                        let from_filter = trigger["from"].as_str();
                        from_filter.map_or(false, |f| f == from_user)
                    }
                    "task_assigned" => pool
                        .iter()
                        .any(|t| t.assignee.as_deref() == Some(member.member_id.as_str())),
                    _ => false,
                };
                
                if should_fire && !duplicate_assignee {
                    let task = self.create_task(...)?;
                    out.push(task);
                }
            }
        }
    }
}
```

#### Bước 5: Dispatch tasks (line 1449-1457)

```rust
self.dispatch_cowork_tasks_batch(
    db,
    workspace_id,
    &members,
    &created_tasks,
    content,
    agent_api,
    self_arc,
)?;
```

---

## 3. Chiều chạy Dispatch Tasks

**File:** `src/cowork/mod.rs` - `dispatch_cowork_tasks_batch` (line 1167-1346)

### 3.1 Kiểm tra DAG Bridge

```rust
let dag_bridge = self.dispatch_bridge.lock().unwrap().clone();
if let Some(ref bridge) = dag_bridge {
    // Sử dụng DAG Dispatch
} else if let Some((ref api, ref db_arc)) = agent_api {
    // Dispatch trực tiếp
} else {
    // Không dispatch
}
```

### 3.2 DAG Dispatch Path (line 1182-1318)

Nếu có DispatchBridge, tasks được route qua DAG:

#### Bước 1: Tạo DispatchTask từ CoworkTask

```rust
let dispatch_tasks: Vec<DispatchTask> = created_tasks
    .iter()
    .map(|task| {
        let assignee_id = task.assignee.as_deref().unwrap_or("");
        let member = members.iter().find(|m| m.member_id == assignee_id);
        let agent_id = assignee_id.to_string();
        let agent_jid = member
            .and_then(|m| m.jid.clone())
            .unwrap_or_else(|| format!("cowork:{}:{}", workspace_id, agent_id));

        // Xử lý dependencies
        let depends_on: Vec<String> = task
            .depends_on
            .as_ref()
            .and_then(|d| serde_json::from_str::<Vec<String>>(d).ok())
            .unwrap_or_default()
            .iter()
            .filter_map(|dep_id| {
                created_tasks
                    .iter()
                    .find(|t| t.id == *dep_id)
                    .map(|t| t.title.clone())
            })
            .collect();

        // Build prompt với context đầy đủ
        let prompt = self::prompt::build_cowork_task_prompt(
            task,
            member.unwrap_or(&default_member),
            &workspace,
            &board,
            &deps,
        );

        // Resolve timeout từ SLA
        let timeout_seconds: u64 = member
            .and_then(|m| m.sla.as_ref())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|v| v.get("maxDurationPerTaskMinutes").and_then(|t| t.as_i64()))
            .map(|mins| (mins * 60) as u64)
            .unwrap_or(1800);  // default 30 phút

        DispatchTask {
            id: String::new(),  // sẽ được generate bởi bridge
            label: task.title.clone(),
            agent_id,
            agent_jid,
            depends_on,
            prompt,
            status: DispatchTaskStatus::Registered,
            result: None,
            created_at: String::new(),
            started_at: None,
            timeout_seconds,
            timeout_at: None,
            completed_at: None,
            is_virtual: false,
            persona_name: None,
        }
    })
    .collect();
```

#### Bước 2: Enqueue vào DispatchBridge

```rust
let admin_folder = members
    .iter()
    .find(|m| m.role == "lead")
    .or_else(|| members.first())
    .map(|m| m.member_id.clone())
    .unwrap_or_else(|| "cowork".into());

match bridge.enqueue_parent(
    goal.to_string(),
    admin_folder,
    Some(workspace.root_dir.clone()),
    dispatch_tasks,
) {
    Ok((parent_id, task_ids)) => {
        // Map DispatchTask ID → CoworkTask ID cho sync status
        let mut map = self.dispatch_task_map.lock().unwrap();
        for (i, dispatch_id) in task_ids.iter().enumerate() {
            if let Some(cowork_task) = created_tasks.get(i) {
                map.insert(
                    dispatch_id.clone(),
                    (cowork_task.id.clone(), workspace_id.to_string()),
                );
            }
        }
    }
    Err(e) => {
        tracing::error!("[Cowork] Failed to enqueue DAG parent: {e}");
    }
}
```

### 3.3 Direct Dispatch Path (line 1319-1336)

Nếu không có DAG Bridge nhưng có agent_api:

```rust
for task in created_tasks {
    let assignee_id = task.assignee.as_deref().unwrap_or("");
    if let Some(member) = members.iter().find(|m| m.member_id == assignee_id) {
        self.send_to_cowork_agent(
            Arc::clone(db_arc),
            Arc::clone(api),
            workspace_id,
            task,
            member,
            Arc::clone(&self_arc),
        );
    }
}
```

---

## 4. Chiều chạy Execute Task (Direct Dispatch)

**File:** `src/cowork/mod.rs` - `send_to_cowork_agent` (line 558-831)

### 4.1 Build Prompt với Context

```rust
let prompt = self::prompt::build_cowork_task_prompt(
    task,
    member,
    &workspace,
    &board,
    &dependent_results,
);
```

**File:** `src/cowork/prompt.rs` - `build_cowork_task_prompt` (line 64-236)

Prompt được build với các phần sau:

1. **Workspace identity** - name, working_dir, root_dir
2. **Shared files context** - danh sách file trong shared/
3. **Task** - title và description
4. **Board context** - brief, guidelines, decisions
5. **Persona** - workspace-scoped persona
6. **Responsibilities** - danh sách trách nhiệm
7. **Acceptance criteria** - tiêu chí hoàn thành
8. **Output format** - format yêu cầu
9. **SLA / Limits** - timeout, token limits, allowed commands
10. **Dependency results** - kết quả từ tasks phụ thuộc
11. **Instructions** - hướng dẫn thực thi

### 4.2 Spawn Background Task

```rust
tokio::spawn(async move {
    // Mark in_progress
    let now = chrono::Utc::now().to_rfc3339();
    let _ = db_clone.update_cowork_task(
        &task_id,
        None,
        None,
        Some("in_progress"),
        None,
        None,
        None,
        None,
        None,
        &now,
    );

    // Insert status message
    let _ = db_clone.insert_cowork_message(&CoworkMessage {
        id: dispatch_msg_id,
        workspace_id: workspace_id_owned.clone(),
        from_member: "system".into(),
        to_member: Some(member_id.clone()),
        message_type: "status".into(),
        content: format!("Dispatched task to {member_id}"),
        attachments: None,
        task_id: Some(task_id.clone()),
        is_read: false,
        created_at: now.clone(),
    });

    // Call process_and_wait
    let result = agent_api.process_and_wait(&jid, &group, &prompt).await;

    // Capture last reply text
    let reply_text = agent_api.get_last_reply_text(&jid);

    // Update status based on result
    let new_status = if result.is_ok() { "done" } else { "blocked" };
    
    if new_status == "done" {
        let _ = db_clone.update_cowork_task_result(
            &task_id,
            Some(prompt.as_str()),
            reply_text.as_deref(),
            None,
            None,
            &now2,
        );
        
        // Validate output format
        let output_validation = validate_output_format(result, member.output_format);
        
        // Fire task result event
        manager.fire_task_result(TaskResultEvent {
            task_id: task_id.clone(),
            workspace_id: workspace_id_owned.clone(),
            title: task_title.clone(),
            input_summary: Some(prompt.clone()),
            result_output: reply_text.clone(),
            references: None,
            artifacts: None,
            completed_at: Some(now2.clone()),
            output_validation,
        });
    }

    // Insert completion message
    let _ = db_clone.insert_cowork_message(&CoworkMessage {
        id: done_msg_id,
        workspace_id: workspace_id_owned.clone(),
        from_member: member_id.clone(),
        to_member: Some("user".into()),
        message_type: if new_status == "done" {
            "result".into()
        } else {
            "alert".into()
        },
        content: content.clone(),
        attachments: None,
        task_id: Some(task_id.clone()),
        is_read: false,
        created_at: now2.clone(),
    });

    // Process task status triggers
    manager.process_task_status_triggers(
        &db_clone,
        &workspace_id_owned,
        &task_id,
        new_status,
        reply_text.as_deref(),
        &now2,
        Some(Arc::clone(&agent_api_followup)),
        Arc::clone(&manager),
    );

    // Process handoff rules if done
    if new_status == "done" {
        // Collect triggered tasks from message_received triggers
        // Dispatch followup tasks
    }

    manager.fire_changed();
});
```

---

## 5. Chiều chạy DAG Dispatch (qua DispatchBridge)

**File:** `src/agent/dispatch_bridge/bridge.rs`

### 5.1 Enqueue Parent

**File:** `src/agent/dispatch_bridge/bridge.rs` - `enqueue_parent` (line 189-235)

```rust
pub fn enqueue_parent(
    &self,
    goal: String,
    admin_folder: String,
    shared_workspace: Option<String>,
    tasks: Vec<DispatchTask>,
) -> std::io::Result<(String, Vec<String>)> {
    self.modify_state(|state| {
        state.seq += 1;
        parent_id = format!("p-{}", state.seq);
        
        for mut t in tasks {
            state.seq += 1;
            let tid = format!("d-{}", state.seq);
            t.id = tid.clone();
            t.created_at = now.clone();
            task_ids.push(tid);
            resolved_tasks.push(t);
        }
        
        let parent = DispatchParent {
            id: parent_id.clone(),
            goal,
            admin_folder,
            shared_workspace,
            status: "queued".into(),
            created_at: now,
            completed_at: None,
            tasks: resolved_tasks,
        };
        state.parents.push(parent);
    })?;
    
    // Activate next queued parent
    self.activate_next_queued(&admin_folder);
    
    Ok((parent_id, task_ids))
}
```

### 5.2 Scheduler Poll Loop

**File:** `src/agent/dispatch_bridge/bridge.rs` - `process_pending` (line 716-787)

Scheduler chạy mỗi 300ms để:

1. **Timeout sweep** - Kiểm tra tasks quá hạn
2. **Launch ready tasks** - Chạy tasks đã sẵn sàng (dependencies satisfied)

```rust
pub(super) fn process_pending(&self) {
    let now = chrono::Utc::now();
    
    // 1. Timeout sweep
    for parent in &state.parents {
        if parent.status != "active" {
            continue;
        }
        for task in &parent.tasks {
            if task.status != DispatchTaskStatus::Processing || task.is_virtual {
                continue;
            }
            let Some(deadline_str) = &task.timeout_at else {
                continue;
            };
            let Ok(deadline) = chrono::DateTime::parse_from_rfc3339(deadline_str) else {
                continue;
            };
            if deadline.with_timezone(&chrono::Utc) < now {
                timed_out.push((task.id.clone(), task.agent_jid.clone()));
            }
        }
    }
    for (task_id, jid) in &timed_out {
        self.mark_task_timeout(task_id, jid);
    }
    
    // 2. Launch ready tasks
    let paused = self.inner.lock().unwrap().paused_admins.clone();
    for parent in &state.parents {
        if parent.status != "active" || paused.contains(&parent.admin_folder) {
            continue;
        }
        for task in &parent.tasks {
            if task.status == DispatchTaskStatus::Registered
                && self.can_start_task(task, &parent.tasks)
            {
                self.start_task(parent, task);
            }
        }
    }
}
```

### 5.3 Start Task

**File:** `src/agent/dispatch_bridge/bridge.rs` - `start_task` (line 791-890)

```rust
fn start_task(&self, parent: &DispatchParent, task: &DispatchTask) {
    // Build augmented prompt with dependencies
    let augmented_prompt = build_augmented_prompt(parent, task);
    
    // Mark as processing
    self.modify_state(|state| {
        if let Some(p) = state.parents.iter_mut().find(|p| p.id == parent.id) {
            if let Some(t) = p.tasks.iter_mut().find(|t| t.id == task.id) {
                t.status = DispatchTaskStatus::Processing;
                t.started_at = Some(now.clone());
                let timeout = Duration::from_secs(task.timeout_seconds);
                t.timeout_at = Some((chrono::Utc::now() + timeout).to_rfc3339());
            }
        }
    });
    
    // Track active task
    self.add_active_task(&task.id, &task.agent_jid);
    
    // Fire lifecycle callback
    self.fire_task_lifecycle(&task.id, "processing", &task.label, &parent.goal, None);
    
    // Call send_to_agent callback để deliver prompt
    if let Some(cb) = self.send_to_agent.lock().unwrap().as_ref() {
        cb(
            &task.agent_jid,
            &task.id,
            &augmented_prompt,
            parent.shared_workspace.as_deref().unwrap_or(""),
        );
    }
}
```

### 5.4 Task Completion

**File:** `src/agent/dispatch_bridge/bridge.rs` - `mark_task_done` (line 500-549)

Khi agent hoàn thành task:

```rust
pub(super) fn mark_task_done(&self, task_id: &str, text: &str) {
    let jid = self.remove_active_task(task_id);
    let now = chrono::Utc::now().to_rfc3339();
    
    self.modify_state(|state| {
        for parent in &mut state.parents {
            if let Some(task) = parent.tasks.iter_mut().find(|t| t.id == task_id) {
                task.status = DispatchTaskStatus::Done;
                task.result = Some(text.to_string());
                task.completed_at = Some(now.clone());
                
                // Nếu tất cả tasks done → mark parent done
                if parent.tasks.iter().all(|t| t.status.is_terminal()) {
                    parent.status = "done".into();
                    parent.completed_at = Some(now.clone());
                }
                return;
            }
        }
    });
    
    // Fire lifecycle callback → sync CoworkTask status
    self.fire_task_lifecycle(task_id, "done", &task_label, &parent_goal, Some(text.to_string()));
    
    // Fire admin activity để reset inactivity timer
    if let Some(folder) = task_admin {
        self.fire_admin_activity(&folder);
    }
    
    // Activate next queued parent
    if let Some(folder) = completed_admin {
        self.activate_next_queued(&folder);
    }
    
    // Process next pending tasks
    self.process_next_pending();
    
    // Revert workspace nếu không còn task nào cho agent này
    if let Some(j) = jid.as_ref() {
        if !self.has_active_jid_tasks(j) {
            self.fire_revert_workspace(j);
        }
    }
}
```

---

## 6. Sync DispatchTask ↔ CoworkTask Status

**File:** `src/cowork/mod.rs` - `on_dispatch_task_lifecycle` (line 280-415)

Callback này được fire từ DispatchBridge khi task status thay đổi:

```rust
pub fn on_dispatch_task_lifecycle(
    &self,
    db: &Arc<Db>,
    dispatch_task_id: &str,
    new_status: &str,
    task_label: &str,
    _parent_goal: &str,
    dispatch_result: Option<&str>,
    agent_api: Option<Arc<dyn AgentApi>>,
    self_arc: Arc<CoworkManager>,
) {
    // Lookup CoworkTask ID từ map
    let (cowork_task_id, workspace_id) = {
        let map = self.dispatch_task_map.lock().unwrap();
        let Some((tid, wid)) = map.get(dispatch_task_id) else {
            return;
        };
        (tid.clone(), wid.clone())
    };
    
    // Map status
    let cowork_status = match new_status {
        "processing" => "in_progress",
        "done" => "done",
        "error" | "timeout" => "blocked",
        _ => return,
    };
    
    // Update CoworkTask
    if cowork_status == "done" {
        db.update_cowork_task_result(
            &cowork_task_id,
            Some(task_label),
            result_opt,
            None,
            None,
            &now,
        )?;
    } else {
        db.update_cowork_task(
            &cowork_task_id,
            None,
            None,
            Some(cowork_status),
            None,
            None,
            None,
            None,
            None,
            &now,
        )?;
    }
    
    if cowork_status == "done" {
        // Validate output format
        let output_validation = validate_output_format(result, member.output_format);
        
        // Fire task result event
        self.fire_task_result(TaskResultEvent {
            task_id: cowork_task_id.clone(),
            workspace_id: workspace_id.clone(),
            title: task_label.to_string(),
            input_summary: Some(task_label.to_string()),
            result_output: result_opt.map(|s| s.to_string()),
            references: None,
            artifacts: None,
            completed_at: Some(now.clone()),
            output_validation,
        });
        
        // Process handoff rules
        if let Some(api) = agent_api {
            self.process_handoff_rules(
                db,
                &workspace_id,
                &cowork_task_id,
                result_opt.unwrap_or(task_label),
                &now,
                api,
                self_arc,
            );
        }
    }
    
    // Process task status triggers
    self.process_task_status_triggers(
        db,
        &workspace_id,
        &cowork_task_id,
        cowork_status,
        result_str,
        &now,
        agent_api,
        self_arc,
    );
}
```

---

## 7. Handoff Rules Processing

**File:** `src/cowork/mod.rs` - `process_handoff_rules` (line 423-552)

Khi task hoàn thành, kiểm tra handoff_rules của assignee:

```rust
fn process_handoff_rules(
    &self,
    db: &Arc<Db>,
    workspace_id: &str,
    completed_task_id: &str,
    result_content: &str,
    now: &str,
    agent_api: Arc<dyn AgentApi>,
    self_arc: Arc<CoworkManager>,
) {
    let completed_task = db.get_cowork_task(completed_task_id)?;
    let assignee_id = completed_task.assignee.as_deref()?;
    let members = db.list_cowork_members(workspace_id)?;
    let assignee = members.iter().find(|m| m.member_id == assignee_id)?;
    let handoff_rules_json = assignee.handoff_rules.as_deref()?;
    let rules: Vec<serde_json::Value> = serde_json::from_str(handoff_rules_json)?;
    
    let mut followup_tasks = Vec::new();
    for rule in &rules {
        let when = rule["when"].as_str().unwrap_or("");
        if when != "task_complete" {
            continue;
        }
        let to = rule["to"].as_str()?;
        
        // Optional gates
        if let Some(u) = rule["unless_result_contains"].as_str() {
            if !u.is_empty() && cowork_handoff_result_has(result_content, u) {
                continue;  // skip rule
            }
        }
        if let Some(o) = rule["only_if_result_contains"].as_str() {
            if !o.is_empty() && !cowork_handoff_result_has(result_content, o) {
                continue;  // skip rule
            }
        }
        
        let handoff_type = rule["type"].as_str().unwrap_or("handoff");
        
        // Create followup task
        let task = self.create_task(
            db,
            workspace_id,
            &task_title,
            Some(&description),
            Some(to),
            None,
            Some("high"),
            None,
            &assignee_id,
            None,
            now,
        )?;
        
        // Post handoff message
        db.insert_cowork_message(&CoworkMessage {
            from_member: assignee_id.clone(),
            to_member: Some(to.to_string()),
            message_type: handoff_type.to_string(),
            content: format!("{assignee_id} → {to}: {}", completed_task.title),
            task_id: Some(task.id.clone()),
            ...
        })?;
        
        followup_tasks.push(task);
    }
    
    // Dispatch followup tasks
    if !followup_tasks.is_empty() {
        self.dispatch_cowork_tasks_batch(
            db,
            workspace_id,
            &members,
            &followup_tasks,
            &completed_task.title,
            Some((agent_api, Arc::clone(db))),
            self_arc,
        );
    }
}
```

---

## 8. Task Status Triggers Processing

**File:** `src/cowork/mod.rs` - `process_task_status_triggers` (line 1082-1165)

Khi task status thay đổi, kiểm tra `task_status_changed` triggers của tất cả members:

```rust
fn process_task_status_triggers(
    &self,
    db: &Arc<Db>,
    workspace_id: &str,
    task_id: &str,
    new_status: &str,
    task_result: Option<&str>,
    now: &str,
    agent_api: Option<Arc<dyn AgentApi>>,
    self_arc: Arc<CoworkManager>,
) {
    let task = db.get_cowork_task(task_id)?;
    let members = db.list_cowork_members(workspace_id)?;
    
    let mut followup_tasks = Vec::new();
    self.collect_task_status_triggers(
        db,
        workspace_id,
        &members,
        &task,
        new_status,
        task_result,
        &[],
        now,
        &mut followup_tasks,
    )?;
    
    if !followup_tasks.is_empty() {
        self.dispatch_cowork_tasks_batch(
            db,
            workspace_id,
            &members,
            &followup_tasks,
            &format!("Task status: {}", new_status),
            agent_api.map(|api| (api, Arc::clone(db))),
            self_arc,
        );
    }
}
```

**File:** `src/cowork/mod.rs` - `collect_task_status_triggers` (line 910-1077)

```rust
fn collect_task_status_triggers(
    &self,
    db: &Db,
    workspace_id: &str,
    members: &[CoworkMember],
    task: &CoworkTask,
    new_status: &str,
    task_result: Option<&str>,
    already_created: &[CoworkTask],
    now: &str,
    out: &mut Vec<CoworkTask>,
) -> Result<()> {
    for member in members {
        if let Some(ref triggers_json) = member.triggers {
            if let Ok(triggers) = serde_json::from_str::<Vec<serde_json::Value>>(triggers_json) {
                for trigger in &triggers {
                    let trigger_type = trigger["type"].as_str().unwrap_or("");
                    if trigger_type != "task_status_changed" {
                        continue;
                    }
                    
                    // Check status filter
                    let status_filter = trigger["status"].as_str();
                    let status_ok = status_filter.map_or(false, |s| s == new_status);
                    if !status_ok {
                        continue;
                    }
                    
                    // Check assignee filter (optional)
                    let assignee_filter = trigger["assignee"].as_str();
                    let assignee_ok = assignee_filter.map_or(true, |a| {
                        task.assignee.as_deref().map_or(false, |ta| ta == a)
                    });
                    if !assignee_ok {
                        continue;
                    }
                    
                    let to = trigger["to"].as_str()?;
                    
                    // Check result gates (optional)
                    if let Some(result) = task_result {
                        if let Some(u) = trigger["unless_result_contains"].as_str() {
                            if !u.is_empty() && cowork_handoff_result_has(result, u) {
                                continue;
                            }
                        }
                        if let Some(o) = trigger["only_if_result_contains"].as_str() {
                            if !o.is_empty() && !cowork_handoff_result_has(result, o) {
                                continue;
                            }
                        }
                    }
                    
                    // Check duplicate assignee
                    let duplicate_assignee = pool.iter().any(|t| t.assignee.as_deref() == Some(to));
                    if duplicate_assignee {
                        continue;
                    }
                    
                    // Create triggered task
                    let task = self.create_task(
                        db,
                        workspace_id,
                        &task_title,
                        Some(&description),
                        Some(to),
                        None,
                        Some("medium"),
                        None,
                        task.assignee.as_deref().unwrap_or("system"),
                        None,
                        now,
                    )?;
                    
                    out.push(task);
                }
            }
        }
    }
    Ok(())
}
```

---

## 9. Output Format Validation

**File:** `src/cowork/mod.rs` - `validate_output_format` (line 57-119)

Khi task hoàn thành, validate output theo member's output format requirements:

```rust
fn validate_output_format(
    output: &str,
    output_format_json: Option<&str>,
) -> Option<OutputValidation> {
    let output_format = output_format_json?;
    let fmt: serde_json::Value = serde_json::from_str(output_format).ok()?;
    
    let expected_format = fmt.get("format").and_then(|v| v.as_str()).map(|s| s.to_string());
    let required_sections: Vec<String> = fmt
        .get("requiredSections")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    
    // Validate format
    let format_valid = if let Some(ref expected) = expected_format {
        match expected.as_str() {
            "json" => serde_json::from_str::<serde_json::Value>(output).is_ok(),
            "markdown" | "plain" => true,
            _ => true,
        }
    } else {
        true
    };
    
    // Check required sections
    let output_lower = output.to_lowercase();
    let required_sections_present: Vec<String> = required_sections
        .iter()
        .filter(|section| {
            let section_lower = section.to_lowercase();
            output_lower.contains(&section_lower)
                || output_lower.contains(&format!("## {}", section_lower))
                || output_lower.contains(&format!("### {}", section_lower))
        })
        .cloned()
        .collect();
    
    let required_sections_missing: Vec<String> = required_sections
        .iter()
        .filter(|section| !required_sections_present.contains(section))
        .cloned()
        .collect();
    
    let overall_compliant = format_valid && required_sections_missing.is_empty();
    
    Some(OutputValidation {
        format_valid,
        expected_format,
        required_sections_present,
        required_sections_missing,
        overall_compliant,
    })
}
```

---

## 10. Checklist Task Creation & Management

### 10.1 Task Creation

**File:** `src/cowork/mod.rs` - `create_task` (line 1809-1855)

```rust
pub fn create_task(
    &self,
    db: &Db,
    workspace_id: &str,
    title: &str,
    description: Option<&str>,
    assignee: Option<&str>,
    reviewer: Option<&str>,
    priority: Option<&str>,
    depends_on: Option<&str>,
    created_by: &str,
    attachments: Option<&str>,
    now: &str,
) -> Result<CoworkTask> {
    let id = format!(
        "cwt-{}",
        Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or("0000")
    );
    let task = CoworkTask {
        id,
        workspace_id: workspace_id.to_string(),
        title: title.to_string(),
        description: description.map(|s| s.to_string()),
        status: "todo".to_string(),  // ← Initial status
        assignee: assignee.map(|s| s.to_string()),
        reviewer: reviewer.map(|s| s.to_string()),
        priority: priority.unwrap_or("medium").to_string(),
        depends_on: depends_on.map(|s| s.to_string()),
        attachments: attachments.map(|s| s.to_string()),
        created_by: created_by.to_string(),
        created_at: now.to_string(),
        updated_at: now.to_string(),
        due_at: None,
        completed_at: None,
        input_summary: None,
        result_output: None,
        references: None,
        artifacts: None,
    };
    db.insert_cowork_task(&task)?;
    self.fire_changed();
    Ok(task)
}
```

### 10.2 Task Status Machine

```
┌──────────┐
│  todo    │ ← Initial status
└────┬─────┘
     │
     ▼
┌──────────────┐
│ in_progress  │ ← Khi task được dispatch
└──────┬───────┘
       │
       ├──────▶ done ← Khi agent hoàn thành thành công
       │
       └──────▶ blocked ← Khi agent gặp lỗi
```

### 10.3 Task Update

**File:** `src/cowork/mod.rs` - `update_task` (line 1861+)

```rust
pub fn update_task(
    &self,
    db: &Db,
    task_id: &str,
    title: Option<&str>,
    description: Option<&str>,
    status: Option<&str>,
    assignee: Option<&str>,
    reviewer: Option<&str>,
    priority: Option<&str>,
    depends_on: Option<&str>,
    attachments: Option<&str>,
    now: &str,
) -> Result<()> {
    db.update_cowork_task(
        task_id,
        title,
        description,
        status,
        assignee,
        reviewer,
        priority,
        depends_on,
        attachments,
        now,
    )?;
    
    // Nếu status thay đổi và có agent_api → process triggers
    if let Some(new_status) = status {
        if let Some(api) = agent_api {
            self.update_task_with_triggers(
                db,
                task_id,
                title,
                description,
                status,
                assignee,
                reviewer,
                priority,
                depends_on,
                attachments,
                now,
                Some(api),
                self_arc,
            )?;
        }
    }
    
    self.fire_changed();
    Ok(())
}
```

---

## 11. Tổng kết luồng chạy hoàn chỉnh

### 11.1 User gửi message vào Cowork Workspace

```
User Message
    ↓
CoworkManager::process_user_message
    ↓
1. Save message to DB
    ↓
2. Get workspace members
    ↓
3. Create task for lead agent (status: todo)
    ↓
4. Collect triggered tasks from member triggers
    ↓
5. dispatch_cowork_tasks_batch
    ↓
    ├─→ [DAG Bridge] → DispatchBridge::enqueue_parent
    │                      ↓
    │                  Scheduler poll (300ms)
    │                      ↓
    │                  start_task (when dependencies satisfied)
    │                      ↓
    │                  send_to_agent callback
    │                      ↓
    │                  AgentPool::process_and_wait
    │                      ↓
    │                  mark_task_done
    │                      ↓
    │                  on_dispatch_task_lifecycle (sync CoworkTask)
    │                      ↓
    │                  process_handoff_rules
    │                      ↓
    │                  process_task_status_triggers
    │
    └─→ [Direct] → send_to_cowork_agent
                      ↓
                  Build prompt with context
                      ↓
                  Spawn background task
                      ↓
                  Mark in_progress
                      ↓
                  AgentPool::process_and_wait
                      ↓
                  Capture result
                      ↓
                  Update status (done/blocked)
                      ↓
                  Validate output format
                      ↓
                  Fire task result event
                      ↓
                  process_handoff_rules
                      ↓
                  process_task_status_triggers
```

### 11.2 Checklist marking flow

```
Task Created (status: todo)
    ↓
Dispatch → status: in_progress
    ↓
Agent executes
    ↓
Success → status: done
    ↓
Validate output format
    ↓
Fire task result event (with validation)
    ↓
UI displays task result + validation status
```

### 11.3 Completion verification

Completion được kiểm tra qua nhiều lớp:

1. **Acceptance Criteria** - Định nghĩa trong member spec, inject vào prompt
2. **Output Format Validation** - Kiểm tra format và required sections
3. **Task Status** - `done` chỉ khi agent process_and_wait thành công
4. **OutputValidation Event** - Fire kèm result để UI hiển thị compliance status

---

## 12. Key Files Reference

| File | Mô tả |
|------|-------|
| `src/cowork/mod.rs` | CoworkManager core logic, task lifecycle, triggers, handoff |
| `src/cowork/prompt.rs` | Prompt building với workspace context |
| `src/agent/dispatch_bridge/bridge.rs` | DAG dispatch scheduler, task lifecycle |
| `src/gateway/websocket_gateway/cowork_handlers.rs` | WebSocket handlers cho cowork |
| `src/gateway/ui_server/cowork.rs` | HTTP API handlers cho cowork |
| `src/agent/virtual_worker_pool.rs` | Virtual agent pool cho dispatch |
| `docs/COWORK_DESIGN.md` | Thiết kế chi tiết Cowork Space |

---

## 13. Triggers & Handoff Rules

### 13.1 Trigger Types

| Type | Mô tả | Parameters |
|------|-------|------------|
| `message_received` | Khi nhận message từ member cụ thể | `from`, `messageType` |
| `on_mention` | Khi được mention bởi member cụ thể | `from` |
| `task_assigned` | Khi task được gán cho member này | - |
| `task_status_changed` | Khi task có status cụ thể | `status`, `assignee`, `to`, `only_if_result_contains`, `unless_result_contains` |
| `board_updated` | Khi board section được cập nhật | `section` |
| `schedule` | Theo cron schedule | `cron` |

### 13.2 Handoff Rules

| Field | Mô tả |
|-------|-------|
| `when` | Khi nào kích hoạt (`task_complete`, `blocked`, `needs_clarification`, `error`) |
| `to` | Member nhận task tiếp theo |
| `type` | Message type (`handoff`, `review_request`, `alert`, `clarification`) |
| `message_template` | Template nội dung message (Mustache) |
| `only_if_result_contains` | Chỉ chạy nếu result chứa text này |
| `unless_result_contains` | Không chạy nếu result chứa text này |

---

## 14. Checklist Items trong Plan

Khi admin agent tạo plan, nó có thể sử dụng Task tool để tạo checklist items. Tuy nhiên, trong Cowork context:

1. **Tasks được tạo qua CoworkManager** - Không qua Task tool
2. **Tasks được dispatch qua DispatchBridge** - DAG hoặc direct
3. **Tasks có dependencies** - Qua `depends_on` field
4. **Tasks có assignee** - Member cụ thể trong workspace
5. **Tasks có triggers** - Tự động tạo followup tasks
6. **Tasks có handoff rules** - Tự động bàn giao khi hoàn thành

Checklist marking được thực hiện qua:
- Task status transitions (todo → in_progress → done/blocked)
- Output validation (format + required sections)
- TaskResultEvent fire với OutputValidation payload

---

## 15. Debug & Monitoring

### 15.1 Logging Points

Key log messages để debug:

- `[Cowork] Task {task_id} → {status}` - Task status changes
- `[Cowork] Handoff rule: created task '{title}' → {to}` - Handoff task creation
- `[Cowork] Task status trigger: created task '{title}' → {to}` - Trigger task creation
- `[DispatchBridge] Task {task_id} done` - Dispatch task completion
- `[DispatchBridge] Enqueued DAG parent {parent_id}` - DAG parent enqueue

### 15.2 WebSocket Events

Real-time events để UI update:

- `cowork:task:updated` - Task status change
- `cowork:message:sent` - New message in channel
- `cowork:workspace:updated` - Workspace change
- `dispatch:update.parents` - Dispatch parent state update

---

## 16. Best Practices

### 16.1 Member Spec Design

1. **Persona** - Định nghĩa rõ vai trò trong workspace
2. **Responsibilities** - Liệt kê trách nhiệm thường trực
3. **Triggers** - Thiết kế workflow tự động với `task_status_changed`
4. **Handoff Rules** - Định nghĩa bàn giao với `only_if_result_contains` gates
5. **Acceptance Criteria** - Rõ ràng, measurable
6. **Output Format** - Define required sections cho consistency

### 16.2 Task Design

1. **Dependencies** - Sử dụng `depends_on` để enforce order
2. **Assignee** - Luôn gán task cho member cụ thể
3. **Priority** - Sử dụng priority để guide scheduling
4. **Description** - Chi tiết, bao gồm context từ tasks phụ thuộc

### 16.3 Workflow Design

1. **Sử dụng triggers thay vì manual assignment** - Tự động hóa workflow
2. **Sử dụng handoff gates** - Điều kiện bàn giao thông minh
3. **Validate output** - Đảm bảo quality trước khi bàn giao
4. **Monitor task status** - Theo dõi progress qua WebSocket events

---

## 17. Ví dụ Workflow

### 17.1 Software Development Workflow

```
User: "Implement login feature"
    ↓
[Lead] Creates planning task (assignee: lead)
    ↓
[Lead] Done → trigger task_status_changed (status: done, to: code-agent)
    ↓
[Code Agent] Receives task "Implement login"
    ↓
[Code Agent] Done → handoff rule (to: review-agent)
    ↓
[Review Agent] Receives task "Review login implementation"
    ↓
[Review Agent] Done → trigger task_status_changed (status: done, to: test-agent)
    ↓
[Test Agent] Receives task "Write tests for login"
    ↓
[Test Agent] Done → handoff rule (to: user)
    ↓
User sees final result
```

### 17.2 Config Example

```yaml
members:
  - memberId: lead
    role: lead
    triggers:
      - type: message_received
        from: user
    
  - memberId: code-agent
    role: worker
    triggers:
      - type: task_status_changed
        status: done
        assignee: lead
        to: code-agent
    handoff:
      - when: task_complete
        to: review-agent
        type: review_request
        message_template: "Review: {{ task.title }}"
    acceptance_criteria:
      - Code compiles
      - Unit tests pass
    
  - memberId: review-agent
    role: reviewer
    triggers:
      - type: message_received
        from: code-agent
        message_type: review_request
    handoff:
      - when: task_complete
        to: test-agent
        type: handoff
        only_if_result_contains: "approved"
    
  - memberId: test-agent
    role: worker
    triggers:
      - type: task_status_changed
        status: done
        assignee: review-agent
        to: test-agent
    handoff:
      - when: task_complete
        to: user
        type: result
```

---

## 18. Kết thúc DAG, chờ không chặn lượt, và chạy lại việc lỗi

Ba tính chất bổ sung 05/09/2026. Cả ba đều vá lỗi *im lặng* — nhìn từ chat không
phân biệt được với "DAG đang chạy bình thường".

### 18.1. DAG không thể tiến nữa thì bị đánh lỗi, không để treo

Vòng quét timeout trong `process_pending` **chỉ nhìn task `processing`**. Một task
không bao giờ *khởi động* thì không có hạn nào cả — và `can_start_task` trả
`false` **vĩnh viễn** với task virtual có persona không tìm thấy trong registry.
Parent nằm `active`, không gì chạy, không gì chạy được, cho tới khi restart daemon.

`sweep_stalled_parents` đóng đúng ca đó. Điều kiện cố ý hẹp: **không có gì
`processing` VÀ không có gì khởi động được**. Không phải chỉ "không có gì
processing" — task bị chặn bởi suất đồng thời đang chờ một task khác *đang chạy*,
giết nó là phá mọi DAG rộng hơn giới hạn của chính nó. Dạng hẹp này đúng vì thứ
duy nhất mở khoá một task là một task khác kết thúc.

Có thời gian ân hạn (`STALL_GRACE_SECONDS`): lúc boot và lúc vừa tạo parent,
persona registry và virtual-worker pool chưa nối xong nên mọi task tạm thời
không khởi động được — thiếu ân hạn thì DAG mới bị giết ngay nhịp đầu.

### 18.2. DAG dài không nên chặn lượt agent

`dispatch_all_tasks` / `create_parent_and_run` poll **ngay trong lượt**, mặc định
900s. DAG dài hơn thì trả về lỗi trong khi daemon vẫn chạy tiếp — trước đây
manager thấy tool lỗi liền re-dispatch **cả DAG**, sinh việc trùng.

`dispatch_status` là ảnh chụp **không chặn** (đếm theo trạng thái + danh sách
task lỗi kèm output). Ghép với `schedule_watch` (xem PHẦN D của
[background-schedule-tasks-guide.md](background-schedule-tasks-guide.md)), DAG
dài không tốn lượt nào trong lúc chạy: mỗi nhịp chỉ là một lời gọi tool, không
LLM; xong mới đánh thức chat.

Lưu ý: parent `done` khi **mọi** task terminal — kể cả task lỗi, vì scheduler là
continue-on-error. Nên prompt đánh thức phải đọc `failedTasks`, đừng mặc định là
thành công.

### 18.3. Chạy lại việc lỗi

| | |
|---|---|
| `POST /api/dispatch/tasks/:task_id/retry` | chạy lại một task |
| `POST /api/dispatch/parents/:parent_id/retry` | chạy lại mọi task lỗi của một dispatch |

Nút bấm có ở cả ba client: web (`InlineDispatchCard.tsx` — header + drawer chi
tiết task), desktop (`message_widgets.dart::InlineDispatchCard`), mobile
(`channel_app` màn Dispatch — header thẻ + sheet chi tiết).

Hai điểm dễ sai:

- **Hồi sinh task phải hồi sinh cả parent.** DAG có task cuối lỗi thì parent đã
  `"done"`, mà scheduler bỏ qua parent không `active` — xếp lại task mà không lật
  parent về `"active"` thì task nằm đó mãi. Có test ghim.
- **Retry của người dùng KHÁC ngân sách `MAX_INFRA_RETRIES`.** Cái kia tự động,
  trần 1 lần, chỉ cho lỗi hạ tầng (`is_retryable_infra_error`). Cái này là người
  nhìn thấy lỗi và quyết định, nên không giới hạn và không xét nguyên nhân.

Retry cũng xoá phán quyết của lần chạy trước (`verification_result`,
`file_changes`, trạng thái checklist) — để lại thì bị đọc nhầm là của lần mới.
Khi daemon từ chối, lý do ("đang chạy", "đã xong") trả về nguyên văn trong body
`400` và cả ba client hiện đúng câu đó, vì "retry thất bại" chung chung chỉ khiến
người dùng bấm lại.

---

## Kết luận

Hệ thống Dispatch & Cowork trong SenClaw cung cấp:

1. **Task lifecycle management** - Từ creation → dispatch → execution → completion
2. **DAG-based orchestration** - Dependency graph với automatic scheduling
3. **Trigger-based automation** - Tự động tạo followup tasks dựa trên events
4. **Handoff rules** - Bàn giao thông minh với conditional gates
5. **Output validation** - Kiểm tra quality trước khi bàn giao
6. **Real-time sync** - WebSocket events cho UI updates

Checklist marking được thực hiện qua task status transitions, với validation và event firing để đảm bảo quality và visibility.
