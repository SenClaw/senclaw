//! Chart-data normalization — the "data → widget" pipe of `emit_widget`.
//!
//! The canonical chart payload the clients render is
//! `{ chartType, series: [{ name, color?, points: [{x,y}] }], xLabel?, yLabel?, stacked? }`.
//! Requiring the model to hand-build that shape is exactly what pushed agents
//! into writing throwaway `/tmp/*.js` scripts just to reshape rows before
//! emitting a chart. This module accepts the shapes an agent naturally has and
//! converts them **daemon-side**, so all three clients keep rendering the one
//! canonical form with zero changes:
//!
//! 1. Canonical `series` (passthrough; points may be `{x,y}`, `[x,y]` pairs,
//!    or plain numbers — the latter get `x = index`).
//! 2. `rows`: an array of flat objects — one row per x, every numeric column
//!    becomes a series (`{ rows: [{date:"26/07", high:37, low:26}, …], x: "date"? }`).
//! 3. `labels` + `values`: two parallel arrays → a single series.
//!
//! Numbers may arrive as strings ("37", "33,5" — scraped pages love comma
//! decimals); they are parsed leniently. Presentation keys (`xLabel`, `yLabel`,
//! `stacked`) pass through untouched.

use serde_json::{Map, Value};

const CHART_TYPES: [&str; 5] = ["bar", "line", "area", "pie", "scatter"];

/// Normalize any accepted chart-data shape into the canonical payload.
/// Errors are written for the model: they say what was wrong and what shapes
/// are accepted, so the next attempt can self-correct.
pub fn normalize_chart_data(data: &Value) -> Result<Value, String> {
    let obj = data
        .as_object()
        .ok_or("chart data must be an object")?
        .clone();

    let chart_type = match obj.get("chartType").and_then(|v| v.as_str()) {
        Some(t) if CHART_TYPES.contains(&t) => t.to_string(),
        Some(t) => {
            return Err(format!(
                "chartType \"{t}\" is not supported; use one of bar | line | area | pie | scatter"
            ))
        }
        // Default matches the desktop renderer's fallback.
        None => "bar".to_string(),
    };

    let series: Vec<Value> = if let Some(series) = obj.get("series").and_then(|v| v.as_array()) {
        series.iter().filter_map(normalize_series).collect()
    } else if let Some(rows) = obj.get("rows").and_then(|v| v.as_array()) {
        series_from_rows(rows, obj.get("x").and_then(|v| v.as_str()))?
    } else if let (Some(labels), Some(values)) = (
        obj.get("labels").and_then(|v| v.as_array()),
        obj.get("values").and_then(|v| v.as_array()),
    ) {
        series_from_labels_values(labels, values, obj.get("name").and_then(|v| v.as_str()))
    } else {
        return Err(
            "chart data must contain `series`, `rows` (array of flat objects — every numeric \
             column becomes a series), or `labels` + `values`"
                .to_string(),
        );
    };

    if series.is_empty()
        || series
            .iter()
            .all(|s| s.get("points").and_then(|p| p.as_array()).map(|a| a.is_empty()) != Some(false))
    {
        return Err("chart has no plottable points after normalization".to_string());
    }

    let mut out = Map::new();
    out.insert("chartType".into(), Value::String(chart_type));
    out.insert("series".into(), Value::Array(series));
    for key in ["xLabel", "yLabel", "stacked"] {
        if let Some(v) = obj.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    Ok(Value::Object(out))
}

/// Lenient number parse: JSON numbers, "37", "33.5", and comma-decimal "33,5".
fn as_num(v: &Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    let s = v.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<f64>() {
        return Some(n);
    }
    // "33,5" → "33.5", but never touch thousand-separated forms like "1,234.5".
    if s.contains(',') && !s.contains('.') {
        return s.replace(',', ".").parse::<f64>().ok();
    }
    None
}

/// `{x,y}` | `[x, y]` | plain number (x = index) → `{x,y}`, else None.
fn normalize_point(p: &Value, index: usize) -> Option<Value> {
    if let Some(obj) = p.as_object() {
        let y = as_num(obj.get("y")?)?;
        let x = obj.get("x").cloned().unwrap_or(Value::from(index));
        return Some(serde_json::json!({ "x": x, "y": y }));
    }
    if let Some(pair) = p.as_array() {
        if pair.len() >= 2 {
            let y = as_num(&pair[1])?;
            return Some(serde_json::json!({ "x": pair[0].clone(), "y": y }));
        }
        return None;
    }
    let y = as_num(p)?;
    Some(serde_json::json!({ "x": index, "y": y }))
}

fn normalize_series(s: &Value) -> Option<Value> {
    let obj = s.as_object()?;
    let points: Vec<Value> = obj
        .get("points")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .enumerate()
                .filter_map(|(i, p)| normalize_point(p, i))
                .collect()
        })
        .unwrap_or_default();
    let mut out = Map::new();
    out.insert(
        "name".into(),
        Value::String(
            obj.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("series")
                .to_string(),
        ),
    );
    if let Some(color) = obj.get("color") {
        out.insert("color".into(), color.clone());
    }
    out.insert("points".into(), Value::Array(points));
    Some(Value::Object(out))
}

/// Keys we recognise as the x column when `x` isn't explicit, in priority
/// order. Vietnamese aliases included — that's the user base.
const X_KEY_HINTS: [&str; 8] = ["x", "date", "day", "time", "label", "name", "ngay", "ngày"];

fn series_from_rows(rows: &[Value], x_key: Option<&str>) -> Result<Vec<Value>, String> {
    let rows: Vec<&Map<String, Value>> =
        rows.iter().filter_map(|r| r.as_object()).collect();
    if rows.is_empty() {
        return Err("`rows` is empty or its entries are not objects".to_string());
    }

    // Resolve the x column: explicit `x` field wins; else a well-known key
    // name; else the first non-numeric (string-valued) column; else row index.
    let first = rows[0];
    let x_key: Option<String> = match x_key {
        Some(k) => {
            if !first.contains_key(k) {
                return Err(format!("x column \"{k}\" does not exist in rows"));
            }
            Some(k.to_string())
        }
        None => X_KEY_HINTS
            .iter()
            .find(|k| first.contains_key(**k))
            .map(|k| k.to_string())
            .or_else(|| {
                first
                    .iter()
                    .find(|(_, v)| v.is_string() && as_num(v).is_none())
                    .map(|(k, _)| k.clone())
            }),
    };

    // Every column (other than x) that is numeric in at least one row becomes
    // a series. NB: serde_json maps iterate keys alphabetically (no
    // preserve_order feature) — series order is deterministic, not authored;
    // pass canonical `series` to control ordering exactly.
    let mut series_keys: Vec<String> = first
        .keys()
        .filter(|k| Some(k.as_str()) != x_key.as_deref())
        .filter(|k| rows.iter().any(|r| r.get(*k).map(|v| as_num(v).is_some()) == Some(true)))
        .cloned()
        .collect();
    // A row set may be ragged; pick up numeric columns that only appear later.
    for r in &rows {
        for k in r.keys() {
            if Some(k.as_str()) != x_key.as_deref()
                && !series_keys.contains(k)
                && r.get(k).map(|v| as_num(v).is_some()) == Some(true)
            {
                series_keys.push(k.clone());
            }
        }
    }
    if series_keys.is_empty() {
        return Err("no numeric columns found in `rows` to plot".to_string());
    }

    let series = series_keys
        .iter()
        .map(|key| {
            let points: Vec<Value> = rows
                .iter()
                .enumerate()
                .filter_map(|(i, r)| {
                    let y = as_num(r.get(key)?)?;
                    let x = x_key
                        .as_deref()
                        .and_then(|k| r.get(k))
                        .cloned()
                        .unwrap_or(Value::from(i));
                    Some(serde_json::json!({ "x": x, "y": y }))
                })
                .collect();
            serde_json::json!({ "name": key, "points": points })
        })
        .collect();
    Ok(series)
}

fn series_from_labels_values(labels: &[Value], values: &[Value], name: Option<&str>) -> Vec<Value> {
    let points: Vec<Value> = labels
        .iter()
        .zip(values.iter())
        .filter_map(|(l, v)| {
            let y = as_num(v)?;
            Some(serde_json::json!({ "x": l.clone(), "y": y }))
        })
        .collect();
    vec![serde_json::json!({
        "name": name.unwrap_or("values"),
        "points": points,
    })]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rows_become_one_series_per_numeric_column() {
        // The exact scenario that pushed an agent into /tmp/weather_data.js:
        // scraped rows with a date column and two temperature columns.
        let data = json!({
            "chartType": "line",
            "rows": [
                { "date": "26/07", "high": 37, "low": 26 },
                { "date": "27/07", "high": 34, "low": 28 },
                { "date": "28/07", "high": "31", "low": "27" }
            ],
            "xLabel": "Ngày", "yLabel": "Nhiệt độ (°C)"
        });
        let out = normalize_chart_data(&data).unwrap();
        assert_eq!(out["chartType"], "line");
        assert_eq!(out["xLabel"], "Ngày");
        let series = out["series"].as_array().unwrap();
        assert_eq!(series.len(), 2);
        // serde_json maps sort keys → "high" then "low".
        assert_eq!(series[0]["name"], "high");
        assert_eq!(series[1]["name"], "low");
        assert_eq!(series[0]["points"][0], json!({ "x": "26/07", "y": 37.0 }));
        // Numeric strings parse.
        assert_eq!(series[0]["points"][2], json!({ "x": "28/07", "y": 31.0 }));
    }

    #[test]
    fn rows_respect_explicit_x_and_reject_unknown_x() {
        let data = json!({
            "rows": [ { "thang": "T1", "doanh_thu": 10, "ma": "A" } ],
            "x": "thang"
        });
        let out = normalize_chart_data(&data).unwrap();
        let series = out["series"].as_array().unwrap();
        // "ma" is non-numeric → skipped; only doanh_thu plots.
        assert_eq!(series.len(), 1);
        assert_eq!(series[0]["name"], "doanh_thu");
        assert_eq!(series[0]["points"][0]["x"], "T1");

        let bad = json!({ "rows": [ { "a": 1 } ], "x": "nope" });
        assert!(normalize_chart_data(&bad).unwrap_err().contains("nope"));
    }

    #[test]
    fn labels_values_make_a_single_series() {
        let data = json!({
            "chartType": "pie",
            "labels": ["A", "B", "C"],
            "values": [1, "2", "3,5"],
            "name": "Tỷ trọng"
        });
        let out = normalize_chart_data(&data).unwrap();
        let series = out["series"].as_array().unwrap();
        assert_eq!(series.len(), 1);
        assert_eq!(series[0]["name"], "Tỷ trọng");
        let pts = series[0]["points"].as_array().unwrap();
        assert_eq!(pts.len(), 3);
        // Comma decimal parsed.
        assert_eq!(pts[2]["y"], 3.5);
    }

    #[test]
    fn canonical_series_pass_through_with_point_shortcuts() {
        let data = json!({
            "chartType": "line",
            "series": [
                { "name": "s1", "color": "#f00", "points": [ { "x": "a", "y": 1 }, ["b", 2], 3 ] }
            ]
        });
        let out = normalize_chart_data(&data).unwrap();
        let pts = out["series"][0]["points"].as_array().unwrap();
        assert_eq!(pts[0], json!({ "x": "a", "y": 1.0 }));
        assert_eq!(pts[1], json!({ "x": "b", "y": 2.0 }));
        // Bare number → x = index.
        assert_eq!(pts[2], json!({ "x": 2, "y": 3.0 }));
        assert_eq!(out["series"][0]["color"], "#f00");
    }

    #[test]
    fn defaults_and_errors_speak_to_the_model() {
        // Missing chartType defaults to bar (desktop renderer's fallback).
        let out = normalize_chart_data(&json!({ "labels": ["a"], "values": [1] })).unwrap();
        assert_eq!(out["chartType"], "bar");
        // Unknown chartType is a correctable error, not a silent default.
        let err = normalize_chart_data(&json!({ "chartType": "donut", "labels": [], "values": [] }))
            .unwrap_err();
        assert!(err.contains("donut"), "{err}");
        // No recognizable data shape → the error lists the accepted ones.
        let err = normalize_chart_data(&json!({ "foo": 1 })).unwrap_err();
        assert!(err.contains("rows"), "{err}");
        // Rows with zero numeric columns.
        let err = normalize_chart_data(&json!({ "rows": [ { "a": "x" } ] })).unwrap_err();
        assert!(err.contains("numeric"), "{err}");
        // Empty points everywhere.
        let err = normalize_chart_data(&json!({ "series": [ { "name": "s", "points": [] } ] }))
            .unwrap_err();
        assert!(err.contains("plottable"), "{err}");
    }

    #[test]
    fn thousand_separated_strings_are_not_mangled() {
        // "1,234.5" must not become 1.2345 — it already has a dot, so the
        // comma-decimal rewrite is skipped and the parse fails (skipped point)
        // rather than silently plotting a wrong number.
        assert_eq!(as_num(&json!("1,234.5")), None);
        assert_eq!(as_num(&json!("33,5")), Some(33.5));
        assert_eq!(as_num(&json!("33.5")), Some(33.5));
        assert_eq!(as_num(&json!(7)), Some(7.0));
    }
}
