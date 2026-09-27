//! `senclaw marketplace ...` — manage plugin sources and the hub store.
//!
//! A *hub* is a remote `marketplace.json` catalog (the default is
//! [`crate::marketplace::DEFAULT_HUB_URL`], overridable with `SENCLAW_HUB_URL`):
//! browse it, then install the plugins you want. Git and local sources are
//! managed here too.

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::config::Config;
use crate::marketplace::manager::{InstallOutcome, MarketplaceManager};
use crate::marketplace::types::{MarketplaceSource, SourceType};

#[derive(Subcommand, Debug)]
pub enum MarketplaceCmd {
    /// List configured sources (hub stores, git repos, local directories)
    List {
        #[arg(long)]
        json: bool,
    },
    /// Add a source. The type is inferred from the URL unless given.
    Add {
        /// Hub catalog URL, git remote, or local directory
        url: String,
        #[arg(long)]
        name: Option<String>,
        /// hub | git | local
        #[arg(long, value_parser = parse_source_type)]
        r#type: Option<SourceType>,
        #[arg(long)]
        branch: Option<String>,
    },
    /// Remove a source and everything it manages on disk
    Remove {
        /// Source id (prefix is enough) or name
        source: String,
    },
    /// Refresh a source: pull a git clone, or re-fetch a hub catalog
    Sync {
        /// Source id (prefix is enough) or name; omit with --all
        source: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// List the plugins of a source — for a hub, the whole catalog
    Plugins {
        /// Source id (prefix is enough) or name; omit for every source
        source: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Install a plugin from a hub catalog
    Install {
        plugin: String,
        /// Source id or name; defaults to the only hub when there is just one
        #[arg(long)]
        source: Option<String>,
        /// Install even when the pre-install security scan says to stop.
        /// The scan still runs and its report is still printed.
        #[arg(long)]
        force: bool,
    },
    /// Remove a plugin installed from a hub
    Uninstall {
        plugin: String,
        #[arg(long)]
        source: Option<String>,
    },
    /// Enable an installed plugin for agents
    Enable {
        plugin: String,
        #[arg(long)]
        source: Option<String>,
    },
    /// Disable an installed plugin without removing it
    Disable {
        plugin: String,
        #[arg(long)]
        source: Option<String>,
    },
}

fn parse_source_type(s: &str) -> Result<SourceType, String> {
    match s.to_ascii_lowercase().as_str() {
        "hub" | "store" => Ok(SourceType::Hub),
        "git" => Ok(SourceType::Git),
        "local" | "dir" => Ok(SourceType::Local),
        other => Err(format!("unknown source type {other:?} (hub | git | local)")),
    }
}

fn type_label(t: SourceType) -> &'static str {
    match t {
        SourceType::Hub => "hub",
        SourceType::Git => "git",
        SourceType::Local => "local",
    }
}

fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// Resolve a user-typed source reference: exact id, id prefix, or name.
fn resolve_source(manager: &MarketplaceManager, needle: &str) -> Result<MarketplaceSource> {
    let sources = manager.get_sources();
    let matches: Vec<&MarketplaceSource> = sources
        .iter()
        .filter(|s| s.id == needle || s.id.starts_with(needle) || s.name == needle)
        .collect();

    match matches.as_slice() {
        [one] => Ok((*one).clone()),
        [] => anyhow::bail!("No source matching {needle:?}. Run `senclaw marketplace list`."),
        many => anyhow::bail!(
            "{needle:?} is ambiguous: {}",
            many.iter()
                .map(|s| format!("{} ({})", short_id(&s.id), s.name))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Resolve the source for a plugin operation: the one named, else the sole hub.
fn resolve_plugin_source(
    manager: &MarketplaceManager,
    source: Option<&str>,
) -> Result<MarketplaceSource> {
    if let Some(needle) = source {
        return resolve_source(manager, needle);
    }
    let hubs: Vec<MarketplaceSource> = manager
        .get_sources()
        .into_iter()
        .filter(|s| s.source_type == SourceType::Hub)
        .collect();
    match hubs.as_slice() {
        [one] => Ok(one.clone()),
        [] => {
            anyhow::bail!("No hub source configured. Add one with `senclaw marketplace add <url>`.")
        }
        _ => anyhow::bail!("Several hubs configured — pass --source <id|name>."),
    }
}

fn open_manager() -> Result<MarketplaceManager> {
    let cfg = Config::from_env();
    Ok(MarketplaceManager::from_config(&cfg))
}

pub async fn run(cmd: MarketplaceCmd) -> Result<()> {
    // Every path touches the filesystem and most touch the network through
    // reqwest's blocking client, which must not run on a reactor thread.
    tokio::task::spawn_blocking(move || run_blocking(cmd))
        .await
        .context("marketplace command panicked")?
}

fn run_blocking(cmd: MarketplaceCmd) -> Result<()> {
    let mut manager = open_manager()?;

    match cmd {
        MarketplaceCmd::List { json } => {
            let sources = manager.get_sources();
            if json {
                println!("{}", serde_json::to_string_pretty(&sources)?);
                return Ok(());
            }
            if sources.is_empty() {
                println!("No marketplace sources. Add one with `senclaw marketplace add <url>`.");
                return Ok(());
            }
            for s in sources {
                println!(
                    "{}  {:<5} {:<24} {}{}",
                    short_id(&s.id),
                    type_label(s.source_type),
                    s.name,
                    s.url.as_deref().unwrap_or(&s.local_path),
                    if s.enabled { "" } else { "  [disabled]" }
                );
                if let Some(err) = &s.sync_error {
                    println!("          sync error: {err}");
                }
            }
        }

        MarketplaceCmd::Add {
            url,
            name,
            r#type,
            branch,
        } => {
            let source_type = r#type.unwrap_or_else(|| infer_type(&url));
            let name = name.unwrap_or_else(|| default_name(&url, source_type));
            let (url_arg, local_path) = match source_type {
                SourceType::Local => (None, Some(url.clone())),
                _ => (Some(url.clone()), None),
            };
            let source =
                manager.add_source(name, source_type, url_arg, branch, local_path, None, None)?;
            println!(
                "Added {} source {} ({})",
                type_label(source_type),
                source.name,
                short_id(&source.id)
            );
            if source_type != SourceType::Local {
                manager.sync_source(&source.id)?;
                println!("Synced.");
            }
        }

        MarketplaceCmd::Remove { source } => {
            let s = resolve_source(&manager, &source)?;
            manager.remove_source(&s.id)?;
            println!("Removed {} ({})", s.name, short_id(&s.id));
        }

        MarketplaceCmd::Sync { source, all } => {
            let targets: Vec<MarketplaceSource> = if all {
                manager.get_sources()
            } else {
                let needle = source.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("Pass a source, or --all to sync every source")
                })?;
                vec![resolve_source(&manager, needle)?]
            };
            for s in targets {
                match manager.sync_source(&s.id) {
                    Ok(()) => println!("Synced {}", s.name),
                    Err(e) => println!("Failed  {}: {e}", s.name),
                }
            }
        }

        MarketplaceCmd::Plugins { source, json } => {
            let sources = match source {
                Some(needle) => vec![resolve_source(&manager, &needle)?],
                None => manager.get_sources(),
            };
            let mut all = Vec::new();
            for s in sources {
                let Some(info) = manager.get_source_info(&s.id)? else {
                    continue;
                };
                if json {
                    all.extend(info.plugins);
                    continue;
                }
                println!(
                    "{} ({})",
                    info.source.name,
                    type_label(info.source.source_type)
                );
                if info.plugins.is_empty() {
                    println!(
                        "  (nothing yet — try `senclaw marketplace sync {}`)",
                        short_id(&s.id)
                    );
                }
                for p in info.plugins {
                    let state = match (p.installed, p.enabled) {
                        (false, _) => "available",
                        (true, true) => "enabled",
                        (true, false) => "installed",
                    };
                    println!(
                        "  {:<10} {:<28} {}",
                        state,
                        p.name,
                        p.description.lines().next().unwrap_or_default()
                    );
                }
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&all)?);
            }
        }

        MarketplaceCmd::Install {
            plugin,
            source,
            force,
        } => {
            let s = resolve_plugin_source(&manager, source.as_deref())?;
            let policy =
                crate::security::ScanPolicy::from_config(&crate::config::Config::from_env());
            match manager.install_hub_plugin(&s.id, &plugin, policy, force)? {
                InstallOutcome::Blocked { report, staged_dir } => {
                    eprintln!("{}", report.summary());
                    eprintln!();
                    anyhow::bail!(
                        "Refusing to install {plugin}: it failed the pre-install security scan. \
                         Nothing was recorded or enabled. The clone is left at {} so you can \
                         review it. To install anyway, re-run with --force.",
                        staged_dir.display(),
                    );
                }
                InstallOutcome::Installed { dir, scan } => {
                    // Warn-level findings print on success too — an install that
                    // turned up something is not the same as a clean one.
                    if let Some(report) = &scan {
                        if !report.findings.is_empty() {
                            println!("{}", report.summary());
                            println!();
                        }
                    }
                    println!("Installed {plugin} from {} → {}", s.name, dir.display());
                }
            }
        }

        MarketplaceCmd::Uninstall { plugin, source } => {
            let s = resolve_plugin_source(&manager, source.as_deref())?;
            if manager.uninstall_hub_plugin(&s.id, &plugin)? {
                println!("Uninstalled {plugin}");
            } else {
                println!("{plugin} was not installed from {}", s.name);
            }
        }

        MarketplaceCmd::Enable { plugin, source } => {
            let s = resolve_plugin_source(&manager, source.as_deref())?;
            manager.set_plugin_enabled(&s.id, &plugin, true)?;
            println!("Enabled {plugin}");
        }

        MarketplaceCmd::Disable { plugin, source } => {
            let s = resolve_plugin_source(&manager, source.as_deref())?;
            manager.set_plugin_enabled(&s.id, &plugin, false)?;
            println!("Disabled {plugin}");
        }
    }

    Ok(())
}

/// Same inference the REST layer uses: catalogs and bare hosts are hubs,
/// repo-shaped URLs are git, everything else is a directory.
fn infer_type(url: &str) -> SourceType {
    let url = url.trim();
    if url.starts_with("git@") {
        return SourceType::Git;
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return SourceType::Local;
    }
    if url.ends_with(".json") {
        SourceType::Hub
    } else if url.ends_with(".git") || url.trim_end_matches('/').matches('/').count() > 2 {
        SourceType::Git
    } else {
        SourceType::Hub
    }
}

fn default_name(url: &str, source_type: SourceType) -> String {
    let base = match source_type {
        SourceType::Hub => crate::marketplace::hub::catalog_home(url),
        _ => url.trim_end_matches('/').to_string(),
    };
    let name = base
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches(".git")
        .to_string();
    if name.is_empty() {
        "Untitled source".to_string()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_source_types_from_urls() {
        assert_eq!(infer_type("https://hub-store.bacnd.com"), SourceType::Hub);
        assert_eq!(
            infer_type("https://hub-store.bacnd.com/marketplace.json"),
            SourceType::Hub
        );
        assert_eq!(infer_type("https://github.com/owner/repo"), SourceType::Git);
        assert_eq!(infer_type("git@github.com:owner/repo.git"), SourceType::Git);
        assert_eq!(infer_type("/srv/plugins"), SourceType::Local);
    }

    #[test]
    fn names_sources_after_their_origin() {
        assert_eq!(
            default_name(
                "https://hub-store.bacnd.com/marketplace.json",
                SourceType::Hub
            ),
            "hub-store.bacnd.com"
        );
        assert_eq!(
            default_name("https://github.com/owner/repo.git", SourceType::Git),
            "github.com/owner/repo"
        );
    }
}
