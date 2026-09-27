//! Persistence for MCP tool aliases (Plugins → Alias).
//!
//! An alias maps the name an agent calls (`alias`) to the tool that actually
//! executes (`target_tool`). Two behaviours fall out of one table:
//!   * **rename** — `alias` is a new name: the roster shows it instead of the
//!     target's registered name and calls to it dispatch to the target.
//!   * **override** — `alias` equals an existing tool name: calls to that name
//!     are redirected to `target_tool`, shadowing the original implementation.
//!
//! `source` is `'user'` for rows created in the web UI and `'app:<app_id>'`
//! for rows imported from a Space App manifest (`mcp.toolAliases`). App rows
//! are imported **disabled** and stay owned by the app: re-imports refresh
//! target/description but never touch `enabled`, so a user's opt-in survives
//! app restarts and updates.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::params;
use serde::Serialize;

/// Source tag for rows created through the web UI.
pub const SOURCE_USER: &str = "user";

/// Source tag for rows imported from a Space App manifest.
pub fn app_source(app_id: &str) -> String {
    format!("app:{app_id}")
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAlias {
    pub alias: String,
    #[serde(rename = "target")]
    pub target_tool: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub source: String,
    pub created_at: i64,
    pub updated_at: i64,
}

fn row_to_alias(r: &rusqlite::Row<'_>) -> rusqlite::Result<ToolAlias> {
    Ok(ToolAlias {
        alias: r.get(0)?,
        target_tool: r.get(1)?,
        description: r.get(2)?,
        enabled: r.get::<_, i64>(3)? != 0,
        source: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
    })
}

const COLS: &str = "alias, target_tool, description, enabled, source, created_at, updated_at";

impl super::Db {
    /// Create a new alias. Returns `false` when an alias with that name
    /// already exists (caller decides whether that's a conflict).
    pub fn create_tool_alias(
        &self,
        alias: &str,
        target_tool: &str,
        description: Option<&str>,
        enabled: bool,
        source: &str,
    ) -> Result<bool> {
        let now = chrono::Utc::now().timestamp_millis();
        self.with_conn(|c| {
            let n = c.execute(
                r#"INSERT INTO mcp_tool_aliases (alias, target_tool, description, enabled, source, created_at, updated_at)
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                   ON CONFLICT(alias) DO NOTHING"#,
                params![alias, target_tool, description, enabled as i64, source, now],
            )?;
            Ok(n > 0)
        })
    }

    /// Update target/description of an existing alias. Returns `false` when
    /// the alias doesn't exist.
    pub fn update_tool_alias(
        &self,
        alias: &str,
        target_tool: &str,
        description: Option<&str>,
    ) -> Result<bool> {
        let now = chrono::Utc::now().timestamp_millis();
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE mcp_tool_aliases SET target_tool = ?2, description = ?3, updated_at = ?4 WHERE alias = ?1",
                params![alias, target_tool, description, now],
            )?;
            Ok(n > 0)
        })
    }

    /// Flip the enabled flag. This is the approval gate for app-declared
    /// aliases. Returns `false` when the alias doesn't exist.
    pub fn set_tool_alias_enabled(&self, alias: &str, enabled: bool) -> Result<bool> {
        let now = chrono::Utc::now().timestamp_millis();
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE mcp_tool_aliases SET enabled = ?2, updated_at = ?3 WHERE alias = ?1",
                params![alias, enabled as i64, now],
            )?;
            Ok(n > 0)
        })
    }

    pub fn delete_tool_alias(&self, alias: &str) -> Result<bool> {
        self.with_conn(|c| {
            let n = c.execute(
                "DELETE FROM mcp_tool_aliases WHERE alias = ?1",
                params![alias],
            )?;
            Ok(n > 0)
        })
    }

    pub fn get_tool_alias(&self, alias: &str) -> Result<Option<ToolAlias>> {
        self.with_conn(|c| {
            use rusqlite::OptionalExtension;
            Ok(c.query_row(
                &format!("SELECT {COLS} FROM mcp_tool_aliases WHERE alias = ?1"),
                params![alias],
                row_to_alias,
            )
            .optional()?)
        })
    }

    pub fn list_tool_aliases(&self) -> Result<Vec<ToolAlias>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {COLS} FROM mcp_tool_aliases ORDER BY source, alias"
            ))?;
            let rows = stmt
                .query_map([], row_to_alias)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Enabled aliases as `alias → (target, description)` — the shape the
    /// process-wide registry in [`crate::tools::tool_alias`] consumes.
    pub fn enabled_tool_alias_map(&self) -> Result<HashMap<String, (String, Option<String>)>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT alias, target_tool, description FROM mcp_tool_aliases WHERE enabled = 1",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        (r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?),
                    ))
                })?
                .collect::<rusqlite::Result<HashMap<_, _>>>()?;
            Ok(rows)
        })
    }

    /// Idempotent import of one app-declared alias. Refreshes target and
    /// description on re-import but NEVER touches `enabled` (the user's
    /// opt-in), and never overwrites a row owned by a different source —
    /// an app cannot hijack a user alias or another app's alias.
    pub fn import_app_tool_alias(
        &self,
        app_id: &str,
        alias: &str,
        target_tool: &str,
        description: Option<&str>,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        let source = app_source(app_id);
        self.with_conn(|c| {
            c.execute(
                r#"INSERT INTO mcp_tool_aliases (alias, target_tool, description, enabled, source, created_at, updated_at)
                   VALUES (?1, ?2, ?3, 0, ?4, ?5, ?5)
                   ON CONFLICT(alias) DO UPDATE SET
                       target_tool = excluded.target_tool,
                       description = excluded.description,
                       updated_at  = excluded.updated_at
                   WHERE mcp_tool_aliases.source = excluded.source"#,
                params![alias, target_tool, description, source, now],
            )?;
            Ok(())
        })
    }

    /// Remove app-owned aliases the app no longer declares. `keep` is the set
    /// of alias names present in the current manifest.
    pub fn prune_app_tool_aliases(&self, app_id: &str, keep: &[String]) -> Result<usize> {
        let source = app_source(app_id);
        self.with_conn(|c| {
            if keep.is_empty() {
                return Ok(c.execute(
                    "DELETE FROM mcp_tool_aliases WHERE source = ?1",
                    params![source],
                )?);
            }
            let placeholders = keep
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 2))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "DELETE FROM mcp_tool_aliases WHERE source = ?1 AND alias NOT IN ({placeholders})"
            );
            let mut args: Vec<&dyn rusqlite::ToSql> = vec![&source];
            for k in keep {
                args.push(k);
            }
            Ok(c.execute(&sql, args.as_slice())?)
        })
    }

    /// Remove every alias owned by `source` (app uninstall). Returns rows removed.
    pub fn delete_tool_aliases_by_source(&self, source: &str) -> Result<usize> {
        self.with_conn(|c| {
            Ok(c.execute(
                "DELETE FROM mcp_tool_aliases WHERE source = ?1",
                params![source],
            )?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;

    fn db() -> Db {
        let cfg = Config::from_env();
        Db::open_in_memory(&cfg).expect("open db")
    }

    #[test]
    fn create_list_toggle_delete_round_trip() {
        let db = db();
        assert!(db
            .create_tool_alias(
                "mcp__browser__nav",
                "mcp__senclaw-browser__browser_navigate",
                Some("short nav"),
                true,
                SOURCE_USER,
            )
            .unwrap());
        // Duplicate create is rejected, not overwritten.
        assert!(!db
            .create_tool_alias("mcp__browser__nav", "other", None, true, SOURCE_USER)
            .unwrap());

        let all = db.list_tool_aliases().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].target_tool, "mcp__senclaw-browser__browser_navigate");
        assert!(all[0].enabled);

        // Enabled map only lists enabled rows.
        assert_eq!(db.enabled_tool_alias_map().unwrap().len(), 1);
        assert!(db.set_tool_alias_enabled("mcp__browser__nav", false).unwrap());
        assert_eq!(db.enabled_tool_alias_map().unwrap().len(), 0);

        assert!(db
            .update_tool_alias("mcp__browser__nav", "mcp__x__y", None)
            .unwrap());
        assert_eq!(
            db.get_tool_alias("mcp__browser__nav")
                .unwrap()
                .unwrap()
                .target_tool,
            "mcp__x__y"
        );

        assert!(db.delete_tool_alias("mcp__browser__nav").unwrap());
        assert!(db.get_tool_alias("mcp__browser__nav").unwrap().is_none());
    }

    #[test]
    fn app_import_preserves_enabled_and_ownership() {
        let db = db();
        db.import_app_tool_alias("ssh-manager", "mcp__ssh__run", "mcp__ssh-manager-mcp__ssh_execute_command", None)
            .unwrap();
        let row = db.get_tool_alias("mcp__ssh__run").unwrap().unwrap();
        assert!(!row.enabled, "app aliases must be imported disabled");
        assert_eq!(row.source, "app:ssh-manager");

        // User opts in; a re-import (app restart) must not reset the flag.
        db.set_tool_alias_enabled("mcp__ssh__run", true).unwrap();
        db.import_app_tool_alias("ssh-manager", "mcp__ssh__run", "mcp__ssh-manager-mcp__ssh_execute_command", Some("run"))
            .unwrap();
        let row = db.get_tool_alias("mcp__ssh__run").unwrap().unwrap();
        assert!(row.enabled, "re-import must preserve the user's opt-in");
        assert_eq!(row.description.as_deref(), Some("run"));

        // Another app (different source) cannot hijack the alias.
        db.import_app_tool_alias("evil-app", "mcp__ssh__run", "mcp__evil__steal", None)
            .unwrap();
        let row = db.get_tool_alias("mcp__ssh__run").unwrap().unwrap();
        assert_eq!(row.target_tool, "mcp__ssh-manager-mcp__ssh_execute_command");
        assert_eq!(row.source, "app:ssh-manager");

        // User-owned rows are equally protected from app imports.
        db.create_tool_alias("mcp__mine__x", "mcp__a__b", None, true, SOURCE_USER)
            .unwrap();
        db.import_app_tool_alias("ssh-manager", "mcp__mine__x", "mcp__c__d", None)
            .unwrap();
        assert_eq!(
            db.get_tool_alias("mcp__mine__x").unwrap().unwrap().target_tool,
            "mcp__a__b"
        );
    }

    #[test]
    fn prune_and_delete_by_source() {
        let db = db();
        db.import_app_tool_alias("app1", "mcp__a__one", "mcp__t__1", None).unwrap();
        db.import_app_tool_alias("app1", "mcp__a__two", "mcp__t__2", None).unwrap();
        db.import_app_tool_alias("app2", "mcp__b__one", "mcp__t__3", None).unwrap();

        // Manifest now only declares `mcp__a__one` → the stale row goes away,
        // other sources untouched.
        let removed = db
            .prune_app_tool_aliases("app1", &["mcp__a__one".to_string()])
            .unwrap();
        assert_eq!(removed, 1);
        assert!(db.get_tool_alias("mcp__a__two").unwrap().is_none());
        assert!(db.get_tool_alias("mcp__b__one").unwrap().is_some());

        // Uninstall wipes everything the app owns.
        assert_eq!(db.delete_tool_aliases_by_source("app:app1").unwrap(), 1);
        assert_eq!(db.delete_tool_aliases_by_source("app:app2").unwrap(), 1);
        assert!(db.list_tool_aliases().unwrap().is_empty());
    }
}
