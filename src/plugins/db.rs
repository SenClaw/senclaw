//! DB CRUD for installed_plugins and plugin_runtime tables.

use anyhow::Result;
use rusqlite::{params, OptionalExtension as _};
use serde::{Deserialize, Serialize};

use crate::db::Db;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPlugin {
    pub slug: String,
    pub display_name: Option<String>,
    pub summary: Option<String>,
    pub version: String,
    pub plugin_type: String,
    pub registry: String,
    pub enabled: bool,
    pub installed_at: i64,
    pub updated_at: i64,
    pub config_json: String,
    pub manifest_json: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRuntime {
    pub slug: String,
    pub status: String,
    pub pid: Option<i64>,
    pub port: Option<i64>,
    pub started_at: Option<i64>,
    pub error_msg: Option<String>,
    pub last_ping: Option<i64>,
}

// ── installed_plugins ─────────────────────────────────────────────────────────

pub fn upsert_plugin(db: &Db, p: &InstalledPlugin) -> Result<()> {
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO installed_plugins
                (slug, display_name, summary, version, plugin_type, registry,
                 enabled, installed_at, updated_at, config_json, manifest_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(slug) DO UPDATE SET
                display_name=excluded.display_name,
                summary=excluded.summary,
                version=excluded.version,
                plugin_type=excluded.plugin_type,
                registry=excluded.registry,
                enabled=excluded.enabled,
                updated_at=excluded.updated_at,
                config_json=excluded.config_json,
                manifest_json=excluded.manifest_json",
            params![
                p.slug,
                p.display_name,
                p.summary,
                p.version,
                p.plugin_type,
                p.registry,
                p.enabled as i32,
                p.installed_at,
                p.updated_at,
                p.config_json,
                p.manifest_json,
            ],
        )?;
        Ok(())
    })
}

pub fn list_plugins(db: &Db) -> Result<Vec<InstalledPlugin>> {
    db.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT slug, display_name, summary, version, plugin_type, registry,
                    enabled, installed_at, updated_at, config_json, manifest_json
             FROM installed_plugins
             ORDER BY installed_at DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(InstalledPlugin {
                    slug: r.get(0)?,
                    display_name: r.get(1)?,
                    summary: r.get(2)?,
                    version: r.get(3)?,
                    plugin_type: r.get(4)?,
                    registry: r.get(5)?,
                    enabled: r.get::<_, i32>(6)? != 0,
                    installed_at: r.get(7)?,
                    updated_at: r.get(8)?,
                    config_json: r.get(9)?,
                    manifest_json: r.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
}

pub fn get_plugin(db: &Db, slug: &str) -> Result<Option<InstalledPlugin>> {
    db.with_conn(|conn| {
        conn.query_row(
            "SELECT slug, display_name, summary, version, plugin_type, registry,
                    enabled, installed_at, updated_at, config_json, manifest_json
             FROM installed_plugins WHERE slug = ?1",
            params![slug],
            |r| {
                Ok(InstalledPlugin {
                    slug: r.get(0)?,
                    display_name: r.get(1)?,
                    summary: r.get(2)?,
                    version: r.get(3)?,
                    plugin_type: r.get(4)?,
                    registry: r.get(5)?,
                    enabled: r.get::<_, i32>(6)? != 0,
                    installed_at: r.get(7)?,
                    updated_at: r.get(8)?,
                    config_json: r.get(9)?,
                    manifest_json: r.get(10)?,
                })
            },
        )
        .optional()
        .map_err(anyhow::Error::from)
    })
}

pub fn delete_plugin(db: &Db, slug: &str) -> Result<()> {
    db.with_conn(|conn| {
        conn.execute(
            "DELETE FROM installed_plugins WHERE slug = ?1",
            params![slug],
        )?;
        Ok(())
    })
}

pub fn set_plugin_enabled(db: &Db, slug: &str, enabled: bool) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE installed_plugins SET enabled = ?1, updated_at = ?2 WHERE slug = ?3",
            params![enabled as i32, now, slug],
        )?;
        Ok(())
    })
}

pub fn update_plugin_config(db: &Db, slug: &str, config_json: &str) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE installed_plugins SET config_json = ?1, updated_at = ?2 WHERE slug = ?3",
            params![config_json, now, slug],
        )?;
        Ok(())
    })
}

// ── plugin_runtime ────────────────────────────────────────────────────────────

pub fn upsert_runtime(db: &Db, rt: &PluginRuntime) -> Result<()> {
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO plugin_runtime
                (slug, status, pid, port, started_at, error_msg, last_ping)
             VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(slug) DO UPDATE SET
                status=excluded.status, pid=excluded.pid, port=excluded.port,
                started_at=excluded.started_at, error_msg=excluded.error_msg,
                last_ping=excluded.last_ping",
            params![
                rt.slug,
                rt.status,
                rt.pid,
                rt.port,
                rt.started_at,
                rt.error_msg,
                rt.last_ping
            ],
        )?;
        Ok(())
    })
}

pub fn get_runtime(db: &Db, slug: &str) -> Result<Option<PluginRuntime>> {
    db.with_conn(|conn| {
        conn.query_row(
            "SELECT slug, status, pid, port, started_at, error_msg, last_ping
             FROM plugin_runtime WHERE slug = ?1",
            params![slug],
            |r| {
                Ok(PluginRuntime {
                    slug: r.get(0)?,
                    status: r.get(1)?,
                    pid: r.get(2)?,
                    port: r.get(3)?,
                    started_at: r.get(4)?,
                    error_msg: r.get(5)?,
                    last_ping: r.get(6)?,
                })
            },
        )
        .optional()
        .map_err(anyhow::Error::from)
    })
}
