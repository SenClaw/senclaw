//! Persistence for per-agent TODO snapshots so the Agent Console replays
//! the last-known list across daemon restarts. Mirrors the storage pattern
//! used by [`super::tool_executions`].

use anyhow::Result;
use rusqlite::{params, OptionalExtension};

#[derive(Debug, Clone)]
pub struct StoredAgentTodos {
    pub agent_jid: String,
    pub agent_name: String,
    pub todos_json: String,
    pub updated_at: String,
}

impl super::Db {
    /// Upsert the current todo list for an agent. `todos_json` is the
    /// already-serialised array — we just store it verbatim.
    pub fn upsert_agent_todos(
        &self,
        agent_jid: &str,
        agent_name: &str,
        todos_json: &str,
        updated_at: &str,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                r#"
                INSERT INTO agent_todos (agent_jid, agent_name, todos_json, updated_at)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(agent_jid) DO UPDATE SET
                    agent_name = excluded.agent_name,
                    todos_json = excluded.todos_json,
                    updated_at = excluded.updated_at
                "#,
                params![agent_jid, agent_name, todos_json, updated_at],
            )?;
            Ok(())
        })
    }

    pub fn get_agent_todos(&self, agent_jid: &str) -> Result<Option<StoredAgentTodos>> {
        self.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT agent_jid, agent_name, todos_json, updated_at
                     FROM agent_todos WHERE agent_jid = ?1",
                    params![agent_jid],
                    |r| {
                        Ok(StoredAgentTodos {
                            agent_jid: r.get(0)?,
                            agent_name: r.get(1)?,
                            todos_json: r.get(2)?,
                            updated_at: r.get(3)?,
                        })
                    },
                )
                .optional()?;
            Ok(row)
        })
    }

    pub fn get_all_agent_todos(&self) -> Result<Vec<StoredAgentTodos>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT agent_jid, agent_name, todos_json, updated_at
                 FROM agent_todos ORDER BY updated_at DESC",
            )?;
            let rows: Vec<StoredAgentTodos> = stmt
                .query_map([], |r| {
                    Ok(StoredAgentTodos {
                        agent_jid: r.get(0)?,
                        agent_name: r.get(1)?,
                        todos_json: r.get(2)?,
                        updated_at: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn delete_agent_todos(&self, agent_jid: &str) -> Result<usize> {
        self.with_conn(|c| {
            Ok(c.execute(
                "DELETE FROM agent_todos WHERE agent_jid = ?1",
                params![agent_jid],
            )?)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::db::Db;

    #[test]
    fn upsert_round_trip() {
        let cfg = Config::from_env();
        let db = Db::open_in_memory(&cfg).expect("open db");

        db.upsert_agent_todos(
            "agent:1",
            "Alpha",
            r#"[{"id":"t1","label":"x"}]"#,
            "2026-05-19T10:00:00Z",
        )
        .unwrap();
        let got = db.get_agent_todos("agent:1").unwrap().expect("row");
        assert_eq!(got.agent_jid, "agent:1");
        assert_eq!(got.agent_name, "Alpha");
        assert_eq!(got.todos_json, r#"[{"id":"t1","label":"x"}]"#);
    }

    #[test]
    fn upsert_overwrites_previous() {
        let cfg = Config::from_env();
        let db = Db::open_in_memory(&cfg).expect("open db");

        db.upsert_agent_todos("agent:1", "Alpha", "[]", "2026-05-19T10:00:00Z")
            .unwrap();
        db.upsert_agent_todos("agent:1", "Alpha-renamed", "[1,2]", "2026-05-19T10:00:05Z")
            .unwrap();

        let got = db.get_agent_todos("agent:1").unwrap().expect("row");
        assert_eq!(got.agent_name, "Alpha-renamed");
        assert_eq!(got.todos_json, "[1,2]");
        assert_eq!(got.updated_at, "2026-05-19T10:00:05Z");
    }

    #[test]
    fn get_all_returns_multiple_rows() {
        let cfg = Config::from_env();
        let db = Db::open_in_memory(&cfg).expect("open db");

        db.upsert_agent_todos("agent:1", "A", "[]", "2026-05-19T10:00:00Z")
            .unwrap();
        db.upsert_agent_todos("agent:2", "B", "[]", "2026-05-19T10:00:01Z")
            .unwrap();
        db.upsert_agent_todos("agent:3", "C", "[]", "2026-05-19T10:00:02Z")
            .unwrap();

        let all = db.get_all_agent_todos().unwrap();
        assert_eq!(all.len(), 3);
        // ORDER BY updated_at DESC
        assert_eq!(all[0].agent_jid, "agent:3");
        assert_eq!(all[2].agent_jid, "agent:1");

        // delete_agent_todos removes a row
        let n = db.delete_agent_todos("agent:2").unwrap();
        assert_eq!(n, 1);
        assert_eq!(db.get_all_agent_todos().unwrap().len(), 2);
    }
}
