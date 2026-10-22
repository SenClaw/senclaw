//! The apps installed on this machine, as the sync service reaches them.
//!
//! A call to an app goes through the daemon's own app proxy
//! (`/api/space/apps/<id>/proxy/…`), the way the app's UI and its MCP tools do:
//! the proxy starts the app if it is stopped, health-gates it, and stamps the
//! app's access token on the request. Nothing here knows an app's port or token.
//! The one exception is [`DaemonApps::answers_without_token`], which asks the
//! app's own port a question the proxy would answer for it.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::params;
use serde_json::Value;

use super::{valid_app_id, AppSyncError, LocalApp, LocalApps};
use crate::apps::manifest::SyncDecl;
use crate::db::Db;

/// Longer than a started app needs to answer, shorter than the peer's own wait
/// for this daemon (`lan::client` gives a call 30 s).
const APP_CALL_TIMEOUT: Duration = Duration::from_secs(25);

pub struct DaemonApps {
    db: Arc<Db>,
    base_url: String,
    http: reqwest::Client,
}

impl DaemonApps {
    pub fn new(db: Arc<Db>) -> Self {
        Self::at(db, crate::util::internal_auth::daemon_base_url())
    }

    /// Reach the daemon's app proxy at `base_url` instead of this daemon's own port.
    pub fn at(db: Arc<Db>, base_url: String) -> Self {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(APP_CALL_TIMEOUT)
            .build()
            .expect("the loopback HTTP client builds (rustls backend)");
        Self { db, base_url, http }
    }

    fn row(&self, app: &str) -> Option<(Value, bool)> {
        self.db
            .with_conn(|conn| {
                let r: Result<(String, i64), rusqlite::Error> =
                    conn.query_row("SELECT manifest, COALESCE(enabled, 1) FROM space_apps WHERE id=?1", params![app], |row| Ok((row.get(0)?, row.get(1)?)));
                Ok(r.ok().and_then(|(m, e)| serde_json::from_str::<Value>(&m).ok().map(|m| (m, e != 0))))
            })
            .ok()
            .flatten()
    }

    fn app_from(id: &str, manifest: &Value) -> Result<LocalApp, AppSyncError> {
        match SyncDecl::parse(manifest) {
            Ok(Some(decl)) => Ok(LocalApp { id: id.to_string(), version: manifest["hub"]["version"].as_str().map(str::to_string), decl }),
            Ok(None) => Err(AppSyncError::AppUnavailable(format!("`{id}` does not declare that it can sync"))),
            Err(e) => Err(AppSyncError::AppUnavailable(format!("`{id}`: {e}"))),
        }
    }
}

#[async_trait]
impl LocalApps for DaemonApps {
    fn lookup(&self, app: &str) -> Result<LocalApp, AppSyncError> {
        if !valid_app_id(app) {
            return Err(AppSyncError::Invalid("not an app id".into()));
        }
        let (manifest, enabled) = self.row(app).ok_or_else(|| AppSyncError::AppUnavailable(format!("`{app}` is not installed on this machine")))?;
        if !enabled {
            return Err(AppSyncError::AppUnavailable(format!("`{app}` is switched off on this machine")));
        }
        Self::app_from(app, &manifest)
    }

    fn syncable(&self) -> Vec<LocalApp> {
        let rows: Vec<(String, String)> = self
            .db
            .with_conn(|conn| {
                let mut st = conn.prepare("SELECT id, manifest FROM space_apps WHERE COALESCE(enabled, 1) = 1 ORDER BY id")?;
                let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.filter_map(Result::ok).collect();
                Ok(rows)
            })
            .unwrap_or_default();
        rows.into_iter()
            .filter(|(id, _)| valid_app_id(id))
            .filter_map(|(id, m)| serde_json::from_str::<Value>(&m).ok().and_then(|m| Self::app_from(&id, &m).ok()))
            .collect()
    }

    async fn call(&self, app: &LocalApp, method: &str, path: &str, body: Value) -> Result<Value, AppSyncError> {
        let url = format!("{}/api/space/apps/{}/proxy{}{}", self.base_url, app.id, app.decl.endpoint, path);
        let mut req = if method == "GET" { self.http.get(&url) } else { self.http.post(&url).json(&body) };
        if let Some((name, value)) = crate::util::internal_auth::header_for(&url) {
            req = req.header(name, value);
        }
        let resp = req.send().await.map_err(|e| AppSyncError::AppUnavailable(format!("`{}` could not be reached: {e}", app.id)))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            return serde_json::from_str(&text).map_err(|_| AppSyncError::Internal(format!("`{}` answered something that is not JSON", app.id)));
        }
        let snippet: String = text.chars().take(200).collect();
        Err(match status.as_u16() {
            401 | 403 => AppSyncError::AppUnavailable(format!("`{}` refused this SenClaw ({status}); restart the app so it picks up its token", app.id)),
            404 => AppSyncError::AppUnavailable(format!("`{}` has no sync routes at {}; update the app", app.id, app.decl.endpoint)),
            502 | 503 | 504 => AppSyncError::AppUnavailable(format!("`{}` is not running: {snippet}", app.id)),
            400 | 413 | 422 => AppSyncError::Invalid(format!("`{}` refused the request: {snippet}", app.id)),
            _ => AppSyncError::Internal(format!("`{}` answered {status}: {snippet}", app.id)),
        })
    }

    async fn answers_without_token(&self, app: &LocalApp) -> Result<bool, AppSyncError> {
        let port = self
            .row(&app.id)
            .and_then(|(m, _)| m["runtime"]["port"].as_u64())
            .filter(|p| *p > 0 && *p < 65536)
            .ok_or_else(|| AppSyncError::AppUnavailable(format!("`{}` has no recorded port to check", app.id)))?;
        let url = format!("http://127.0.0.1:{port}{}/schema", app.decl.endpoint);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| AppSyncError::AppUnavailable(format!("`{}` could not be checked on its own port: {e}", app.id)))?;
        match resp.status().as_u16() {
            200..=299 => Ok(true),
            401 | 403 => Ok(false),
            other => Err(AppSyncError::AppUnavailable(format!("`{}` answered {other} on its own port, so its token guard cannot be confirmed", app.id))),
        }
    }
}
