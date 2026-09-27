//! Persistence for tool auto-accept / deny rules. Today the rules live
//! in the in-memory [`crate::agent::permission_bridge::PermissionBridge`]
//! and were only echoed to the client's `localStorage`. We now persist
//! the canonical list in SQLite so the server is the source of truth and
//! the rules survive browser changes, daemon restarts, and multi-client
//! setups.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};

#[derive(Debug, Clone)]
pub struct StoredToolRule {
    pub id: String,
    /// Full serialised `ToolAutoAcceptRule` JSON. We keep it as a blob so
    /// new matcher / action variants don't require a schema migration.
    pub rule_json: String,
    pub updated_at: String,
}

impl super::Db {
    /// Insert or replace a rule by ID.
    pub fn upsert_tool_rule(&self, id: &str, rule_json: &str, updated_at: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                r#"
                INSERT INTO tool_rules (id, rule_json, updated_at)
                VALUES (?1, ?2, ?3)
                ON CONFLICT(id) DO UPDATE SET
                    rule_json = excluded.rule_json,
                    updated_at = excluded.updated_at
                "#,
                params![id, rule_json, updated_at],
            )?;
            Ok(())
        })
    }

    pub fn delete_tool_rule(&self, id: &str) -> Result<usize> {
        self.with_conn(|c| Ok(c.execute("DELETE FROM tool_rules WHERE id = ?1", params![id])?))
    }

    pub fn list_tool_rules(&self) -> Result<Vec<StoredToolRule>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, rule_json, updated_at FROM tool_rules ORDER BY updated_at ASC",
            )?;
            let rows: Vec<StoredToolRule> = stmt
                .query_map([], |r| {
                    Ok(StoredToolRule {
                        id: r.get(0)?,
                        rule_json: r.get(1)?,
                        updated_at: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn get_tool_rule(&self, id: &str) -> Result<Option<StoredToolRule>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT id, rule_json, updated_at FROM tool_rules WHERE id = ?1",
                params![id],
                |r| {
                    Ok(StoredToolRule {
                        id: r.get(0)?,
                        rule_json: r.get(1)?,
                        updated_at: r.get(2)?,
                    })
                },
            )
            .optional()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::db::Db;

    #[test]
    fn upsert_list_delete_round_trip() {
        let cfg = Config::from_env();
        let db = Db::open_in_memory(&cfg).expect("open db");

        db.upsert_tool_rule(
            "rule-a",
            r#"{"id":"rule-a","action":"auto_accept"}"#,
            "2026-05-19T10:00:00Z",
        )
        .unwrap();
        db.upsert_tool_rule(
            "rule-b",
            r#"{"id":"rule-b","action":"auto_deny"}"#,
            "2026-05-19T10:00:01Z",
        )
        .unwrap();

        let all = db.list_tool_rules().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "rule-a");
        assert_eq!(all[1].id, "rule-b");

        // Upsert overwrites
        db.upsert_tool_rule(
            "rule-a",
            r#"{"id":"rule-a","action":"auto_deny"}"#,
            "2026-05-19T10:00:02Z",
        )
        .unwrap();
        let got = db.get_tool_rule("rule-a").unwrap().unwrap();
        assert!(got.rule_json.contains(r#""action":"auto_deny""#));
        assert_eq!(got.updated_at, "2026-05-19T10:00:02Z");

        // Delete
        assert_eq!(db.delete_tool_rule("rule-a").unwrap(), 1);
        assert_eq!(db.list_tool_rules().unwrap().len(), 1);
        assert!(db.get_tool_rule("rule-a").unwrap().is_none());
    }
}
