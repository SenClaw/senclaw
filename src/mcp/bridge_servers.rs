//! The SenClaw MCP servers behind the HTTP bridge (`/mcp/{core|kanban}`): the
//! aggregated `core-server` (scoped to one chat, exactly as the daemon scopes
//! it for its own agents) and the kanban server.

use crate::config::Config;
use crate::mcp::helper::{core_mcp_config, CoreMcpParams, McpServerConfig};
use crate::types::GroupBinding;

/// Built-in servers worth exposing to an external coding agent. Left out on
/// purpose: the ones duplicating its own tools (workspace files, browser, js,
/// sandbox, ocr, patterns, litho).
pub const BRIDGE_CORE_SERVERS: &[&str] = &[
    "senclaw-schedule",
    "senclaw-background",
    "senclaw-usage",
    "senclaw-send",
    "senclaw-dispatch",
    "senclaw-memory",
    "senclaw-wiki",
    "senclaw-space",
    "senclaw-profile",
    "senclaw-link",
];

pub const CORE_NAME: &str = "senclaw-core";
pub const KANBAN_NAME: &str = "senclaw-kanban";

/// The chat Claude Code acts in: `jid` when it names a known group, else the
/// first plain chat, else any group.
pub fn resolve_group(db: &crate::db::Db, jid: Option<&str>) -> Option<GroupBinding> {
    if let Some(j) = jid.filter(|j| !j.is_empty()) {
        if let Ok(Some(g)) = db.get_group(j) {
            return Some(g);
        }
    }
    let groups = db.list_groups().ok()?;
    groups
        .iter()
        .find(|g| g.group_type == "chat")
        .or_else(|| groups.first())
        .cloned()
}

/// `senclaw core-server` scoped to `binding`, hosting [`BRIDGE_CORE_SERVERS`].
///
/// `project` is the binding's project when it has one: the external agent
/// then works where the session does — in the project's root, with the
/// session's own workspace-state file.
pub fn core_server(
    cfg: &Config,
    binding: &GroupBinding,
    project: Option<&crate::projects::Project>,
) -> McpServerConfig {
    let home = crate::util::paths::senclaw_home();
    let state_file = home.join(crate::projects::workspace_state_file_name(
        &crate::projects::workspace_state_key(binding),
    ));
    let workspace = match project {
        Some(p) => std::path::PathBuf::from(&p.root),
        None => crate::util::paths::senclaw_data_home().join("workspace").join(&binding.folder),
    };
    let project_root = project.map(|p| p.root.clone());
    let project_bank = project.map(|p| crate::projects::project_bank(&p.id));
    let project_dirs = project.map(|p| vec![p.root.clone()]);
    let s = |p: &std::path::Path| p.to_string_lossy().to_string();
    let (db, state, ws) = (s(&cfg.paths.db_path), s(&state_file), s(&workspace));
    let (profiles, user_profile) = (s(&cfg.paths.profiles_dir), s(&cfg.paths.user_profile_path));
    let (dispatch_state, virtual_dir, wiki) = (
        s(&cfg.paths.dispatch_state_path),
        s(&cfg.paths.virtual_agents_dir),
        s(&cfg.paths.wiki_dir),
    );
    let nonempty = |v: &str| (!v.is_empty()).then_some(v.to_string());
    let (key, base, litho_model) = (
        nonempty(&cfg.memory.openai_api_key),
        nonempty(&cfg.memory.openai_base_url),
        nonempty(&cfg.mcp.litho_model_efficient),
    );
    let mut c = core_mcp_config(CoreMcpParams {
        db_path: &db,
        group_folder: &binding.folder,
        chat_jid: &binding.jid,
        workspace_state_file: &state,
        default_workspace: &ws,
        allowed_work_dirs: project_dirs.as_deref().or(binding.allowed_work_dirs.as_deref()),
        profiles_dir: &profiles,
        memory_folder: &binding.folder,
        embedding_provider: Some(cfg.memory.embedding_provider.as_str()),
        openai_api_key: key.as_deref(),
        openai_base_url: base.as_deref(),
        custom_memory_dir: None,
        dispatch_state_path: &dispatch_state,
        virtual_agents_dir: &virtual_dir,
        wiki_dir: &wiki,
        send_bridge_port: 18081,
        bot_token: binding.bot_token.as_deref(),
        ws_port: cfg.ws_port,
        agent_id: &binding.jid,
        ui_port: cfg.ui_server.port,
        browser_v2: false,
        litho_binary: cfg.mcp.litho_binary.as_str(),
        litho_model_efficient: litho_model.as_deref(),
        user_profile_path: &user_profile,
        js_timeout_ms: 5_000,
        js_memory_mb: 128,
        servers: BRIDGE_CORE_SERVERS,
        project_root: project_root.as_deref(),
        project_bank: project_bank.as_deref(),
    });
    c.name = CORE_NAME.into();
    c
}

/// Tells the model which chat SenClaw's tools act for: several of them (schedule,
/// background, dispatch) take `group_folder` / `chat_jid` as explicit arguments,
/// which SenClaw's own agents learn from their system prompt.
pub fn scope_instructions(binding: &GroupBinding) -> String {
    format!(
        "SenClaw tools act for the chat \"{}\". When a SenClaw tool asks for `group_folder` use \"{}\", \
         and for `chat_jid` use \"{}\".",
        binding.name, binding.folder, binding.jid
    )
}

/// `senclaw kanban-server` (the board's own DB; no chat scope). `ui_port` is the
/// daemon's, which the child's model-backed tools call back into.
pub fn kanban_server(ui_port: u16) -> McpServerConfig {
    let mut cfg = McpServerConfig::new(KANBAN_NAME, "kanban-server");
    cfg.env.extend(crate::kanban::llm::child_env(ui_port));
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kanban_child_is_pointed_at_this_daemon() {
        let cfg = kanban_server(38788);
        assert_eq!(cfg.env["SENCLAW_BASE_URL"], "http://127.0.0.1:38788");
    }

    #[test]
    fn scope_names_folder_and_jid() {
        let g: GroupBinding = serde_json::from_value(serde_json::json!({
            "jid": "web:x", "folder": "fx", "name": "X", "channel": "", "group_type": "chat",
            "requires_trigger": false, "allowed_tools": null, "allowed_paths": null, "allowed_work_dirs": null,
            "bot_token": null, "max_messages": null, "llm_config_id": null, "last_active": null, "added_at": ""
        }))
        .unwrap();
        let s = scope_instructions(&g);
        assert!(s.contains("\"fx\"") && s.contains("\"web:x\""));
    }

}
