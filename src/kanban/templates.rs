//! Column templates for Kanban boards.
//!
//! A template is an ordered set of workflow columns (title/role/color/WIP). Two
//! builtins ship with SenClaw — `standard` (the Hermes 6-stage flow the
//! dispatcher expects) and `advanced` (adds Backlog + Review with WIP limits) —
//! plus `simple` (classic 3-column). Custom templates are user-managed (create /
//! import / export / delete) from Plugins → Kanban and stored in the Kanban DB.

use anyhow::{anyhow, Result};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::db::Db;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateColumn {
    pub title: String,
    /// Workflow role: triage|todo|ready|in_progress|blocked|done|custom.
    #[serde(default = "custom_role")]
    pub role: String,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub wip_limit: Option<i64>,
}

fn custom_role() -> String {
    "custom".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnTemplate {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Builtin templates cannot be deleted or overwritten.
    #[serde(default)]
    pub builtin: bool,
    pub columns: Vec<TemplateColumn>,
}

fn col(title: &str, role: &str, color: &str, wip: Option<i64>) -> TemplateColumn {
    TemplateColumn {
        title: title.into(),
        role: role.into(),
        color: Some(color.into()),
        wip_limit: wip,
    }
}

/// The builtin templates. `standard` mirrors `DEFAULT_WORKFLOW` — the flow the
/// autonomous dispatcher understands (todo auto-promotes to ready; done/blocked
/// receive complete/block).
pub fn builtins() -> Vec<ColumnTemplate> {
    vec![
        ColumnTemplate {
            id: "standard".into(),
            name: "Standard (Hermes)".into(),
            description: "Triage → Todo → Ready → In Progress → Blocked → Done. \
                          The autonomous dispatcher's native workflow."
                .into(),
            builtin: true,
            columns: vec![
                col("Triage", "triage", "#a855f7", None),
                col("Todo", "todo", "#64748b", None),
                col("Ready", "ready", "#0ea5e9", None),
                col("In Progress", "in_progress", "#3b82f6", None),
                col("Blocked", "blocked", "#ef4444", None),
                col("Done", "done", "#22c55e", None),
            ],
        },
        ColumnTemplate {
            id: "advanced".into(),
            name: "Advanced (review + WIP)".into(),
            description: "Adds a Backlog and a human Review gate, with WIP limits \
                          on the flow stages."
                .into(),
            builtin: true,
            columns: vec![
                col("Triage", "triage", "#a855f7", None),
                col("Backlog", "custom", "#94a3b8", None),
                col("Todo", "todo", "#64748b", None),
                col("Ready", "ready", "#0ea5e9", Some(5)),
                col("In Progress", "in_progress", "#3b82f6", Some(3)),
                col("Review", "custom", "#f59e0b", Some(3)),
                col("Blocked", "blocked", "#ef4444", None),
                col("Done", "done", "#22c55e", None),
            ],
        },
        ColumnTemplate {
            id: "simple".into(),
            name: "Simple (classic)".into(),
            description: "To Do → In Progress → Done. No dispatcher automation \
                          (no Ready column)."
                .into(),
            builtin: true,
            columns: vec![
                col("To Do", "todo", "#64748b", None),
                col("In Progress", "in_progress", "#3b82f6", None),
                col("Done", "done", "#22c55e", None),
            ],
        },
    ]
}

/// A URL/id-safe slug from a template name.
fn slugify(name: &str) -> String {
    let s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "template".into()
    } else {
        s
    }
}

impl Db {
    /// All templates: builtins first, then custom (from the DB).
    pub fn list_templates(&self) -> Result<Vec<ColumnTemplate>> {
        let mut out = builtins();
        let customs: Vec<ColumnTemplate> = self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, description, columns_json FROM column_templates ORDER BY name",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })?
                .filter_map(|r| r.ok())
                .filter_map(|(id, name, description, cols)| {
                    serde_json::from_str::<Vec<TemplateColumn>>(&cols)
                        .ok()
                        .map(|columns| ColumnTemplate {
                            id,
                            name,
                            description,
                            builtin: false,
                            columns,
                        })
                })
                .collect();
            Ok(rows)
        })?;
        out.extend(customs);
        Ok(out)
    }

    pub fn get_template(&self, id: &str) -> Result<Option<ColumnTemplate>> {
        Ok(self.list_templates()?.into_iter().find(|t| t.id == id))
    }

    /// Create/overwrite a CUSTOM template (builtin ids are refused). Returns the id.
    pub fn save_template(
        &self,
        name: &str,
        description: &str,
        columns: &[TemplateColumn],
    ) -> Result<String> {
        if name.trim().is_empty() {
            return Err(anyhow!("template name is required"));
        }
        if columns.is_empty() {
            return Err(anyhow!("template needs at least one column"));
        }
        let id = slugify(name);
        if builtins().iter().any(|b| b.id == id) {
            return Err(anyhow!("'{id}' is a builtin template — pick another name"));
        }
        let json = serde_json::to_string(columns)?;
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO column_templates(id, name, description, columns_json, created_at)
                 VALUES(?1,?2,?3,?4,strftime('%s','now'))
                 ON CONFLICT(id) DO UPDATE SET name=?2, description=?3, columns_json=?4",
                params![id, name.trim(), description.trim(), json],
            )?;
            Ok(())
        })?;
        Ok(id)
    }

    /// Delete a CUSTOM template. Builtins are refused.
    pub fn delete_template(&self, id: &str) -> Result<()> {
        if builtins().iter().any(|b| b.id == id) {
            return Err(anyhow!("cannot delete a builtin template"));
        }
        self.with_conn(|c| {
            let n = c.execute("DELETE FROM column_templates WHERE id=?1", params![id])?;
            if n == 0 {
                return Err(anyhow!("template '{id}' not found"));
            }
            Ok(())
        })
    }

    /// Create a board seeded from a template's columns.
    pub fn create_board_from_template(
        &self,
        title: &str,
        description: &str,
        workspace_dir: Option<&str>,
        template: &ColumnTemplate,
        now: i64,
    ) -> Result<i64> {
        let board_id = self.create_board(title, description, false, workspace_dir, now)?;
        self.with_conn(|c| {
            for (i, tc) in template.columns.iter().enumerate() {
                c.execute(
                    "INSERT INTO columns(board_id, title, role, color, wip_limit, ord, created_at)
                     VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        board_id,
                        tc.title,
                        tc.role,
                        tc.color,
                        tc.wip_limit,
                        i as i64,
                        now
                    ],
                )?;
            }
            Ok(())
        })?;
        Ok(board_id)
    }
}
