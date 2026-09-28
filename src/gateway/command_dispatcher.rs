//! Parse and execute slash commands (honored in every group — every chat has
//! full admin privileges).
//! Mirrors `src-old/gateway/CommandDispatcher.ts`.

use std::sync::LazyLock;

use regex::Regex;

use crate::db::Db;
use crate::types::{ScheduledTask, TaskRunLog, TaskStatus};

pub const COMMANDS_HELP: &str = "\
📋 Available commands:
  list_tasks [folder]       — list tasks (optionally filter by group folder)
  task_logs <taskId> [n]    — show latest n execution logs (default 20)
  pause_task <taskId>       — pause task
  resume_task <taskId>      — resume task
  cancel_task <taskId>      — cancel task (mark completed, keep record)
  del_task <taskId>         — delete task (remove permanently)
  history                   — show conversation history stats
  reset                     — reset session (clear chat history)
  plugin help               — marketplace / plugin commands
  pair [list]               — chats waiting to be connected
  pair approve <CODE>       — let a waiting chat in
  pair reject <ID>          — turn a waiting chat away
  help                      — show this help";

/// Try parsing text as an admin command and execute it.
/// Returns command output text, or None (not a command — handle via agent).
/// `chat_jid` is required for `reset` and `history` commands.
pub fn dispatch_command(db: &Db, text: &str, chat_jid: Option<&str>) -> Option<String> {
    let t = text.trim();
    if re_help().is_match(t) {
        return Some(COMMANDS_HELP.to_string());
    }

    if let Some(caps) = re_list_tasks().captures(t) {
        let folder = caps.get(1).map(|m| m.as_str().to_string());
        let tasks = match &folder {
            Some(f) => db.get_tasks_by_group(f).unwrap_or_default(),
            None => db.list_all_tasks().unwrap_or_default(),
        };
        return Some(format_task_list(&tasks, folder.as_deref()));
    }

    // ===== Pairing =====
    //
    // Reachable from a channel chat and from a web/desktop chat session alike,
    // because both call this function. Only a chat that already has a binding
    // gets this far: an unbound one is answered with a code and returns before
    // command dispatch, so an unknown chat can never approve itself.
    if re_pair_list().is_match(t) {
        return Some(format_pending_pairings(db));
    }

    if let Some(caps) = re_pair_approve().captures(t) {
        let code = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        // A code from the SenClaw extension's side panel pairs the extension
        // (the browser engine's `/browser/ext`), not a chat.
        let ext_hub = crate::browser_agent::extension::hub();
        if ext_hub.has_pending(code) {
            return Some(match ext_hub.approve_code(code) {
                Ok(ext_id) => format!(
                    "✅ Đã kết nối extension SenClaw ({ext_id}).\n\
                     Agent chỉ dùng Chrome của bạn khi bạn yêu cầu."
                ),
                Err(e) => format!("❌ {e}"),
            });
        }
        return Some(match crate::gateway::pairing::approve_by_code(db, code) {
            Ok(a) => format!(
                "✅ Đã duyệt {} ({}) → agent '{}'.\n\
                 Bảo họ nhắn lại cho bot để bắt đầu.",
                if a.pairing.sender_name.is_empty() {
                    a.pairing.chat_jid.clone()
                } else {
                    a.pairing.sender_name.clone()
                },
                a.pairing.chat_jid,
                a.agent_folder
            ),
            // The refusals are already worded for a person — an expired code, a
            // request somebody already handled, and a channel with no agent are
            // three different problems. Do not flatten them.
            Err(e) => format!("❌ {e}"),
        });
    }

    if let Some(caps) = re_pair_reject().captures(t) {
        let id: i64 = caps.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0);
        return Some(match crate::gateway::pairing::reject(db, id) {
            Ok(p) => format!("Đã từ chối request {} ({}).", p.id, p.chat_jid),
            Err(e) => format!("❌ {e}"),
        });
    }

    if let Some(caps) = re_task_logs().captures(t) {
        let task_id = caps.get(1)?.as_str();
        let limit: u32 = caps
            .get(2)
            .map(|m| m.as_str().parse().unwrap_or(20))
            .unwrap_or(20);
        let logs = db.get_task_run_logs(task_id, limit).unwrap_or_default();
        return Some(format_task_logs(task_id, &logs));
    }

    if let Some(caps) = re_manage_task().captures(t) {
        let action = caps.get(1)?.as_str().to_lowercase();
        let task_id = caps.get(2)?.as_str();
        let new_status = match action.as_str() {
            "pause" => TaskStatus::Paused,
            "resume" => TaskStatus::Active,
            "cancel" => TaskStatus::Completed,
            _ => return None,
        };
        if db.update_task_status(task_id, new_status).is_err() {
            return Some(format!("❌ Failed to update task {task_id}"));
        }
        let label = match action.as_str() {
            "pause" => "paused",
            "resume" => "resumed",
            _ => "cancelled",
        };
        return Some(format!("✅ Task {task_id} {label}"));
    }

    if let Some(caps) = re_del_task().captures(t) {
        let task_id = caps.get(1)?.as_str();
        match db.delete_task(task_id) {
            Ok(true) => return Some(format!("🗑️ Task {task_id} deleted")),
            Ok(false) => return Some(format!("❌ Task not found: {task_id}")),
            Err(_) => return Some(format!("❌ Failed to delete task {task_id}")),
        }
    }

    if re_history().is_match(t) {
        let jid = chat_jid?;
        let count = db.count_group_messages(jid).unwrap_or(0);
        let last_ts = db.get_last_agent_timestamp(jid).ok().flatten();
        let cursor = last_ts.as_deref().unwrap_or("(none)");
        return Some(format!(
            "📊 Conversation history — {jid}\n  Messages: {count}\n  Last agent cursor: {cursor}\n\n\
             Send `/reset` to clear all chat history."
        ));
    }

    if re_reset().is_match(t) {
        let jid = chat_jid?;
        let count = db.delete_group_messages_for_jid(jid).unwrap_or(0);
        let _ = db.delete_agent_timestamp(jid);
        return Some(format!(
            "🗑️ Session reset — cleared {count} messages for {jid}"
        ));
    }

    None
}

// ===== Regex =====

fn re_help() -> &'static Regex {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/?help$").unwrap());
    &RE
}
fn re_list_tasks() -> &'static Regex {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^/?list[_\s]tasks?(?:\s+(\S+))?$").unwrap());
    &RE
}
fn re_task_logs() -> &'static Regex {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^/?task[_\s]logs?\s+(\S+)(?:\s+(\d+))?$").unwrap());
    &RE
}
fn re_manage_task() -> &'static Regex {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^/?(pause|resume|cancel)[_\s]task\s+(\S+)$").unwrap());
    &RE
}
fn re_del_task() -> &'static Regex {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/?del[_\s]task\s+(\S+)$").unwrap());
    &RE
}
fn re_history() -> &'static Regex {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/?history$").unwrap());
    &RE
}
fn re_pair_list() -> &'static Regex {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^/?pair(\s+list)?$").unwrap());
    &RE
}

fn re_pair_approve() -> &'static Regex {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^/?pair\s+approve\s+(\S+)$").unwrap());
    &RE
}

fn re_pair_reject() -> &'static Regex {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^/?pair\s+reject\s+(\d+)$").unwrap());
    &RE
}

fn re_reset() -> &'static Regex {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^/?reset[_\s]?(session)?$").unwrap());
    &RE
}

// ===== Formatting =====

fn format_task_list(tasks: &[ScheduledTask], folder: Option<&str>) -> String {
    let title = match folder {
        Some(f) => format!("📋 Task List - {f} ({} items)", tasks.len()),
        None => format!("📋 All Tasks ({} items)", tasks.len()),
    };
    if tasks.is_empty() {
        return format!("{title}\nNo tasks");
    }
    let status_icon = |s: TaskStatus| match s {
        TaskStatus::Active => "🟢",
        TaskStatus::Paused => "⏸",
        _ => "⏹",
    };
    let mut lines = vec![title, String::new()];
    for t in tasks {
        lines.push(format!(
            "{} {} · {}",
            status_icon(t.status),
            t.group_folder,
            t.context_mode.as_str()
        ));
        lines.push(format!("   ID: {}", t.id));
        lines.push(format!(
            "   Schedule: {} ({})",
            t.schedule_value,
            t.schedule_type.as_str()
        ));
        let preview: String = if t.prompt.chars().count() > 60 {
            format!("{}…", t.prompt.chars().take(60).collect::<String>())
        } else {
            t.prompt.clone()
        };
        lines.push(format!("   Content: {preview}"));
        if let Some(ref nr) = t.next_run {
            lines.push(format!("   Next: {nr}"));
        }
        if let Some(ref lr) = t.last_run {
            lines.push(format!("   Last: {lr}"));
        }
        lines.push(String::new());
    }
    lines.join("\n").trim_end().to_string()
}

fn format_task_logs(task_id: &str, logs: &[TaskRunLog]) -> String {
    if logs.is_empty() {
        return format!("📜 Task {task_id} No execution records yet");
    }
    let mut lines = vec![
        format!(
            "📜 Execution Logs — {task_id} (latest {} entries)",
            logs.len()
        ),
        String::new(),
    ];
    for log in logs {
        let icon = match log.status {
            crate::types::RunStatus::Success => "✅",
            crate::types::RunStatus::Error => "❌",
        };
        let dur = log
            .duration_ms
            .map(|d| format!("  ({d}ms)"))
            .unwrap_or_default();
        lines.push(format!("{icon} {}{dur}", log.run_at));
        if let Some(ref result) = log.result {
            let preview: String = if result.chars().count() > 120 {
                format!("{}…", result.chars().take(120).collect::<String>())
            } else {
                result.clone()
            };
            lines.push(format!("   {preview}"));
        }
        if let Some(ref err) = log.error {
            let preview: String = if err.chars().count() > 120 {
                format!("{}…", err.chars().take(120).collect::<String>())
            } else {
                err.clone()
            };
            lines.push(format!("   Error: {preview}"));
        }
        lines.push(String::new());
    }
    lines.join("\n").trim_end().to_string()
}


/// The waiting list, as a chat message.
///
/// Shows the sender's name beside the code: approving is a judgement about a
/// person, and a bare `tg:…:user:812…` gives nothing to judge. Expired rows are
/// labelled rather than hidden — vanishing silently reads as "the bot never got
/// my message".
fn format_pending_pairings(db: &Db) -> String {
    let rows = match db.list_pairings(true) {
        Ok(r) => r,
        Err(e) => return format!("❌ không đọc được danh sách pairing: {e}"),
    };
    if rows.is_empty() {
        return "Không có chat nào đang chờ kết nối.".to_string();
    }
    let mut out = String::from("🔗 Chat đang chờ kết nối:\n");
    for p in &rows {
        let expired = crate::db::pairings::is_expired(&p.expires_at);
        out.push_str(&format!(
            "\n#{}  {}  —  {} ({}){}",
            p.id,
            p.code,
            if p.sender_name.is_empty() {
                "(không rõ tên)"
            } else {
                &p.sender_name
            },
            if p.chat_type == "group" {
                "GROUP — duyệt là cho cả group vào"
            } else {
                "DM"
            },
            if expired { "  [HẾT HẠN]" } else { "" },
        ));
    }
    out.push_str("\n\nDuyệt:  pair approve <CODE>\nTừ chối: pair reject <ID>");
    out
}


#[cfg(test)]
mod pairing_command_tests {
    use super::*;

    fn seeded() -> (Db, i64) {
        let db = Db::open_in_memory(&crate::config::Config::from_env()).unwrap();
        let now = "2026-09-09T00:00:00Z";
        let ch = db
            .insert_channel("telegram", "TG", r#"{"botToken":"bot-A"}"#, now)
            .unwrap();
        let agent = db
            .insert_agent("main", "Main", false, None, None, "", None, now)
            .unwrap();
        db.insert_binding(None, agent, ch, None, None, now).unwrap();
        (db, ch)
    }

    fn knock(db: &Db, ch: i64, jid: &str) -> crate::types::ChannelPairing {
        db.create_pairing(ch, jid, "user", jid, "Doudji", None, Some("bot-A"))
            .unwrap()
    }

    #[test]
    fn pair_list_names_the_person_not_just_the_jid() {
        let (db, ch) = seeded();
        let p = knock(&db, ch, "tg:1:user:2");
        let out = dispatch_command(&db, "/pair", None).unwrap();
        assert!(out.contains(&p.code), "{out}");
        assert!(out.contains("Doudji"), "{out}");
    }

    #[test]
    fn bare_pair_and_pair_list_are_the_same_command() {
        let (db, _) = seeded();
        assert!(dispatch_command(&db, "pair", None).is_some());
        assert!(dispatch_command(&db, "/pair list", None).is_some());
    }

    #[test]
    fn approving_by_code_binds_the_chat() {
        let (db, ch) = seeded();
        let p = knock(&db, ch, "tg:1:user:2");
        let out = dispatch_command(&db, &format!("/pair approve {}", p.code), None).unwrap();
        assert!(out.contains("Đã duyệt"), "{out}");
        assert!(db.get_binding_by_jid("tg:1:user:2").unwrap().is_some());
    }

    #[test]
    fn a_bad_code_is_refused_in_words_the_user_can_act_on() {
        let (db, _) = seeded();
        let out = dispatch_command(&db, "/pair approve ZZZZZZZZ", None).unwrap();
        assert!(out.starts_with("❌"), "{out}");
    }

    #[test]
    fn rejecting_leaves_the_chat_unbound() {
        let (db, ch) = seeded();
        let p = knock(&db, ch, "tg:1:user:2");
        let out = dispatch_command(&db, &format!("/pair reject {}", p.id), None).unwrap();
        assert!(out.contains("từ chối"), "{out}");
        assert!(db.get_binding_by_jid("tg:1:user:2").unwrap().is_none());
    }

    #[test]
    fn ordinary_chat_is_not_swallowed_as_a_pair_command() {
        let (db, _) = seeded();
        // "pair" inside a sentence must reach the agent, not the dispatcher.
        assert!(dispatch_command(&db, "giúp tôi pair thiết bị bluetooth", None).is_none());
        assert!(dispatch_command(&db, "pairing status?", None).is_none());
    }
}
