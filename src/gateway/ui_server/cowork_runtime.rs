//! Cowork runtime — minimal trigger evaluator + task creator.
//!
//! Wired into `handle_message_send`: every user message in a cowork group
//! lands here. We persist:
//!
//!   1. A "primary" task carrying the message (assigned to the manager).
//!   2. One follow-up task per member whose triggers match this message.
//!
//! Trigger schema mirrors the legacy CoworkManager (`b307fa8:src/cowork/mod.rs::
//! collect_triggered_tasks`): a JSON array of typed objects. We support
//! `message_received` (from?, messageType?) and `on_mention` (from).
//! Status/cron triggers are no-ops here — they fire from elsewhere.
//!
//! This is intentionally lightweight: no agent dispatch yet, no handoff
//! chain, no output validation. The point is: when you chat into a cowork
//! group, the team task list grows. The UI shows you what triggered.

use std::sync::Arc;

use crate::db::cowork_tasks::CoworkTeamTask;
use crate::db::cowork_teams::TeamMember;
use crate::db::Db;
use crate::util::local_time::local_iso_string_now;

/// Truncate `s` to at most `max_chars` chars without splitting UTF-8.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

/// Match a single trigger object against this user message.
///
/// Only message-time triggers are evaluated. Returns true if the rule's
/// `from`/`messageType` constraints are satisfied (or absent).
fn trigger_fires_on_user_message(rule: &serde_json::Value, sender: &str, msg_type: &str) -> bool {
    let ttype = rule.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match ttype {
        "message_received" => {
            let from_ok = rule
                .get("from")
                .and_then(|v| v.as_str())
                .map_or(true, |f| f == sender);
            let mt_ok = rule
                .get("messageType")
                .and_then(|v| v.as_str())
                .map_or(true, |m| m == msg_type);
            from_ok && mt_ok
        }
        "on_mention" => {
            // Only fire when the rule specifies who's mentioning. We
            // treat the sender as the "mentioner" — UI will tighten this
            // once a real @mention parser is wired.
            rule.get("from")
                .and_then(|v| v.as_str())
                .is_some_and(|f| f == sender)
        }
        _ => false,
    }
}

fn count_matching_rules(member: &TeamMember, sender: &str, msg_type: &str) -> usize {
    let Some(raw) = member.triggers.as_deref() else {
        return 0;
    };
    let Ok(list) = serde_json::from_str::<Vec<serde_json::Value>>(raw) else {
        return 0;
    };
    list.iter()
        .filter(|r| trigger_fires_on_user_message(r, sender, msg_type))
        .count()
}

/// Entry point: persist tasks for a user message in a cowork team.
///
/// Fire-and-forget — failures are logged but never propagated up to the
/// chat send path. The user gets their message routed to the manager even
/// if the cowork bookkeeping breaks.
/// Find the latest non-terminal manager task for a team. Used by the
/// state-transition hooks below to upgrade the right row when an agent
/// state event fires for a cowork chat.
pub fn latest_manager_task_id(db: &Arc<Db>, team_id: &str) -> Option<String> {
    let team = db.get_cowork_team(team_id).ok().flatten()?;
    let tasks = db.list_cowork_team_tasks(team_id).ok()?;
    tasks
        .into_iter()
        .filter(|t| t.assignee.as_deref() == Some(team.manager_folder.as_str()))
        .filter(|t| matches!(t.status.as_str(), "todo" | "in_progress"))
        .max_by(|a, b| a.created_at.cmp(&b.created_at))
        .map(|t| t.id)
}

/// Transition the manager's pending task to `in_progress` when the agent
/// starts working. No-op if no pending task or already in_progress.
pub fn on_agent_processing(db: &Arc<Db>, team_id: &str) {
    let Some(task_id) = latest_manager_task_id(db, team_id) else {
        return;
    };
    let now = local_iso_string_now();
    let _ = db.update_cowork_team_task(
        &task_id,
        None,
        None,
        Some("in_progress"),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        &now,
    );
    tracing::info!("[cowork_runtime] team={team_id} task={task_id} → in_progress (agent started)");
}

/// Classify the agent's reply to decide the task's terminal status.
/// Returns `"done"` for substantive completions, `"blocked"` for replies that
/// look like failures, unanswered questions, or timeouts — so the board
/// accurately reflects whether the work actually finished.
fn classify_reply(reply: &str) -> &'static str {
    let lower = reply.to_lowercase();
    let char_count = reply.chars().count();

    // Very short replies are almost never real completions in a cowork context.
    if char_count < 40 {
        return "blocked";
    }

    // Timeout / infrastructure failure signals from the dispatch bridge or
    // virtual worker pool.
    let failure_signals = [
        "timeout",
        "timed out",
        "không thể hoàn thành",
        "không hoàn thành được",
        "giới hạn cho phép",
        "every dispatch task failed",
        "deadlock",
        "cancelled by user",
    ];
    for sig in &failure_signals {
        if lower.contains(sig) {
            return "blocked";
        }
    }

    // Reply that is purely a question back to the user — the agent punted
    // instead of doing the work.  Heuristic: ends with `?` and has no code
    // block / tool output.
    let trimmed = reply.trim();
    if trimmed.ends_with('?') && !trimmed.contains("```") && char_count < 500 {
        return "blocked";
    }

    "done"
}

/// Mark the manager's pending task based on result quality.  Substantive
/// completions become `done`; failures / unanswered questions become
/// `blocked` so the board surfaces that work is still pending.
pub fn on_agent_reply(db: &Arc<Db>, team_id: &str, reply: &str) {
    let Some(task_id) = latest_manager_task_id(db, team_id) else {
        return;
    };
    let now = local_iso_string_now();
    let status = classify_reply(reply);
    let completed_at = if status == "done" {
        Some(now.as_str())
    } else {
        None
    };
    let _ = db.update_cowork_team_task(
        &task_id,
        None,
        None,
        Some(status),
        None,
        None,
        None,
        None,
        Some(reply),
        None,
        completed_at,
        &now,
    );
    tracing::info!(
        "[cowork_runtime] team={team_id} task={task_id} → {status} (result_len={})",
        reply.len()
    );
}

/// Build the team-context preamble that we prepend to a user message in a
/// cowork chat. The manager sees this every turn so it knows it's in DAG
/// mode, which members exist, and that its job is to PLAN → DELEGATE →
/// SYNTHESIZE rather than execute the underlying work itself.
///
/// Returns `None` for unknown teams or solo teams (no members) — caller
/// should fall back to the raw message in that case.
pub fn team_context_preamble(db: &Arc<Db>, team_id: &str) -> Option<String> {
    let team = db.get_cowork_team(team_id).ok().flatten()?;
    if team.members.is_empty() {
        return None;
    }

    // Custom preamble override (team settings): use it verbatim, then append the
    // user-request footer so the model still sees the message.
    if let Some(custom) = team
        .settings
        .manager_preamble
        .as_ref()
        .filter(|p| !p.trim().is_empty())
    {
        let mut s = custom.clone();
        s.push_str("\n\n---\n\nUser request (delegate per workflow above):\n");
        return Some(s);
    }

    let mut s = String::new();
    s.push_str("[Cowork DAG context — you are the LEAD / orchestrator of this team]\n");
    s.push_str(&format!("Team: {}\n", team.name));
    s.push_str(&format!(
        "Your role: lead (folder: {})\n",
        team.manager_folder
    ));
    if let Some(ref ws) = team.workspace_dir {
        s.push_str(&format!("Shared workspace: {ws}\n"));
    }
    s.push_str(
        "\n## CRITICAL RULES\n\
         DAG mode is active. You orchestrate by DISPATCHING work to your team \
         members — you do NOT do the work yourself, and you do NOT use the generic \
         `Task` tool. Delegate with `DispatchCreateParentAndRun`.\n\
         \n\
         NEVER attempt to execute the user's request directly (writing code, running \
         commands, creating files). You are the ORCHESTRATOR — your only job is to \
         break the request into tasks and dispatch them to the right team members.\n\
         \n\
         A dispatch needs at least TWO tasks — it is a fan-out, not a hand-off. When a \
         goal resists splitting, pair the work with a second member who verifies the \
         result; that keeps you an orchestrator without inventing filler work.\n\
         \n\
         NEVER ask the user clarifying questions for tasks your members can figure out \
         autonomously. Make reasonable assumptions and delegate. Only ask when a \
         fundamental requirement is genuinely ambiguous (e.g. target language not \
         specified among multiple possibilities).\n",
    );
    s.push_str(
        "\n## Your team members\n\
         Use each one as a DAG node's `agentName` (exactly `persona:<folder>` as shown):\n",
    );
    for m in team.members.iter() {
        let role = m.role.as_deref().unwrap_or("specialist");
        let resp = m.responsibilities.as_deref().unwrap_or("—");
        s.push_str(&format!(" • `persona:{}` ({role}) — {resp}\n", m.folder));
        if let Some(ac) = m
            .acceptance_criteria
            .as_deref()
            .filter(|v| !v.trim().is_empty())
        {
            s.push_str(&format!("     acceptance: {ac}\n"));
        }
        if let Some(of) = m.output_format.as_deref().filter(|v| !v.trim().is_empty()) {
            s.push_str(&format!("     output: {of}\n"));
        }
    }
    s.push_str(
        "\n## MANDATORY workflow\n\
         1. (optional) RESEARCH — use read-only tools (Read/Grep/Glob) if you need \
            context before planning the graph.\n\
         2. PLAN — decide which members to engage and in what order. Map the user's \
            request to concrete sub-tasks, one per member. Design the dependency chain \
            (e.g. implementer first, then reviewer dependsOn implementer, then tester \
            dependsOn both). Each member's prompt must be SELF-CONTAINED: include all \
            context, file paths, requirements, and acceptance criteria they need.\n\
         3. DISPATCH — call `DispatchCreateParentAndRun` ONCE with the full task graph:\n\
            • `goal` = the user's request, verbatim or lightly rephrased\n\
            • one task per member you need to engage\n\
            • `agentName` = `persona:<member folder>` from the list above\n\
            • `label` = the member folder (used by `dependsOn`)\n\
            • `prompt` = that member's slice of the work, including:\n\
              – WHAT to do (concrete deliverables, not vague direction)\n\
              – WHERE (file paths, project root, workspace dir)\n\
              – HOW to verify their own output (run tests, check build, etc.)\n\
              – Acceptance criteria from the member definition above\n\
            • `dependsOn` = labels of members whose output this one needs. \
              Prerequisite results are auto-injected into the prompt.\n\
            • `timeoutSeconds` = set higher (1800-3600) for tasks that involve \
              npm install, cargo build, or other slow operations\n\
            This BLOCKS until every member finishes and returns all their results.\n\
         4. SYNTHESIZE — after dispatch returns, merge the members' outputs into ONE \
            final answer for the user, attributing which member contributed what. \
            Include: what was built/changed, where to find it, how to run it, and \
            any issues encountered. Do NOT stop after dispatching.\n\
         \n\
         ## Example dispatch for \"build a Todo app in React\":\n\
         ```json\n\
         {\n\
           \"goal\": \"Build a Todo app in React with add/delete/toggle\",\n\
           \"timeoutSeconds\": 1800,\n\
           \"tasks\": [\n\
             {\"label\": \"code-writer\", \"agentName\": \"persona:code-writer\",\n\
              \"prompt\": \"Create a React Todo app at ~/workspace/todo-app with: ...detailed spec...\",\n\
              \"dependsOn\": []},\n\
             {\"label\": \"code-reviewer\", \"agentName\": \"persona:code-reviewer\",\n\
              \"prompt\": \"Review the React Todo app created by code-writer at ~/workspace/todo-app. ...\",\n\
              \"dependsOn\": [\"code-writer\"]},\n\
             {\"label\": \"test-engineer\", \"agentName\": \"persona:test-engineer\",\n\
              \"prompt\": \"Write and run tests for the Todo app at ~/workspace/todo-app. ...\",\n\
              \"dependsOn\": [\"code-writer\"]}\n\
           ]\n\
         }\n\
         ```\n\
         \n\
         ## ERROR HANDLING\n\
         If some tasks report status error/failed:\n\
         • Use the successful results and any partial output included in the report.\n\
         • Do NOT re-dispatch the whole DAG — only re-dispatch the single failed task \
           if its output is truly required.\n\
         • NEVER do the members' work yourself (you don't have their tools), and \
           NEVER attribute content to a member whose output you did not receive.\n\
         \n\
         ## WHAT NOT TO DO (anti-patterns)\n\
         ❌ Asking the user \"bạn có muốn tôi…\" — just dispatch and do it\n\
         ❌ Running npm/cargo/code commands yourself — delegate to a member\n\
         ❌ Creating only 1 task when you have 3 team members — use the team\n\
         ❌ Skipping dispatch for \"simple\" requests — if it involves work, delegate\n\
         ❌ Stopping after dispatch without synthesizing the results\n\
         \n\
         If the request is purely conversational (greeting, small talk), say so \
         explicitly: \"No specialist needed — answering directly.\" and answer. \
         Otherwise delegation is REQUIRED.\n",
    );
    s.push_str("\n---\n\nUser request (orchestrate the team per the workflow above):\n");
    Some(s)
}

pub fn on_user_message(db: &Arc<Db>, team_id: &str, content: &str) {
    let Ok(Some(team)) = db.get_cowork_team(team_id) else {
        tracing::debug!("[cowork_runtime] no team for id={team_id}");
        return;
    };

    // Auto-task creation can be disabled per-team via settings.
    if team.settings.auto_create_tasks == Some(false) {
        tracing::debug!("[cowork_runtime] auto-task disabled for team={team_id}");
        return;
    }

    let now = local_iso_string_now();
    let title = truncate_chars(content.trim(), 60);
    if title.is_empty() {
        return;
    }
    let sender = "user";
    let msg_type = "status"; // mirrors legacy: user messages default to "status"

    // 1. Primary task — always created, assigned to the manager.
    let primary = CoworkTeamTask {
        id: uuid::Uuid::new_v4().to_string(),
        team_id: team_id.to_string(),
        title: title.clone(),
        description: Some(content.to_string()),
        status: "todo".to_string(),
        assignee: Some(team.manager_folder.clone()),
        reviewer: None,
        priority: "high".to_string(),
        depends_on: Vec::new(),
        result_output: None,
        created_at: now.clone(),
        updated_at: now.clone(),
        due_at: None,
        completed_at: None,
    };
    if let Err(e) = db.insert_cowork_team_task(&primary) {
        tracing::warn!("[cowork_runtime] primary insert failed: {e}");
    } else {
        tracing::info!(
            "[cowork_runtime] team={team_id} primary task created for manager={}",
            team.manager_folder
        );
    }

    // 2. Pre-create a backlog task for every team member so the board shows
    //    the planned decomposition upfront.  These start as `backlog` and get
    //    promoted to `in_progress` / `done` by the dispatch lifecycle bridge.
    //    Members with matching triggers get `todo` instead (ready to start).
    let mut member_task_ids: Vec<String> = Vec::new();
    for member in team.members.iter() {
        let triggered = count_matching_rules(member, sender, msg_type) > 0;
        let status = if triggered { "todo" } else { "backlog" };
        let role_label = member.role.as_deref().unwrap_or("member");
        let member_title = format!("[{}] {}", role_label, title);
        let tid = uuid::Uuid::new_v4().to_string();
        member_task_ids.push(tid.clone());
        let task = CoworkTeamTask {
            id: tid,
            team_id: team_id.to_string(),
            title: member_title,
            description: Some(content.to_string()),
            status: status.to_string(),
            assignee: Some(member.folder.clone()),
            reviewer: None,
            priority: "medium".to_string(),
            depends_on: vec![primary.id.clone()],
            result_output: None,
            created_at: now.clone(),
            updated_at: now.clone(),
            due_at: None,
            completed_at: None,
        };
        if let Err(e) = db.insert_cowork_team_task(&task) {
            tracing::warn!(
                "[cowork_runtime] member task insert failed for {}: {e}",
                member.folder
            );
        } else {
            tracing::info!(
                "[cowork_runtime] team={team_id} member task ({status}) for {}",
                member.folder
            );
        }
    }
}

/// Bridge a DispatchBridge task lifecycle event to the CoworkTeamTask board.
///
/// The `label` from dispatch is the member folder (e.g. "code-writer").
/// We find the most recent non-terminal CoworkTeamTask assigned to that
/// folder and update its status to match the dispatch status.
///
/// This is wired as the `set_task_lifecycle_callback` in `run_daemon()`.
pub fn on_dispatch_task_lifecycle(
    db: &Arc<Db>,
    _dispatch_task_id: &str,
    dispatch_status: &str,
    label: &str,
    _parent_goal: &str,
    result: Option<String>,
) {
    let member_folder = label.strip_prefix("persona:").unwrap_or(label);

    let cowork_status = match dispatch_status {
        "registered" => "todo",
        "processing" => "in_progress",
        "done" => "done",
        "error" | "timeout" => "blocked",
        _ => return,
    };

    let now = local_iso_string_now();

    // Scan all teams — a member folder may appear in multiple teams, but
    // we only update the most recent non-terminal task per team.
    let teams = match db.list_cowork_teams() {
        Ok(t) => t,
        Err(_) => return,
    };

    for team in &teams {
        let has_member = team.members.iter().any(|m| m.folder == member_folder);
        if !has_member {
            continue;
        }
        let tasks = match db.list_cowork_team_tasks(&team.id) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let target = tasks
            .into_iter()
            .filter(|t| t.assignee.as_deref() == Some(member_folder))
            .filter(|t| !matches!(t.status.as_str(), "done" | "blocked"))
            .max_by(|a, b| a.created_at.cmp(&b.created_at));
        let Some(task) = target else { continue };

        let completed_at = if cowork_status == "done" {
            Some(now.as_str())
        } else {
            None
        };
        let result_ref = result.as_deref();
        let _ = db.update_cowork_team_task(
            &task.id,
            None,
            None,
            Some(cowork_status),
            None,
            None,
            None,
            None,
            result_ref,
            None,
            completed_at,
            &now,
        );
        tracing::info!(
            "[cowork_runtime] dispatch lifecycle: team={} member={} task={} → {}",
            team.id,
            member_folder,
            task.id,
            cowork_status
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_done_for_substantive_reply() {
        let reply = "Tôi đã tạo dự án React Todo app thành công tại ~/workspace/todo-app. \
                      Project structure: src/App.tsx, src/components/TodoList.tsx, \
                      src/components/TodoItem.tsx. Chạy `npm run dev` để khởi động.";
        assert_eq!(classify_reply(reply), "done");
    }

    #[test]
    fn classify_blocked_for_question_reply() {
        let reply = "Bạn có muốn tôi thử lại từng bước một (bắt đầu bằng việc khởi tạo \
                      dự án trước, sau đó mới viết code) hay bạn đã có sẵn một dự án React \
                      rỗng để chúng ta bắt đầu viết code ngay?";
        assert_eq!(classify_reply(reply), "blocked");
    }

    #[test]
    fn classify_blocked_for_timeout() {
        let reply = "Có vẻ như quá trình khởi tạo dự án đang gặp lỗi timeout ở hệ thống ngầm. \
                      Do npm install có thể tốn khá nhiều thời gian nên các tác vụ đã không thể \
                      hoàn thành trong giới hạn cho phép.";
        assert_eq!(classify_reply(reply), "blocked");
    }

    #[test]
    fn classify_blocked_for_very_short() {
        assert_eq!(classify_reply("OK"), "blocked");
        assert_eq!(classify_reply("Đã xong."), "blocked");
    }

    #[test]
    fn classify_done_for_long_question_with_code() {
        let reply = "Here is the implementation:\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n\nDoes this look good?";
        assert_eq!(classify_reply(reply), "done");
    }
}
