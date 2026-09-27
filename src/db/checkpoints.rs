//! `chat_checkpoints` — one row per shadow-git commit made after a tool wrote
//! into a chat's working directory. The row is the link between "the step in
//! the transcript" and "the commit that can restore it".

use anyhow::Result;
use rusqlite::{params, OptionalExtension};

use crate::types::ChatCheckpoint;

use super::rows::row_to_checkpoint;

impl super::Db {
    #[allow(clippy::too_many_arguments)]
    pub fn insert_checkpoint(
        &self,
        chat_jid: &str,
        sha: &str,
        parent_sha: Option<&str>,
        tool_name: &str,
        summary: &str,
        workspace: &str,
        files_changed: i64,
    ) -> Result<ChatCheckpoint> {
        let created_at = chrono::Utc::now().to_rfc3339();
        let id: i64 = self.with_conn(|c| {
            c.execute(
                "INSERT INTO chat_checkpoints
                   (chat_jid, sha, parent_sha, tool_name, summary, workspace, files_changed, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    chat_jid,
                    sha,
                    parent_sha,
                    tool_name,
                    summary,
                    workspace,
                    files_changed,
                    created_at
                ],
            )?;
            Ok(c.last_insert_rowid())
        })?;
        Ok(ChatCheckpoint {
            id,
            chat_jid: chat_jid.to_string(),
            sha: sha.to_string(),
            parent_sha: parent_sha.map(str::to_string),
            tool_name: tool_name.to_string(),
            summary: summary.to_string(),
            workspace: workspace.to_string(),
            files_changed,
            created_at,
        })
    }

    /// Newest first.
    pub fn list_checkpoints(&self, chat_jid: &str) -> Result<Vec<ChatCheckpoint>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM chat_checkpoints WHERE chat_jid = ?1 ORDER BY id DESC LIMIT 500",
            )?;
            let rows = stmt.query_map(params![chat_jid], |r| Ok(row_to_checkpoint(r)))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row??);
            }
            Ok(out)
        })
    }

    pub fn get_checkpoint(&self, id: i64) -> Result<Option<ChatCheckpoint>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT * FROM chat_checkpoints WHERE id = ?1",
                params![id],
                |r| Ok(row_to_checkpoint(r)),
            )
            .optional()?
            .transpose()
        })
    }

    pub fn delete_checkpoints_for_jid(&self, chat_jid: &str) -> Result<usize> {
        self.with_conn(|c| {
            Ok(c.execute(
                "DELETE FROM chat_checkpoints WHERE chat_jid = ?1",
                params![chat_jid],
            )?)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::db::Db;

    #[test]
    fn insert_list_get_round_trip() {
        let db = Db::open_in_memory(&Config::from_env()).unwrap();
        let a = db
            .insert_checkpoint("web:1", "aaa", None, "Edit", "Edit a.rs", "/tmp/x", 1)
            .unwrap();
        let b = db
            .insert_checkpoint("web:1", "bbb", Some("aaa"), "Bash", "cargo fmt", "/tmp/x", 3)
            .unwrap();
        db.insert_checkpoint("web:2", "ccc", None, "Write", "other chat", "/tmp/y", 1)
            .unwrap();

        let list = db.list_checkpoints("web:1").unwrap();
        assert_eq!(list.len(), 2, "other chats' rows must not leak");
        assert_eq!(list[0].id, b.id, "newest first");
        assert_eq!(list[1].id, a.id);
        assert_eq!(list[0].parent_sha.as_deref(), Some("aaa"));

        let got = db.get_checkpoint(a.id).unwrap().unwrap();
        assert_eq!(got.sha, "aaa");
        assert_eq!(got.files_changed, 1);
        assert!(db.get_checkpoint(9999).unwrap().is_none());

        assert_eq!(db.delete_checkpoints_for_jid("web:1").unwrap(), 2);
        assert!(db.list_checkpoints("web:1").unwrap().is_empty());
    }
}
