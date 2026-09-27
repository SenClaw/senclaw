//! Time tool — returns the current date and time.
//!
//! Read-only tool with no side effects.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::zen_core::{Tool, ToolContext, ToolOutput, ToolResultMessage};

pub struct TimeTool;

#[async_trait]
impl Tool for TimeTool {
    fn name(&self) -> &str {
        "Time"
    }

    fn description(&self) -> &str {
        "Get the current date and time with optional timezone offset"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "timezone": {
                    "type": "string",
                    "description": "Optional timezone offset like +07:00, -05:00, or Z for UTC"
                }
            },
            "required": []
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn validate_input(
        &self,
        input: &Value,
        _ctx: &ToolContext<'_>,
    ) -> std::result::Result<(), String> {
        if let Some(tz) = input.get("timezone").and_then(|v| v.as_str()) {
            if !tz.is_empty() {
                parse_offset(tz)?;
            }
        }
        Ok(())
    }

    async fn call(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<Vec<ToolOutput>> {
        let tz_str = input.get("timezone").and_then(|v| v.as_str()).unwrap_or("");

        let tz = if tz_str.is_empty() {
            *chrono::Local::now().offset()
        } else {
            parse_offset(tz_str).map_err(|e| anyhow::anyhow!("{e}"))?
        };

        let now = chrono::Utc::now().with_timezone(&tz);

        let output = serde_json::json!({
            "iso8601": now.to_rfc3339(),
            "unix_ms": now.timestamp_millis(),
            "unix_s": now.timestamp(),
            "timezone": tz.to_string(),
            "weekday": now.format("%A").to_string(),
            "date": now.format("%Y-%m-%d").to_string(),
            "time": now.format("%H:%M:%S").to_string(),
        });

        let text = format!(
            "{} {} ({})",
            output["date"].as_str().unwrap_or(""),
            output["time"].as_str().unwrap_or(""),
            output["weekday"].as_str().unwrap_or(""),
        );

        Ok(vec![ToolOutput::Result {
            data: output,
            result_for_assistant: text,
        }])
    }

    fn gen_tool_result_message(&self, data: &Value, _input: &Value) -> ToolResultMessage {
        ToolResultMessage {
            title: "Time".into(),
            summary: data
                .get("date")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            content: data.clone(),
        }
    }

    fn get_display_title(&self, _input: &Value) -> String {
        "Time".into()
    }
}

fn parse_offset(s: &str) -> std::result::Result<chrono::FixedOffset, String> {
    if s.eq_ignore_ascii_case("Z") || s.eq_ignore_ascii_case("UTC") {
        return chrono::FixedOffset::east_opt(0).ok_or_else(|| "Invalid UTC offset".into());
    }
    let s = s.trim();
    let sign = if s.starts_with('-') { -1 } else { 1 };
    let s = s.trim_start_matches(&['+', '-'][..]);
    let mut parts = s.splitn(2, ':');
    let hours: i32 = parts.next().unwrap_or("").parse().map_err(|_| {
        format!("Invalid timezone offset: \"{s}\". Expected format: +07:00, -05:00, or Z.")
    })?;
    let minutes: i32 = parts
        .next()
        .unwrap_or("0")
        .parse()
        .map_err(|_| format!("Invalid timezone offset minutes: \"{s}\""))?;
    if hours > 23 || minutes > 59 {
        return Err(format!(
            "Invalid timezone offset: \"{s}\". Hours must be 0-23, minutes 0-59."
        ));
    }
    chrono::FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
        .ok_or_else(|| format!("Invalid timezone offset: \"{s}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_tool_returns_valid_output() {
        let tool = TimeTool;
        let ctx = ToolContext {
            agent_id: "test",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(tool.call(serde_json::json!({}), &ctx));
        assert!(result.is_ok());
        let outputs = result.unwrap();
        assert_eq!(outputs.len(), 1);
        match &outputs[0] {
            ToolOutput::Result { data, .. } => {
                assert!(data["iso8601"].as_str().is_some());
                assert!(data["unix_ms"].as_i64().is_some());
                assert!(data["date"].as_str().is_some());
                assert!(data["time"].as_str().is_some());
                assert!(data["weekday"].as_str().is_some());
            }
            _ => panic!("expected Result output"),
        }
    }

    #[test]
    fn time_tool_with_timezone() {
        let tool = TimeTool;
        let ctx = ToolContext {
            agent_id: "test",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(tool.call(serde_json::json!({"timezone": "+07:00"}), &ctx));
        assert!(result.is_ok());
        let outputs = result.unwrap();
        match &outputs[0] {
            ToolOutput::Result { data, .. } => {
                assert_eq!(data["timezone"].as_str().unwrap(), "+07:00");
            }
            _ => panic!("expected Result output"),
        }
    }

    #[test]
    fn time_tool_invalid_timezone() {
        let tool = TimeTool;
        let ctx = ToolContext {
            agent_id: "test",
            working_dir: "/tmp",
            agent_data_dir: "/tmp",
            abort: tokio_util::sync::CancellationToken::new(),
            event_bus: None,
            response_registry: None,
            edit_format: Default::default(),
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result =
            rt.block_on(tool.validate_input(&serde_json::json!({"timezone": "invalid"}), &ctx));
        assert!(result.is_err());
    }

    #[test]
    fn parse_offset_valid() {
        assert_eq!(
            parse_offset("+07:00").unwrap(),
            chrono::FixedOffset::east_opt(7 * 3600).unwrap()
        );
        assert_eq!(
            parse_offset("-05:00").unwrap(),
            chrono::FixedOffset::east_opt(-5 * 3600).unwrap()
        );
        assert_eq!(
            parse_offset("Z").unwrap(),
            chrono::FixedOffset::east_opt(0).unwrap()
        );
        assert_eq!(
            parse_offset("UTC").unwrap(),
            chrono::FixedOffset::east_opt(0).unwrap()
        );
    }
}
