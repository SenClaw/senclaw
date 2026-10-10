//! App workers: installed Space Apps that execute dispatched work items for an
//! assignee they declare (`"workers"` in `senclaw-manifest.json`). The
//! dispatcher hands such items to the app over loopback instead of running a
//! SenClaw persona — see `app_space_sdk::worker` for the wire contract and
//! `crate::agent::mcp_dispatch::app_worker` for the client.

use serde_json::Value;

/// One `workers[]` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerDecl {
    /// Primary assignee name (`claude-code`).
    pub assignee: String,
    /// Other names routed to the same worker (`claude`).
    pub aliases: Vec<String>,
    /// Where the app mounts `app_space_sdk::worker::worker_router`.
    pub path: String,
}

impl WorkerDecl {
    pub fn matches(&self, assignee: &str) -> bool {
        let a = assignee.trim();
        self.assignee.eq_ignore_ascii_case(a) || self.aliases.iter().any(|x| x.eq_ignore_ascii_case(a))
    }

    /// The `workers` array of a manifest. Entries without an assignee are
    /// skipped; a missing `path` means `/api/worker`.
    pub fn parse_all(manifest: &Value) -> Vec<WorkerDecl> {
        manifest
            .get("workers")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|w| {
                        let assignee = w.get("assignee")?.as_str()?.trim().to_string();
                        if assignee.is_empty() {
                            return None;
                        }
                        let aliases = w
                            .get("aliases")
                            .and_then(Value::as_array)
                            .map(|x| x.iter().filter_map(|s| s.as_str().map(|s| s.trim().to_string())).collect())
                            .unwrap_or_default();
                        let path = w
                            .get("path")
                            .and_then(Value::as_str)
                            .map(|p| format!("/{}", p.trim().trim_matches('/')))
                            .unwrap_or_else(|| "/api/worker".into());
                        Some(WorkerDecl { assignee, aliases, path })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Where to send a job for one assignee.
#[derive(Debug, Clone)]
pub struct WorkerTarget {
    pub app_id: String,
    /// `http://127.0.0.1:<port><path>`.
    pub base_url: String,
    /// The app's access token: the app answers only requests carrying it.
    pub token: Option<String>,
    /// Whether the app runs in the background (supervised, always up). A
    /// session app would only start on its proxy, which the worker client
    /// does not go through — so the dispatcher refuses it with a reason.
    pub background: bool,
    pub port: u16,
}

/// Who handles an assignee.
#[derive(Debug, Clone)]
pub enum Resolved {
    /// No enabled app declares it.
    None,
    /// Exactly one does.
    One(WorkerTarget),
    /// Several do — ambiguous, so nobody gets the item (app ids listed).
    Conflict(Vec<String>),
}

/// The enabled app that declares `assignee` as a worker.
pub fn resolve(db: &crate::db::Db, assignee: &str) -> Resolved {
    let apps: Vec<(String, Value)> = db
        .with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT id, manifest FROM space_apps WHERE enabled = 1")?;
            let rows: Vec<(String, Value)> = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .filter_map(|r| r.ok())
                .filter_map(|(id, m)| serde_json::from_str::<Value>(&m).ok().map(|v| (id, v)))
                .collect();
            Ok(rows)
        })
        .unwrap_or_default();
    let mut found: Vec<(String, Value, WorkerDecl)> = apps
        .into_iter()
        .filter_map(|(id, m)| WorkerDecl::parse_all(&m).into_iter().find(|d| d.matches(assignee)).map(|d| (id, m, d)))
        .collect();
    if found.len() > 1 {
        let mut ids: Vec<String> = found.into_iter().map(|f| f.0).collect();
        ids.sort();
        return Resolved::Conflict(ids);
    }
    let Some((app_id, manifest, decl)) = found.pop() else {
        return Resolved::None;
    };
    let spec = crate::apps::RuntimeSpec::parse(&manifest);
    Resolved::One(WorkerTarget {
        token: crate::apps::token::load(db, &app_id).ok().flatten(),
        base_url: format!("http://127.0.0.1:{}{}", spec.port, decl.path),
        background: spec.mode.is_background(),
        port: spec.port,
        app_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn worker_declarations_parse_with_defaults() {
        let m = json!({ "workers": [
            { "assignee": "claude-code", "aliases": ["Claude"], "path": "api/worker/" },
            { "assignee": "codex" },
            { "aliases": ["nameless"] }
        ]});
        let w = WorkerDecl::parse_all(&m);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].path, "/api/worker");
        assert!(w[0].matches(" claude "));
        assert!(w[0].matches("CLAUDE-CODE"));
        assert!(!w[0].matches("codex"));
        assert_eq!(w[1].path, "/api/worker");
        assert!(WorkerDecl::parse_all(&json!({})).is_empty());
    }
}
