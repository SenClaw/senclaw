//! Background maintenance for token accounting: seeds default pricing,
//! rebuilds the `llm_usage_daily` rollup hourly, and prunes raw
//! `llm_usage_log` rows past retention. Spawned once from `run_daemon`.

use std::sync::Arc;
use std::time::Duration;

use crate::db::usage::ModelPricing;
use crate::db::Db;

/// Raw-log retention. The daily rollup keeps history beyond this.
const RETENTION_DAYS: i64 = 90;
const HOURLY: Duration = Duration::from_secs(3600);

/// USD per 1M tokens. Cache read ≈ 0.1× input, cache write ≈ 1.25× input
/// (Anthropic 5-minute-TTL rates). Seeded with INSERT OR IGNORE so user edits
/// via /api/usage/pricing are never clobbered. Models without a row show as
/// "n/a" in the UI — never a fabricated $0.
fn default_pricing() -> Vec<ModelPricing> {
    fn row(model: &str, input: f64, output: f64) -> ModelPricing {
        // Round to 6 decimals: `3.0 * 0.1` is 0.30000000000000004 in f64 and
        // that artifact would leak into the pricing UI verbatim.
        fn r6(v: f64) -> f64 {
            (v * 1_000_000.0).round() / 1_000_000.0
        }
        ModelPricing {
            model: model.to_string(),
            input_per_1m: input,
            output_per_1m: output,
            cache_read_per_1m: Some(r6(input * 0.1)),
            cache_write_per_1m: Some(r6(input * 1.25)),
        }
    }
    vec![
        row("claude-fable-5", 10.0, 50.0),
        row("claude-opus-5", 5.0, 25.0),
        row("claude-opus-4-8", 5.0, 25.0),
        row("claude-opus-4-7", 5.0, 25.0),
        row("claude-opus-4-6", 5.0, 25.0),
        row("claude-sonnet-5", 3.0, 15.0),
        row("claude-sonnet-4-6", 3.0, 15.0),
        row("claude-haiku-4-5", 1.0, 5.0),
    ]
}

/// Rebuild the rollup for today and yesterday (UTC). Idempotent; yesterday is
/// included so rows that arrived just before midnight are re-costed once more.
fn aggregate_recent(db: &Db) {
    let today = chrono::Utc::now().date_naive();
    for date in [today - chrono::Duration::days(1), today] {
        let d = date.format("%Y-%m-%d").to_string();
        if let Err(e) = db.usage_aggregate_date(&d) {
            tracing::warn!(date = %d, error = %e, "[usage] daily aggregation failed");
        }
    }
}

fn prune(db: &Db) {
    let cutoff =
        (chrono::Utc::now() - chrono::Duration::days(RETENTION_DAYS)).timestamp_millis();
    match db.usage_prune_log(cutoff) {
        Ok(n) if n > 0 => tracing::info!(rows = n, "[usage] pruned raw usage log"),
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "[usage] prune failed"),
    }
}

/// Seed pricing and spawn the hourly maintenance task.
pub fn start(db: Arc<Db>) {
    if let Err(e) = db.usage_pricing_seed(&default_pricing()) {
        tracing::warn!(error = %e, "[usage] pricing seed failed");
    }
    tokio::spawn(async move {
        // First pass shortly after boot so the dashboard has data without
        // waiting an hour; prune runs on the same cadence (cheap DELETE).
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            aggregate_recent(&db);
            prune(&db);
            tokio::time::sleep(HOURLY).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_pricing_has_cache_rates() {
        let rows = default_pricing();
        assert!(rows.iter().any(|r| r.model == "claude-opus-5"));
        let opus = rows.iter().find(|r| r.model == "claude-opus-5").unwrap();
        assert_eq!(opus.cache_read_per_1m, Some(0.5));
        assert_eq!(opus.cache_write_per_1m, Some(6.25));
    }
}
