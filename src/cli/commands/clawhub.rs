//! `senclaw clawhub ...`. Port target: src-old/cli/commands/clawhub.ts

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::Subcommand;

use crate::clawhub::auth::{
    clear_stored_token, get_config_path, read_stored_token, write_stored_token,
};
use crate::clawhub::client::{
    download_skill_zip, get_skill_meta, publish_skill, search_skills, whoami, DEFAULT_REGISTRY,
};
use crate::clawhub::lockfile::{
    extract_zip_to_dir, read_lockfile, read_skill_origin, write_lockfile, write_skill_origin,
};
use crate::clawhub::signal::emit_skills_refresh;
use crate::config::Config;

const LOGIN_TIMEOUT_SECS: u64 = 120;

fn read_line_with_timeout(timeout_secs: u64) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut line = String::new();
        let _ = stdin.lock().read_line(&mut line);
        let _ = tx.send(line);
    });
    match rx.recv_timeout(Duration::from_secs(timeout_secs)) {
        Ok(line) => Some(line.trim().to_string()),
        Err(_) => None,
    }
}

fn get_registry() -> String {
    std::env::var("CLAWHUB_REGISTRY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_string())
}

fn get_managed_dir(config: &Config) -> PathBuf {
    config.paths.managed_skills_dir.clone()
}

fn is_valid_slug(slug: &str) -> bool {
    !slug.is_empty() && !slug.contains('/') && !slug.contains('\\') && !slug.contains("..")
}

fn require_slug(raw: &str) -> Result<String> {
    let slug = raw.trim();
    if slug.is_empty() || !is_valid_slug(slug) {
        anyhow::bail!("Invalid slug: {raw}");
    }
    Ok(slug.to_string())
}

fn format_relative_time(ts_ms: u64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let diff_ms = now_ms.saturating_sub(ts_ms);
    let days = diff_ms / 86_400_000;
    if days > 30 {
        format!("{}mo ago", days / 30)
    } else if days > 0 {
        format!("{days}d ago")
    } else {
        let hours = diff_ms / 3_600_000;
        if hours > 0 {
            format!("{hours}h ago")
        } else {
            let mins = diff_ms / 60_000;
            if mins > 0 {
                format!("{mins}m ago")
            } else {
                "just now".to_string()
            }
        }
    }
}

async fn resolve_group_skills_dir(config: &Config, group: &str) -> Result<PathBuf> {
    if group.trim().is_empty()
        || group.contains('/')
        || group.contains('\\')
        || group.contains("..")
    {
        anyhow::bail!("Invalid group id: {group}");
    }
    let group_dir = config.paths.workspace_dir.join(group);
    tokio::fs::metadata(&group_dir).await?;
    Ok(group_dir.join("skills"))
}

#[derive(Subcommand, Debug)]
pub enum ClawhubCmd {
    /// Authenticate against ClawHub
    Login {
        /// API token (clh_...)
        #[arg(long)]
        token: Option<String>,
    },
    /// Drop the cached ClawHub credentials
    Logout,
    /// Show current ClawHub session
    Whoami,
    /// Search the ClawHub marketplace
    Search {
        /// Search query
        query: String,
        /// Max results
        #[arg(long, default_value = "10")]
        limit: u32,
    },
    /// Install a skill from ClawHub
    Install {
        /// Skill slug
        slug: String,
        /// Force reinstall
        #[arg(long)]
        force: bool,
        /// Target version
        #[arg(long)]
        version: Option<String>,
        /// Target group (workspace subfolder)
        #[arg(long)]
        group: Option<String>,
    },
    /// Update installed skills
    Update {
        /// Skill slug (or omit with --all)
        slug: Option<String>,
        /// Update all installed skills
        #[arg(long)]
        all: bool,
        /// Force reinstall even if same version
        #[arg(long)]
        force: bool,
        /// Target version
        #[arg(long)]
        version: Option<String>,
    },
    /// List installed ClawHub skills
    List,
    /// Uninstall a skill
    Uninstall {
        /// Skill slug
        slug: String,
        /// Skip confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Publish a skill to ClawHub
    Publish {
        /// Path to skill directory
        path: String,
        /// Dry run (validate only, no upload)
        #[arg(long)]
        dry_run: bool,
        /// Override registry URL
        #[arg(long)]
        registry: Option<String>,
        /// Comma-separated tags (default: latest)
        #[arg(long, default_value = "latest")]
        tags: String,
    },
}

pub async fn run(cmd: ClawhubCmd) -> Result<()> {
    let config = Config::from_env();

    match cmd {
        ClawhubCmd::Login { token } => cmd_login(token).await,
        ClawhubCmd::Logout => cmd_logout().await,
        ClawhubCmd::Whoami => cmd_whoami().await,
        ClawhubCmd::Search { query, limit } => cmd_search(&query, limit).await,
        ClawhubCmd::Install {
            slug,
            force,
            version,
            group,
        } => cmd_install(&config, &slug, force, version.as_deref(), group.as_deref()).await,
        ClawhubCmd::Update {
            slug,
            all,
            force,
            version,
        } => cmd_update(&config, slug.as_deref(), all, force, version.as_deref()).await,
        ClawhubCmd::List => cmd_list(&config).await,
        ClawhubCmd::Uninstall { slug, yes } => cmd_uninstall(&config, &slug, yes).await,
        ClawhubCmd::Publish {
            path,
            dry_run,
            registry,
            tags,
        } => cmd_publish(&path, dry_run, registry.as_deref(), &tags).await,
    }
}

// ===== login =====

async fn cmd_login(provided_token: Option<String>) -> Result<()> {
    let mut token = provided_token
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    if token.is_none() {
        eprintln!("Get your API token at: https://clawhub.ai/settings/tokens");
        eprintln!();
        if !io::stdin().is_terminal() {
            anyhow::bail!("Login failed: no --token provided and stdin is not a TTY.");
        }
        eprint!("Paste your ClawHub token: ");
        io::stdout().flush().ok();
        match read_line_with_timeout(LOGIN_TIMEOUT_SECS) {
            Some(t) if !t.is_empty() => token = Some(t),
            _ => {}
        }
    }

    let token = token.ok_or_else(|| {
        anyhow::anyhow!("Login failed: no token entered.\nTip: use --token flag: senclaw clawhub login --token clh_...")
    })?;

    if !token.starts_with("clh_") {
        anyhow::bail!(
            "Login failed: invalid token format \"{}...\" (expected clh_...).",
            &token[..token.len().min(12)]
        );
    }

    write_stored_token(&token)?;

    // Verify against API
    eprint!("Verifying token...\r");
    match whoami(Some(&get_registry()), Some(&token)).await {
        Ok(user) => {
            let name = user
                .display_name
                .as_deref()
                .or(user.handle.as_deref())
                .unwrap_or("(unknown)");
            eprint!("                  \r");
            println!("Login successful. Logged in as {name}");
            println!("Token saved to: {}", get_config_path().display());
        }
        Err(e) => {
            eprint!("                  \r");
            println!("Token saved to: {}", get_config_path().display());
            println!("Note: online verification skipped ({e})");
            println!("Run \"senclaw clawhub whoami\" to verify when rate limit clears.");
        }
    }
    Ok(())
}

// ===== logout =====

async fn cmd_logout() -> Result<()> {
    let existing = read_stored_token();
    if existing.is_none() {
        println!("Not logged in.");
        return Ok(());
    }
    clear_stored_token()?;
    println!("Logged out. Token removed.");
    Ok(())
}

// ===== whoami =====

async fn cmd_whoami() -> Result<()> {
    let token = read_stored_token()
        .ok_or_else(|| anyhow::anyhow!("Not logged in. Run: senclaw clawhub login"))?;

    match whoami(Some(&get_registry()), Some(&token)).await {
        Ok(user) => {
            println!(
                "Handle:      {}",
                user.handle.as_deref().unwrap_or("(not set)")
            );
            println!(
                "DisplayName: {}",
                user.display_name.as_deref().unwrap_or("(not set)")
            );
            println!("Registry:    {}", DEFAULT_REGISTRY);
            println!("Config:      {}", get_config_path().display());
        }
        Err(e) => {
            anyhow::bail!("Whoami failed: {e}");
        }
    }
    Ok(())
}

// ===== search =====

async fn cmd_search(query: &str, limit: u32) -> Result<()> {
    if query.trim().is_empty() {
        anyhow::bail!("Query required");
    }

    eprint!("Searching...\r");
    let results = search_skills(query, Some(&get_registry()), Some(limit), None).await?;
    eprint!("           \r");

    if results.is_empty() {
        println!("No results.");
        return Ok(());
    }

    for r in &results {
        let version = r
            .version
            .as_deref()
            .map(|v| format!(" v{v}"))
            .unwrap_or_default();
        let age = r
            .updated_at
            .map(|ts| format!("  {}", format_relative_time(ts * 1000)))
            .unwrap_or_default();
        let summary = r
            .summary
            .as_deref()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect::<String>();
        let summary_str = if summary.is_empty() {
            String::new()
        } else {
            format!("  {summary}")
        };
        println!("{}{}{}{}", r.slug, version, age, summary_str);
    }
    Ok(())
}

// ===== install =====

async fn cmd_install(
    config: &Config,
    slug: &str,
    force: bool,
    version: Option<&str>,
    group: Option<&str>,
) -> Result<()> {
    let slug = require_slug(slug)?;
    let managed_dir = if let Some(group) = group {
        resolve_group_skills_dir(config, group).await?
    } else {
        get_managed_dir(config)
    };
    let target = managed_dir.join(&slug);

    if !force && target.exists() {
        anyhow::bail!(
            "Already installed: {}\nUse --force to reinstall.",
            target.display()
        );
    }

    let registry = get_registry();

    eprint!("Resolving {slug}...\r");
    let meta = get_skill_meta(&slug, Some(&registry), None).await?;

    if meta
        .moderation
        .as_ref()
        .map(|m| m.is_malware_blocked)
        .unwrap_or(false)
    {
        anyhow::bail!("Blocked: {slug} is flagged as malicious and cannot be installed.");
    }

    if meta
        .moderation
        .as_ref()
        .map(|m| m.is_suspicious)
        .unwrap_or(false)
        && !force
    {
        eprint!("                              \r");
        eprintln!("\nWarning: \"{slug}\" is flagged as suspicious.");
        eprintln!("   Review the skill code before use.\n");
        eprint!("Install anyway? [y/N] ");
        io::stdout().flush().ok();
        match read_line_with_timeout(60) {
            Some(line) if line.to_lowercase().starts_with('y') => {}
            _ => {
                println!("Installation cancelled.");
                return Ok(());
            }
        }
    }

    let resolved_version = version
        .map(|v| v.to_string())
        .or_else(|| meta.latest_version.as_ref().map(|v| v.version.clone()))
        .ok_or_else(|| anyhow::anyhow!("Could not resolve version for {slug}"))?;

    eprint!("Downloading {slug}@{resolved_version}...\r");
    let zip_buf = download_skill_zip(&slug, &resolved_version, Some(&registry), None).await?;

    if force && target.exists() {
        tokio::fs::remove_dir_all(&target).await?;
    }
    extract_zip_to_dir(&zip_buf, &target)?;

    let _ = write_skill_origin(
        &target,
        &crate::clawhub::lockfile::SkillOrigin {
            version: 1,
            registry: DEFAULT_REGISTRY.to_string(),
            slug: slug.clone(),
            installed_version: resolved_version.clone(),
            installed_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        },
    );

    let mut lock = read_lockfile(&managed_dir);
    lock.skills.insert(
        slug.clone(),
        crate::clawhub::lockfile::LockfileEntry {
            version: Some(resolved_version.clone()),
            installed_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        },
    );
    write_lockfile(&managed_dir, &lock)?;

    eprint!("                                        \r");
    println!(
        "✓ Installed {slug}@{resolved_version} → {}",
        target.display()
    );
    let _ = emit_skills_refresh(config);
    Ok(())
}

// ===== update =====

async fn cmd_update(
    config: &Config,
    slug_arg: Option<&str>,
    all: bool,
    force: bool,
    version: Option<&str>,
) -> Result<()> {
    if slug_arg.is_none() && !all {
        anyhow::bail!("Provide <slug> or --all");
    }
    if slug_arg.is_some() && all {
        anyhow::bail!("Use either <slug> or --all, not both");
    }

    let managed_dir = get_managed_dir(config);
    let mut lock = read_lockfile(&managed_dir);
    let slugs: Vec<String> = if let Some(slug) = slug_arg {
        vec![require_slug(slug)?]
    } else {
        lock.skills
            .keys()
            .filter(|s| is_valid_slug(s))
            .cloned()
            .collect()
    };

    if slugs.is_empty() {
        println!("No installed skills.");
        return Ok(());
    }

    let registry = get_registry();

    for slug in &slugs {
        eprint!("Checking {slug}...\r");
        match get_skill_meta(slug, Some(&registry), None).await {
            Ok(meta) => {
                if meta
                    .moderation
                    .as_ref()
                    .map(|m| m.is_malware_blocked)
                    .unwrap_or(false)
                {
                    eprint!("                    \r");
                    println!("{slug}: blocked as malicious, skipping");
                    continue;
                }

                let latest = meta.latest_version.as_ref().map(|v| v.version.clone());
                let Some(ref latest) = latest else {
                    eprint!("                    \r");
                    println!("{slug}: not found on registry");
                    continue;
                };

                let target_version = version.unwrap_or(latest);
                let current = lock.skills.get(slug).and_then(|e| e.version.as_deref());
                if !force && current == Some(target_version) {
                    eprint!("                    \r");
                    println!("{slug}: up to date ({})", current.unwrap_or("unknown"));
                    continue;
                }

                let target = managed_dir.join(slug);
                eprint!("Updating {slug} → {target_version}...\r");
                let zip_buf =
                    download_skill_zip(slug, target_version, Some(&registry), None).await?;
                if target.exists() {
                    tokio::fs::remove_dir_all(&target).await?;
                }
                extract_zip_to_dir(&zip_buf, &target)?;

                let existing_origin = read_skill_origin(&target);
                let _ = write_skill_origin(
                    &target,
                    &crate::clawhub::lockfile::SkillOrigin {
                        version: 1,
                        registry: existing_origin
                            .as_ref()
                            .map(|o| o.registry.clone())
                            .unwrap_or_else(|| DEFAULT_REGISTRY.to_string()),
                        slug: existing_origin
                            .as_ref()
                            .map(|o| o.slug.clone())
                            .unwrap_or_else(|| slug.clone()),
                        installed_version: target_version.to_string(),
                        installed_at: existing_origin
                            .as_ref()
                            .map(|o| o.installed_at)
                            .unwrap_or_else(|| {
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis() as u64
                            }),
                    },
                );

                lock.skills.insert(
                    slug.clone(),
                    crate::clawhub::lockfile::LockfileEntry {
                        version: Some(target_version.to_string()),
                        installed_at: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as u64,
                    },
                );
                eprint!("                                        \r");
                println!("{slug}: updated → {target_version}");
            }
            Err(e) => {
                eprint!("                                        \r");
                eprintln!("{slug}: failed — {e}");
            }
        }
    }

    write_lockfile(&managed_dir, &lock)?;
    let _ = emit_skills_refresh(config);
    Ok(())
}

// ===== list =====

async fn cmd_list(config: &Config) -> Result<()> {
    let managed_dir = get_managed_dir(config);
    let lock = read_lockfile(&managed_dir);
    let mut entries: Vec<(_, _)> = lock.skills.iter().collect();
    entries
        .sort_by(|(a, _): &(&String, &crate::clawhub::lockfile::LockfileEntry), (b, _)| a.cmp(b));

    if entries.is_empty() {
        println!("No ClawHub skills installed.");
        println!("Install dir: {}", managed_dir.display());
        return Ok(());
    }

    println!("Installed in: {}\n", managed_dir.display());
    for (slug, entry) in entries {
        let version = entry.version.as_deref().unwrap_or("(unknown)");
        let date = chrono::DateTime::from_timestamp(
            (entry.installed_at / 1000) as i64,
            ((entry.installed_at % 1000) * 1_000_000) as u32,
        )
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".to_string());
        println!("  {slug}  v{version}  (installed {date})");
    }
    Ok(())
}

// ===== uninstall =====

async fn cmd_uninstall(config: &Config, slug: &str, yes: bool) -> Result<()> {
    let slug = require_slug(slug)?;
    let managed_dir = get_managed_dir(config);
    let mut lock = read_lockfile(&managed_dir);

    if !lock.skills.contains_key(&slug) {
        anyhow::bail!("Not installed: {slug}");
    }

    if !yes {
        eprint!("Uninstall {slug}? [y/N] ");
        io::stdout().flush().ok();
        match read_line_with_timeout(60) {
            Some(line) if line.to_lowercase().starts_with('y') => {}
            _ => {
                println!("Cancelled.");
                return Ok(());
            }
        }
    }

    let target = managed_dir.join(&slug);
    tokio::fs::remove_dir_all(&target).await?;
    lock.skills.remove(&slug);
    write_lockfile(&managed_dir, &lock)?;
    let _ = emit_skills_refresh(config);
    println!("✓ Uninstalled {slug}");
    Ok(())
}

// ===== publish =====

async fn cmd_publish(
    skill_path: &str,
    dry_run: bool,
    registry: Option<&str>,
    tags: &str,
) -> Result<()> {
    let resolved = std::path::absolute(Path::new(skill_path))?;

    // Verify directory exists
    tokio::fs::metadata(&resolved).await?;

    // Read SKILL.md
    let skill_md_path = resolved.join("SKILL.md");
    let skill_md = tokio::fs::read_to_string(&skill_md_path)
        .await
        .map_err(|_| anyhow::anyhow!("Error: SKILL.md not found in {}", resolved.display()))?;

    // Parse frontmatter
    let fm = parse_simple_frontmatter(&skill_md);

    let display_name = fm
        .get("name")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Error: SKILL.md is missing the \"name\" field"))?;

    let version = fm
        .get("version")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Error: SKILL.md is missing the \"version\" field (required for publish)"
            )
        })?;

    let dir_slug = resolved
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug = fm
        .get("slug")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or(dir_slug);

    let changelog = fm
        .get("changelog")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let tag_list: Vec<String> = tags
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();

    println!("Skill:       {display_name}");
    println!("Slug:        {slug}");
    println!("Version:     {version}");
    println!("Tags:        {}", tag_list.join(", "));
    if !changelog.is_empty() {
        println!("Changelog:   {changelog}");
    }
    println!();

    // Collect files
    let files = collect_skill_files(&resolved)?;
    let file_names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
    println!("Files:       {} ({})", files.len(), file_names.join(", "));

    if dry_run {
        println!("[dry-run] Skipping upload.");
        return Ok(());
    }

    eprint!("Uploading to ClawHub...\r");
    let result = publish_skill(
        &slug,
        &display_name,
        &version,
        &changelog,
        &tag_list,
        files,
        registry,
        None,
    )
    .await?;
    eprint!("                       \r");
    println!("Published:   {}@{}", result.slug, result.version);
    println!("View at:     https://clawhub.ai/skills/{}", result.slug);
    Ok(())
}

// ===== Frontmatter parser =====

fn parse_simple_frontmatter(content: &str) -> std::collections::HashMap<String, String> {
    let mut result = std::collections::HashMap::new();
    let rest = match content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
    {
        Some(r) => r,
        None => return result,
    };
    let body = match rest
        .split_once("\n---")
        .or_else(|| rest.split_once("\r\n---"))
    {
        Some((b, _)) => b,
        None => return result,
    };

    let mut current_key: Option<String> = None;
    let mut block_lines: Vec<String> = Vec::new();
    let mut in_block = false;

    let flush_block =
        |key: &mut Option<String>,
         lines: &mut Vec<String>,
         in_block: &mut bool,
         result: &mut std::collections::HashMap<String, String>| {
            if let Some(ref k) = key {
                if *in_block {
                    result.insert(k.clone(), lines.join("\n").trim_end().to_string());
                }
            }
            *key = None;
            lines.clear();
            *in_block = false;
        };

    for line in body.lines() {
        if in_block {
            if line.starts_with(' ') || line.starts_with('\t') {
                block_lines.push(line.trim().to_string());
                continue;
            }
            flush_block(
                &mut current_key,
                &mut block_lines,
                &mut in_block,
                &mut result,
            );
        }

        if let Some((key, val)) = line.split_once(':') {
            let key = key.trim().to_string();
            if !key
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic())
                .unwrap_or(false)
            {
                continue;
            }
            let val = val.trim().to_string();
            if val == "|" || val == ">" {
                current_key = Some(key);
                block_lines = Vec::new();
                in_block = true;
            } else {
                result.insert(key, val);
            }
        }
    }
    flush_block(
        &mut current_key,
        &mut block_lines,
        &mut in_block,
        &mut result,
    );
    result
}

// ===== File collection =====

fn collect_skill_files(dir: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    let mut files = Vec::new();
    walk_skill_dir(dir, dir, &mut files)?;
    Ok(files)
}

fn walk_skill_dir(
    base_dir: &Path,
    current_dir: &Path,
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<()> {
    for entry in std::fs::read_dir(current_dir)? {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(base_dir)?
            .to_string_lossy()
            .replace('\\', "/");

        if entry.file_type()?.is_dir() {
            walk_skill_dir(base_dir, &path, out)?;
        } else {
            if rel == ".clawhub" {
                continue;
            }
            let data = std::fs::read(&path)?;
            out.push((rel, data));
        }
    }
    Ok(())
}
