use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "senclaw",
    version = senclaw::build_info::CLAP_VERSION,
    about = "SenClaw — multi-group AI gateway"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the SenClaw daemon (default when no subcommand is given)
    Start,
    /// Show version and build information (release, commit, build time, target)
    Version,
    /// Manage local skills
    Skills {
        #[command(subcommand)]
        cmd: senclaw::cli::commands::skills::SkillsCmd,
    },
    /// Interact with ClawHub
    Clawhub {
        #[command(subcommand)]
        cmd: senclaw::cli::commands::clawhub::ClawhubCmd,
    },
    /// Publish Space Apps to the senclaw hub
    Hub {
        #[command(subcommand)]
        cmd: senclaw::cli::commands::hub::HubCmd,
    },
    /// Manage plugin marketplace sources and the hub store
    Marketplace {
        #[command(subcommand)]
        cmd: senclaw::cli::commands::marketplace::MarketplaceCmd,
    },
    /// Security-scan a plugin directory or Space App zip without installing it
    Scan(senclaw::cli::commands::scan::ScanCmd),
    /// Scaffold a Space App, a skill or a sub-agent from a template
    Create {
        #[command(subcommand)]
        cmd: senclaw::cli::commands::create::CreateCmd,
    },
    /// Manage Feishu wiki
    Wiki {
        #[command(subcommand)]
        cmd: senclaw::cli::commands::wiki::WikiCmd,
    },
    /// Manage messaging channels
    Channel {
        #[command(subcommand)]
        cmd: senclaw::cli::commands::channel::ChannelCmd,
    },
    /// Speak the Agent Client Protocol on stdio (Zed / JetBrains / Kiro) against the running daemon
    Acp(senclaw::cli::commands::acp::AcpCmd),
    /// Approve chats asking to connect to a channel (pairing codes)
    Pairing {
        #[command(subcommand)]
        cmd: senclaw::cli::commands::pairing::PairingCmd,
    },
    /// Run a one-shot disposable agent task (for hook scripts: reflection / summarization / analysis).
    AgentTask(senclaw::cli::commands::agent_task::AgentTaskCmd),
    /// List / inspect / run saved workflows (DAGs of agent + script steps)
    Workflow {
        #[command(subcommand)]
        cmd: senclaw::cli::commands::workflow::WorkflowCmd,
    },
    /// Download the Web UI bundle (first run only) and start the daemon serving it
    Web {
        /// Re-download the Web UI bundle even if present
        #[arg(long)]
        force: bool,
        /// Release tag to download from (e.g. v0.3.0). Default: latest.
        #[arg(long)]
        version: Option<String>,
    },
    /// Install a component that does not ship in this binary (the native desktop app)
    Install {
        #[command(subcommand)]
        cmd: senclaw::cli::commands::desktop::InstallCmd,
    },
    /// Remove a component installed by `senclaw install`
    Uninstall {
        #[command(subcommand)]
        cmd: senclaw::cli::commands::desktop::UninstallCmd,
    },
    /// Update SenClaw to the latest version (binary + Web UI bundle if present)
    Update {
        /// Release tag to update to (e.g. v0.3.0). Default: latest.
        #[arg(long)]
        version: Option<String>,
    },
    /// Manage engine runtimes (GGUF/MLX, decision, OCR, Whisper, TTS) — install,
    /// select, inspect. See docs/runtime-protocol.md.
    Runtime {
        #[command(subcommand)]
        cmd: senclaw::cli::commands::runtime::RuntimeCmd,
    },

    // ===== MCP servers (spawned as subprocesses by sema-core) =====
    /// Start the schedule MCP server (stdio JSON-RPC)
    ScheduleServer,
    /// Start the background-tasks MCP server (stdio JSON-RPC)
    BackgroundServer,
    /// Start the workspace MCP server (stdio JSON-RPC)
    WorkspaceServer,
    /// Start the memory MCP server (stdio JSON-RPC)
    MemoryServer,
    /// Start the send MCP server (stdio JSON-RPC)
    SendServer,
    /// Start the dispatch MCP server (stdio JSON-RPC)
    DispatchServer,
    /// Start the virtual agent MCP server (stdio JSON-RPC)
    VirtualServer,
    /// Start the admin MCP server (stdio JSON-RPC)
    AdminServer,
    /// Start the Wiki MCP server — Feishu/Lark (stdio JSON-RPC)
    WikiServer,
    /// Start the project-docs MCP server (stdio JSON-RPC)
    DocsServer,
    /// Start the browser MCP server (stdio JSON-RPC)
    BrowserServer,
    /// Start the Space MCP server — notes, calendar, email, sync (stdio JSON-RPC)
    SpaceServer,
    /// Start the Litho (deepwiki-rs) MCP server (stdio JSON-RPC)
    LithoServer,
    /// Start the cognitive memory MCP server — graph + Hebbian (stdio JSON-RPC)
    CognitiveServer,
    /// Start the OCR MCP server — PaddleOCR + MNN (stdio JSON-RPC)
    OcrServer,
    PatternsServer,
    /// Start the sandboxed JavaScript executor MCP server (stdio JSON-RPC)
    JsServer,
    /// Start the OS-sandbox MCP server — sbx_* tools (stdio JSON-RPC)
    SandboxServer,
    /// Start the token-usage accounting MCP server (stdio JSON-RPC)
    UsageServer,
    /// Start the Soul Core MCP server — profile_* tools over USER.md (stdio JSON-RPC)
    UserProfileServer,
    /// Internal: brush (rust-bash) sandbox child — reads a script from stdin,
    /// runs it in-process, writes output to <dir>/stdout|stderr, exits with the
    /// script's code. Spawned by the code REPL so the timeout is kill-enforced.
    #[command(hide = true)]
    BrushSandbox {
        /// Parent-provided working/output directory.
        dir: std::path::PathBuf,
    },
    /// Train the GraphSAGE re-ranker on the current cognitive graph.
    /// Writes weights to ~/.senclaw/cognitive/sage_<dim>.bin.
    CognitiveTrain {
        /// Training epochs. Default 20.
        #[arg(long, default_value_t = 20)]
        epochs: usize,
        /// Learning rate. Default 1e-3.
        #[arg(long, default_value_t = 1e-3)]
        lr: f32,
        /// Negative samples per positive edge. Default 3.
        #[arg(long, default_value_t = 3)]
        neg_per_pos: usize,
        /// Maximum nodes to include from the graph. None = all.
        #[arg(long)]
        max_nodes: Option<usize>,
    },
    /// Run one cognitive maintenance sweep on demand: cleanup junk, merge
    /// duplicate entities, and infer associative links. Safe while running.
    CognitiveMaintain,
    /// Start the built-in Kanban board MCP server (stdio JSON-RPC). Native —
    /// talks to the Kanban DB directly.
    KanbanServer,
    /// Start the aggregated Zen Kit MCP server (stdio JSON-RPC) — hosts every
    /// built-in server (wiki, workspace, memory, …) whose env is present, in
    /// one process instead of one subprocess each.
    CoreServer,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();

    // MCP servers MUST log to stderr (stdout is reserved for JSON-RPC).
    // For daemon and CLI commands, stdout logging is fine.
    let is_mcp = matches!(
        cli.command,
        Some(
            Command::ScheduleServer
                | Command::BackgroundServer
                | Command::WorkspaceServer
                | Command::MemoryServer
                | Command::SendServer
                | Command::DispatchServer
                | Command::VirtualServer
                | Command::AdminServer
                | Command::WikiServer
                | Command::DocsServer
                | Command::BrowserServer
                | Command::SpaceServer
                | Command::LithoServer
                | Command::CognitiveServer
                | Command::OcrServer
                | Command::PatternsServer
                | Command::JsServer
                | Command::UsageServer
                | Command::UserProfileServer
                | Command::BrushSandbox { .. }
                | Command::KanbanServer
                | Command::CoreServer
        )
    );

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    if is_mcp {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    match cli.command.unwrap_or(Command::Start) {
        Command::Version => {
            println!("{}", senclaw::build_info::pretty());
            Ok(())
        }
        Command::Start => {
            let mut cfg = senclaw::config::Config::from_env();
            // Settings UI persists embedding choices to global_config.json.
            // Layer them on top of env so the UI actually drives runtime.
            let gcp = cfg.paths.global_config_path.clone();
            cfg.apply_persisted_overrides(&gcp);
            senclaw::run_daemon(cfg).await
        }
        Command::Skills { cmd } => senclaw::cli::commands::skills::run(cmd).await,
        Command::Clawhub { cmd } => senclaw::cli::commands::clawhub::run(cmd).await,
        Command::Hub { cmd } => senclaw::cli::commands::hub::run(cmd).await,
        Command::Marketplace { cmd } => senclaw::cli::commands::marketplace::run(cmd).await,
        Command::Scan(cmd) => senclaw::cli::commands::scan::run(cmd).await,
        Command::Create { cmd } => senclaw::cli::commands::create::run(cmd).await,
        Command::Wiki { cmd } => senclaw::cli::commands::wiki::run(cmd).await,
        Command::Channel { cmd } => senclaw::cli::commands::channel::run(cmd).await,
        Command::Pairing { cmd } => senclaw::cli::commands::pairing::run(cmd).await,
        Command::Acp(cmd) => senclaw::cli::commands::acp::run(cmd).await,
        Command::AgentTask(cmd) => senclaw::cli::commands::agent_task::run(cmd).await,
        Command::Workflow { cmd } => senclaw::cli::commands::workflow::run(cmd).await,
        Command::Web { force, version } => {
            senclaw::cli::commands::distrib::run_web(force, version).await
        }
        Command::Install { cmd } => senclaw::cli::commands::desktop::run_install(cmd).await,
        Command::Uninstall { cmd } => senclaw::cli::commands::desktop::run_uninstall(cmd).await,
        Command::Update { version } => senclaw::cli::commands::distrib::run_update(version).await,
        Command::Runtime { cmd } => senclaw::cli::commands::runtime::run(cmd).await,

        // MCP servers
        Command::ScheduleServer => senclaw::mcp::schedule_server::run_stdio_server().await,
        Command::BackgroundServer => senclaw::mcp::background_server::run_stdio_server().await,
        Command::WorkspaceServer => senclaw::mcp::workspace_server::run_stdio_server().await,
        Command::MemoryServer => senclaw::mcp::memory_server::run_stdio_server().await,
        Command::SendServer => senclaw::mcp::send_server::run_stdio_server().await,
        Command::DispatchServer => senclaw::mcp::dispatch_server::run_stdio_server().await,
        Command::VirtualServer => senclaw::mcp::virtual_server::run_stdio_server().await,
        Command::AdminServer => senclaw::mcp::admin_server::run_stdio_server().await,
        Command::WikiServer => senclaw::mcp::wiki_server::run_stdio_server().await,
        Command::DocsServer => senclaw::mcp::docs_server::run_stdio_server().await,
        Command::BrowserServer => senclaw::mcp::browser_server::run_stdio_server().await,
        Command::SpaceServer => senclaw::mcp::space_server::run_stdio_server().await,
        Command::LithoServer => senclaw::mcp::litho_server::run_stdio_server().await,
        Command::CognitiveServer => senclaw::mcp::cognitive_server::run_stdio_server().await,
        Command::OcrServer => senclaw::mcp::ocr_server::run_stdio_server().await,
        Command::PatternsServer => senclaw::mcp::patterns_server::run_stdio_server().await,
        Command::JsServer => senclaw::mcp::js_server::run_stdio_server().await,
        Command::SandboxServer => senclaw::mcp::sandbox_server::run_stdio_server().await,
        Command::UsageServer => senclaw::mcp::usage_server::run_stdio_server().await,
        Command::UserProfileServer => {
            senclaw::mcp::user_profile_server::run_stdio_server().await
        }
        Command::BrushSandbox { dir } => {
            senclaw::gateway::ui_server::bash_sandbox::child_main(&dir).await
        }
        Command::CognitiveTrain {
            epochs,
            lr,
            neg_per_pos,
            max_nodes,
        } => senclaw::cli::commands::cognitive::train(epochs, lr, neg_per_pos, max_nodes).await,
        Command::CognitiveMaintain => senclaw::cli::commands::cognitive::maintain().await,
        Command::KanbanServer => senclaw::kanban::mcp::run_stdio_server().await,
        Command::CoreServer => senclaw::mcp::core_server::run_stdio_server().await,
    }
}
