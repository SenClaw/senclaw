//! `senclaw skills ...`. Port target: src-old/cli/commands/skills.ts

use anyhow::Result;
use clap::Subcommand;

use crate::clawhub::signal::emit_skills_refresh;
use crate::config::Config;
use crate::skills::disabled::{
    disable_skill, enable_skill, is_skill_disabled, read_disabled_skills,
};
use crate::skills::scan::{get_source_defs, load_all_local_skills, scan_source};

#[derive(Subcommand, Debug)]
pub enum SkillsCmd {
    /// List all available skills
    List {
        #[arg(long)]
        verbose: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show details for a specific skill
    Info { name: String },
    /// Check skill sources
    Check,
    /// Disable a skill by name
    Disable { name: String },
    /// Re-enable a disabled skill
    Enable { name: String },
    /// Signal the daemon to reload skill registries
    Refresh,
}

pub async fn run(cmd: SkillsCmd) -> Result<()> {
    let config = Config::from_env();

    match cmd {
        SkillsCmd::List { verbose, json } => {
            let skills = load_all_local_skills(&config);
            let disabled = read_disabled_skills();

            if json {
                let output: Vec<serde_json::Value> = skills
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "name": s.name,
                            "version": s.version,
                            "source": s.source,
                            "description": s.description,
                            "dir": s.dir,
                            "filePath": s.file_path,
                            "disabled": disabled.contains(&s.name),
                            "triggers": s.metadata.triggers,
                            "eligible": s.eligible,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&output)?);
                return Ok(());
            }

            if skills.is_empty() {
                println!("No skills found.");
                return Ok(());
            }

            for (i, s) in skills.iter().enumerate() {
                let version = s
                    .version
                    .as_deref()
                    .map(|v| format!(" v{v}"))
                    .unwrap_or_default();
                let disabled_tag = if disabled.contains(&s.name) {
                    "  [disabled]"
                } else {
                    ""
                };
                if verbose {
                    println!("- {}{}{}", s.name, version, disabled_tag);
                    println!("  source:  {}", s.source);
                    println!("  dir:     {}", s.dir.display());
                    println!("  desc:    {}", s.description);
                } else {
                    println!("- {}{}  [{}]{}", s.name, version, s.source, disabled_tag);
                    if !s.description.is_empty() {
                        println!("  {}", s.description);
                    }
                }
                if i < skills.len() - 1 {
                    println!();
                }
            }
        }
        SkillsCmd::Info { name } => {
            // Scan ungated so we can report ineligible skills too (with reason).
            let sources = get_source_defs(&config);
            let mut found: Option<crate::skills::scan::SkillEntry> = None;
            for def in &sources {
                if let Some(s) = scan_source(def).into_iter().find(|s| s.name == name) {
                    found = Some(s); // later (higher-priority) source wins
                }
            }
            let Some(skill) = found else {
                anyhow::bail!("Skill not found: {name}");
            };
            let disabled = read_disabled_skills();
            let status = if disabled.contains(&skill.name) {
                "disabled"
            } else {
                "enabled"
            };
            let m = &skill.metadata;
            println!("Name:        {}", skill.name);
            println!(
                "Version:     {}",
                skill.version.as_deref().unwrap_or("(not set)")
            );
            println!("Source:      {}", skill.source);
            println!("Status:      {status}");
            println!(
                "Eligible:    {}",
                match &skill.ineligible_reason {
                    None => "yes".to_string(),
                    Some(r) => format!("no ({r})"),
                }
            );
            println!("Directory:   {}", skill.dir.display());
            println!("File:        {}", skill.file_path.display());
            println!("Description: {}", skill.description);
            if !m.triggers.is_empty() {
                println!("Triggers:    {}", m.triggers.join(", "));
            }
            if !m.allowed_tools.is_empty() {
                println!("Allowed-tools: {}", m.allowed_tools.join(", "));
            }
            if !m.os.is_empty() {
                println!("OS:          {}", m.os.join(", "));
            }
            if !m.requires_env.is_empty() {
                println!("Requires env: {}", m.requires_env.join(", "));
            }
            if !m.requires_bins.is_empty() {
                println!("Requires bins: {}", m.requires_bins.join(", "));
            }
            if !m.requires_any_bins.is_empty() {
                println!("Requires any-bins: {}", m.requires_any_bins.join(", "));
            }
            if let Some(pe) = &m.primary_env {
                println!("Primary env: {pe}");
            }
            if !m.params.is_empty() {
                println!("Params:");
                for p in &m.params {
                    let req = if p.required { "required" } else { "optional" };
                    println!(
                        "  - {} ({}, {}){}",
                        p.name,
                        p.type_,
                        req,
                        p.description
                            .as_deref()
                            .map(|d| format!(": {d}"))
                            .unwrap_or_default()
                    );
                }
            }
        }
        SkillsCmd::Check => {
            let sources = get_source_defs(&config);
            let disabled = read_disabled_skills();
            let mut total_dirs = 0usize;
            let mut total_skills = 0usize;

            for def in &sources {
                let exists = std::path::Path::new(&def.dir).exists();
                total_dirs += 1;
                let skills = if exists { scan_source(def) } else { Vec::new() };
                total_skills += skills.len();
                let status = if exists {
                    format!("{} skills", skills.len())
                } else {
                    "not found".to_string()
                };
                println!("  [{}] {}  →  {}", def.source, def.dir.display(), status);
            }

            println!();
            println!("Total: {total_skills} skill(s) across {total_dirs} sources");

            if !disabled.is_empty() {
                let mut sorted: Vec<String> = disabled.iter().cloned().collect();
                sorted.sort();
                println!();
                println!("Disabled ({}): {}", sorted.len(), sorted.join(", "));
            }

            // Warn about duplicate names
            let all_skills: Vec<_> = sources.iter().flat_map(|def| scan_source(def)).collect();
            let mut name_count: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for s in &all_skills {
                *name_count.entry(&s.name).or_insert(0) += 1;
            }
            let dupes: Vec<_> = name_count.iter().filter(|(_, &c)| c > 1).collect();
            if !dupes.is_empty() {
                println!();
                println!("Duplicate skill names (higher-priority source wins):");
                for (name, count) in dupes {
                    println!("  {name}  ({count} sources)");
                }
            }
        }
        SkillsCmd::Disable { name } => {
            let skills = load_all_local_skills(&config);
            let skill = skills.iter().find(|s| s.name == name);
            let Some(skill) = skill else {
                anyhow::bail!(
                    "Skill not found: {name}\nRun \"senclaw skills list\" to see available skills."
                );
            };
            if is_skill_disabled(&name) {
                println!("Already disabled: {name}");
                return Ok(());
            }
            disable_skill(&name);
            println!("Disabled: {name}  [{}]", skill.source);
            println!("Run \"senclaw skills enable {name}\" to re-enable.");
            let _ = emit_skills_refresh(&config);
        }
        SkillsCmd::Enable { name } => {
            if !is_skill_disabled(&name) {
                let skills = load_all_local_skills(&config);
                let skill = skills.iter().find(|s| s.name == name);
                if skill.is_none() {
                    anyhow::bail!(
                        "Skill not found: {name}\nRun \"senclaw skills list\" to see available skills."
                    );
                }
                println!("Already enabled: {name}");
                return Ok(());
            }
            enable_skill(&name);
            let skills = load_all_local_skills(&config);
            let skill = skills.iter().find(|s| s.name == name);
            println!(
                "Enabled: {}{}",
                name,
                skill
                    .map(|s| format!("  [{}]", s.source))
                    .unwrap_or_default()
            );
            let _ = emit_skills_refresh(&config);
        }
        SkillsCmd::Refresh => {
            emit_skills_refresh(&config)?;
            println!("Refresh signal sent. Daemon will reload skill registries for all agents.");
        }
    }

    Ok(())
}
