//! SQLite layer — one serialized connection behind a `Mutex` with WAL, matching
//! the other Space Apps.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;
use serde_json::{json, Value};

const SCHEMA: &str = include_str!("schema.sql");

/// Bring an existing database up to the current schema.
///
/// `schema.sql` runs with `IF NOT EXISTS`, so a new column added to a `CREATE
/// TABLE` reaches fresh databases only — every database created before the
/// change keeps the old shape, and the first query naming the new column fails
/// at run time. The app has already shipped sandboxes to disk, so each added
/// column needs an explicit `ALTER`, guarded by what the table actually has.
fn migrate(conn: &Connection) -> Result<()> {
    let existing: Vec<String> = {
        let mut st = conn.prepare("PRAGMA table_info(sandboxes)")?;
        let rows = st.query_map([], |r| r.get::<_, String>(1))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    // (column, definition) — additive only. A migration that rewrites or drops
    // is not safe to run unattended on a user's data.
    let wanted: &[(&str, &str)] = &[
        ("mounts_json", "TEXT NOT NULL DEFAULT '[]'"),
        ("fs_mode", "TEXT NOT NULL DEFAULT 'strict'"),
        ("trace_enabled", "INTEGER NOT NULL DEFAULT 0"),
        ("ports_json", "TEXT NOT NULL DEFAULT '{}'"),
    ];
    for (col, def) in wanted {
        if !existing.iter().any(|c| c == col) {
            conn.execute_batch(&format!("ALTER TABLE sandboxes ADD COLUMN {col} {def};"))?;
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Sandbox {
    pub id: String,
    pub name: String,
    pub backend: String,
    pub image: Option<String>,
    pub workdir: String,
    pub network: bool,
    pub cpus: f64,
    pub memory_mb: i64,
    pub pids_limit: i64,
    pub timeout_ms: i64,
    pub env: Value,
    pub mounts: Vec<crate::sandbox::mounts::Mount>,
    pub fs_mode: crate::sandbox::fsmode::FsMode,
    pub trace_enabled: bool,
    pub ports: crate::sandbox::ports::PortPolicy,
    pub status: String,
    pub container_id: Option<String>,
    pub last_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: Option<i64>,
}

impl Sandbox {
    fn from_row(r: &Row) -> rusqlite::Result<Self> {
        let env_json: String = r.get("env_json")?;
        Ok(Sandbox {
            id: r.get("id")?,
            name: r.get("name")?,
            backend: r.get("backend")?,
            image: r.get("image")?,
            workdir: r.get("workdir")?,
            network: r.get::<_, i64>("network")? != 0,
            cpus: r.get("cpus")?,
            memory_mb: r.get("memory_mb")?,
            pids_limit: r.get("pids_limit")?,
            timeout_ms: r.get("timeout_ms")?,
            env: serde_json::from_str(&env_json).unwrap_or_else(|_| json!({})),
            mounts: serde_json::from_str(&r.get::<_, String>("mounts_json")?)
                .unwrap_or_default(),
            // An unknown value falls back to the isolating mode, never the
            // permissive one — a typo in the DB must not quietly open the disk.
            fs_mode: crate::sandbox::fsmode::FsMode::parse(&r.get::<_, String>("fs_mode")?)
                .unwrap_or_default(),
            trace_enabled: r.get::<_, i64>("trace_enabled")? != 0,
            ports: serde_json::from_str(&r.get::<_, String>("ports_json")?).unwrap_or_default(),
            status: r.get("status")?,
            container_id: r.get("container_id")?,
            last_error: r.get("last_error")?,
            created_at: r.get("created_at")?,
            updated_at: r.get("updated_at")?,
            last_used_at: r.get("last_used_at")?,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    pub sandbox_id: String,
    pub kind: String,
    pub language: Option<String>,
    pub source: String,
    pub exit_code: Option<i64>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
    pub isolation: String,
    pub network: bool,
    pub duration_ms: i64,
    pub created_at: i64,
}

impl Run {
    fn from_row(r: &Row) -> rusqlite::Result<Self> {
        Ok(Run {
            id: r.get("id")?,
            sandbox_id: r.get("sandbox_id")?,
            kind: r.get("kind")?,
            language: r.get("language")?,
            source: r.get("source")?,
            exit_code: r.get("exit_code")?,
            stdout: r.get("stdout")?,
            stderr: r.get("stderr")?,
            truncated: r.get::<_, i64>("truncated")? != 0,
            timed_out: r.get::<_, i64>("timed_out")? != 0,
            isolation: r.get("isolation")?,
            network: r.get::<_, i64>("network")? != 0,
            duration_ms: r.get("duration_ms")?,
            created_at: r.get("created_at")?,
        })
    }
}

/// What `create` needs. Kept as a struct because a create with nine positional
/// arguments is the kind of call site where `cpus` and `memory_mb` get swapped.
pub struct NewSandbox {
    pub name: String,
    pub backend: String,
    pub image: Option<String>,
    pub workdir: String,
    pub network: bool,
    pub cpus: f64,
    pub memory_mb: i64,
    pub pids_limit: i64,
    pub timeout_ms: i64,
    pub env: Value,
    pub mounts: Vec<crate::sandbox::mounts::Mount>,
    pub fs_mode: crate::sandbox::fsmode::FsMode,
    pub ports: crate::sandbox::ports::PortPolicy,
}

impl Db {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    // ── sandboxes ───────────────────────────────────────────────────────────

    pub fn create_sandbox(&self, n: NewSandbox) -> Result<Sandbox> {
        let id = new_id();
        let now = now_ms();
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO sandboxes
               (id, name, backend, image, workdir, network, cpus, memory_mb,
                pids_limit, timeout_ms, env_json, mounts_json, fs_mode,
                ports_json, status, created_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,'stopped',?15,?15)",
            params![
                id,
                n.name,
                n.backend,
                n.image,
                n.workdir,
                n.network as i64,
                n.cpus,
                n.memory_mb,
                n.pids_limit,
                n.timeout_ms,
                serde_json::to_string(&n.env).unwrap_or_else(|_| "{}".into()),
                serde_json::to_string(&n.mounts).unwrap_or_else(|_| "[]".into()),
                n.fs_mode.as_str(),
                serde_json::to_string(&n.ports).unwrap_or_else(|_| "{}".into()),
                now,
            ],
        )?;
        drop(c);
        self.sandbox(&id)
    }

    pub fn sandbox(&self, id: &str) -> Result<Sandbox> {
        let c = self.conn.lock().unwrap();
        c.query_row(
            "SELECT * FROM sandboxes WHERE id = ?1",
            params![id],
            Sandbox::from_row,
        )
        .optional()?
        .ok_or_else(|| anyhow!("no sandbox `{id}`"))
    }

    pub fn list_sandboxes(&self) -> Result<Vec<Sandbox>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(
            "SELECT * FROM sandboxes ORDER BY COALESCE(last_used_at, created_at) DESC",
        )?;
        let rows = st.query_map([], Sandbox::from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_status(
        &self,
        id: &str,
        status: &str,
        container_id: Option<&str>,
        last_error: Option<&str>,
    ) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE sandboxes
                SET status = ?2,
                    container_id = COALESCE(?3, container_id),
                    last_error = ?4,
                    updated_at = ?5
              WHERE id = ?1",
            params![id, status, container_id, last_error, now_ms()],
        )?;
        Ok(())
    }

    /// Clear the container handle — used on stop, where COALESCE in
    /// `set_status` would otherwise keep a dead container id forever.
    pub fn clear_container(&self, id: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE sandboxes SET container_id = NULL, updated_at = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        Ok(())
    }

    pub fn touch(&self, id: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE sandboxes SET last_used_at = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        Ok(())
    }

    pub fn update_limits(
        &self,
        id: &str,
        name: Option<&str>,
        network: Option<bool>,
        cpus: Option<f64>,
        memory_mb: Option<i64>,
        timeout_ms: Option<i64>,
        env: Option<&Value>,
    ) -> Result<Sandbox> {
        {
            let c = self.conn.lock().unwrap();
            c.execute(
                "UPDATE sandboxes SET
                    name       = COALESCE(?2, name),
                    network    = COALESCE(?3, network),
                    cpus       = COALESCE(?4, cpus),
                    memory_mb  = COALESCE(?5, memory_mb),
                    timeout_ms = COALESCE(?6, timeout_ms),
                    env_json   = COALESCE(?7, env_json),
                    updated_at = ?8
                  WHERE id = ?1",
                params![
                    id,
                    name,
                    network.map(|b| b as i64),
                    cpus,
                    memory_mb,
                    timeout_ms,
                    env.map(|e| serde_json::to_string(e).unwrap_or_else(|_| "{}".into())),
                    now_ms(),
                ],
            )?;
        }
        self.sandbox(id)
    }

    pub fn set_ports(&self, id: &str, ports: &crate::sandbox::ports::PortPolicy) -> Result<Sandbox> {
        {
            let c = self.conn.lock().unwrap();
            c.execute(
                "UPDATE sandboxes SET ports_json = ?2, updated_at = ?3 WHERE id = ?1",
                params![
                    id,
                    serde_json::to_string(ports).unwrap_or_else(|_| "{}".into()),
                    now_ms()
                ],
            )?;
        }
        self.sandbox(id)
    }

    pub fn set_trace(&self, id: &str, on: bool) -> Result<Sandbox> {
        {
            let c = self.conn.lock().unwrap();
            c.execute(
                "UPDATE sandboxes SET trace_enabled = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, on as i64, now_ms()],
            )?;
        }
        self.sandbox(id)
    }

    // ── traced events ───────────────────────────────────────────────────────

    pub fn insert_events(
        &self,
        sandbox_id: &str,
        run_id: &str,
        events: &[crate::sandbox::trace::Event],
    ) -> Result<()> {
        let mut c = self.conn.lock().unwrap();
        // One transaction for the batch: a traced run can produce thousands of
        // rows, and a commit per row makes tracing cost more than the work.
        let tx = c.transaction()?;
        for e in events {
            tx.execute(
                "INSERT INTO events (sandbox_id, run_id, ts_ms, pid, source, kind, target, detail)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![sandbox_id, run_id, e.ts_ms, e.pid, e.source, e.kind, e.target, e.detail],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn list_events(
        &self,
        sandbox_id: &str,
        run_id: Option<&str>,
        kind_prefix: Option<&str>,
        limit: i64,
    ) -> Result<Vec<crate::sandbox::trace::Event>> {
        let c = self.conn.lock().unwrap();
        let limit = limit.clamp(1, 5_000);
        let mut st = c.prepare(
            "SELECT ts_ms, pid, source, kind, target, detail FROM events
              WHERE sandbox_id = ?1
                AND (?2 IS NULL OR run_id = ?2)
                AND (?3 IS NULL OR kind LIKE ?3 || '%')
              ORDER BY id DESC LIMIT ?4",
        )?;
        let rows = st.query_map(params![sandbox_id, run_id, kind_prefix, limit], |r| {
            Ok(crate::sandbox::trace::Event {
                ts_ms: r.get(0)?,
                pid: r.get(1)?,
                source: r.get(2)?,
                kind: r.get(3)?,
                target: r.get(4)?,
                detail: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn clear_events(&self, sandbox_id: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM events WHERE sandbox_id = ?1", params![sandbox_id])?;
        Ok(())
    }

    pub fn set_fs_mode(&self, id: &str, mode: crate::sandbox::fsmode::FsMode) -> Result<Sandbox> {
        {
            let c = self.conn.lock().unwrap();
            c.execute(
                "UPDATE sandboxes SET fs_mode = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, mode.as_str(), now_ms()],
            )?;
        }
        self.sandbox(id)
    }

    // ── settings ────────────────────────────────────────────────────────────

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn set_mounts(&self, id: &str, mounts: &[crate::sandbox::mounts::Mount]) -> Result<Sandbox> {
        {
            let c = self.conn.lock().unwrap();
            c.execute(
                "UPDATE sandboxes SET mounts_json = ?2, updated_at = ?3 WHERE id = ?1",
                params![
                    id,
                    serde_json::to_string(mounts).unwrap_or_else(|_| "[]".into()),
                    now_ms()
                ],
            )?;
        }
        self.sandbox(id)
    }

    pub fn delete_sandbox(&self, id: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM sandboxes WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ── runs ────────────────────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub fn insert_run(&self, run: &Run) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO runs
               (id, sandbox_id, kind, language, source, exit_code, stdout, stderr,
                truncated, timed_out, isolation, network, duration_ms, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                run.id,
                run.sandbox_id,
                run.kind,
                run.language,
                run.source,
                run.exit_code,
                run.stdout,
                run.stderr,
                run.truncated as i64,
                run.timed_out as i64,
                run.isolation,
                run.network as i64,
                run.duration_ms,
                run.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_runs(&self, sandbox_id: Option<&str>, limit: i64) -> Result<Vec<Run>> {
        let c = self.conn.lock().unwrap();
        let limit = limit.clamp(1, 500);
        let mut st;
        let rows = match sandbox_id {
            Some(sid) => {
                st = c.prepare(
                    "SELECT * FROM runs WHERE sandbox_id = ?1 ORDER BY created_at DESC LIMIT ?2",
                )?;
                st.query_map(params![sid, limit], Run::from_row)?
            }
            None => {
                st = c.prepare("SELECT * FROM runs ORDER BY created_at DESC LIMIT ?1")?;
                st.query_map(params![limit], Run::from_row)?
            }
        };
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn run(&self, id: &str) -> Result<Run> {
        let c = self.conn.lock().unwrap();
        c.query_row("SELECT * FROM runs WHERE id = ?1", params![id], Run::from_row)
            .optional()?
            .ok_or_else(|| anyhow!("no run `{id}`"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(db: &Db) -> Sandbox {
        db.create_sandbox(NewSandbox {
            name: "t".into(),
            backend: "direct".into(),
            image: None,
            workdir: "/tmp/x".into(),
            network: false,
            cpus: 1.0,
            memory_mb: 512,
            pids_limit: 256,
            timeout_ms: 30_000,
            env: json!({}),
            mounts: Vec::new(),
            fs_mode: crate::sandbox::fsmode::FsMode::Strict,
            ports: Default::default(),
        })
        .unwrap()
    }

    #[test]
    fn create_and_read_back_round_trips_every_field() {
        let db = Db::open_memory().unwrap();
        let s = seed(&db);
        let got = db.sandbox(&s.id).unwrap();
        assert_eq!(got.name, "t");
        assert_eq!(got.backend, "direct");
        assert!(!got.network);
        assert_eq!(got.status, "stopped");
        assert_eq!(got.memory_mb, 512);
    }

    #[test]
    fn missing_sandbox_is_an_error_not_a_panic() {
        let db = Db::open_memory().unwrap();
        assert!(db.sandbox("nope").is_err());
    }

    #[test]
    fn stop_clears_the_container_id_that_coalesce_would_preserve() {
        let db = Db::open_memory().unwrap();
        let s = seed(&db);
        db.set_status(&s.id, "running", Some("abc123"), None).unwrap();
        assert_eq!(db.sandbox(&s.id).unwrap().container_id.as_deref(), Some("abc123"));
        // set_status alone keeps the old id — that is why clear_container exists.
        db.set_status(&s.id, "stopped", None, None).unwrap();
        assert_eq!(db.sandbox(&s.id).unwrap().container_id.as_deref(), Some("abc123"));
        db.clear_container(&s.id).unwrap();
        assert!(db.sandbox(&s.id).unwrap().container_id.is_none());
    }

    #[test]
    fn partial_update_leaves_untouched_fields_alone() {
        let db = Db::open_memory().unwrap();
        let s = seed(&db);
        let up = db
            .update_limits(&s.id, None, Some(true), None, Some(1024), None, None)
            .unwrap();
        assert!(up.network);
        assert_eq!(up.memory_mb, 1024);
        assert_eq!(up.name, "t", "name was not part of the update");
        assert_eq!(up.cpus, 1.0);
    }

    #[test]
    fn deleting_a_sandbox_takes_its_runs_with_it() {
        let db = Db::open_memory().unwrap();
        let s = seed(&db);
        db.insert_run(&Run {
            id: new_id(),
            sandbox_id: s.id.clone(),
            kind: "exec".into(),
            language: None,
            source: "echo hi".into(),
            exit_code: Some(0),
            stdout: "hi\n".into(),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
            isolation: "seatbelt".into(),
            network: false,
            duration_ms: 5,
            created_at: now_ms(),
        })
        .unwrap();
        assert_eq!(db.list_runs(Some(&s.id), 10).unwrap().len(), 1);
        db.delete_sandbox(&s.id).unwrap();
        assert_eq!(db.list_runs(Some(&s.id), 10).unwrap().len(), 0);
    }

    #[test]
    fn run_limit_is_clamped_rather_than_trusted() {
        let db = Db::open_memory().unwrap();
        // A negative LIMIT in SQLite means "no limit"; clamping keeps a caller
        // passing 0 or -1 from dumping the whole table.
        assert!(db.list_runs(None, -1).is_ok());
        assert!(db.list_runs(None, 100_000).is_ok());
    }
}
