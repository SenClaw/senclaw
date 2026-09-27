//! `senclaw create …` — scaffold a Space App, a skill or a sub-agent.
//!
//! The three subcommands share one engine ([`crate::scaffold`]) and differ only
//! in which template they reach for and where the result lands. Everything the
//! engine needs can be passed as a flag, so this is usable from a script; the
//! flags that are usually left off (`--port`, `--id`, `--icon`) are derived.

use anyhow::Result;
use clap::{Args, Subcommand};
use std::path::PathBuf;

use crate::config::Config;
use crate::scaffold::{self, source, spec::Kind, CreateRequest};

#[derive(Subcommand, Debug)]
pub enum CreateCmd {
    /// Create a Space App from a template (rust | go | node | python)
    App(AppArgs),
    /// Create a skill (SKILL.md the agent can load)
    Skill(NewArgs),
    /// Create a sub-agent persona (.md used by dispatch / virtual workers)
    #[command(alias = "subagent", alias = "persona")]
    SubAgent(NewArgs),
    /// List the templates available (from the repo, plus the built-in ones)
    List {
        /// Do not contact the templates repo; list only what is built in
        #[arg(long)]
        offline: bool,
        /// Templates repo. Default: SENCLAW_TEMPLATES_REPO, else the official one
        #[arg(long)]
        repo: Option<String>,
        /// Branch or tag to read templates from. Default: main
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Refresh the local clone of the templates repo
    Update {
        /// Templates repo. Default: SENCLAW_TEMPLATES_REPO, else the official one
        #[arg(long)]
        repo: Option<String>,
        /// Branch or tag to read templates from. Default: main
        #[arg(long = "ref")]
        git_ref: Option<String>,
    },
}

/// The app-only flags, on top of [`NewArgs`].
///
/// Split out so `create skill --help` does not advertise `--lang` and `--port`,
/// which it would then ignore without a word.
#[derive(Args, Debug)]
pub struct AppArgs {
    #[command(flatten)]
    pub common: NewArgs,

    /// Language of the app. Default: rust.
    #[arg(long, value_name = "rust|go|node|python")]
    pub lang: Option<String>,

    /// Port for the app. Default: the first free one from 4800.
    #[arg(long)]
    pub port: Option<u16>,
}

#[derive(Args, Debug)]
pub struct NewArgs {
    /// Name. `"My Todo"` and `my-todo` both work; the id is slugified from it.
    pub name: String,

    /// Template name in the repo, or a path to a local template directory.
    #[arg(long, short = 't')]
    pub template: Option<String>,

    /// Override the slug derived from the name.
    #[arg(long)]
    pub id: Option<String>,

    /// Where to write. Default: ./<id> for an app, the live directory for a
    /// skill or a sub-agent.
    #[arg(long, short = 'd')]
    pub dir: Option<PathBuf>,

    /// One-line description, used in the manifest and the UI.
    #[arg(long = "desc")]
    pub description: Option<String>,

    /// Emoji shown in the Space Apps list.
    #[arg(long)]
    pub icon: Option<String>,

    /// Extra template variable, repeatable: `--var key=value`.
    #[arg(long = "var", value_name = "KEY=VALUE")]
    pub vars: Vec<String>,

    /// Templates repo (default: the official one, or SENCLAW_TEMPLATES_REPO).
    #[arg(long)]
    pub repo: Option<String>,

    /// Branch or tag to read templates from.
    #[arg(long = "ref")]
    pub git_ref: Option<String>,

    /// Do not contact the templates repo; use the built-in templates.
    #[arg(long)]
    pub offline: bool,

    /// Re-clone the templates repo before rendering.
    #[arg(long, conflicts_with = "offline")]
    pub refresh: bool,

    /// Write into a directory that already has files.
    #[arg(long)]
    pub force: bool,

    /// Render and validate, print what would be written, write nothing.
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(cmd: CreateCmd) -> Result<()> {
    let config = Config::from_env();
    match cmd {
        CreateCmd::App(args) => create(Kind::App, args.common, args.lang, args.port, &config),
        CreateCmd::Skill(args) => create(Kind::Skill, args, None, None, &config),
        CreateCmd::SubAgent(args) => create(Kind::SubAgent, args, None, None, &config),
        CreateCmd::List {
            offline,
            repo,
            git_ref,
            json,
        } => list(&config, offline, repo, git_ref, json),
        CreateCmd::Update { repo, git_ref } => update(&config, repo, git_ref),
    }
}

fn create(
    kind: Kind,
    args: NewArgs,
    lang: Option<String>,
    port: Option<u16>,
    config: &Config,
) -> Result<()> {
    let lang = match &lang {
        Some(l) => Some(scaffold::Lang::parse(l).ok_or_else(|| {
            anyhow::anyhow!("--lang {l:?} không hỗ trợ. Dùng: rust | go | node | python")
        })?),
        None => None,
    };

    let vars = args
        .vars
        .iter()
        .map(|v| scaffold::vars::parse_var(v))
        .collect::<Result<Vec<_>>>()?;

    let prefer = if args.offline {
        source::Prefer::Offline
    } else if args.refresh {
        source::Prefer::Refresh
    } else {
        source::Prefer::Git
    };

    let req = CreateRequest {
        raw_name: args.name.clone(),
        id: args.id.clone(),
        kind,
        template: args.template.clone(),
        lang,
        dir: args.dir.clone(),
        port,
        description: args.description.clone(),
        icon: args.icon.clone(),
        vars,
        repo: args.repo.clone(),
        git_ref: args.git_ref.clone(),
        prefer,
        force: args.force,
        dry_run: args.dry_run,
    };

    let report = scaffold::run(&req, config, &mut |w| eprintln!("  ! {w}"))?;

    let verb = if report.written {
        "Đã tạo"
    } else {
        "Sẽ tạo (dry-run)"
    };
    println!(
        "{verb} {} {} tại {}",
        report.kind.as_str(),
        report.vars.get("id").cloned().unwrap_or_default(),
        report.dest.display()
    );
    println!("  template: {}", report.origin);
    if let Some(port) = report.vars.get("port") {
        println!("  cổng:     {port}");
    }
    if report.kind == Kind::App {
        if let Some(mcp) = report.vars.get("mcp_name") {
            println!("  MCP:      {mcp}");
        }
    }

    println!("  {} file:", report.files.len());
    for f in &report.files {
        println!("    {}", f.rel);
    }

    for w in &report.warnings {
        eprintln!("  ! {w}");
    }

    if report.written && !report.post_create.is_empty() {
        println!("\nTiếp theo:");
        // `cd` first, because every line after it assumes the project dir.
        if report.kind == Kind::App {
            println!("  cd {}", display_relative(&report.dest));
        }
        for step in &report.post_create {
            println!("  {step}");
        }
    }

    Ok(())
}

fn list(
    config: &Config,
    offline: bool,
    repo: Option<String>,
    git_ref: Option<String>,
    json: bool,
) -> Result<()> {
    let git = scaffold::git_source(config, repo, git_ref);

    // Repo first so a template that exists in both is reported once, from the
    // source that would actually be used.
    let mut rows: Vec<(String, String, String)> = Vec::new();
    if !offline {
        match crate::marketplace::git_sync::clone_or_pull(&git.repo, &git.reference, &git.cache_dir)
        {
            Ok(()) => {
                for (name, dir) in source::list_repo(&git.cache_dir) {
                    let desc = crate::scaffold::spec::TemplateSpec::load(&dir, &name)
                        .ok()
                        .and_then(|s| s.description)
                        .unwrap_or_default();
                    rows.push((name, desc, "git".into()));
                }
            }
            Err(e) => eprintln!("  ! không đồng bộ được {}: {e}", git.repo),
        }
    }

    for name in scaffold::bundled::names() {
        if rows.iter().any(|(n, _, _)| n == name) {
            continue;
        }
        let desc = scaffold::bundled::spec_json(name)
            .and_then(|raw| crate::scaffold::spec::TemplateSpec::parse(raw, name).ok())
            .and_then(|s| s.description)
            .unwrap_or_default();
        rows.push((name.to_string(), desc, "bundled".into()));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    if json {
        let out: Vec<serde_json::Value> = rows
            .iter()
            .map(|(name, desc, src)| {
                serde_json::json!({ "name": name, "description": desc, "source": src })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    let width = rows.iter().map(|(n, _, _)| n.len()).max().unwrap_or(10);
    for (name, desc, src) in &rows {
        println!("  {name:<width$}  [{src}] {desc}");
    }
    println!("\nDùng: senclaw create app <tên> --template <template>");
    Ok(())
}

fn update(config: &Config, repo: Option<String>, git_ref: Option<String>) -> Result<()> {
    let git = scaffold::git_source(config, repo, git_ref);
    if git.cache_dir.exists() {
        std::fs::remove_dir_all(&git.cache_dir)?;
    }
    crate::marketplace::git_sync::clone_or_pull(&git.repo, &git.reference, &git.cache_dir)?;
    let names: Vec<String> = source::list_repo(&git.cache_dir)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    println!(
        "Đã cập nhật {} (nhánh {}) → {}",
        git.repo,
        git.reference,
        git.cache_dir.display()
    );
    println!("  {} template: {}", names.len(), names.join(", "));
    Ok(())
}

/// Print `./todo` rather than an absolute path when the destination is under the
/// current directory — the `cd` line is meant to be copied.
fn display_relative(p: &std::path::Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| p.strip_prefix(&cwd).ok().map(|r| format!("./{}", r.display())))
        .unwrap_or_else(|| p.display().to_string())
}
