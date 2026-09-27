//! Persistence for the pre-turn skill router's log (`skill_route_log`): per
//! turn, the legacy matcher's decision next to the router's, so shadow mode
//! can be judged on real requests before the router is switched on.

use anyhow::Result;
use rusqlite::params;
use serde::Serialize;

use crate::decision::skill_route::RouteReport;

const MAX_ROWS: i64 = 5_000;
const MAX_PROMPT_CHARS: usize = 300;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillRouteRow {
    pub id: i64,
    pub at: i64,
    pub chat_jid: String,
    pub prompt: String,
    pub mode: String,
    pub pre_trigger: bool,
    pub legacy_name: Option<String>,
    pub legacy_force: bool,
    pub route_name: Option<String>,
    pub route_force: bool,
    pub reason: String,
    pub engine_pick: Option<String>,
    pub engine_p: Option<f64>,
    pub model: Option<String>,
    pub latency_ms: Option<f64>,
    pub fallback: Option<String>,
    pub candidates: serde_json::Value,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillRouteStats {
    pub total: i64,
    /// Turns where the legacy matcher and the router did the same thing.
    pub same: i64,
    /// Skills the legacy matcher loaded outright, and the router did.
    pub legacy_loads: i64,
    pub route_loads: i64,
    /// Legacy loaded a skill that the router only hinted (or dropped).
    pub loads_withheld: i64,
    /// The engine gave no answer; the keyword rule decided.
    pub fallbacks: i64,
}

impl super::Db {
    pub fn insert_skill_route_log(&self, chat_jid: &str, prompt: &str, mode: &str, pre_trigger: bool, r: &RouteReport) -> Result<i64> {
        let prompt: String = prompt.chars().take(MAX_PROMPT_CHARS).collect();
        let candidates = serde_json::to_string(&r.candidates).unwrap_or_else(|_| "[]".into());
        let at = chrono::Utc::now().timestamp_millis();
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO skill_route_log \
                   (at, chat_jid, prompt, mode, pre_trigger, legacy_name, legacy_force, route_name, route_force, \
                    reason, engine_pick, engine_p, model, latency_ms, fallback, candidates) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    at,
                    chat_jid,
                    prompt,
                    mode,
                    pre_trigger as i64,
                    r.legacy.as_ref().map(|l| l.name.as_str()),
                    r.legacy.as_ref().is_some_and(|l| l.force) as i64,
                    r.route.as_ref().map(|l| l.name.as_str()),
                    r.route.as_ref().is_some_and(|l| l.force) as i64,
                    r.reason,
                    r.engine_pick,
                    r.engine_p,
                    r.model,
                    r.latency_ms,
                    r.fallback,
                    candidates
                ],
            )?;
            let id = c.last_insert_rowid();
            c.execute(
                "DELETE FROM skill_route_log WHERE id <= (SELECT MAX(id) FROM skill_route_log) - ?1",
                params![MAX_ROWS],
            )?;
            Ok(id)
        })
    }

    pub fn list_skill_route_log(&self, limit: usize) -> Result<Vec<SkillRouteRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, at, chat_jid, prompt, mode, pre_trigger, legacy_name, legacy_force, route_name, route_force, \
                        reason, engine_pick, engine_p, model, latency_ms, fallback, candidates \
                 FROM skill_route_log ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = stmt
                .query_map(params![limit as i64], |r| {
                    let candidates: String = r.get(16)?;
                    Ok(SkillRouteRow {
                        id: r.get(0)?,
                        at: r.get(1)?,
                        chat_jid: r.get(2)?,
                        prompt: r.get(3)?,
                        mode: r.get(4)?,
                        pre_trigger: r.get::<_, i64>(5)? != 0,
                        legacy_name: r.get(6)?,
                        legacy_force: r.get::<_, i64>(7)? != 0,
                        route_name: r.get(8)?,
                        route_force: r.get::<_, i64>(9)? != 0,
                        reason: r.get(10)?,
                        engine_pick: r.get(11)?,
                        engine_p: r.get(12)?,
                        model: r.get(13)?,
                        latency_ms: r.get(14)?,
                        fallback: r.get(15)?,
                        candidates: serde_json::from_str(&candidates).unwrap_or(serde_json::Value::Null),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    pub fn skill_route_stats(&self) -> Result<SkillRouteStats> {
        self.with_conn(|c| {
            let stats = c.query_row(
                "SELECT COUNT(*), \
                   COALESCE(SUM(COALESCE(legacy_name, '') = COALESCE(route_name, '') AND legacy_force = route_force), 0), \
                   COALESCE(SUM(legacy_force), 0), COALESCE(SUM(route_force), 0), \
                   COALESCE(SUM(legacy_force = 1 AND route_force = 0), 0), \
                   COALESCE(SUM(fallback IS NOT NULL), 0) \
                 FROM skill_route_log",
                [],
                |r| {
                    Ok(SkillRouteStats {
                        total: r.get(0)?,
                        same: r.get(1)?,
                        legacy_loads: r.get(2)?,
                        route_loads: r.get(3)?,
                        loads_withheld: r.get(4)?,
                        fallbacks: r.get(5)?,
                    })
                },
            )?;
            Ok(stats)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::matching::{Scored, SkillRoute};

    fn report(legacy: Option<(&str, bool)>, route: Option<(&str, bool)>, fallback: bool) -> RouteReport {
        let r = |x: Option<(&str, bool)>| x.map(|(n, f)| SkillRoute { name: n.into(), force: f });
        RouteReport {
            legacy: r(legacy),
            route: r(route),
            reason: "test".into(),
            candidates: vec![Scored { name: "weather-xem".into(), score: 15, phrase_hit: false }],
            engine_pick: Some("x-browse".into()),
            engine_p: Some(0.9),
            model: Some("multilingual".into()),
            latency_ms: Some(70.0),
            fallback: fallback.then(|| "timeout".into()),
        }
    }

    #[test]
    fn the_log_counts_withheld_loads_and_agreement() {
        let config = crate::config::Config::from_env();
        let db = super::super::Db::open_in_memory(&config).unwrap();
        db.insert_skill_route_log("web:a", "chào bạn", "shadow", true, &report(Some(("weather-xem", true)), Some(("weather-xem", false)), false)).unwrap();
        db.insert_skill_route_log("web:a", "hẹn giờ 10 phút", "shadow", true, &report(Some(("clock-timer", true)), Some(("clock-timer", true)), false)).unwrap();
        db.insert_skill_route_log("web:a", &"x".repeat(500), "on", true, &report(None, None, true)).unwrap();
        let s = db.skill_route_stats().unwrap();
        assert_eq!((s.total, s.same, s.legacy_loads, s.route_loads, s.loads_withheld, s.fallbacks), (3, 2, 2, 1, 1, 1));
        let rows = db.list_skill_route_log(10).unwrap();
        assert_eq!(rows[0].prompt.chars().count(), MAX_PROMPT_CHARS);
        assert_eq!(rows[2].candidates[0]["phraseHit"], false);
    }
}
