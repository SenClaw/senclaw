---
name: schedule
description: Recurring schedule management — create, update, pause, delete automated agent tasks with Agent/DAG/Plan modes via Space MCP
version: 1.0.0
when-to-use: When the user wants to set up recurring automated tasks, create periodic agent jobs, manage cron-like schedules, pause/resume/delete scheduled runs, change agent execution mode (Agent/DAG/Plan), or check run history and errors of scheduled tasks.
triggers:
  # --- Core keywords (Vietnamese) ---
  - định kỳ
  - lịch định kỳ
  - tự động
  - chạy tự động
  - đặt lịch
  - hẹn giờ
  - cron
  # --- Create (Vietnamese) ---
  - tạo lịch
  - thêm lịch
  - đặt lịch chạy
  - mỗi ngày
  - mỗi sáng
  - mỗi tối
  - mỗi tuần
  - mỗi tháng
  - hàng ngày
  - hàng tuần
  - hàng tháng
  # --- Manage (Vietnamese) ---
  - tạm dừng lịch
  - dừng lịch
  - kích hoạt lại
  - xoá lịch
  - sửa lịch
  - đổi giờ chạy
  - đổi tần suất
  # --- Agent mode (Vietnamese) ---
  - chế độ chạy
  - chế độ agent
  - chế độ dag
  - chế độ plan
  - đổi chế độ
  # --- Status / history (Vietnamese) ---
  - xem lịch định kỳ
  - lịch sử chạy
  - lần chạy
  - lỗi lịch
  - kết quả chạy
  # --- Waiting on a long-running job (Vietnamese) ---
  - chờ kết quả
  - đợi kết quả
  - chờ xong
  - đợi xong
  - khi nào xong
  - bao giờ xong
  - theo dõi tiến độ
  - kiểm tra lại sau
  # --- Waiting on a long-running job (English) ---
  - wait for result
  - wait until done
  - check back later
  - poll until
  - watch the job
  - long-running task
  # --- English keywords ---
  - recurring
  - schedule
  - scheduled task
  - cron job
  - every day
  - every morning
  - every week
  - every month
  - daily
  - weekly
  - monthly
  - periodic
  - automate
  - automation
  # --- English manage ---
  - pause schedule
  - resume schedule
  - delete schedule
  - update schedule
  - run history
  - schedule error
  # --- Agent mode (English) ---
  - agent mode
  - dag mode
  - plan mode
  - switch mode
  - execution mode
mcp_servers:
  - senclaw-space
  - senclaw-schedule
---

# Schedule — Recurring Agent Tasks

Manage recurring scheduled tasks that run automatically. Each schedule owns a **dedicated chat session** — the system auto-creates a conversation thread so every agent run has full history. Tools register as `space_recurring_*` on the `senclaw-space` MCP server. Call them by the **canonical bridge name** `mcp__space__<verb>` — the resolver strips the redundant `space_` prefix once (e.g. `space_recurring_create` → `mcp__space__recurring_create`). The bare `space_<verb>(...)` notation used below maps to the same tool.

## Required Tool Discovery

Before calling any schedule action, make sure the concrete MCP tool is visible. If not, call `ToolSearch` first:

```
ToolSearch { query: "select:mcp__space__recurring_create" }
ToolSearch { query: "select:mcp__space__recurring_list" }
```

If an exact `select:` query returns no match, search by keywords such as `space recurring create`, then call the exact tool name returned.

Only tell the user the schedule was created after the concrete tool call returns a success result.

---

## Create a Recurring Schedule

```
space_recurring_create(prompt, label?, time_local?, frequency?, weekday?, day_of_month?, cron_advanced?, agent_mode?)
```

| Parameter | Required | Description |
|-----------|----------|-------------|
| `prompt` | yes | What the agent should do each run (e.g. "Tìm giá vàng SJC hôm nay") |
| `label` | no | Display name for the schedule and its chat session; auto-generated from prompt if omitted |
| `time_local` | conditional | Run time in local clock, format `"HH:MM"` (24h). Required unless using `cron_advanced` |
| `frequency` | no | `"daily"` \| `"weekdays"` \| `"weekly"` \| `"monthly"`. Default `"daily"` |
| `weekday` | no | `0`=Sun .. `6`=Sat — used when frequency=`"weekly"` |
| `day_of_month` | no | `1`..`28` — used when frequency=`"monthly"` |
| `cron_advanced` | no | Raw 5-field cron expression. **Overrides** time_local/frequency when provided |
| `agent_mode` | no | `"agent"` (default) \| `"dag"` \| `"plan"` — how the task is executed |

### Two ways to set the schedule:
1. **Simple** (recommended): `time_local` + `frequency` (+ `weekday`/`day_of_month`)
2. **Advanced**: `cron_advanced` with a 5-field cron expression

### Simple examples:
```json
// Every day at 7:00am
{ "prompt": "Báo giá vàng SJC", "time_local": "07:00", "frequency": "daily" }

// Weekdays at 8:30am with DAG mode
{ "prompt": "Tổng hợp email và Slack", "time_local": "08:30", "frequency": "weekdays", "agent_mode": "dag" }

// Every Monday at 9:00am
{ "prompt": "Weekly report", "time_local": "09:00", "frequency": "weekly", "weekday": 1 }

// 15th of each month at 2pm with Plan mode
{ "prompt": "Monthly billing review", "time_local": "14:00", "frequency": "monthly", "day_of_month": 15, "agent_mode": "plan" }
```

### Common cron patterns (for `cron_advanced`):
| Intent | Cron |
|--------|------|
| Every day at 7am | `0 7 * * *` |
| Every day at 9pm | `0 21 * * *` |
| Every Monday at 9am | `0 9 * * 1` |
| Every Friday at 5pm | `0 17 * * 5` |
| Every weekday at 8am | `0 8 * * 1-5` |
| Every hour | `0 * * * *` |
| Every 6 hours | `0 */6 * * *` |

---

## Agent Mode

| Mode | When to use | How it works |
|------|-------------|--------------|
| `agent` | Simple, single-step tasks (price checks, summaries, notifications) | One agent runs the prompt directly in the schedule's chat session |
| `dag` | Multi-step tasks requiring parallel work or tool orchestration | Dispatches the prompt to a DAG team of virtual workers |
| `plan` | Complex tasks that benefit from planning before execution | Agent creates a step-by-step plan, then executes each step |

**Default is `agent`** — only use `dag`/`plan` when the task complexity warrants it.

### When to suggest each mode:

| Task pattern | Mode |
|-------------|------|
| "Tìm giá vàng mỗi sáng" (simple lookup + report) | `agent` |
| "Kiểm tra email, Slack, và tóm tắt" (multi-source aggregation) | `dag` |
| "Phân tích dữ liệu bán hàng tuần, so sánh và viết báo cáo" (multi-step analysis) | `plan` |
| "Gửi thông báo nhắc họp" (simple notification) | `agent` |
| "Dọn dẹp repo: kiểm tra CI, review PR, merge" (orchestrated workflow) | `dag` |

---

## List All Recurring Schedules

```
space_recurring_list()
```
Returns all schedules with: `id`, `label`, `prompt`, `chat_jid`, `schedule_value`, `agent_mode`, `status`, `next_run`, `last_run`, `last_status`.

---

## Get Schedule Detail

```
space_recurring_get(id)
```
Returns full schedule info plus the **20 most recent run logs** (each with `run_at`, `duration_ms`, `status`, `result`, `error`).

Use this when the user asks:
- "lịch X chạy thế nào?" / "how did schedule X run?"
- "xem lịch sử chạy" / "show run history"
- "lần chạy gần nhất có lỗi gì không?" / "any errors in recent runs?"

---

## Update a Recurring Schedule

```
space_recurring_update(id, prompt?, label?, status?, time_local?, frequency?, weekday?, day_of_month?, cron_advanced?, agent_mode?)
```
- Only provided fields are changed — omit fields you don't want to modify
- `status`: `"active"` | `"paused"` | `"completed"`

### Common update operations:
```json
// Pause a schedule
space_recurring_update(id, { status: "paused" })

// Resume a paused schedule
space_recurring_update(id, { status: "active" })

// Change to DAG mode
space_recurring_update(id, { agent_mode: "dag" })

// Change time to 8am weekdays
space_recurring_update(id, { time_local: "08:00", frequency: "weekdays" })

// Change the prompt
space_recurring_update(id, { prompt: "New task description" })

// Change label
space_recurring_update(id, { label: "Báo cáo sáng" })
```

---

## Delete a Recurring Schedule

```
space_recurring_delete(id)
```
Permanently deletes the schedule **and its chat session**. Cannot be undone.

**Always confirm with the user before deleting:**
> "Xoá lịch 'Báo giá vàng mỗi sáng' và toàn bộ lịch sử chat? Hành động này không thể hoàn tác."

---

## How Scheduled Tasks Execute

When a schedule triggers:
1. The `TaskScheduler` daemon detects `next_run <= now` for an active task
2. It advances `next_run` to the next occurrence (prevents re-pickup)
3. The task runs in its dedicated chat session (`schedule:<id>`)
4. The agent receives the `prompt` and executes according to `agent_mode`
5. Output is stored in `task_run_logs` (status, result, duration, error)
6. On completion, `last_run` and `last_result` are updated on the task

### What happens when the daemon is offline:
- Missed runs are **not replayed** — the task simply advances to the next scheduled time
- The `last_run` field will show a gap in execution
- Use `space_recurring_get(id)` to check the run logs for gaps

---

## Workflow: User Asks to Set Up a Recurring Task

1. Parse the user's intent: **what** to do, **how often**, **what time**, **which mode**
2. Convert natural-language schedule to `time_local` + `frequency` (or `cron_advanced`)
3. Choose appropriate `agent_mode` based on task complexity
4. Call `space_recurring_create` with the parameters
5. Confirm to user: show the label, schedule expression, next run time, and agent mode
6. Mention they can view/edit the schedule in **Định kỳ** tab of the Space UI

### Example conversation flow:
```
User: "Mỗi sáng 7h kiểm tra giá vàng và Bitcoin rồi báo cáo"

Agent thinking:
- what: "Kiểm tra giá vàng và Bitcoin, tạo báo cáo"
- when: daily at 07:00
- mode: "agent" (simple lookup + report)

→ space_recurring_create({
    prompt: "Kiểm tra giá vàng SJC và Bitcoin hôm nay, so sánh với hôm qua, tạo báo cáo tóm tắt",
    label: "Báo giá vàng & Bitcoin sáng",
    time_local: "07:00",
    frequency: "daily",
    agent_mode: "agent"
  })

→ "Đã tạo lịch 'Báo giá vàng & Bitcoin sáng' — chạy mỗi ngày lúc 07:00, chế độ Agent.
   Lần chạy tiếp theo: 07:00 ngày mai. Bạn có thể xem và chỉnh sửa trong tab Định kỳ."
```

---

## Workflow: User Asks to Manage Existing Schedules

### "Xem tất cả lịch" / "Show all schedules"
1. Call `space_recurring_list()`
2. Present as a table: label, schedule, mode, status, next run, last status

### "Tạm dừng lịch X" / "Pause schedule X"
1. Call `space_recurring_list()` to find the schedule by name/label
2. Call `space_recurring_update(id, { status: "paused" })`
3. Confirm: "Đã tạm dừng lịch 'X'. Dùng lệnh kích hoạt lại khi cần."

### "Xoá lịch X" / "Delete schedule X"
1. Find the schedule
2. Confirm with user (deletion is permanent)
3. Call `space_recurring_delete(id)`

### "Lịch X lỗi / không chạy" / "Schedule X has errors"
1. Call `space_recurring_get(id)` to see run logs
2. Check recent runs for `status: "error"` entries
3. Report the error messages to the user
4. Suggest fixes (update prompt, change mode, etc.)

---

## When to Use Schedule Tools

### Creating schedules
| User says | Tool & parameters |
|-----------|-------------------|
| "mỗi sáng 7h kiểm tra giá vàng" / "check gold price every morning at 7am" | `space_recurring_create` with `time_local: "07:00", frequency: "daily"` |
| "đặt lịch báo giá vàng mỗi sáng" / "schedule daily gold price report" | `space_recurring_create` with prompt + time_local + frequency |
| "mỗi thứ 2 gửi báo cáo tuần" / "send weekly report every Monday" | `space_recurring_create` with `frequency: "weekly", weekday: 1` |
| "ngày 15 hàng tháng kiểm tra billing" / "check billing on 15th monthly" | `space_recurring_create` with `frequency: "monthly", day_of_month: 15` |
| "thứ 2 đến thứ 6 lúc 8h30 tổng hợp email" / "weekdays at 8:30 summarize email" | `space_recurring_create` with `frequency: "weekdays", time_local: "08:30"` |
| "mỗi giờ kiểm tra server" / "check server every hour" | `space_recurring_create` with `cron_advanced: "0 * * * *"` |
| "mỗi 6 tiếng kiểm tra" / "check every 6 hours" | `space_recurring_create` with `cron_advanced: "0 */6 * * *"` |
| "định kỳ mỗi ngày lúc 9h tối" / "every day at 9pm" | `space_recurring_create` with `time_local: "21:00", frequency: "daily"` |

### Choosing agent mode
| User says | Tool & parameters |
|-----------|-------------------|
| "chạy bằng DAG" / "use DAG mode" | `agent_mode: "dag"` |
| "chạy bằng Plan" / "use Plan mode" | `agent_mode: "plan"` |
| "tác vụ phức tạp, cần lên kế hoạch" / "complex task, needs planning" | `agent_mode: "plan"` |
| "cần nhiều agent làm song song" / "need multiple agents in parallel" | `agent_mode: "dag"` |
| (simple task, no mode specified) | Default `agent_mode: "agent"` |

### Viewing & monitoring
| User says | Tool & parameters |
|-----------|-------------------|
| "xem lịch định kỳ" / "list schedules" / "show all schedules" | `space_recurring_list()` |
| "lịch X chạy thế nào?" / "how did schedule X run?" | `space_recurring_get(id)` — shows 20 recent runs |
| "lần chạy gần nhất" / "last run result" | `space_recurring_get(id)` → check `runs[0]` |
| "có lỗi gì không?" / "any errors?" | `space_recurring_get(id)` → filter runs with `status: "error"` |
| "lịch sử chạy" / "run history" | `space_recurring_get(id)` → show all run logs |
| "lần chạy tiếp theo khi nào?" / "when is the next run?" | `space_recurring_list()` → check `next_run` field |

### Updating schedules
| User says | Tool & parameters |
|-----------|-------------------|
| "đổi giờ chạy sang 8h" / "change run time to 8am" | `space_recurring_update(id, time_local: "08:00")` |
| "đổi sang chạy hàng tuần" / "switch to weekly" | `space_recurring_update(id, frequency: "weekly", weekday: 1)` |
| "đổi prompt" / "change the task" | `space_recurring_update(id, prompt: "new prompt")` |
| "đổi tên lịch" / "rename schedule" | `space_recurring_update(id, label: "new label")` |
| "đổi sang chế độ DAG" / "switch to DAG mode" | `space_recurring_update(id, agent_mode: "dag")` |
| "đổi sang chế độ Plan" / "switch to Plan mode" | `space_recurring_update(id, agent_mode: "plan")` |
| "đổi cron" / "change cron expression" | `space_recurring_update(id, cron_advanced: "0 8 * * 1-5")` |

### Pausing & resuming
| User says | Tool & parameters |
|-----------|-------------------|
| "tạm dừng lịch X" / "pause schedule X" | `space_recurring_update(id, status: "paused")` |
| "dừng lịch" / "stop schedule" | `space_recurring_update(id, status: "paused")` |
| "kích hoạt lại" / "resume" / "bật lại" | `space_recurring_update(id, status: "active")` |
| "tạm nghỉ tuần này" / "skip this week" | `space_recurring_update(id, status: "paused")` → remind to resume |

### Deleting schedules
| User says | Tool & parameters |
|-----------|-------------------|
| "xoá lịch X" / "delete schedule X" | Confirm first → `space_recurring_delete(id)` |
| "bỏ lịch này" / "remove this schedule" | Confirm first → `space_recurring_delete(id)` |
| "không cần nữa" / "don't need it anymore" | Find by context → confirm → `space_recurring_delete(id)` |


---

## Waiting on a long-running job — `schedule_watch`

Recurring schedules above are the **user's** automation. This section is a
different thing: what to do when **you** hand work to something slow and the
answer is not ready before your turn ends.

`schedule_watch` re-checks a condition on an interval and
wakes **this chat** the moment it holds.

### When to use it

The moment you delegate to anything that returns "in progress":

- an AI Office task (`office_*`), a dispatch/DAG run, a cowork task board
- a Space App job — build, import, render, scrape, ETL
- anything whose tool answer is a job id plus a status

**For a DAG specifically:** `dispatch_all_tasks` and `create_parent_and_run`
block your turn until every subtask is terminal (900s default). That is fine for
a short graph. For anything longer, call `create_parent` (which returns
immediately), then watch `dispatch_status`:

```json
{
  "label": "DAG p-20260905-1130",
  "tool": "mcp__core__dispatch_status",
  "args": { "parentId": "p-20260905-1130" },
  "done_path": "status",
  "done_op": "equals",
  "done_value": "done",
  "interval_secs": 60,
  "timeout_secs": 7200,
  "resume_prompt": "DAG p-20260905-1130 đã chạy xong. Tóm tắt: {{result}}. Gọi dispatch_status để lấy kết quả từng task, tổng hợp rồi trả lời user. Nếu có task lỗi, nói rõ task nào lỗi và vì sao — đừng bịa kết quả cho nó."
}
```

A DAG parent reaches `"done"` when every subtask is terminal — including the
failed ones, because the scheduler is continue-on-error. So the resume prompt
must check `failedTasks`, not assume success.

### What NOT to do

| Anti-pattern | Why it fails |
|---|---|
| `sleep 30` then poll once | Burns the turn, and a job slower than the sleep is still unfinished. Then you give up. |
| "Ask me again later for progress" | Pushes your job onto the user. The whole point is that it comes back on its own. |
| Polling in a loop inside one turn | Blocks the session and hits the turn ceiling. |
| `background_*` tools | They **cannot reply to a chat** by design — an OS notification is their only reach. |

### How

```json
{
  "label": "AI Office #27",
  "tool": "mcp__ai-office-mcp__office_get_task",
  "args": { "id": 27 },
  "done_path": "status",
  "done_op": "in",
  "done_values": ["done", "completed", "failed"],
  "interval_secs": 60,
  "timeout_secs": 3600,
  "resume_prompt": "Nhiệm vụ AI Office #27 (nghiên cứu thị trường cafe Hà Nội) đã kết thúc. Trạng thái/kết quả: {{result}}. Lấy báo cáo đầy đủ bằng office_get_task rồi tổng hợp và trả lời user."
}
```

Then **end your turn** and tell the user you will report back — do not keep polling.

| Parameter | Notes |
|---|---|
| `resume_prompt` | Required. You are woken with this **and nothing else** — restate what was being waited on. `{{result}}` = the probe payload |
| `tool` + `args` | Re-called each check with **no LLM**, so waiting is nearly free. Use the **exact name your tool list shows** — built-ins are `mcp__core__<tool>` under the default bundled server, not `mcp__senclaw-<domain>__<tool>`. Omit only when the job cannot be checked in one call — you are then woken every interval to check by hand, costing a full turn each time |
| `done_path` | Dot path into the result. **Call the probe tool once first and read the path off its real answer** — a guessed path resolves to nothing, counts as "not ready", and the watch waits out its full deadline on a finished job. AI Office's `office_get_task` answers `{events, steps, task}`, so the path is `task.status` — not `status`, not `data.task.status` |
| `done_op` | `exists` (default), `equals`, `contains`, `in`, and their negations. Case-insensitive |
| `interval_secs` | Default 60, clamped to 15–3600 |
| `timeout_secs` | Default 3600, clamped to ≤ 86400 |

### Two mistakes that waste the whole deadline

Both are silent — the watch keeps running and reports nothing:

- **Missing a required argument.** The tool answers with a rejection every
  check. Pass every argument it needs.
- **A guessed `done_path`.** A path that resolves to nothing counts as "not
  reported yet", so the watch never finishes.

Call the probe tool once before arming. Its answer gives you both.

### Guarantees

- **It always reports back.** On success, on deadline, and after repeated probe
  errors — the chat is told either way. It never ends silently.
- **Transient errors do not kill it.** A stopped session Space App is its resting
  state; the watch tolerates several consecutive failures before giving up.
- **Include terminal failure states in `done_values`** (`failed`, `error`,
  `cancelled`). A watch that only matches `done` waits out its full deadline on a
  job that died in the first minute.
