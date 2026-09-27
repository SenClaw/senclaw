//! Code artifact persistence — CRUD for the `code_artifacts` table.
//!
//! An artifact is a saved, reusable code snippet (JavaScript / TypeScript /
//! Bash) "published" from the Code executor REPL so it can be browsed, re-run,
//! and shared across agents.

use anyhow::Result;
use rusqlite::{params, Row};
use serde::{Deserialize, Serialize};

use super::Db;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeArtifact {
    pub id: String,
    pub name: String,
    /// `javascript` | `typescript` | `bash`
    pub language: String,
    pub code: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn row_to_artifact(r: &Row<'_>) -> rusqlite::Result<CodeArtifact> {
    let tags_json: String = r.get("tags_json")?;
    Ok(CodeArtifact {
        id: r.get("id")?,
        name: r.get("name")?,
        language: r.get("language")?,
        code: r.get("code")?,
        description: r.get("description")?,
        tags: serde_json::from_str(&tags_json).unwrap_or_default(),
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

impl Db {
    pub fn insert_code_artifact(&self, a: &CodeArtifact) -> Result<()> {
        let tags_json = serde_json::to_string(&a.tags).unwrap_or_else(|_| "[]".into());
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO code_artifacts \
                   (id, name, language, code, description, tags_json, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    a.id,
                    a.name,
                    a.language,
                    a.code,
                    a.description,
                    tags_json,
                    a.created_at,
                    a.updated_at
                ],
            )?;
            Ok(())
        })
    }

    pub fn list_code_artifacts(&self) -> Result<Vec<CodeArtifact>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, language, code, description, tags_json, created_at, updated_at \
                 FROM code_artifacts ORDER BY updated_at DESC",
            )?;
            let rows = stmt
                .query_map([], row_to_artifact)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn get_code_artifact(&self, id: &str) -> Result<Option<CodeArtifact>> {
        self.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT id, name, language, code, description, tags_json, created_at, updated_at \
                     FROM code_artifacts WHERE id = ?1",
                    params![id],
                    row_to_artifact,
                )
                .ok();
            Ok(row)
        })
    }

    pub fn update_code_artifact(
        &self,
        id: &str,
        name: &str,
        language: &str,
        code: &str,
        description: &str,
        tags: &[String],
        updated_at: &str,
    ) -> Result<bool> {
        let tags_json = serde_json::to_string(tags).unwrap_or_else(|_| "[]".into());
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE code_artifacts \
                 SET name = ?1, language = ?2, code = ?3, description = ?4, tags_json = ?5, updated_at = ?6 \
                 WHERE id = ?7",
                params![name, language, code, description, tags_json, updated_at, id],
            )?;
            Ok(n > 0)
        })
    }

    pub fn delete_code_artifact(&self, id: &str) -> Result<bool> {
        self.with_conn(|c| {
            let n = c.execute("DELETE FROM code_artifacts WHERE id = ?1", params![id])?;
            Ok(n > 0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_db() -> Db {
        let mut cfg = Config::from_env();
        let dir = std::env::temp_dir().join(format!("artifact-db-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        cfg.paths.db_path = dir.join("test.db");
        // `Db::open` opens TWO files and `cognitive_db_path` is configured
        // independently — leaving it alone points this test at the developer's
        // real ~/.senclaw/senclaw_cognitive.db and runs schema migrations on it.
        cfg.paths.cognitive_db_path = dir.join("test_cognitive.db");
        Db::open(&cfg).unwrap()
    }

    #[test]
    fn crud_roundtrip() {
        let db = test_db();
        let a = CodeArtifact {
            id: "a1".into(),
            name: "sum".into(),
            language: "bash".into(),
            code: "echo $((1+2))".into(),
            description: "adds".into(),
            tags: vec!["math".into()],
            created_at: "2026-01-01 00:00:00".into(),
            updated_at: "2026-01-01 00:00:00".into(),
        };
        db.insert_code_artifact(&a).unwrap();

        let list = db.list_code_artifacts().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].tags, vec!["math".to_string()]);

        let got = db.get_code_artifact("a1").unwrap().unwrap();
        assert_eq!(got.code, "echo $((1+2))");

        assert!(db
            .update_code_artifact(
                "a1",
                "sum2",
                "bash",
                "echo hi",
                "",
                &[],
                "2026-01-02 00:00:00"
            )
            .unwrap());
        assert_eq!(db.get_code_artifact("a1").unwrap().unwrap().name, "sum2");

        assert!(db.delete_code_artifact("a1").unwrap());
        assert!(db.get_code_artifact("a1").unwrap().is_none());
    }
}
