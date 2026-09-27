//! Custom cowork template persistence.
//!
//! Built-in templates live in code (`BUILTIN_TEMPLATES` in
//! `gateway::ui_server::cowork`). This module stores the *user-authored*
//! templates: editable squad blueprints (manager + members + behaviour
//! settings) that the user manages from the Cowork UI and instantiates the
//! same way as built-ins.

use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::cowork_teams::{CoworkTeamSettings, TeamMember};
use super::Db;

/// A user-authored cowork template row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoworkTemplate {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_icon")]
    pub icon: String,
    pub manager_folder: String,
    #[serde(default = "default_role")]
    pub manager_role: String,
    #[serde(default)]
    pub members: Vec<TeamMember>,
    #[serde(default)]
    pub settings: CoworkTeamSettings,
    pub created_at: String,
    pub updated_at: String,
}

fn default_icon() -> String {
    "🧩".into()
}
fn default_role() -> String {
    "lead".into()
}

impl Db {
    pub fn insert_cowork_template(&self, t: &CoworkTemplate) -> Result<()> {
        let members_json = serde_json::to_string(&t.members).unwrap_or_else(|_| "[]".into());
        let settings_json = t.settings.to_json();
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO cowork_templates \
                   (id, name, description, icon, manager_folder, manager_role, members_json, settings_json, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    t.id, t.name, t.description, t.icon, t.manager_folder, t.manager_role,
                    members_json, settings_json, t.created_at, t.updated_at,
                ],
            )?;
            Ok(())
        })
    }

    pub fn list_cowork_templates(&self) -> Result<Vec<CoworkTemplate>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, description, icon, manager_folder, manager_role, members_json, settings_json, created_at, updated_at \
                 FROM cowork_templates ORDER BY updated_at DESC",
            )?;
            let rows = stmt
                .query_map([], row_to_template)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn get_cowork_template(&self, id: &str) -> Result<Option<CoworkTemplate>> {
        self.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT id, name, description, icon, manager_folder, manager_role, members_json, settings_json, created_at, updated_at \
                     FROM cowork_templates WHERE id = ?1",
                    params![id],
                    row_to_template,
                )
                .ok();
            Ok(row)
        })
    }

    pub fn update_cowork_template(&self, t: &CoworkTemplate) -> Result<()> {
        let members_json = serde_json::to_string(&t.members).unwrap_or_else(|_| "[]".into());
        let settings_json = t.settings.to_json();
        self.with_conn(|c| {
            c.execute(
                "UPDATE cowork_templates SET \
                   name = ?2, description = ?3, icon = ?4, manager_folder = ?5, manager_role = ?6, \
                   members_json = ?7, settings_json = ?8, updated_at = ?9 \
                 WHERE id = ?1",
                params![
                    t.id,
                    t.name,
                    t.description,
                    t.icon,
                    t.manager_folder,
                    t.manager_role,
                    members_json,
                    settings_json,
                    t.updated_at,
                ],
            )?;
            Ok(())
        })
    }

    pub fn delete_cowork_template(&self, id: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute("DELETE FROM cowork_templates WHERE id = ?1", params![id])?;
            Ok(())
        })
    }
}

fn row_to_template(r: &rusqlite::Row<'_>) -> rusqlite::Result<CoworkTemplate> {
    let members_json: String = r.get(6)?;
    let settings_json: String = r.get(7).unwrap_or_else(|_| "{}".into());
    let members = serde_json::from_str::<Vec<TeamMember>>(&members_json).unwrap_or_default();
    Ok(CoworkTemplate {
        id: r.get(0)?,
        name: r.get(1)?,
        description: r.get(2)?,
        icon: r.get(3)?,
        manager_folder: r.get(4)?,
        manager_role: r.get(5)?,
        members,
        settings: CoworkTeamSettings::from_json(&settings_json),
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
    })
}
