//! MCP server config builder. Port target: src-old/mcp/mcpHelper.ts
//!
//! Builds config structs consumed by AgentPool when registering MCP servers.
//! Each builder takes typed parameters instead of env-vars; the env-var model
//! used in the TS subprocess architecture is replaced by direct function
//! arguments in Rust.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// The engine DB opened from `SENCLAW_DB_PATH`, shared by every child server in
/// the process.
///
/// Each MCP server used to be its own subprocess, so "one DB handle per server"
/// cost nothing. Inside the aggregated `senclaw-core` process four children
/// (schedule, background, usage, space) want the same file, and opening it four
/// times means four SQLite connections and four WAL readers for identical data.
/// Returns `None` when the var is absent — the caller then skips that child.
/// A path that is set but unopenable stays an error: silently dropping the
/// schedule/background tools would look like the engine simply doesn't have
/// them.
pub fn shared_env_db() -> anyhow::Result<Option<Arc<crate::db::Db>>> {
    // The error is cached as a String because `anyhow::Error` is not `Clone`
    // and every later caller must see the same verdict as the first.
    static DB: OnceLock<Result<Option<Arc<crate::db::Db>>, String>> = OnceLock::new();
    let cell = DB.get_or_init(|| {
        let Ok(db_path) = std::env::var("SENCLAW_DB_PATH") else {
            return Ok(None);
        };
        let mut config = crate::config::Config::from_env();
        config.paths.db_path = std::path::PathBuf::from(&db_path);
        crate::db::Db::open(&config)
            .map(|db| Some(Arc::new(db)))
            .map_err(|e| format!("open DB at {db_path}: {e}"))
    });
    match cell {
        Ok(db) => Ok(db.clone()),
        Err(msg) => Err(anyhow::anyhow!(msg.clone())),
    }
}

/// Describes how to launch and communicate with an MCP server subprocess.
/// Mirrors `MCPServerConfig` from sema-core.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

impl McpServerConfig {
    pub fn new(name: &str, server_path: &str) -> Self {
        // Desktop app (senclaw-app) is a different binary that cannot dispatch
        // `*-server` subcommands, so it sets SENCLAW_BIN to the bundled CLI.
        let command = std::env::var("SENCLAW_BIN").ok().unwrap_or_else(|| {
            std::env::current_exe()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "senclaw".to_owned())
        });
        Self {
            name: name.to_owned(),
            transport: "stdio".to_owned(),
            command,
            args: vec![server_path.to_owned()],
            env: HashMap::new(),
        }
    }
}

// ===== ScheduleTool =====

pub fn schedule_mcp_config(db_path: &str, group_folder: &str, chat_jid: &str) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-schedule", "schedule-server");
    cfg.env.insert("SENCLAW_DB_PATH".into(), db_path.to_owned());
    cfg.env
        .insert("SENCLAW_GROUP_FOLDER".into(), group_folder.to_owned());
    cfg.env
        .insert("SENCLAW_CHAT_JID".into(), chat_jid.to_owned());
    cfg
}

// ===== BackgroundTool =====

/// MCP config for `senclaw-background` — lets a chat create/manage autonomous
/// background tasks. Owner is pinned from `group_folder`, never a tool param.
pub fn background_mcp_config(db_path: &str, group_folder: &str, chat_jid: &str) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-background", "background-server");
    cfg.env.insert("SENCLAW_DB_PATH".into(), db_path.to_owned());
    cfg.env
        .insert("SENCLAW_GROUP_FOLDER".into(), group_folder.to_owned());
    cfg.env
        .insert("SENCLAW_CHAT_JID".into(), chat_jid.to_owned());
    cfg
}

// ===== UsageTool =====

/// MCP config for `senclaw-usage` — read-only token/cost accounting queries
/// (usage_overview / usage_breakdown / usage_query).
pub fn usage_mcp_config(db_path: &str) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-usage", "usage-server");
    cfg.env.insert("SENCLAW_DB_PATH".into(), db_path.to_owned());
    cfg
}

// ===== UserProfileTool (Soul Core) =====

/// MCP config for `senclaw-profile` — read/update `USER.md`.
///
/// `chat_jid` is not decoration: it is what caps how much of the profile the
/// session may read and whether it may write at all. Passing an empty JID
/// makes the server behave as an unknown context, i.e. public-only.
pub fn user_profile_mcp_config(user_profile_path: &str, chat_jid: &str) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-profile", "user-profile-server");
    cfg.env.insert(
        "SENCLAW_USER_PROFILE_PATH".into(),
        user_profile_path.to_owned(),
    );
    cfg.env
        .insert("SENCLAW_CHAT_JID".into(), chat_jid.to_owned());
    cfg
}

// ===== DocsTool =====

/// MCP config for `senclaw-docs` — write documentation into the project the
/// chat is working in.
///
/// It takes the *same* env pair as the workspace server on purpose: the two
/// must agree on which directory is current, and reading the same state file
/// is what makes them agree after a `workspace_switch`.
pub fn docs_mcp_config(state_file: &str, default_workspace: &str) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-docs", "docs-server");
    cfg.env
        .insert("SENCLAW_WORKSPACE_STATE_FILE".into(), state_file.to_owned());
    cfg.env.insert(
        "SENCLAW_DEFAULT_WORKSPACE".into(),
        default_workspace.to_owned(),
    );
    cfg
}

// ===== WorkspaceTool =====

pub fn workspace_mcp_config(
    state_file: &str,
    default_workspace: &str,
    allowed_work_dirs: Option<&[String]>,
) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-workspace", "workspace-server");
    cfg.env
        .insert("SENCLAW_WORKSPACE_STATE_FILE".into(), state_file.to_owned());
    cfg.env.insert(
        "SENCLAW_DEFAULT_WORKSPACE".into(),
        default_workspace.to_owned(),
    );
    let dirs_str = match allowed_work_dirs {
        None => String::new(),
        Some(list) => serde_json::to_string(list).unwrap_or_default(),
    };
    cfg.env.insert("SENCLAW_ALLOWED_WORK_DIRS".into(), dirs_str);
    cfg
}

// ===== CognitiveTool =====

pub fn cognitive_mcp_config(
    db_path: &str,
    group_folder: &str,
    llm_disabled: bool,
) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-cognitive", "cognitive-server");
    cfg.env.insert("SENCLAW_DB_PATH".into(), db_path.to_owned());
    cfg.env
        .insert("SENCLAW_GROUP_FOLDER".into(), group_folder.to_owned());
    if llm_disabled {
        cfg.env
            .insert("SENCLAW_COG_LLM_DISABLED".into(), "1".to_owned());
    }
    cfg
}

// ===== SpaceTool =====

/// `ui_port` is what makes the `space_app_*` lifecycle tools work: starting and
/// stopping a Space App needs the daemon's `SpaceMcpLauncher`, which lives in
/// the daemon process and not in this subprocess, so those tools go back out
/// over loopback HTTP. The notes/calendar tools never leave the DB.
pub fn space_mcp_config(
    db_path: &str,
    group_folder: &str,
    chat_jid: &str,
    ui_port: u16,
) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-space", "space-server");
    cfg.env.insert("SENCLAW_DB_PATH".into(), db_path.to_owned());
    cfg.env
        .insert("SENCLAW_GROUP_FOLDER".into(), group_folder.to_owned());
    cfg.env
        .insert("SENCLAW_CHAT_JID".into(), chat_jid.to_owned());
    cfg.env.insert(
        "SENCLAW_SPACE_API_URL".into(),
        format!("http://127.0.0.1:{ui_port}"),
    );
    with_daemon_token(&mut cfg);
    cfg
}

/// Hand a subprocess the daemon's API token.
///
/// Loopback stops being a credential under auth mode `always`, and these
/// servers reach the daemon over loopback HTTP. The child cannot inherit the
/// token — the daemon deliberately does not `setenv` it — so it travels in the
/// config's own env map.
fn with_daemon_token(cfg: &mut McpServerConfig) {
    if let Some(t) = crate::util::internal_auth::daemon_token() {
        cfg.env.insert("SENCLAW_API_TOKEN".into(), t);
    }
}

// ===== MemoryTool =====

pub fn memory_mcp_config(
    db_path: &str,
    folder: &str,
    profiles_dir: &str,
    embedding_provider: Option<&str>,
    openai_api_key: Option<&str>,
    openai_base_url: Option<&str>,
    custom_memory_dir: Option<&str>,
) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-memory", "memory-server");
    cfg.env.insert("SENCLAW_DB_PATH".into(), db_path.to_owned());
    cfg.env.insert("SENCLAW_FOLDER".into(), folder.to_owned());
    cfg.env
        .insert("SENCLAW_PROFILES_DIR".into(), profiles_dir.to_owned());
    if let Some(p) = embedding_provider {
        cfg.env
            .insert("SENCLAW_EMBEDDING_PROVIDER".into(), p.to_owned());
    }
    if let Some(k) = openai_api_key {
        cfg.env
            .insert("SENCLAW_OPENAI_API_KEY".into(), k.to_owned());
    }
    if let Some(u) = openai_base_url {
        cfg.env
            .insert("SENCLAW_OPENAI_BASE_URL".into(), u.to_owned());
    }
    if let Some(d) = custom_memory_dir {
        cfg.env
            .insert("SENCLAW_CUSTOM_MEMORY_DIR".into(), d.to_owned());
    }
    cfg
}

// ===== SendTool =====

pub fn send_mcp_config(
    bridge_port: u16,
    chat_jid: &str,
    bot_token: Option<&str>,
    db_path: &str,
) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-send", "send-server");
    cfg.env
        .insert("SENCLAW_SEND_BRIDGE_PORT".into(), bridge_port.to_string());
    cfg.env
        .insert("SENCLAW_CHAT_JID".into(), chat_jid.to_owned());
    if let Some(tok) = bot_token {
        cfg.env.insert("SENCLAW_BOT_TOKEN".into(), tok.to_owned());
    }
    cfg.env.insert("SENCLAW_DB_PATH".into(), db_path.to_owned());
    cfg
}

// ===== DispatchTool =====

pub fn dispatch_mcp_config(
    state_path: &str,
    admin_folder: &str,
    chat_jid: &str,
    agents_config_dir: Option<&str>,
) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-dispatch", "dispatch-server");
    cfg.env
        .insert("SENCLAW_DISPATCH_STATE_PATH".into(), state_path.to_owned());
    cfg.env
        .insert("SENCLAW_ADMIN_FOLDER".into(), admin_folder.to_owned());
    // The admin folder is the agent profile, which several chats share. The
    // chat jid is what lets a DAG card land in the conversation that asked
    // for it.
    cfg.env
        .insert("SENCLAW_CHAT_JID".into(), chat_jid.to_owned());
    if let Some(d) = agents_config_dir {
        cfg.env
            .insert("SENCLAW_AGENTS_CONFIG_DIR".into(), d.to_owned());
    }
    cfg
}

// ===== VirtualAgent =====

pub fn virtual_mcp_config(
    agents_config_dir: &str,
    admin_folder: &str,
    default_workspace: &str,
) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-virtual", "virtual-server");
    cfg.env.insert(
        "SENCLAW_AGENTS_CONFIG_DIR".into(),
        agents_config_dir.to_owned(),
    );
    cfg.env
        .insert("SENCLAW_ADMIN_FOLDER".into(), admin_folder.to_owned());
    cfg.env.insert(
        "SENCLAW_DEFAULT_WORKSPACE".into(),
        default_workspace.to_owned(),
    );
    cfg
}

// ===== Wiki (local git, `crate::wiki`) =====

pub fn wiki_mcp_config(wiki_dir: &str) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-wiki", "wiki-server");
    cfg.env
        .insert("SENCLAW_WIKI_DIR".into(), wiki_dir.to_owned());
    cfg
}

// ===== Zen Patterns =====

/// Build the MCP config for the Patterns subprocess. Like OCR it owns no
/// state: the registry, the LLM config and the git checkouts all live in the
/// daemon, and a second copy in the subprocess would race the first one.
pub fn patterns_mcp_config(ui_port: u16) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-patterns", "patterns-server");
    cfg.env.insert(
        "SENCLAW_PATTERNS_API_URL".into(),
        format!("http://127.0.0.1:{ui_port}"),
    );
    with_daemon_token(&mut cfg);
    cfg
}

// ===== OCR =====

/// Build the MCP config for the OCR subprocess. The subprocess does not link
/// MNN — it forwards image bytes to the daemon's `/api/ocr/recognize` endpoint.
pub fn ocr_mcp_config(ui_port: u16) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-ocr", "ocr-server");
    cfg.env.insert(
        "SENCLAW_OCR_BRIDGE_URL".into(),
        format!("http://127.0.0.1:{ui_port}"),
    );
    with_daemon_token(&mut cfg);
    cfg
}

// ===== OS sandbox (sbx_* tools) =====

/// OS-sandbox executor (`senclaw-sandbox`). State is the shared engine DB
/// under `~/.senclaw/sandbox` — the subprocess opens it itself, so the only
/// thing to forward is a non-default data root.
pub fn sandbox_mcp_config() -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-sandbox", "sandbox-server");
    for key in ["SENCLAW_SANDBOX_DATA_DIR", "SANDBOX_DATA_DIR"] {
        if let Ok(v) = std::env::var(key) {
            if !v.trim().is_empty() {
                cfg.env.insert(key.into(), v);
            }
        }
    }
    cfg
}

// ===== JS executor =====

/// Sandboxed JavaScript executor (QuickJS). No state is shared with the daemon;
/// the subprocess just needs its default timeout / memory limits.
pub fn js_mcp_config(default_timeout_ms: u64, default_memory_mb: u64) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-js", "js-server");
    cfg.env.insert(
        "SENCLAW_JS_TIMEOUT_MS".into(),
        default_timeout_ms.to_string(),
    );
    cfg.env
        .insert("SENCLAW_JS_MEMORY_MB".into(), default_memory_mb.to_string());
    cfg
}

// ===== Browser =====

/// `agent_id` identifies the agent this server instance serves; the extension
/// allocates one tab per agent_id so concurrent agents don't share a tab.
/// `v2` selects the Jev + LLM browser engine (`crate::browser_agent`), whose
/// tools call the daemon's `/api/browser-agent/*` over loopback — hence the UI
/// port and the daemon token. Without it the legacy extension tools talk to
/// the WS gateway.
pub fn browser_mcp_config(ws_port: u16, ui_port: u16, agent_id: &str, v2: bool) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-browser", "browser-server");
    cfg.env
        .insert("SENCLAW_WS_PORT".into(), ws_port.to_string());
    if !agent_id.is_empty() {
        cfg.env
            .insert("SENCLAW_AGENT_ID".into(), agent_id.to_string());
    }
    if v2 {
        cfg.env.insert("SENCLAW_BROWSER_ENGINE".into(), "v2".into());
        cfg.env.insert(
            "SENCLAW_BROWSER_API_URL".into(),
            format!("http://127.0.0.1:{ui_port}"),
        );
        with_daemon_token(&mut cfg);
    }
    cfg
}

// ===== Litho (deepwiki-rs CLI) =====

/// Litho documentation generator — wraps the external `deepwiki-rs` binary.
pub fn litho_mcp_config(
    litho_binary: &str,
    llm_api_base_url: Option<&str>,
    llm_api_key: Option<&str>,
    model_efficient: Option<&str>,
) -> McpServerConfig {
    let mut cfg = McpServerConfig::new("senclaw-litho", "litho-server");
    cfg.env
        .insert("SENCLAW_LITHO_BINARY".into(), litho_binary.to_owned());
    if let Some(u) = llm_api_base_url {
        if !u.is_empty() {
            cfg.env
                .insert("SENCLAW_LITHO_LLMAPI_BASE_URL".into(), u.to_owned());
        }
    }
    if let Some(k) = llm_api_key {
        if !k.is_empty() {
            cfg.env
                .insert("SENCLAW_LITHO_LLMAPI_KEY".into(), k.to_owned());
        }
    }
    if let Some(m) = model_efficient {
        if !m.is_empty() {
            cfg.env
                .insert("SENCLAW_LITHO_MODEL_EFFICIENT".into(), m.to_owned());
        }
    }
    cfg
}

// ===== Zen Kit (all built-ins in one process) =====

/// Everything the bundled built-in servers need, in the shape the callers
/// already have it. Grouped into a struct because the union of fifteen
/// servers' parameters is far past what reads sensibly as arguments.
#[derive(Debug, Clone, Copy)]
pub struct CoreMcpParams<'a> {
    pub db_path: &'a str,
    pub group_folder: &'a str,
    pub chat_jid: &'a str,
    pub workspace_state_file: &'a str,
    pub default_workspace: &'a str,
    pub allowed_work_dirs: Option<&'a [String]>,
    pub profiles_dir: &'a str,
    pub memory_folder: &'a str,
    pub embedding_provider: Option<&'a str>,
    pub openai_api_key: Option<&'a str>,
    pub openai_base_url: Option<&'a str>,
    pub custom_memory_dir: Option<&'a str>,
    pub dispatch_state_path: &'a str,
    pub virtual_agents_dir: &'a str,
    pub wiki_dir: &'a str,
    pub send_bridge_port: u16,
    pub bot_token: Option<&'a str>,
    pub ws_port: u16,
    pub agent_id: &'a str,
    pub ui_port: u16,
    /// The browser tools run on the v2 engine (see [`browser_mcp_config`]).
    pub browser_v2: bool,
    pub litho_binary: &'a str,
    pub litho_model_efficient: Option<&'a str>,
    /// Path to `USER.md` (Soul Core). See [`user_profile_mcp_config`].
    pub user_profile_path: &'a str,
    pub js_timeout_ms: u64,
    pub js_memory_mb: u64,
    /// Which built-in servers to host, by name (`senclaw-wiki`, …).
    ///
    /// Presence of env alone cannot decide this: the servers share keys, so
    /// configuring dispatch also happens to satisfy the virtual-agent server.
    /// The caller says what it wants and the list is passed through verbatim.
    pub servers: &'a [&'a str],
}

/// The built-in servers `AgentPool` has always injected into a chat session.
/// Kept next to [`core_mcp_config`] so "what the aggregated server hosts" and
/// "what the separate subprocesses were" stay one list, not two.
pub const DEFAULT_CORE_SERVERS: &[&str] = &[
    "senclaw-schedule",
    "senclaw-background",
    "senclaw-usage",
    "senclaw-workspace",
    "senclaw-send",
    "senclaw-dispatch",
    "senclaw-memory",
    "senclaw-wiki",
    "senclaw-space",
    "senclaw-litho",
    "senclaw-browser",
    "senclaw-ocr",
    "senclaw-patterns",
    "senclaw-js",
    "senclaw-sandbox",
    "senclaw-profile",
];

/// Env var carrying [`CoreMcpParams::servers`] to the subprocess.
pub const CORE_SERVERS_ENV: &str = "SENCLAW_CORE_SERVERS";

/// One config that replaces the fifteen per-server ones: same command, same
/// env, a single `core-server` subprocess.
///
/// The env map is assembled by *calling the per-server builders above* and
/// merging what they produce, so a variable added to one of them reaches the
/// aggregated server without anyone remembering to update this function. The
/// keys don't collide — the servers that share one (`SENCLAW_DB_PATH`,
/// `SENCLAW_DEFAULT_WORKSPACE`) are given the same value anyway.
pub fn core_mcp_config(p: CoreMcpParams<'_>) -> McpServerConfig {
    // Registered name and the name the server reports in `serverInfo` must be
    // the same string, or the app's MCP screen shows a server it cannot match
    // to the one it configured. Share the constant rather than repeat it.
    let mut cfg = McpServerConfig::new(crate::mcp::core_server::SERVER_NAME, "core-server");
    let parts = [
        schedule_mcp_config(p.db_path, p.group_folder, p.chat_jid),
        background_mcp_config(p.db_path, p.group_folder, p.chat_jid),
        usage_mcp_config(p.db_path),
        workspace_mcp_config(
            p.workspace_state_file,
            p.default_workspace,
            p.allowed_work_dirs,
        ),
        send_mcp_config(p.send_bridge_port, p.chat_jid, p.bot_token, p.db_path),
        dispatch_mcp_config(
            p.dispatch_state_path,
            p.group_folder,
            p.chat_jid,
            Some(p.virtual_agents_dir),
        ),
        virtual_mcp_config(p.virtual_agents_dir, p.group_folder, p.default_workspace),
        memory_mcp_config(
            p.db_path,
            p.memory_folder,
            p.profiles_dir,
            p.embedding_provider,
            p.openai_api_key,
            p.openai_base_url,
            p.custom_memory_dir,
        ),
        wiki_mcp_config(p.wiki_dir),
        space_mcp_config(p.db_path, p.group_folder, p.chat_jid, p.ui_port),
        litho_mcp_config(
            p.litho_binary,
            p.openai_base_url,
            p.openai_api_key,
            p.litho_model_efficient,
        ),
        browser_mcp_config(p.ws_port, p.ui_port, p.agent_id, p.browser_v2),
        ocr_mcp_config(p.ui_port),
        patterns_mcp_config(p.ui_port),
        js_mcp_config(p.js_timeout_ms, p.js_memory_mb),
        sandbox_mcp_config(),
        user_profile_mcp_config(p.user_profile_path, p.chat_jid),
    ];
    for part in parts {
        cfg.env.extend(part.env);
    }
    cfg.env
        .insert(CORE_SERVERS_ENV.into(), p.servers.join(","));
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;


    fn sample_params<'a>(allowed: &'a [String]) -> CoreMcpParams<'a> {
        CoreMcpParams {
            db_path: "/data/db.sqlite",
            group_folder: "team-a",
            chat_jid: "tg:group:1",
            workspace_state_file: "/data/workspace-state.json",
            default_workspace: "/data/workspace",
            allowed_work_dirs: Some(allowed),
            profiles_dir: "/data/agents",
            memory_folder: "team-a",
            embedding_provider: Some("openai"),
            openai_api_key: Some("sk-test"),
            openai_base_url: Some("https://api.example/v1"),
            custom_memory_dir: None,
            dispatch_state_path: "/data/dispatch.json",
            virtual_agents_dir: "/data/virtual",
            wiki_dir: "/data/wiki",
            send_bridge_port: 18081,
            bot_token: Some("bot-token"),
            ws_port: 18789,
            agent_id: "tg:group:1",
            ui_port: 18788,
            browser_v2: false,
            litho_binary: "deepwiki-rs",
            litho_model_efficient: None,
            user_profile_path: "/data/USER.md",
            js_timeout_ms: 5_000,
            js_memory_mb: 128,
            servers: DEFAULT_CORE_SERVERS,
        }
    }

    /// The bundled config must carry everything the separate configs did —
    /// a variable that goes missing here silently drops that child's tools.
    #[test]
    fn bundled_config_is_a_superset_of_the_separate_ones() {
        let allowed = vec!["/data/workspace".to_string()];
        let cfg = core_mcp_config(sample_params(&allowed));

        let separate = [
            schedule_mcp_config("/data/db.sqlite", "team-a", "tg:group:1"),
            usage_mcp_config("/data/db.sqlite"),
            workspace_mcp_config(
                "/data/workspace-state.json",
                "/data/workspace",
                Some(&allowed),
            ),
            wiki_mcp_config("/data/wiki"),
            browser_mcp_config(18789, 18788, "tg:group:1", false),
            ocr_mcp_config(18788),
            space_mcp_config("/data/db.sqlite", "team-a", "tg:group:1", 18788),
            js_mcp_config(5_000, 128),
        ];
        for part in separate {
            for (key, value) in part.env {
                assert_eq!(
                    cfg.env.get(&key),
                    Some(&value),
                    "bundled config lost {key} (from {})",
                    part.name,
                );
            }
        }
    }

    /// The subprocess is told which children to host: env presence alone would
    /// hand a chat the virtual-agent server it never asked for, because
    /// dispatch and virtual read the same variables.
    #[test]
    fn bundled_config_names_the_children_it_wants() {
        let allowed = vec!["/data/workspace".to_string()];
        let cfg = core_mcp_config(sample_params(&allowed));

        let listed = cfg.env.get(CORE_SERVERS_ENV).expect("allowlist");
        assert!(listed.contains("senclaw-wiki"), "{listed}");
        assert!(listed.contains("senclaw-workspace"), "{listed}");
        assert!(!listed.contains("senclaw-virtual"), "{listed}");
        assert_eq!(cfg.args, vec!["core-server".to_string()]);
    }
}
