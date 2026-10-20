//! A2A tasks backed by kanban cards. Incoming tasks land on the SLink board,
//! owned by the sending peer through the card's tenant (`sen-link:<peer-id>` —
//! the tenant keeps the code name, it is data that old cards already carry);
//! a peer only ever sees its own cards.

use serde_json::{json, Value};

use super::a2a::TaskState;
use super::settings::{DEFAULT_BOARD_TITLE, LEGACY_BOARD_TITLE};
use crate::kanban::db::{Card, Comment, Db};

const BOARD_DESCRIPTION: &str = "Tasks received from SLink peers (A2A).";

pub fn tenant_of(peer_id: &str) -> String {
    format!("sen-link:{peer_id}")
}

/// Task state for a card in column `role` with these comments.
pub fn state_for(role: &str, comments: &[Comment]) -> TaskState {
    let last_dispatch = comments.iter().rev().find(|c| matches!(c.kind.as_str(), "cancel" | "block" | "complete"));
    match role {
        "done" => TaskState::Completed,
        "in_progress" => TaskState::Working,
        "blocked" => match last_dispatch {
            Some(c) if c.kind == "cancel" => TaskState::Canceled,
            Some(c) if c.body.starts_with("gave_up") => TaskState::Failed,
            _ => TaskState::InputRequired,
        },
        _ => TaskState::Submitted,
    }
}

/// The A2A `Task` for a card.
pub fn task_json(card: &Card, role: &str, comments: &[Comment]) -> Value {
    let state = state_for(role, comments);
    let artifacts: Vec<Value> = comments
        .iter()
        .filter(|c| matches!(c.kind.as_str(), "complete" | "worktree"))
        .map(|c| {
            json!({
                "artifactId": format!("comment-{}", c.id),
                "name": c.kind,
                "parts": [{ "text": c.body }],
            })
        })
        .collect();
    let status_text = comments.last().map(|c| c.body.clone());
    let mut status = json!({
        "state": state.as_str(),
        "timestamp": chrono::DateTime::from_timestamp(card.updated_at, 0).map(|t| t.to_rfc3339()),
    });
    if let Some(t) = status_text {
        status["message"] = json!({
            "messageId": format!("status-{}-{}", card.id, card.updated_at),
            "role": "ROLE_AGENT",
            "parts": [{ "text": t }],
        });
    }
    json!({
        "id": card.id.to_string(),
        "contextId": format!("card-{}", card.id),
        "status": status,
        "artifacts": artifacts,
        "metadata": { "title": card.title, "assignee": card.assignee, "column": role },
    })
}

/// The card `task_id` names, if `peer_id` owns it.
pub fn owned_card(db: &Db, peer_id: &str, task_id: &str) -> Option<Card> {
    let id: i64 = task_id.parse().ok()?;
    let card = db.card_row(id).ok()??;
    (card.tenant.as_deref() == Some(tenant_of(peer_id).as_str())).then_some(card)
}

pub fn task_for(db: &Db, card: &Card) -> anyhow::Result<Value> {
    let role = db.card_role(card.id)?.unwrap_or_default();
    let comments = db.comments_of_card(card.id)?;
    Ok(task_json(card, &role, &comments))
}

/// The board incoming tasks land on, with the title it carries now. For the
/// default title a board still named after the old code name counts too — it
/// is the same board from before the display name changed.
pub fn find_board(db: &Db, title: &str) -> anyhow::Result<Option<(i64, String)>> {
    let boards = db.list_boards()?;
    let wanted = |t: &str| boards.iter().find(|b| b.title == t).map(|b| (b.id, b.title.clone()));
    Ok(wanted(title).or_else(|| (title == DEFAULT_BOARD_TITLE).then(|| wanted(LEGACY_BOARD_TITLE)).flatten()))
}

/// The board incoming tasks land on; created with the default workflow on first
/// use. A board under the old code name is renamed in place, keeping its cards.
pub fn ensure_board(db: &Db, title: &str, workspace_dir: Option<&str>, now: i64) -> anyhow::Result<i64> {
    match find_board(db, title)? {
        Some((id, current)) => {
            if current != title {
                db.rename_board(id, title, BOARD_DESCRIPTION, now)?;
            }
            Ok(id)
        }
        None => db.create_board(title, BOARD_DESCRIPTION, true, workspace_dir, now),
    }
}

pub struct NewTask<'a> {
    pub peer_id: &'a str,
    pub peer_name: &'a str,
    pub text: &'a str,
    pub assignee: Option<&'a str>,
    pub auto_run: bool,
    pub worktree: bool,
}

/// Create the card for an incoming task. Without `auto_run` it waits in
/// Triage for a person — a remote peer never starts work here on its own.
pub fn create(db: &Db, board_id: i64, t: &NewTask<'_>, now: i64) -> anyhow::Result<Card> {
    let role = if t.auto_run { "ready" } else { "triage" };
    let column = db
        .column_by_role(board_id, role)?
        .ok_or_else(|| anyhow::anyhow!("board {board_id} has no `{role}` column"))?;
    let first = t.text.lines().find(|l| !l.trim().is_empty()).unwrap_or("Task from SLink").trim();
    let title: String = first.chars().take(120).collect();
    let description = format!("{}\n\n— from SLink peer **{}**", t.text.trim(), t.peer_name);
    let mut labels = vec!["SLink"];
    if t.worktree {
        labels.push("worktree");
    }
    let labels = serde_json::to_string(&labels)?;
    let id = db.add_card(
        column,
        &title,
        &description,
        None,
        t.assignee,
        Some(&tenant_of(t.peer_id)),
        Some(&labels),
        None,
        now,
    )?;
    db.card_row(id)?.ok_or_else(|| anyhow::anyhow!("card {id} vanished"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, Db) {
        let t = tempfile::tempdir().unwrap();
        let db = Db::open(&t.path().join("kanban.db")).unwrap();
        (t, db)
    }

    fn comment(kind: &str, body: &str) -> Comment {
        Comment { id: 1, author: "dispatcher".into(), body: body.into(), kind: kind.into(), created_at: 0 }
    }

    #[test]
    fn column_roles_map_to_task_states() {
        assert_eq!(state_for("triage", &[]), TaskState::Submitted);
        assert_eq!(state_for("ready", &[]), TaskState::Submitted);
        assert_eq!(state_for("in_progress", &[]), TaskState::Working);
        assert_eq!(state_for("done", &[]), TaskState::Completed);
        assert_eq!(state_for("blocked", &[comment("block", "need a key")]), TaskState::InputRequired);
        assert_eq!(state_for("blocked", &[comment("block", "gave_up: boom")]), TaskState::Failed);
        assert_eq!(state_for("blocked", &[comment("cancel", "canceled")]), TaskState::Canceled);
    }

    #[test]
    fn incoming_tasks_wait_in_triage_unless_the_peer_may_auto_run() {
        let (_t, db) = db();
        let board = ensure_board(&db, "SLink", None, 1).unwrap();
        assert_eq!(ensure_board(&db, "SLink", None, 2).unwrap(), board);
        let mk =|auto_run| NewTask { peer_id: "p1", peer_name: "B", text: "Fix the login bug\nmore", assignee: Some("claude-code"), auto_run, worktree: true };
        let c = create(&db, board, &mk(false), 3).unwrap();
        assert_eq!(c.title, "Fix the login bug");
        assert_eq!(db.card_role(c.id).unwrap().as_deref(), Some("triage"));
        assert!(c.labels.as_deref().unwrap().contains("worktree"));
        assert!(c.labels.as_deref().unwrap().contains("SLink"));
        assert!(c.description.contains("from SLink peer **B**"));
        let r = create(&db, board, &mk(true), 3).unwrap();
        assert_eq!(db.card_role(r.id).unwrap().as_deref(), Some("ready"));

        assert!(owned_card(&db, "p1", &c.id.to_string()).is_some());
        assert!(owned_card(&db, "p2", &c.id.to_string()).is_none());
        let task = task_for(&db, &c).unwrap();
        assert_eq!(task["status"]["state"], "TASK_STATE_SUBMITTED");
        assert_eq!(task["id"], c.id.to_string());
    }

    #[test]
    fn a_board_under_the_old_code_name_is_renamed_in_place_and_keeps_its_cards() {
        let (_t, db) = db();
        let old = ensure_board(&db, "sen-link", None, 1).unwrap();
        let card = create(&db, old, &NewTask { peer_id: "p1", peer_name: "B", text: "Do X", assignee: None, auto_run: false, worktree: false }, 2).unwrap();

        // Asked for by the default title, the old board is found and renamed, not duplicated.
        assert_eq!(find_board(&db, "SLink").unwrap(), Some((old, "sen-link".to_string())));
        assert_eq!(ensure_board(&db, "SLink", None, 3).unwrap(), old);
        let boards = db.list_boards().unwrap();
        assert_eq!(boards.len(), 1);
        assert_eq!(boards[0].title, "SLink");
        assert!(owned_card(&db, "p1", &card.id.to_string()).is_some(), "its cards are still there");

        // A custom title never borrows the old board, and a missing board is not an error.
        assert_eq!(find_board(&db, "Mine").unwrap(), None);
        assert_ne!(ensure_board(&db, "Mine", None, 4).unwrap(), old);
    }
}
