//! Cowork team tasks — CRUD for manager-tracked work items.
//!
//! Schema mirrors the legacy `cowork_tasks` table in src/db/cowork/tasks.rs
//! at git `b307fa8` (kanban-style statuses, assignee + reviewer references,
//! depends_on graph). Pared down to the fields the manager actually uses
//! for delegation — the heavier output_validation / artifacts / references
//! fields from v0 were rarely populated and have been dropped.

use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::Db;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoworkTeamTask {
    pub id: String,
    pub team_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// One of: backlog / todo / in_progress / review / done / blocked.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
    /// One of: low / medium / high / critical.
    pub priority: String,
    /// JSON array of task ids this task waits on.
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_output: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

fn parse_depends(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

impl Db {
    pub fn insert_cowork_team_task(&self, t: &CoworkTeamTask) -> Result<()> {
        let deps = serde_json::to_string(&t.depends_on).unwrap_or_else(|_| "[]".into());
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO cowork_team_tasks
                 (id, team_id, title, description, status, assignee, reviewer, priority,
                  depends_on, result_output, created_at, updated_at, due_at, completed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    t.id,
                    t.team_id,
                    t.title,
                    t.description,
                    t.status,
                    t.assignee,
                    t.reviewer,
                    t.priority,
                    deps,
                    t.result_output,
                    t.created_at,
                    t.updated_at,
                    t.due_at,
                    t.completed_at,
                ],
            )?;
            Ok(())
        })
    }

    pub fn list_cowork_team_tasks(&self, team_id: &str) -> Result<Vec<CoworkTeamTask>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, team_id, title, description, status, assignee, reviewer, priority,
                        depends_on, result_output, created_at, updated_at, due_at, completed_at
                 FROM cowork_team_tasks WHERE team_id = ?1 ORDER BY created_at",
            )?;
            let rows = stmt
                .query_map(params![team_id], |r| {
                    let deps_json: String = r.get(8)?;
                    Ok(CoworkTeamTask {
                        id: r.get(0)?,
                        team_id: r.get(1)?,
                        title: r.get(2)?,
                        description: r.get(3)?,
                        status: r.get(4)?,
                        assignee: r.get(5)?,
                        reviewer: r.get(6)?,
                        priority: r.get(7)?,
                        depends_on: parse_depends(&deps_json),
                        result_output: r.get(9)?,
                        created_at: r.get(10)?,
                        updated_at: r.get(11)?,
                        due_at: r.get(12)?,
                        completed_at: r.get(13)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_cowork_team_task(
        &self,
        id: &str,
        title: Option<&str>,
        description: Option<&str>,
        status: Option<&str>,
        assignee: Option<&str>,
        reviewer: Option<&str>,
        priority: Option<&str>,
        depends_on: Option<&[String]>,
        result_output: Option<&str>,
        due_at: Option<&str>,
        completed_at: Option<&str>,
        now: &str,
    ) -> Result<()> {
        let deps_json =
            depends_on.map(|d| serde_json::to_string(d).unwrap_or_else(|_| "[]".into()));
        self.with_conn(|c| {
            if let Some(t) = title {
                c.execute(
                    "UPDATE cowork_team_tasks SET title=?1, updated_at=?2 WHERE id=?3",
                    params![t, now, id],
                )?;
            }
            if let Some(d) = description {
                c.execute(
                    "UPDATE cowork_team_tasks SET description=?1, updated_at=?2 WHERE id=?3",
                    params![d, now, id],
                )?;
            }
            if let Some(s) = status {
                c.execute(
                    "UPDATE cowork_team_tasks SET status=?1, updated_at=?2 WHERE id=?3",
                    params![s, now, id],
                )?;
            }
            if let Some(a) = assignee {
                c.execute(
                    "UPDATE cowork_team_tasks SET assignee=?1, updated_at=?2 WHERE id=?3",
                    params![a, now, id],
                )?;
            }
            if let Some(r) = reviewer {
                c.execute(
                    "UPDATE cowork_team_tasks SET reviewer=?1, updated_at=?2 WHERE id=?3",
                    params![r, now, id],
                )?;
            }
            if let Some(p) = priority {
                c.execute(
                    "UPDATE cowork_team_tasks SET priority=?1, updated_at=?2 WHERE id=?3",
                    params![p, now, id],
                )?;
            }
            if let Some(deps) = deps_json {
                c.execute(
                    "UPDATE cowork_team_tasks SET depends_on=?1, updated_at=?2 WHERE id=?3",
                    params![deps, now, id],
                )?;
            }
            if let Some(r) = result_output {
                c.execute(
                    "UPDATE cowork_team_tasks SET result_output=?1, updated_at=?2 WHERE id=?3",
                    params![r, now, id],
                )?;
            }
            if let Some(d) = due_at {
                c.execute(
                    "UPDATE cowork_team_tasks SET due_at=?1, updated_at=?2 WHERE id=?3",
                    params![d, now, id],
                )?;
            }
            if let Some(c2) = completed_at {
                c.execute(
                    "UPDATE cowork_team_tasks SET completed_at=?1, updated_at=?2 WHERE id=?3",
                    params![c2, now, id],
                )?;
            }
            Ok(())
        })
    }

    pub fn delete_cowork_team_task(&self, id: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM cowork_team_tasks WHERE id = ?1", params![id])?;
            Ok(())
        })
    }

    pub fn delete_cowork_team_tasks_for_team(&self, team_id: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "DELETE FROM cowork_team_tasks WHERE team_id = ?1",
                params![team_id],
            )?;
            Ok(())
        })
    }
}
