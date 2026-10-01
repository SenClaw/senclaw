//! Application configuration. Mirrors `src-old/config.ts`.
//!
//! Env vars are read once at process start via [`Config::from_env`]. Brand-
//! prefixed vars use `SENCLAW_*` (renamed from `SENCLAW_*`); platform-level
//! vars (`TELEGRAM_BOT_TOKEN`, `FEISHU_APP_ID`, …) are unchanged.
//! Default paths live under `~/.senclaw/` and `~/senclaw/`.

use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub agent_folder: String,
}

#[derive(Debug, Clone)]
pub struct FeishuConfig {
    pub app_id: String,
    pub app_secret: String,
    pub domain: String,
}

#[derive(Debug, Clone)]
pub struct WechatConfig {
    pub enabled: bool,
    pub api_base_url: String,
    pub agent_folder: String,
}

/// Note: `ADMIN_TELEGRAM_USER_ID` used to live here. It was parsed and never
/// read by anything — a config knob that looked like an allowlist and enforced
/// nothing, which is worse than an absent one. Telegram access is decided by
/// pairing approval now ([`crate::gateway::pairing`]).
#[derive(Debug, Clone)]
pub struct AdminConfig {
    pub feishu_open_id: String,
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub max_concurrent: u32,
    pub max_messages_per_group: u32,
}

/// Trust boundaries that are cheap to get wrong and expensive to get wrong.
/// Every field here defaults to the safe value; each override widens what
/// untrusted input may do.
#[derive(Debug, Clone)]
pub struct SecurityConfig {
    /// Let plugin-marketplace `hooks.json` files register `type: "command"`
    /// hooks — i.e. arbitrary `sh -c` at agent lifecycle events, with daemon
    /// privileges, from third-party code.
    ///
    /// Default **false**: a marketplace plugin's `command` string is never
    /// inspected, so allowing it is a supply-chain RCE and a
    /// restart-surviving persistence foothold. User-authored global and
    /// workspace `hooks.json` are unaffected — they keep full Command support.
    ///
    /// Override: `SENCLAW_ALLOW_MARKETPLACE_COMMAND_HOOKS=true`.
    /// See `docs/agent-security-hooks.md` §3.4(b) and §6.5.
    pub allow_marketplace_command_hooks: bool,

    /// Plugins allowed to register `type: "command"` hooks *by name*, after the
    /// operator has read that package's scan report.
    ///
    /// This is the per-package form of the flag above, and the one to reach for:
    /// `allow_marketplace_command_hooks` re-opens shell execution to every
    /// installed plugin at once, including ones installed later that nobody has
    /// looked at. Naming a package here consents to that package only.
    ///
    /// Override: `SENCLAW_MARKETPLACE_COMMAND_HOOK_PLUGINS=ecc,other-plugin`.
    pub marketplace_command_hook_plugins: Vec<String>,

    /// Statically scan a marketplace plugin or Space App package before the
    /// daemon executes anything it ships (`SENCLAW_SCAN_BEFORE_INSTALL`,
    /// default **true**).
    ///
    /// Installing is not passive: a Space App's `runtime.start` is run through
    /// `sh -c` the moment extraction finishes, and a plugin is enabled in the
    /// same call that clones it. The scan is the only point where a human sees
    /// those strings before they run.
    pub scan_before_install: bool,

    /// Lowest finding severity that refuses an install
    /// (`SENCLAW_SCAN_BLOCK_LEVEL`, default `critical`).
    ///
    /// Findings below it are reported and the install proceeds. Raising this to
    /// `high` also stops packages that merely declare an MCP server command;
    /// that is a defensible posture for a shared host but noisy for a personal
    /// one, which is why the default only stops the unambiguous cases.
    pub scan_block_level: crate::security::scan::Severity,
}

/// Written out rather than derived: `#[derive(Default)]` would make
/// `scan_before_install` false and silently turn the install gate off for any
/// caller that used `..Default::default()`. A security default has to be
/// stated, not inferred from the type.
impl Default for SecurityConfig {
    fn default() -> Self {
        SecurityConfig {
            allow_marketplace_command_hooks: false,
            marketplace_command_hook_plugins: Vec::new(),
            scan_before_install: true,
            scan_block_level: crate::security::scan::Severity::Critical,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub interval_sec: u64,
    pub notify_max_delay_minutes: u64,
}

/// Per-profile state directory inside `profiles_dir/{folder}/` (sessions, plans).
pub const AGENT_STATE_DIR: &str = ".sen";

#[derive(Debug, Clone)]
pub struct PathsConfig {
    pub db_path: PathBuf,
    /// Cognitive memory database — separate SQLite file from the main
    /// `db_path` so the user can wipe the cognitive graph (which is
    /// rebuildable from sources like SOUL.md / user chat) without
    /// touching irreplaceable data (channel messages, scheduled tasks).
    /// Defaults to a sibling `senclaw_cognitive.db` next to `db_path`.
    pub cognitive_db_path: PathBuf,
    pub profiles_dir: PathBuf,
    pub workspace_dir: PathBuf,
    /// Soul Core — who the *human* is (`~/.senclaw/USER.md`).
    ///
    /// Deliberately under `senclaw_home`, not `senclaw_data` where
    /// `profiles_dir` lives: the profile belongs to the person, not to any one
    /// agent profile, and keeping it out of `profiles_dir` is what stops
    /// `spawn_soul_watcher` / persona ingest from ever treating it as an
    /// agent's `SOUL.md`. See [`crate::user_profile`].
    pub user_profile_path: PathBuf,
    /// Machine-local environment notes (`~/.senclaw/TOOLS.md`) — SSH hosts,
    /// device names, TTS voices. Kept out of skills so skills stay shareable.
    pub tools_notes_path: PathBuf,
    /// User-editable operating rules (`~/.senclaw/AGENTS.md`), appended to the
    /// system prompt after the hardcoded base.
    pub agents_rules_path: PathBuf,
    pub global_config_path: PathBuf,
    pub dispatch_state_path: PathBuf,
    pub managed_skills_dir: PathBuf,
    pub managed_plugins_dir: PathBuf,
    pub wiki_dir: PathBuf,
    pub hooks_path: PathBuf,
    pub virtual_agents_dir: PathBuf,
    /// Optional bundled-skills dir; empty when unset (TS treats blank as disabled).
    pub bundled_skills_dir: Option<PathBuf>,
    pub workspace_templates_dir: PathBuf,
    /// Git clones of the scaffold templates `senclaw create` renders from.
    /// A cache, not state: deleting it costs one clone on the next create.
    pub scaffold_templates_dir: PathBuf,
    /// Marketplace configuration path
    pub marketplace_config_path: PathBuf,
    /// Marketplace state path
    pub marketplace_state_path: PathBuf,
    /// Marketplace git clones directory
    pub marketplace_clones_dir: PathBuf,
    /// Local model storage (MLX weights, tokenizers, configs).
    pub local_models_dir: PathBuf,
    /// Whisper ASR model storage, separate from LLM/local-model storage.
    pub whisper_models_dir: PathBuf,
    /// TTS (Text-to-Speech) model storage — separate from LLM and Whisper storage.
    /// Default: `~/.senclaw/tts-models`. Override with `SENCLAW_TTS_MODELS_DIR`.
    pub tts_models_dir: PathBuf,
    /// OCR model storage (PaddleOCR `.mnn` weights + keys file), separate from
    /// other model trees. Default: `~/.senclaw/ocr-models`. Override with
    /// `SENCLAW_OCR_MODELS_DIR`.
    pub ocr_models_dir: PathBuf,
    /// Screen captures taken from the desktop tray. Served read-only over
    /// `/api/space/screenshots/*`, so notes/events can reference a shot by URL.
    /// Default: `~/.senclaw/screenshots`. Override with
    /// `SENCLAW_SCREENSHOTS_DIR`.
    pub screenshots_dir: PathBuf,
    /// Documents attached to chat messages, kept per chat under
    /// `<root>/<sanitized jid>/`. The agent is handed the on-disk path
    /// alongside the extracted text so it can Read/grep the whole file when the
    /// inlined preview isn't enough. Default: `~/.senclaw/uploads`. Override
    /// with `SENCLAW_UPLOADS_DIR`.
    pub uploads_dir: PathBuf,
    /// Workflow definitions (`<name>.md` with YAML frontmatter).
    /// Default: `~/senclaw/workflows`. Override with `SENCLAW_WORKFLOWS_DIR`.
    pub workflows_dir: PathBuf,
    /// Persistent per-workflow workspaces (default root; each workflow gets
    /// `<root>/<name>/`). Default: `~/senclaw/workflow-data`. Override with
    /// `SENCLAW_WORKFLOW_DATA_DIR`.
    pub workflow_data_dir: PathBuf,
    /// Workflow run-history state file. Default:
    /// `~/.senclaw/workflow-runs.json`. Override with
    /// `SENCLAW_WORKFLOW_STATE_PATH`.
    pub workflow_state_path: PathBuf,
    /// Zen Kit state: `installed.json` (what each kit created, for removal)
    /// plus `hooks/<kit_id>.json` (hooks a kit registered — one file per kit so
    /// uninstalling is a delete, and the user's own `hooks.json` is never
    /// rewritten by a kit). Default: `~/.senclaw/kits`. Override with
    /// `SENCLAW_KITS_DIR`.
    pub kits_dir: PathBuf,
    /// Zen Patterns root: `sources.json`, the `user/` source, git checkouts
    /// under `sources/`, and shared `strategies/`. Default:
    /// `~/.senclaw/patterns`. Override with `SENCLAW_PATTERNS_DIR`.
    pub patterns_dir: PathBuf,
    /// Installed runtime packages: `<id>/<version>/senclaw-runtime.json` plus
    /// `settings.json`, `running.json`, `index-cache.json`. Default:
    /// `~/.senclaw/runtimes`. Override with `SENCLAW_RUNTIMES_DIR`.
    pub runtimes_dir: PathBuf,
    /// Per-runtime persistent data (each runtime's own `settings.json` and
    /// caches). Default: `~/.senclaw/runtime-data`. Override with
    /// `SENCLAW_RUNTIME_DATA_DIR`.
    pub runtime_data_dir: PathBuf,
    /// `<id>.log` / `<id>--<model-key>.log` per runtime, 5 MB with one
    /// rotation. Default: `~/.senclaw/logs/runtimes`. Override with
    /// `SENCLAW_RUNTIME_LOGS_DIR`.
    pub runtime_logs_dir: PathBuf,
    /// Read-only root of runtimes a desktop build ships alongside the daemon —
    /// a bundled package appears with `source: "bundled"`, shadowed by a
    /// user-installed copy of the same id+version. `SENCLAW_BUNDLED_RUNTIMES_DIR`,
    /// else `<dir of current_exe>/runtimes`; `None` when neither exists.
    pub bundled_runtimes_dir: Option<PathBuf>,
    /// Where the runtime catalog (`runtimes/index.json`) is fetched from.
    /// `SENCLAW_RUNTIME_INDEX_URL`, else the daemon's own repo on GitHub;
    /// `file://` is accepted (development, offline test fixtures).
    pub runtime_index_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingProvider {
    None,
    Openai,
    Openrouter,
    Ollama,
    Local,
}

impl EmbeddingProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Openai => "openai",
            Self::Openrouter => "openrouter",
            Self::Ollama => "ollama",
            Self::Local => "local",
        }
    }

    fn parse(raw: &str) -> Self {
        match raw {
            "openai" => Self::Openai,
            "openrouter" => Self::Openrouter,
            "ollama" => Self::Ollama,
            "local" => Self::Local,
            _ => Self::None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UiServerConfig {
    pub port: u16,
    pub ws_token: Option<String>,
    /// Bind host for the daemon's own HTTP UI (18788) *and* WS gateway
    /// (18789). Default `127.0.0.1`. Deliberately a separate knob from the
    /// Space-App `SENCLAW_BIND_HOST`: apps have no auth of their own, while
    /// the daemon requires the API token from non-loopback peers as soon as
    /// this is not a loopback host.
    pub bind_host: String,
    /// Optional API-token override. When unset the daemon uses (or creates)
    /// `~/.senclaw/api_token`. Whether it is *enforced* is
    /// [`UiServerConfig::auth_mode`].
    pub api_token: Option<String>,
    /// When the API token is demanded: `auto` (default — only when
    /// `bind_host` is non-loopback, and only from non-loopback peers),
    /// `always` (every peer, the only correct setting behind a same-host
    /// reverse proxy), or `off`. `SENCLAW_AUTH_MODE`; the operator can
    /// override it live from the UI.
    pub auth_mode: crate::gateway::ui_server::auth::AuthMode,
    /// Whether `SENCLAW_AUTH_MODE` was actually set — a value that merely
    /// equals the default must not be reported as configured.
    pub auth_mode_from_env: bool,
    /// `Secure` on the session cookie. `None` = infer from
    /// `X-Forwarded-Proto`. `SENCLAW_AUTH_COOKIE_SECURE`.
    pub cookie_secure: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct McpConfig {
    /// Timeout in seconds for MCP tool calls (default: 300 = 5 minutes)
    pub request_timeout_secs: u64,
    /// Watchdog check interval in seconds (default: 60 = 1 minute)
    pub watchdog_interval_secs: u64,
    /// Enable watchdog monitoring (default: true)
    pub watchdog_enabled: bool,
    /// Binary name or path for Litho (`deepwiki-rs`). Override with `SENCLAW_LITHO_BINARY`.
    pub litho_binary: String,
    /// Optional `--model-efficient` for Litho (`SENCLAW_LITHO_MODEL_EFFICIENT`).
    pub litho_model_efficient: String,
    /// Host every built-in MCP server in one `core-server` subprocess instead
    /// of one subprocess per server (default: true). `SENCLAW_MCP_BUNDLED=0`
    /// restores the per-server spawn — worth having, because bundled children
    /// share a process and a crash takes all of them down together.
    pub bundled: bool,
}

#[derive(Debug, Clone)]
pub struct MemoryConfig {
    pub embedding_provider: EmbeddingProvider,
    pub openai_api_key: String,
    pub openai_base_url: String,
    pub openai_model: String,
    pub openrouter_api_key: String,
    pub openrouter_base_url: String,
    pub openrouter_model: String,
    pub ollama_base_url: String,
    pub ollama_model: String,
    pub local_model_path: String,
    pub local_model: String,
    /// 0 means "use provider default" (see [`Config::resolve_dimensions`]).
    pub embedding_dimensions: u32,
    pub chunk_size: u32,
    pub chunk_overlap: u32,
    pub search_max_results: u32,
    pub search_min_score: f32,
    pub pre_retrieval: bool,
    /// Auto-cognify each user message into the cognitive graph on arrival.
    /// Fires in a background `tokio::spawn` so it never adds latency to the
    /// agent's reply. When disabled, the cognitive graph only grows via
    /// explicit `cog_add` / `cog_cognify` MCP tool calls.
    pub cognitive_reflection: bool,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub hub_url: String,
    pub channel_id: String,
    pub encryption_key: String,
    pub access_token: String,
}

/// Governance knobs for the cognitive memory layer.
///
/// Backstory: cognify runs an LLM per user message (via P14 auto-reflection)
/// + per CogAdd call. On a local model with thinking enabled (Qwen3, R1
/// family) one extraction can emit 2000+ tokens (mostly `<think>` reasoning)
/// and tie up the runtime for over a minute. Without limits, a busy chat can
/// queue 5+ concurrent cognify calls and saturate the engine. The knobs
/// below cap input size, total in-flight cognify calls, and output bytes so
/// the cognitive layer can't drown out the foreground agent.
#[derive(Debug, Clone)]
pub struct CognitiveConfig {
    /// Master switch. When false, cognify is short-circuited everywhere
    /// (CogAdd → ok with `llm_skipped`, reflection → no-op, SOUL ingest →
    /// chunk-only). Useful for slow local models where the cognitive layer
    /// would just queue forever.
    pub enabled: bool,
    /// Maximum cognify calls allowed to run at once. Acquired via semaphore
    /// in `CognitiveSystem::cognify`. When the cap is hit, new calls block
    /// until a permit frees. 1 = strictly serial.
    pub max_concurrent: usize,
    /// Hard cap on bytes the cognify LLM can stream back. Local-MLX
    /// adapters close the receiver once exceeded — Qwen3 `<think>` blocks
    /// that run away get cut at this byte budget instead of decoding to
    /// `eos`. The JSON we actually care about is < 1 KB; default 8 KB
    /// leaves ample headroom for reasoning preamble.
    pub max_output_chars: usize,
    /// Reflection (auto-cognify on user messages, P14) skips messages
    /// shorter than this — short ack/yes/no has no facts to extract.
    pub reflect_min_chars: usize,
    /// Reflection skips messages longer than this — a 10 KB paste would
    /// blow up the prompt token count. Caller can still CogAdd manually.
    pub reflect_max_chars: usize,
    /// Minimum interval between reflection *flushes* for the same agent.
    /// Defends against busy-chat storms. 0 = no cooldown.
    pub reflect_cooldown_ms: u64,
    /// Session-window idle timeout: after the last buffered turn, wait this
    /// long for the conversation to go quiet before flushing the window to
    /// cognify. Buffering several turns into one extraction call is what
    /// lets facts that span turns ("deadline khi nào?" → "tháng 8") reach
    /// the graph. 0 = flush per message (legacy behavior).
    pub reflect_window_idle_ms: u64,
    /// Cadence for the periodic maintenance sweep (cleanup junk +
    /// merge duplicate entities). `0` disables the sweep entirely; the
    /// user can still trigger it manually from the Settings UI.
    pub maintenance_interval_hours: u64,
}

impl Default for CognitiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Local LLMs don't parallelise well; 1 = serial cognify is the
            // safe default. Remote APIs can crank this up via env.
            max_concurrent: 1,
            // ~8 KB ≈ 2K tokens — enough for a `<think>` preamble + JSON.
            max_output_chars: 8 * 1024,
            reflect_min_chars: 20,
            // ~2 KB ≈ 500 tokens — anything longer is a paste, not a
            // sentence; user can still CogAdd it explicitly.
            reflect_max_chars: 2000,
            // 2 s cooldown lets multi-line replies in quick succession
            // queue without firing 5 cognifies back-to-back.
            reflect_cooldown_ms: 2000,
            // 2 min of silence closes a conversation window — long enough
            // to span a question→answer exchange, short enough that facts
            // land in the graph while the chat is still warm.
            reflect_window_idle_ms: 120_000,
            // Daily maintenance. Cheap on small graphs; bigger ones can
            // raise the cadence via the Settings UI.
            maintenance_interval_hours: 24,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub telegram: TelegramConfig,
    pub admin: AdminConfig,
    pub agent: AgentConfig,
    pub security: SecurityConfig,
    pub scheduler: SchedulerConfig,
    pub paths: PathsConfig,
    pub memory: MemoryConfig,
    pub cognitive: CognitiveConfig,
    pub ui_server: UiServerConfig,
    pub mcp: McpConfig,
    pub dispatch: DispatchConfig,
    pub background: BackgroundConfig,
    /// Space-App health supervisor interval in seconds (`SENCLAW_SPACE_SUPERVISE_SECS`).
    /// 0 disables the supervisor. Default 20. Only **background** apps are
    /// supervised — a session app being down is its resting state.
    pub space_supervise_secs: u64,
    /// How often the idle reaper looks for session apps to stop
    /// (`SENCLAW_SPACE_IDLE_SWEEP_SECS`). 0 disables it, which makes every
    /// session app behave like a background one once first used. Default 10 —
    /// the sweep is a lock and a subtraction per running app, and a coarser
    /// tick just makes the per-app `idleTimeoutSecs` less accurate.
    pub space_idle_sweep_secs: u64,
    /// Space-App API contract version this daemon advertises
    /// (`SENCLAW_API_VERSION`). Injected into every app process, sent back on
    /// every app-scoped response, and compared against the version an app
    /// declares. Override only to pin an older contract while debugging — the
    /// default is the compiled [`crate::apps::token::API_VERSION`].
    pub space_api_version: u32,
    /// What happens to an app-scoped request that carries no access token
    /// (`SENCLAW_APP_TOKEN_MODE`: `off` | `warn` | `strict`). Default
    /// **`strict`** — an app that does not prove who it is does not get another
    /// app's data. Set `warn` to find out what would break, `off` for an app
    /// that talks to the daemon with its own HTTP client and cannot be taught
    /// to send the token. A token that *is* present is verified and scoped in
    /// every mode. See [`crate::apps::token::TokenMode`] and
    /// docs/space-app-api-token.md.
    pub space_app_token_mode: crate::apps::token::TokenMode,
    /// Whether `SENCLAW_APP_TOKEN_MODE` was actually set. Without this a mode
    /// that merely equals the default would be reported to the UI as "the
    /// operator configured this", and the UI would offer no way back.
    pub space_app_token_mode_from_env: bool,
    pub ws_port: u16,
    /// POSIX shell override for workflow script steps
    /// (`SENCLAW_WORKFLOW_SHELL`). None = auto (`/bin/sh` on POSIX).
    pub workflow_shell: Option<String>,
    /// Plugin hub seeded as the default marketplace source on first run
    /// (`SENCLAW_HUB_URL`). Either the site root or the catalog document; a
    /// bare host gets `/marketplace.json` appended. Empty disables seeding.
    pub marketplace_hub_url: String,
}

/// BackgroundScheduler — autonomous work SenClaw runs by itself: periodic
/// upkeep, an App's standing duties, unattended follow-up. No chat session, no
/// reply. See `docs/background-tasks-design.md`.
#[derive(Debug, Clone)]
pub struct BackgroundConfig {
    /// Master switch (`SENCLAW_BACKGROUND_ENABLED`). On by default — core
    /// upkeep tasks live here, and the per-task `status` is the real switch.
    pub enabled: bool,
    /// Poll cadence in seconds (`SENCLAW_BACKGROUND_INTERVAL_SECS`), floored
    /// at 5.
    pub interval_secs: u64,
    /// Max background sessions at once (`SENCLAW_BACKGROUND_MAX_CONCURRENT`).
    pub max_concurrent: usize,
    /// Max concurrent runs for one owner (`SENCLAW_BACKGROUND_PER_OWNER`), so
    /// one App can't starve the rest. Matches `dispatch.per_assignee`'s intent.
    pub per_owner: usize,
    /// Default per-run timeout (`SENCLAW_BACKGROUND_TIMEOUT_SECS`).
    pub default_timeout_secs: u64,
    /// Default turn budget (`SENCLAW_BACKGROUND_MAX_TURNS`).
    pub max_agent_turns: usize,
    /// Run history retention in days (`SENCLAW_BACKGROUND_RETENTION_DAYS`).
    pub retention_days: i64,
    /// Max tasks one owner may register (`SENCLAW_BACKGROUND_MAX_TASKS_PER_OWNER`).
    pub max_tasks_per_owner: i64,
    /// Max *active* tasks per owner (`SENCLAW_BACKGROUND_MAX_ACTIVE_PER_OWNER`).
    pub max_active_per_owner: i64,
    /// Backoff ceiling in seconds (`SENCLAW_BACKGROUND_BACKOFF_MAX_SECS`).
    pub backoff_max_secs: i64,
}

/// MCPDispatcher — autonomously runs ready tasks from dispatch sources (e.g. the
/// Kanban Space App) through persona worker agents. Off by default.
#[derive(Debug, Clone)]
pub struct DispatchConfig {
    /// Master switch (`SENCLAW_DISPATCH_ENABLED`).
    pub enabled: bool,
    /// Poll cadence in seconds (`SENCLAW_DISPATCH_INTERVAL_SECS`).
    pub interval_secs: u64,
    /// Max worker agents at once (`SENCLAW_DISPATCH_MAX_CONCURRENT`).
    pub max_concurrent: usize,
    /// Max concurrent items per assignee (`SENCLAW_DISPATCH_PER_ASSIGNEE`).
    pub per_assignee: usize,
    /// Kanban app base URL to dispatch from (`SENCLAW_DISPATCH_KANBAN_URL`).
    /// Empty = no Kanban source.
    pub kanban_url: String,
    /// Cap on a worker's agent turns (`SENCLAW_DISPATCH_MAX_TURNS`).
    pub max_agent_turns: usize,
    /// Per-item run timeout (`SENCLAW_DISPATCH_TIMEOUT_SECS`).
    pub default_timeout_secs: u64,
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn env_or(key: &str, fallback: &str) -> String {
    env::var(key).unwrap_or_else(|_| fallback.to_owned())
}

fn env_bool(key: &str, fallback: bool) -> bool {
    match env::var(key) {
        Ok(v) => v == "true",
        Err(_) => fallback,
    }
}

/// Comma-separated list, trimmed, with blanks dropped. An unset or all-blank
/// variable yields an empty list, which for a security allowlist means "nothing
/// is allowed" — the safe reading.
fn env_csv(key: &str) -> Vec<String> {
    env::var(key)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn env_int<T: std::str::FromStr>(key: &str, fallback: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse::<T>().ok())
        .unwrap_or(fallback)
}

fn env_path(key: &str, fallback: PathBuf) -> PathBuf {
    match env::var(key) {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => fallback,
    }
}

impl Config {
    /// Read env vars (no `.env` loading — the binary entrypoint already calls
    /// `dotenvy::dotenv()`).
    pub fn from_env() -> Self {
        let h = home();
        let senclaw_home = h.join(".senclaw");
        let senclaw_data = h.join("senclaw");

        Self {
            telegram: TelegramConfig {
                bot_token: env_or("TELEGRAM_BOT_TOKEN", ""),
                agent_folder: env_or("TELEGRAM_AGENT_FOLDER", "main"),
            },
            admin: AdminConfig {
                feishu_open_id: env_or("ADMIN_FEISHU_OPEN_ID", ""),
            },
            agent: AgentConfig {
                max_concurrent: env_int("MAX_CONCURRENT_AGENTS", 5),
                max_messages_per_group: env_int("MAX_MESSAGES_PER_GROUP", 100),
            },
            security: SecurityConfig {
                allow_marketplace_command_hooks: env_bool(
                    "SENCLAW_ALLOW_MARKETPLACE_COMMAND_HOOKS",
                    false,
                ),
                marketplace_command_hook_plugins: env_csv(
                    "SENCLAW_MARKETPLACE_COMMAND_HOOK_PLUGINS",
                ),
                scan_before_install: env_bool("SENCLAW_SCAN_BEFORE_INSTALL", true),
                scan_block_level: std::env::var("SENCLAW_SCAN_BLOCK_LEVEL")
                    .ok()
                    .and_then(|v| crate::security::scan::Severity::parse(&v))
                    .unwrap_or(crate::security::scan::Severity::Critical),
            },
            scheduler: SchedulerConfig {
                interval_sec: env_int("SCHEDULER_INTERVAL_SEC", 60),
                notify_max_delay_minutes: env_int("NOTIFY_MAX_DELAY_MINUTES", 30),
            },
            paths: PathsConfig {
                db_path: env_path("DB_PATH", senclaw_home.join("senclaw.db")),
                cognitive_db_path: env_path(
                    "COGNITIVE_DB_PATH",
                    senclaw_home.join("senclaw_cognitive.db"),
                ),
                // `AGENTS_DIR` is the pre-0.1.3 name, still honoured.
                profiles_dir: env_path(
                    "PROFILES_DIR",
                    env_path("AGENTS_DIR", senclaw_data.join("profiles")),
                ),
                workspace_dir: env_path("WORKSPACE_DIR", senclaw_data.join("workspace")),
                user_profile_path: env_path(
                    "SENCLAW_USER_PROFILE_PATH",
                    senclaw_home.join("USER.md"),
                ),
                tools_notes_path: env_path(
                    "SENCLAW_TOOLS_NOTES_PATH",
                    senclaw_home.join("TOOLS.md"),
                ),
                agents_rules_path: env_path(
                    "SENCLAW_AGENTS_RULES_PATH",
                    senclaw_home.join("AGENTS.md"),
                ),
                global_config_path: env_path(
                    "SENCLAW_CONFIG_PATH",
                    senclaw_home.join("config.json"),
                ),
                dispatch_state_path: env_path(
                    "SENCLAW_DISPATCH_STATE_PATH",
                    senclaw_home.join("dispatch-state.json"),
                ),
                managed_skills_dir: env_path(
                    "MANAGED_SKILLS_DIR",
                    senclaw_home.join("managed").join("skills"),
                ),
                managed_plugins_dir: env_path(
                    "MANAGED_PLUGINS_DIR",
                    senclaw_home.join("managed").join("plugins"),
                ),
                wiki_dir: env_path("WIKI_DIR", senclaw_data.join("wiki")),
                hooks_path: env_path("SENCLAW_HOOKS_PATH", senclaw_home.join("hooks.json")),
                virtual_agents_dir: env_path(
                    "SENCLAW_VIRTUAL_AGENTS_DIR",
                    senclaw_data.join("virtual-agents"),
                ),
                bundled_skills_dir: {
                    let raw = env::var("SENCLAW_BUNDLED_SKILLS_DIR").ok();
                    let resolved: Option<PathBuf> = match raw {
                        Some(ref v) if !v.trim().is_empty() => {
                            let p = PathBuf::from(v);
                            if p.exists() {
                                Some(p)
                            } else {
                                None
                            }
                        }
                        _ => {
                            // Fallback: <project>/skills/ (mirrors TS __dirname + ../skills)
                            let project_skills =
                                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("skills");
                            if project_skills.exists() {
                                Some(project_skills)
                            } else {
                                None
                            }
                        }
                    };
                    resolved
                },
                workspace_templates_dir: env_path(
                    "SENCLAW_WORKSPACE_TEMPLATES_DIR",
                    senclaw_data.join("workspace-templates"),
                ),
                scaffold_templates_dir: env_path(
                    "SENCLAW_TEMPLATES_DIR",
                    senclaw_home.join("templates"),
                ),
                marketplace_config_path: env_path(
                    "SENCLAW_MARKETPLACE_CONFIG_PATH",
                    senclaw_home.join("marketplace.json"),
                ),
                marketplace_state_path: env_path(
                    "SENCLAW_MARKETPLACE_STATE_PATH",
                    senclaw_home.join("marketplace-state.json"),
                ),
                marketplace_clones_dir: env_path(
                    "SENCLAW_MARKETPLACE_CLONES_DIR",
                    senclaw_home.join("marketplace"),
                ),
                local_models_dir: env_path(
                    "SENCLAW_LOCAL_MODELS_DIR",
                    senclaw_home.join("local-models"),
                ),
                whisper_models_dir: env_path(
                    "SENCLAW_WHISPER_MODELS_DIR",
                    senclaw_home.join("whisper-models"),
                ),
                tts_models_dir: env_path("SENCLAW_TTS_MODELS_DIR", senclaw_home.join("tts-models")),
                ocr_models_dir: env_path("SENCLAW_OCR_MODELS_DIR", senclaw_home.join("ocr-models")),
                screenshots_dir: env_path(
                    "SENCLAW_SCREENSHOTS_DIR",
                    senclaw_home.join("screenshots"),
                ),
                uploads_dir: env_path("SENCLAW_UPLOADS_DIR", senclaw_home.join("uploads")),
                workflows_dir: env_path("SENCLAW_WORKFLOWS_DIR", senclaw_data.join("workflows")),
                workflow_data_dir: env_path(
                    "SENCLAW_WORKFLOW_DATA_DIR",
                    senclaw_data.join("workflow-data"),
                ),
                workflow_state_path: env_path(
                    "SENCLAW_WORKFLOW_STATE_PATH",
                    senclaw_home.join("workflow-runs.json"),
                ),
                kits_dir: env_path("SENCLAW_KITS_DIR", senclaw_home.join("kits")),
                patterns_dir: env_path(
                    "SENCLAW_PATTERNS_DIR",
                    senclaw_home.join("patterns"),
                ),
                runtimes_dir: env_path("SENCLAW_RUNTIMES_DIR", senclaw_home.join("runtimes")),
                runtime_data_dir: env_path(
                    "SENCLAW_RUNTIME_DATA_DIR",
                    senclaw_home.join("runtime-data"),
                ),
                runtime_logs_dir: env_path(
                    "SENCLAW_RUNTIME_LOGS_DIR",
                    senclaw_home.join("logs").join("runtimes"),
                ),
                bundled_runtimes_dir: {
                    let raw = env::var("SENCLAW_BUNDLED_RUNTIMES_DIR").ok();
                    let resolved: Option<PathBuf> = match raw {
                        Some(ref v) if !v.trim().is_empty() => Some(PathBuf::from(v)),
                        _ => env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("runtimes"))),
                    };
                    resolved.filter(|p| p.is_dir())
                },
                runtime_index_url: env::var("SENCLAW_RUNTIME_INDEX_URL")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| crate::runtime::index::DEFAULT_INDEX_URL.to_string()),
            },
            memory: MemoryConfig {
                embedding_provider: EmbeddingProvider::parse(&env_or(
                    "SENCLAW_EMBEDDING_PROVIDER",
                    "none",
                )),
                openai_api_key: env_or("SENCLAW_OPENAI_API_KEY", ""),
                openai_base_url: env_or("SENCLAW_OPENAI_BASE_URL", "https://api.openai.com/v1"),
                openai_model: env_or("SENCLAW_OPENAI_MODEL", "text-embedding-3-small"),
                openrouter_api_key: env_or("SENCLAW_OPENROUTER_API_KEY", ""),
                openrouter_base_url: env_or(
                    "SENCLAW_OPENROUTER_BASE_URL",
                    "https://openrouter.ai/api/v1",
                ),
                openrouter_model: env_or(
                    "SENCLAW_OPENROUTER_MODEL",
                    "openai/text-embedding-3-small",
                ),
                ollama_base_url: env_or("SENCLAW_OLLAMA_BASE_URL", "http://localhost:11434"),
                ollama_model: env_or("SENCLAW_OLLAMA_MODEL", "nomic-embed-text"),
                local_model_path: env_or("SENCLAW_LOCAL_MODEL_PATH", ""),
                local_model: env_or("SENCLAW_LOCAL_MODEL", ""),
                embedding_dimensions: env_int("SENCLAW_EMBEDDING_DIMENSIONS", 0),
                chunk_size: env_int("SENCLAW_CHUNK_SIZE", 400),
                chunk_overlap: env_int("SENCLAW_CHUNK_OVERLAP", 80),
                search_max_results: env_int("SENCLAW_SEARCH_MAX_RESULTS", 5),
                search_min_score: env_int("SENCLAW_SEARCH_MIN_SCORE", 0.5_f32),
                pre_retrieval: env_bool("SENCLAW_PRE_RETRIEVAL", false),
                cognitive_reflection: env_bool("SENCLAW_COGNITIVE_REFLECTION", true),
            },
            cognitive: CognitiveConfig {
                enabled: env_bool("SENCLAW_COGNITIVE_ENABLED", true),
                max_concurrent: env_int::<usize>("SENCLAW_COGNITIVE_MAX_CONCURRENT", 1).max(1),
                max_output_chars: env_int::<usize>("SENCLAW_COGNITIVE_MAX_OUTPUT_CHARS", 8 * 1024)
                    .max(256),
                reflect_min_chars: env_int::<usize>("SENCLAW_COGNITIVE_REFLECT_MIN_CHARS", 20),
                reflect_max_chars: env_int::<usize>("SENCLAW_COGNITIVE_REFLECT_MAX_CHARS", 2000)
                    .max(100),
                reflect_cooldown_ms: env_int::<u64>("SENCLAW_COGNITIVE_REFLECT_COOLDOWN_MS", 2000),
                reflect_window_idle_ms: env_int::<u64>(
                    "SENCLAW_COGNITIVE_REFLECT_WINDOW_IDLE_MS",
                    120_000,
                ),
                maintenance_interval_hours: env_int::<u64>(
                    "SENCLAW_COGNITIVE_MAINTENANCE_HOURS",
                    24,
                ),
            },
            ui_server: UiServerConfig {
                port: env_int("SENCLAW_UI_PORT", 18788),
                ws_token: match env::var("SENCLAW_WS_TOKEN") {
                    Ok(v) if !v.trim().is_empty() => Some(v),
                    _ => None,
                },
                bind_host: match env::var("SENCLAW_UI_BIND_HOST") {
                    Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
                    _ => "127.0.0.1".to_string(),
                },
                api_token: match env::var("SENCLAW_API_TOKEN") {
                    Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
                    _ => None,
                },
                auth_mode: match env::var("SENCLAW_AUTH_MODE") {
                    Ok(v) if !v.trim().is_empty() => {
                        crate::gateway::ui_server::auth::AuthMode::from_env_value(&v)
                    }
                    _ => crate::gateway::ui_server::auth::DEFAULT_AUTH_MODE,
                },
                auth_mode_from_env: env::var("SENCLAW_AUTH_MODE")
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false),
                cookie_secure: match env::var("SENCLAW_AUTH_COOKIE_SECURE") {
                    Ok(v) if !v.trim().is_empty() => Some(matches!(
                        v.trim().to_ascii_lowercase().as_str(),
                        "1" | "true" | "yes" | "on"
                    )),
                    _ => None,
                },
            },
            mcp: McpConfig {
                request_timeout_secs: env_int("SENCLAW_MCP_REQUEST_TIMEOUT_SECS", 300),
                watchdog_interval_secs: env_int("SENCLAW_MCP_WATCHDOG_INTERVAL_SECS", 60),
                watchdog_enabled: env_bool("SENCLAW_MCP_WATCHDOG_ENABLED", true),
                litho_binary: env_or("SENCLAW_LITHO_BINARY", "deepwiki-rs"),
                litho_model_efficient: env_or("SENCLAW_LITHO_MODEL_EFFICIENT", ""),
                bundled: env_bool("SENCLAW_MCP_BUNDLED", true),
            },
            dispatch: DispatchConfig {
                enabled: env_bool("SENCLAW_DISPATCH_ENABLED", false),
                interval_secs: env_int("SENCLAW_DISPATCH_INTERVAL_SECS", 30),
                max_concurrent: env_int("SENCLAW_DISPATCH_MAX_CONCURRENT", 3),
                per_assignee: env_int("SENCLAW_DISPATCH_PER_ASSIGNEE", 1),
                kanban_url: env_or("SENCLAW_DISPATCH_KANBAN_URL", "http://127.0.0.1:4400"),
                max_agent_turns: env_int("SENCLAW_DISPATCH_MAX_TURNS", 40),
                default_timeout_secs: env_int("SENCLAW_DISPATCH_TIMEOUT_SECS", 600),
            },
            background: BackgroundConfig {
                enabled: env_bool("SENCLAW_BACKGROUND_ENABLED", true),
                interval_secs: env_int("SENCLAW_BACKGROUND_INTERVAL_SECS", 20),
                max_concurrent: env_int("SENCLAW_BACKGROUND_MAX_CONCURRENT", 3),
                per_owner: env_int("SENCLAW_BACKGROUND_PER_OWNER", 1),
                default_timeout_secs: env_int("SENCLAW_BACKGROUND_TIMEOUT_SECS", 300),
                max_agent_turns: env_int("SENCLAW_BACKGROUND_MAX_TURNS", 40),
                retention_days: env_int("SENCLAW_BACKGROUND_RETENTION_DAYS", 30),
                max_tasks_per_owner: env_int("SENCLAW_BACKGROUND_MAX_TASKS_PER_OWNER", 20),
                max_active_per_owner: env_int("SENCLAW_BACKGROUND_MAX_ACTIVE_PER_OWNER", 10),
                backoff_max_secs: env_int("SENCLAW_BACKGROUND_BACKOFF_MAX_SECS", 3600),
            },
            space_supervise_secs: env_int("SENCLAW_SPACE_SUPERVISE_SECS", 20),
            space_idle_sweep_secs: env_int("SENCLAW_SPACE_IDLE_SWEEP_SECS", 10),
            space_api_version: env_int("SENCLAW_API_VERSION", crate::apps::token::API_VERSION)
                .max(crate::apps::token::MIN_API_VERSION),
            space_app_token_mode: crate::apps::token::TokenMode::from_env_value(
                &env::var("SENCLAW_APP_TOKEN_MODE").unwrap_or_default(),
            ),
            space_app_token_mode_from_env: env::var("SENCLAW_APP_TOKEN_MODE")
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false),
            ws_port: env_int("SENCLAW_WS_PORT", 18789),
            workflow_shell: env::var("SENCLAW_WORKFLOW_SHELL")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            marketplace_hub_url: env::var("SENCLAW_HUB_URL")
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| crate::marketplace::DEFAULT_HUB_URL.to_string()),
        }
    }

    /// Resolve embedding vector dimensions for a given provider, honoring the
    /// user override when present. Mirrors `resolveDimensions` in db.ts.
    pub fn resolve_dimensions(provider: EmbeddingProvider, configured: u32) -> u32 {
        if configured > 0 {
            return configured;
        }
        match provider {
            EmbeddingProvider::Local => 384,
            _ => 1536,
        }
    }

    /// Layer the persisted Settings → Embedding UI choices on top of the
    /// env-derived defaults. Env still wins when set explicitly; this only
    /// fills in `memory.*` fields the user has chosen in the UI.
    ///
    /// **Call this at daemon boot** (`run_daemon`) — it's the bridge between
    /// the persisted JSON config and the in-memory `Config`. Without it the
    /// Settings page silently writes a file the daemon never reads.
    pub fn apply_persisted_overrides(&mut self, global_config_path: &std::path::Path) {
        // Embedding provider + per-provider credentials.
        if let Some(ec) = crate::gateway::group_manager::load_embedding_config(global_config_path) {
            // Provider — only override when the user actually picked one.
            let parsed = EmbeddingProvider::parse(&ec.provider);
            if parsed != EmbeddingProvider::None || !ec.provider.is_empty() && ec.provider != "none"
            {
                self.memory.embedding_provider = parsed;
            }

            // Per-provider fields. `skip_serializing_if = "String::is_empty"`
            // on the source struct means empty strings *are* meaningful here
            // — they mean "use env default", so we only patch when non-empty.
            if !ec.api_key.is_empty() {
                match parsed {
                    EmbeddingProvider::Openai => self.memory.openai_api_key = ec.api_key.clone(),
                    EmbeddingProvider::Openrouter => {
                        self.memory.openrouter_api_key = ec.api_key.clone()
                    }
                    _ => {}
                }
            }
            if !ec.base_url.is_empty() {
                match parsed {
                    EmbeddingProvider::Openai => self.memory.openai_base_url = ec.base_url.clone(),
                    EmbeddingProvider::Openrouter => {
                        self.memory.openrouter_base_url = ec.base_url.clone()
                    }
                    EmbeddingProvider::Ollama => self.memory.ollama_base_url = ec.base_url.clone(),
                    _ => {}
                }
            }
            if !ec.model_name.is_empty() {
                match parsed {
                    EmbeddingProvider::Openai => self.memory.openai_model = ec.model_name.clone(),
                    EmbeddingProvider::Openrouter => {
                        self.memory.openrouter_model = ec.model_name.clone()
                    }
                    EmbeddingProvider::Ollama => self.memory.ollama_model = ec.model_name.clone(),
                    EmbeddingProvider::Local => self.memory.local_model = ec.model_name.clone(),
                    _ => {}
                }
            }
            if !ec.model_path.is_empty() && parsed == EmbeddingProvider::Local {
                self.memory.local_model_path = ec.model_path.clone();
            }
            if let Some(d) = ec.dimensions {
                if d > 0 {
                    self.memory.embedding_dimensions = d;
                }
            }
        }

        // Cognitive governance knobs. Same precedence as embedding: env
        // wins when explicitly set; UI fills in everything else.
        if let Some(cc) = crate::gateway::group_manager::load_cognitive_config(global_config_path) {
            if let Some(v) = cc.enabled {
                self.cognitive.enabled = v;
            }
            if let Some(v) = cc.max_concurrent {
                self.cognitive.max_concurrent = v.max(1);
            }
            if let Some(v) = cc.max_output_chars {
                self.cognitive.max_output_chars = v.max(256);
            }
            if let Some(v) = cc.reflect_min_chars {
                self.cognitive.reflect_min_chars = v;
            }
            if let Some(v) = cc.reflect_max_chars {
                self.cognitive.reflect_max_chars = v.max(100);
            }
            if let Some(v) = cc.reflect_cooldown_ms {
                self.cognitive.reflect_cooldown_ms = v;
            }
            if let Some(v) = cc.reflect_window_idle_ms {
                self.cognitive.reflect_window_idle_ms = v;
            }
            if let Some(v) = cc.auto_reflection {
                self.memory.cognitive_reflection = v;
            }
            if let Some(v) = cc.maintenance_interval_hours {
                self.cognitive.maintenance_interval_hours = v;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_provider_parses() {
        assert_eq!(
            EmbeddingProvider::parse("openai"),
            EmbeddingProvider::Openai
        );
        assert_eq!(EmbeddingProvider::parse("local"), EmbeddingProvider::Local);
        assert_eq!(EmbeddingProvider::parse("garbage"), EmbeddingProvider::None);
    }

    #[test]
    fn resolve_dimensions_uses_override_when_positive() {
        assert_eq!(
            Config::resolve_dimensions(EmbeddingProvider::Openai, 3072),
            3072
        );
    }

    #[test]
    fn resolve_dimensions_local_default_384() {
        assert_eq!(Config::resolve_dimensions(EmbeddingProvider::Local, 0), 384);
    }

    #[test]
    fn resolve_dimensions_other_default_1536() {
        assert_eq!(
            Config::resolve_dimensions(EmbeddingProvider::Openai, 0),
            1536
        );
        assert_eq!(
            Config::resolve_dimensions(EmbeddingProvider::Ollama, 0),
            1536
        );
    }
}
