//! Persistence for plans produced by `ExitPlanMode`. We capture the
//! markdown content at the moment the agent requests user approval so
//! the plan is queryable as session history even if the user never
//! actually clicks approve.

use anyhow::Result;
use rusqlite::{params, OptionalExtension};

#[derive(Debug, Clone)]
pub struct StoredPlan {
    pub id: String,
    pub chat_jid: String,
    pub agent_id: String,
    pub title: String,
    pub file_path: String,
    pub content_md: String,
    /// One of `pending` (default), `startEditing`, `clearContextAndStart`.
    pub approval: String,
    pub created_at: String,
    pub approved_at: Option<String>,
}

impl super::Db {
    pub fn insert_plan(&self, plan: &StoredPlan) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                r#"
                INSERT INTO plans
                  (id, chat_jid, agent_id, title, file_path, content_md,
                   approval, created_at, approved_at)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
                "#,
                params![
                    plan.id,
                    plan.chat_jid,
                    plan.agent_id,
                    plan.title,
                    plan.file_path,
                    plan.content_md,
                    plan.approval,
                    plan.created_at,
                    plan.approved_at,
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_plan(&self, id: &str) -> Result<Option<StoredPlan>> {
        self.with_conn(|c| {
            Ok(c.query_row(
                "SELECT id, chat_jid, agent_id, title, file_path, content_md,
                        approval, created_at, approved_at
                 FROM plans WHERE id = ?1",
                params![id],
                |r| {
                    Ok(StoredPlan {
                        id: r.get(0)?,
                        chat_jid: r.get(1)?,
                        agent_id: r.get(2)?,
                        title: r.get(3)?,
                        file_path: r.get(4)?,
                        content_md: r.get(5)?,
                        approval: r.get(6)?,
                        created_at: r.get(7)?,
                        approved_at: r.get(8)?,
                    })
                },
            )
            .optional()?)
        })
    }

    pub fn list_plans_for_chat(
        &self,
        chat_jid: &str,
        limit: Option<u32>,
    ) -> Result<Vec<StoredPlan>> {
        self.with_conn(|c| {
            let lim = limit.unwrap_or(200) as i64;
            let mut stmt = c.prepare(
                "SELECT id, chat_jid, agent_id, title, file_path, content_md,
                        approval, created_at, approved_at
                 FROM plans
                 WHERE chat_jid = ?1
                 ORDER BY created_at DESC, id DESC
                 LIMIT ?2",
            )?;
            let rows: Vec<StoredPlan> = stmt
                .query_map(params![chat_jid, lim], |r| {
                    Ok(StoredPlan {
                        id: r.get(0)?,
                        chat_jid: r.get(1)?,
                        agent_id: r.get(2)?,
                        title: r.get(3)?,
                        file_path: r.get(4)?,
                        content_md: r.get(5)?,
                        approval: r.get(6)?,
                        created_at: r.get(7)?,
                        approved_at: r.get(8)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn update_plan_approval(
        &self,
        id: &str,
        approval: &str,
        approved_at: &str,
    ) -> Result<usize> {
        self.with_conn(|c| {
            Ok(c.execute(
                "UPDATE plans SET approval = ?2, approved_at = ?3 WHERE id = ?1",
                params![id, approval, approved_at],
            )?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::StoredPlan;
    use crate::config::Config;
    use crate::db::Db;

    fn make(id: &str, jid: &str, created_at: &str) -> StoredPlan {
        StoredPlan {
            id: id.into(),
            chat_jid: jid.into(),
            agent_id: "main".into(),
            title: "test plan".into(),
            file_path: format!("/tmp/{id}.md"),
            content_md: "# Plan\n\n- step 1".into(),
            approval: "pending".into(),
            created_at: created_at.into(),
            approved_at: None,
        }
    }

    #[test]
    fn insert_get_list_order_and_isolation() {
        let cfg = Config::from_env();
        let db = Db::open_in_memory(&cfg).expect("open db");

        db.insert_plan(&make("p1", "tg:a", "2026-05-19T10:00:00Z"))
            .unwrap();
        db.insert_plan(&make("p2", "tg:a", "2026-05-19T10:00:01Z"))
            .unwrap();
        db.insert_plan(&make("p3", "tg:b", "2026-05-19T10:00:02Z"))
            .unwrap();

        let got = db.get_plan("p2").unwrap().unwrap();
        assert_eq!(got.chat_jid, "tg:a");

        // Per-chat list, DESC by created_at
        let a = db.list_plans_for_chat("tg:a", None).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].id, "p2");
        assert_eq!(a[1].id, "p1");

        let b = db.list_plans_for_chat("tg:b", None).unwrap();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].id, "p3");

        // Update approval
        let n = db
            .update_plan_approval("p1", "startEditing", "2026-05-19T10:01:00Z")
            .unwrap();
        assert_eq!(n, 1);
        let p1 = db.get_plan("p1").unwrap().unwrap();
        assert_eq!(p1.approval, "startEditing");
        assert_eq!(p1.approved_at.as_deref(), Some("2026-05-19T10:01:00Z"));
    }
}
