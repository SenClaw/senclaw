//! Persistence for LLM token accounting (`llm_usage_log`, `llm_usage_daily`,
//! `model_pricing`). Written by [`crate::usage::UsageRecorder`]'s flush task;
//! read by the `/api/usage/*` endpoints, the hourly aggregator and the
//! background-run totals wiring.

use anyhow::Result;
use rusqlite::params;
use serde::Serialize;

use crate::usage::UsageEvent;

/// One `model_pricing` row. Prices are USD per 1M tokens; `None` cache
/// prices fall back to `input_per_1m`.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct ModelPricing {
    pub model: String,
    #[serde(rename = "inputPer1m")]
    pub input_per_1m: f64,
    #[serde(rename = "outputPer1m")]
    pub output_per_1m: f64,
    #[serde(rename = "cacheReadPer1m", default)]
    pub cache_read_per_1m: Option<f64>,
    #[serde(rename = "cacheWritePer1m", default)]
    pub cache_write_per_1m: Option<f64>,
}

impl ModelPricing {
    /// USD cost of the given token counts under this pricing row.
    pub fn cost(&self, input: u64, output: u64, cache_creation: u64, cache_read: u64) -> f64 {
        let cr = self.cache_read_per_1m.unwrap_or(self.input_per_1m);
        let cw = self.cache_write_per_1m.unwrap_or(self.input_per_1m);
        (input as f64 * self.input_per_1m
            + output as f64 * self.output_per_1m
            + cache_creation as f64 * cw
            + cache_read as f64 * cr)
            / 1_000_000.0
    }
}

/// Find the pricing row for a concrete model id: exact match first, then the
/// longest pricing key that is a prefix of `model` (so `claude-sonnet-4-5`
/// matches `claude-sonnet-4-5-20250929`). Returns `None` when unpriced —
/// callers must surface "n/a", never a fake $0.
pub fn match_pricing<'a>(pricing: &'a [ModelPricing], model: &str) -> Option<&'a ModelPricing> {
    if let Some(p) = pricing.iter().find(|p| p.model == model) {
        return Some(p);
    }
    pricing
        .iter()
        .filter(|p| model.starts_with(p.model.as_str()))
        .max_by_key(|p| p.model.len())
}

/// Aggregated token totals plus the cost of the priced share. `unpriced_tokens`
/// is the token volume excluded from `est_cost_usd` because no pricing row
/// matched its model (never silently priced at $0).
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct UsageTotals {
    pub calls: u64,
    #[serde(rename = "inputTokens")]
    pub input_tokens: u64,
    #[serde(rename = "outputTokens")]
    pub output_tokens: u64,
    #[serde(rename = "cacheCreationTokens")]
    pub cache_creation_tokens: u64,
    #[serde(rename = "cacheReadTokens")]
    pub cache_read_tokens: u64,
    #[serde(rename = "estCostUsd")]
    pub est_cost_usd: f64,
    #[serde(rename = "unpricedTokens")]
    pub unpriced_tokens: u64,
}

/// One breakdown row (`key` = model / source / jid / app_id).
#[derive(Debug, Clone, Serialize)]
pub struct UsageBreakdownRow {
    pub key: String,
    #[serde(flatten)]
    pub totals: UsageTotals,
}

/// One day of the rollup, summed across all dimensions for charting.
#[derive(Debug, Clone, Serialize)]
pub struct UsageDailyRow {
    pub date: String,
    #[serde(flatten)]
    pub totals: UsageTotals,
}

/// One raw `llm_usage_log` row for the debug log endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct UsageLogRow {
    pub id: i64,
    pub ts: i64,
    pub source: String,
    pub jid: String,
    #[serde(rename = "agentId")]
    pub agent_id: String,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "appId")]
    pub app_id: String,
    pub profile: String,
    pub provider: String,
    pub model: String,
    #[serde(rename = "inputTokens")]
    pub input_tokens: u64,
    #[serde(rename = "outputTokens")]
    pub output_tokens: u64,
    #[serde(rename = "cacheCreationTokens")]
    pub cache_creation_tokens: u64,
    #[serde(rename = "cacheReadTokens")]
    pub cache_read_tokens: u64,
    #[serde(rename = "latencyMs")]
    pub latency_ms: u64,
    pub ok: bool,
    pub estimated: bool,
}

/// Valid `by` dimensions for [`super::Db::usage_breakdown`].
pub const BREAKDOWN_KEYS: [&str; 4] = ["model", "source", "jid", "app_id"];

impl super::Db {
    /// Batch-insert usage events in one transaction. Called by the recorder's
    /// flush task only.
    pub fn insert_usage_events(&self, events: &[UsageEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        self.with_conn_mut(|c| {
            let tx = c.transaction()?;
            {
                let mut stmt = tx.prepare_cached(
                    r#"
                    INSERT INTO llm_usage_log
                      (ts, source, jid, agent_id, session_id, app_id,
                       profile, provider, model,
                       input_tokens, output_tokens,
                       cache_creation_tokens, cache_read_tokens,
                       latency_ms, ok, estimated)
                    VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
                    "#,
                )?;
                for ev in events {
                    stmt.execute(params![
                        ev.ts,
                        ev.source.as_str(),
                        ev.jid,
                        ev.agent_id,
                        ev.session_id,
                        ev.app_id,
                        ev.profile,
                        ev.provider,
                        ev.model,
                        ev.input_tokens as i64,
                        ev.output_tokens as i64,
                        ev.cache_creation_tokens as i64,
                        ev.cache_read_tokens as i64,
                        ev.latency_ms as i64,
                        ev.ok as i64,
                        ev.estimated as i64,
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// All pricing rows (small table, loaded whole for in-Rust matching).
    pub fn usage_pricing_all(&self) -> Result<Vec<ModelPricing>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT model, input_per_1m, output_per_1m, cache_read_per_1m, cache_write_per_1m
                 FROM model_pricing ORDER BY model",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(ModelPricing {
                        model: r.get(0)?,
                        input_per_1m: r.get(1)?,
                        output_per_1m: r.get(2)?,
                        cache_read_per_1m: r.get(3)?,
                        cache_write_per_1m: r.get(4)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    pub fn usage_pricing_upsert(&self, p: &ModelPricing) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                r#"
                INSERT INTO model_pricing
                  (model, input_per_1m, output_per_1m, cache_read_per_1m, cache_write_per_1m)
                VALUES (?1,?2,?3,?4,?5)
                ON CONFLICT(model) DO UPDATE SET
                  input_per_1m=excluded.input_per_1m,
                  output_per_1m=excluded.output_per_1m,
                  cache_read_per_1m=excluded.cache_read_per_1m,
                  cache_write_per_1m=excluded.cache_write_per_1m
                "#,
                params![
                    p.model,
                    p.input_per_1m,
                    p.output_per_1m,
                    p.cache_read_per_1m,
                    p.cache_write_per_1m
                ],
            )?;
            Ok(())
        })
    }

    pub fn usage_pricing_delete(&self, model: &str) -> Result<bool> {
        self.with_conn(|c| {
            let n = c.execute("DELETE FROM model_pricing WHERE model = ?1", params![model])?;
            Ok(n > 0)
        })
    }

    /// Seed default pricing rows without clobbering user edits.
    pub fn usage_pricing_seed(&self, rows: &[ModelPricing]) -> Result<()> {
        self.with_conn(|c| {
            for p in rows {
                c.execute(
                    r#"
                    INSERT OR IGNORE INTO model_pricing
                      (model, input_per_1m, output_per_1m, cache_read_per_1m, cache_write_per_1m)
                    VALUES (?1,?2,?3,?4,?5)
                    "#,
                    params![
                        p.model,
                        p.input_per_1m,
                        p.output_per_1m,
                        p.cache_read_per_1m,
                        p.cache_write_per_1m
                    ],
                )?;
            }
            Ok(())
        })
    }

    /// Totals over `[since_ms, until_ms)` straight from the raw log (bounded
    /// by the 90-day retention — fine for today/week/month windows).
    pub fn usage_totals(&self, since_ms: i64, until_ms: i64) -> Result<UsageTotals> {
        let pricing = self.usage_pricing_all()?;
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                r#"
                SELECT model, COUNT(*),
                       SUM(input_tokens), SUM(output_tokens),
                       SUM(cache_creation_tokens), SUM(cache_read_tokens)
                FROM llm_usage_log
                WHERE ts >= ?1 AND ts < ?2
                GROUP BY model
                "#,
            )?;
            let per_model = stmt
                .query_map(params![since_ms, until_ms], read_model_sums)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(fold_model_sums(&per_model, &pricing))
        })
    }

    /// Group totals by one of [`BREAKDOWN_KEYS`] over the last `since_ms..until_ms`
    /// window, costed per underlying model. Rows sorted by total tokens desc.
    pub fn usage_breakdown(
        &self,
        by: &str,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vec<UsageBreakdownRow>> {
        anyhow::ensure!(BREAKDOWN_KEYS.contains(&by), "invalid breakdown key: {by}");
        let pricing = self.usage_pricing_all()?;
        self.with_conn(|c| {
            // `by` is validated against the fixed whitelist above, never
            // caller-controlled SQL.
            let sql = format!(
                r#"
                SELECT {by}, model, COUNT(*),
                       SUM(input_tokens), SUM(output_tokens),
                       SUM(cache_creation_tokens), SUM(cache_read_tokens)
                FROM llm_usage_log
                WHERE ts >= ?1 AND ts < ?2
                GROUP BY {by}, model
                "#
            );
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt
                .query_map(params![since_ms, until_ms], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        ModelSums {
                            model: r.get(1)?,
                            calls: r.get::<_, i64>(2)? as u64,
                            input: r.get::<_, i64>(3)? as u64,
                            output: r.get::<_, i64>(4)? as u64,
                            cache_creation: r.get::<_, i64>(5)? as u64,
                            cache_read: r.get::<_, i64>(6)? as u64,
                        },
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            let mut grouped: std::collections::BTreeMap<String, Vec<ModelSums>> =
                std::collections::BTreeMap::new();
            for (key, sums) in rows {
                grouped.entry(key).or_default().push(sums);
            }
            let mut out: Vec<UsageBreakdownRow> = grouped
                .into_iter()
                .map(|(key, sums)| UsageBreakdownRow {
                    key,
                    totals: fold_model_sums(&sums, &pricing),
                })
                .collect();
            out.sort_by_key(|r| {
                std::cmp::Reverse(
                    r.totals.input_tokens
                        + r.totals.output_tokens
                        + r.totals.cache_creation_tokens
                        + r.totals.cache_read_tokens,
                )
            });
            Ok(out)
        })
    }

    /// Per-day rollup rows (summed across dimensions) for the last `days`
    /// days, oldest first. Reads `llm_usage_daily` — run the aggregator first.
    pub fn usage_daily(&self, days: u32) -> Result<Vec<UsageDailyRow>> {
        let since = (chrono::Utc::now() - chrono::Duration::days(days as i64))
            .format("%Y-%m-%d")
            .to_string();
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                r#"
                SELECT date, SUM(calls),
                       SUM(input_tokens), SUM(output_tokens),
                       SUM(cache_creation_tokens), SUM(cache_read_tokens),
                       SUM(est_cost_usd),
                       SUM(CASE WHEN est_cost_usd IS NULL
                            THEN input_tokens + output_tokens
                               + cache_creation_tokens + cache_read_tokens
                            ELSE 0 END)
                FROM llm_usage_daily
                WHERE date >= ?1
                GROUP BY date
                ORDER BY date ASC
                "#,
            )?;
            let rows = stmt
                .query_map(params![since], |r| {
                    Ok(UsageDailyRow {
                        date: r.get(0)?,
                        totals: UsageTotals {
                            calls: r.get::<_, i64>(1)? as u64,
                            input_tokens: r.get::<_, i64>(2)? as u64,
                            output_tokens: r.get::<_, i64>(3)? as u64,
                            cache_creation_tokens: r.get::<_, i64>(4)? as u64,
                            cache_read_tokens: r.get::<_, i64>(5)? as u64,
                            est_cost_usd: r.get::<_, Option<f64>>(6)?.unwrap_or(0.0),
                            unpriced_tokens: r.get::<_, i64>(7)? as u64,
                        },
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Recent raw rows, newest first, keyset-paged by `before` id.
    pub fn usage_log_recent(&self, limit: u32, before: Option<i64>) -> Result<Vec<UsageLogRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare_cached(
                r#"
                SELECT id, ts, source, jid, agent_id, session_id, app_id,
                       profile, provider, model,
                       input_tokens, output_tokens,
                       cache_creation_tokens, cache_read_tokens,
                       latency_ms, ok, estimated
                FROM llm_usage_log
                WHERE (?2 IS NULL OR id < ?2)
                ORDER BY id DESC
                LIMIT ?1
                "#,
            )?;
            let rows = stmt
                .query_map(params![limit as i64, before], |r| {
                    Ok(UsageLogRow {
                        id: r.get(0)?,
                        ts: r.get(1)?,
                        source: r.get(2)?,
                        jid: r.get(3)?,
                        agent_id: r.get(4)?,
                        session_id: r.get(5)?,
                        app_id: r.get(6)?,
                        profile: r.get(7)?,
                        provider: r.get(8)?,
                        model: r.get(9)?,
                        input_tokens: r.get::<_, i64>(10)? as u64,
                        output_tokens: r.get::<_, i64>(11)? as u64,
                        cache_creation_tokens: r.get::<_, i64>(12)? as u64,
                        cache_read_tokens: r.get::<_, i64>(13)? as u64,
                        latency_ms: r.get::<_, i64>(14)? as u64,
                        ok: r.get::<_, i64>(15)? != 0,
                        estimated: r.get::<_, i64>(16)? != 0,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    /// Total (billed input, output) tokens recorded for one jid — used to fill
    /// `background_runs.tokens_in/out` for jid `bg:<run_id>`.
    pub fn usage_sum_for_jid(&self, jid: &str) -> Result<(u64, u64)> {
        self.with_conn(|c| {
            let (i, o): (i64, i64) = c.query_row(
                r#"
                SELECT COALESCE(SUM(input_tokens + cache_creation_tokens + cache_read_tokens), 0),
                       COALESCE(SUM(output_tokens), 0)
                FROM llm_usage_log WHERE jid = ?1
                "#,
                params![jid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok((i as u64, o as u64))
        })
    }

    /// Rebuild the `llm_usage_daily` rollup for one UTC `date` ("YYYY-MM-DD")
    /// from the raw log. Idempotent: deletes the date's rows first.
    pub fn usage_aggregate_date(&self, date: &str) -> Result<usize> {
        let pricing = self.usage_pricing_all()?;
        let day_start = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")?
            .and_hms_opt(0, 0, 0)
            .expect("valid midnight")
            .and_utc()
            .timestamp_millis();
        let day_end = day_start + 86_400_000;

        self.with_conn_mut(|c| {
            let tx = c.transaction()?;
            let rows: Vec<(String, String, String, String, ModelSums)> = {
                let mut stmt = tx.prepare(
                    r#"
                    SELECT source, jid, app_id, model, COUNT(*),
                           SUM(input_tokens), SUM(output_tokens),
                           SUM(cache_creation_tokens), SUM(cache_read_tokens)
                    FROM llm_usage_log
                    WHERE ts >= ?1 AND ts < ?2
                    GROUP BY source, jid, app_id, model
                    "#,
                )?;
                let collected = stmt
                    .query_map(params![day_start, day_end], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            ModelSums {
                                model: r.get(3)?,
                                calls: r.get::<_, i64>(4)? as u64,
                                input: r.get::<_, i64>(5)? as u64,
                                output: r.get::<_, i64>(6)? as u64,
                                cache_creation: r.get::<_, i64>(7)? as u64,
                                cache_read: r.get::<_, i64>(8)? as u64,
                            },
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                collected
            };

            tx.execute("DELETE FROM llm_usage_daily WHERE date = ?1", params![date])?;
            let n = rows.len();
            {
                let mut ins = tx.prepare(
                    r#"
                    INSERT INTO llm_usage_daily
                      (date, source, jid, app_id, model, calls,
                       input_tokens, output_tokens,
                       cache_creation_tokens, cache_read_tokens, est_cost_usd)
                    VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                    "#,
                )?;
                for (source, jid, app_id, model, s) in rows {
                    let cost = match_pricing(&pricing, &model)
                        .map(|p| p.cost(s.input, s.output, s.cache_creation, s.cache_read));
                    ins.execute(params![
                        date,
                        source,
                        jid,
                        app_id,
                        model,
                        s.calls as i64,
                        s.input as i64,
                        s.output as i64,
                        s.cache_creation as i64,
                        s.cache_read as i64,
                        cost,
                    ])?;
                }
            }
            tx.commit()?;
            Ok(n)
        })
    }

    /// Delete raw rows older than `cutoff_ms`. The daily rollup keeps history.
    pub fn usage_prune_log(&self, cutoff_ms: i64) -> Result<usize> {
        self.with_conn(|c| {
            let n = c.execute("DELETE FROM llm_usage_log WHERE ts < ?1", params![cutoff_ms])?;
            Ok(n)
        })
    }
}

/// Per-model sums used as the costing unit.
#[derive(Debug, Clone)]
struct ModelSums {
    model: String,
    calls: u64,
    input: u64,
    output: u64,
    cache_creation: u64,
    cache_read: u64,
}

fn read_model_sums(r: &rusqlite::Row<'_>) -> rusqlite::Result<ModelSums> {
    Ok(ModelSums {
        model: r.get(0)?,
        calls: r.get::<_, i64>(1)? as u64,
        input: r.get::<_, i64>(2)? as u64,
        output: r.get::<_, i64>(3)? as u64,
        cache_creation: r.get::<_, i64>(4)? as u64,
        cache_read: r.get::<_, i64>(5)? as u64,
    })
}

fn fold_model_sums(per_model: &[ModelSums], pricing: &[ModelPricing]) -> UsageTotals {
    let mut t = UsageTotals::default();
    for s in per_model {
        t.calls += s.calls;
        t.input_tokens += s.input;
        t.output_tokens += s.output;
        t.cache_creation_tokens += s.cache_creation;
        t.cache_read_tokens += s.cache_read;
        match match_pricing(pricing, &s.model) {
            Some(p) => {
                t.est_cost_usd += p.cost(s.input, s.output, s.cache_creation, s.cache_read)
            }
            None => {
                t.unpriced_tokens += s.input + s.output + s.cache_creation + s.cache_read;
            }
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::usage::{UsageEvent, UsageSource};

    fn ev(ts: i64, source: UsageSource, model: &str, input: u64, output: u64) -> UsageEvent {
        UsageEvent {
            ts,
            model: model.to_string(),
            input_tokens: input,
            output_tokens: output,
            jid: "web:test".into(),
            app_id: if source == UsageSource::Bridge {
                "crm".into()
            } else {
                String::new()
            },
            ..UsageEvent::new(source)
        }
    }

    #[test]
    fn insert_totals_aggregate_breakdown_prune_roundtrip() {
        let db = Db::open_in_memory(&Config::from_env()).unwrap();
        let now = chrono::Utc::now().timestamp_millis();

        db.usage_pricing_seed(&[ModelPricing {
            model: "claude-opus-5".into(),
            input_per_1m: 5.0,
            output_per_1m: 25.0,
            cache_read_per_1m: Some(0.5),
            cache_write_per_1m: Some(6.25),
        }])
        .unwrap();

        db.insert_usage_events(&[
            ev(now, UsageSource::Agent, "claude-opus-5", 1_000_000, 100_000),
            ev(now, UsageSource::Bridge, "claude-opus-5", 500_000, 50_000),
            ev(now, UsageSource::Cognitive, "mystery-model", 10_000, 1_000),
        ])
        .unwrap();

        // Totals: priced 1.5M in + 150k out → 1.5*5 + 0.15*25 = 11.25 USD;
        // the unpriced model's 11k tokens land in unpriced_tokens.
        let t = db.usage_totals(now - 1000, now + 1000).unwrap();
        assert_eq!(t.calls, 3);
        assert_eq!(t.input_tokens, 1_510_000);
        assert_eq!(t.output_tokens, 151_000);
        assert!((t.est_cost_usd - 11.25).abs() < 1e-6, "{}", t.est_cost_usd);
        assert_eq!(t.unpriced_tokens, 11_000);

        // Prefix matching: a dated snapshot id resolves to the base price row.
        let pricing = db.usage_pricing_all().unwrap();
        assert!(match_pricing(&pricing, "claude-opus-5-20260101").is_some());
        assert!(match_pricing(&pricing, "gpt-nope").is_none());

        // Breakdown by source: three rows, agent first (largest volume).
        let rows = db
            .usage_breakdown("source", now - 1000, now + 1000)
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].key, "agent");

        // Daily rollup for today picks up all three dimension rows.
        let date = chrono::Utc::now().date_naive().format("%Y-%m-%d").to_string();
        let n = db.usage_aggregate_date(&date).unwrap();
        assert_eq!(n, 3);
        let daily = db.usage_daily(2).unwrap();
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0].totals.calls, 3);
        assert_eq!(daily[0].totals.unpriced_tokens, 11_000);

        // Retention prune removes rows older than the cutoff only.
        db.insert_usage_events(&[ev(
            now - 200 * 86_400_000,
            UsageSource::Agent,
            "claude-opus-5",
            1,
            1,
        )])
        .unwrap();
        let pruned = db.usage_prune_log(now - 90 * 86_400_000).unwrap();
        assert_eq!(pruned, 1);
        assert_eq!(db.usage_log_recent(10, None).unwrap().len(), 3);

        // Background-run totals sum by jid (billed input includes cache).
        let (in_sum, out_sum) = db.usage_sum_for_jid("web:test").unwrap();
        assert_eq!(in_sum, 1_510_000);
        assert_eq!(out_sum, 151_000);
    }
}
