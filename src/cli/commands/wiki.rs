//! `senclaw wiki ...`. Port target: src-old/cli/commands/wiki.ts

use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

use crate::wiki::manager::WikiManager;

fn get_wiki_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("WIKI_DIR") {
        return PathBuf::from(dir);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".senclaw")
        .join("wiki")
}

fn read_stdin() -> String {
    if io::stdin().is_terminal() {
        return String::new();
    }
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf).ok();
    buf
}

#[derive(Subcommand, Debug)]
pub enum WikiCmd {
    /// Print wiki directory tree
    Tree,
    /// Save document from stdin
    Save {
        /// Relative path within wiki (required)
        #[arg(long)]
        path: String,
        /// Comma-separated tags
        #[arg(long)]
        tags: Option<String>,
        /// Content source label
        #[arg(long, default_value = "agent")]
        source: String,
        /// Git commit message
        #[arg(long)]
        msg: Option<String>,
    },
    /// Full-text search by title/tags/content
    Search {
        /// Search query
        query: String,
        /// Max results (default 10)
        #[arg(long, default_value = "10")]
        limit: usize,
        /// Filter by comma-separated tags
        #[arg(long)]
        tags: Option<String>,
    },
    /// Create a directory
    Mkdir {
        /// Directory path within wiki
        path: String,
    },
    /// Show wiki statistics
    Stats,
}

pub async fn run(cmd: WikiCmd) -> Result<()> {
    let wm = WikiManager::new(get_wiki_dir());
    wm.ensure_init().await?;

    match cmd {
        WikiCmd::Tree => {
            let text = wm.tree_text()?;
            if text.is_empty() {
                println!("(empty wiki)");
            } else {
                println!("{text}");
            }
        }
        WikiCmd::Save {
            path,
            tags,
            source,
            msg,
        } => {
            let content = read_stdin();
            if content.trim().is_empty() {
                anyhow::bail!("no content received on stdin");
            }
            let tag_list: Option<Vec<String>> = tags.map(|t| {
                t.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            });
            wm.write_file(
                &path,
                &content,
                Some(&source),
                tag_list.as_deref(),
                msg.as_deref(),
            )
            .await?;
            let result = serde_json::json!({ "path": path, "action": "created" });
            println!("{result}");
        }
        WikiCmd::Search { query, limit, tags } => {
            let tag_list: Option<Vec<String>> = tags.map(|t| {
                t.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            });
            let results = wm.search(&query, tag_list.as_deref(), Some(limit))?;
            if results.is_empty() {
                println!("No results found.");
            } else {
                for r in &results {
                    let tag_str = if r.tags.is_empty() {
                        String::new()
                    } else {
                        format!("  [{}]", r.tags.join(", "))
                    };
                    println!("{}  --  {}{}", r.path, r.title, tag_str);
                }
            }
        }
        WikiCmd::Mkdir { path } => {
            wm.mkdir(&path).await?;
            println!("Created: {path}");
        }
        WikiCmd::Stats => {
            let stats = wm.get_stats()?;
            println!("Wiki Statistics");
            println!("-------------------------------------");
            println!(
                "Total: {} docs | {} directories",
                stats.total_files, stats.total_dirs
            );
            println!();

            if !stats.by_category.is_empty() {
                println!("Category distribution:");
                let max_count = stats.by_category.iter().map(|c| c.count).max().unwrap_or(1);
                for cat in &stats.by_category {
                    let bar_len = ((cat.count as f64 / max_count as f64) * 20.0).ceil() as usize;
                    let bar = "█".repeat(bar_len);
                    println!("  {:<20} {}  {} docs", cat.dir, bar, cat.count);
                }
                println!();
            }

            if !stats.by_tag.is_empty() {
                let top_tags: Vec<String> = stats
                    .by_tag
                    .iter()
                    .take(10)
                    .map(|t| format!("[{}×{}]", t.tag, t.count))
                    .collect();
                println!("Top tags: {}", top_tags.join(" "));
                println!();
            }

            if !stats.recent_files.is_empty() {
                println!("Recently updated:");
                for f in stats.recent_files.iter().take(5) {
                    println!("  {}  --  {}", f.path, f.title);
                }
            }
        }
    }

    Ok(())
}
