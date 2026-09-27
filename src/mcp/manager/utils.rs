//! Utility functions for MCP tool name parsing and detection.

/// Parse `mcp__server__tool` into `(server, tool)`.
pub fn parse_mcp_tool_name(full_name: &str) -> Option<(String, String)> {
    let mut parts: Vec<&str> = full_name.splitn(3, "__").collect();
    if parts.len() == 3 && parts[0] == "mcp" {
        let tool = parts.pop().unwrap().to_string();
        let server = parts.pop().unwrap().to_string();
        Some((server, tool))
    } else {
        None
    }
}

/// Check whether a tool name follows the `mcp__` convention.
pub fn is_mcp_tool(tool_name: &str) -> bool {
    tool_name.starts_with("mcp__")
}
