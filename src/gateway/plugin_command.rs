//! Parse and execute `/plugin` slash commands inside a chat session.
//!
//! Mirrors the marketplace REST/UI panel, but driven from a messaging channel
//! or the Web UI chat box. Honored in every chat (every chat is admin).
//!
//! Grammar:
//! ```text
//!   /plugin help
//!   /plugin marketplace add <url> [as <name>]
//!   /plugin marketplace list
//!   /plugin marketplace remove <name-or-url>
//!   /plugin list [<source>]
//!   /plugin install <name>[@<source>]
//!   /plugin uninstall <name>[@<source>]
//! ```
//! `<source>` matches a marketplace source by name (exact, case-insensitive)
//! then by substring; omitting `@<source>` targets the sole hub when there is
//! exactly one.

use std::sync::{Arc, Mutex};

use crate::marketplace::manager::{InstallOutcome, MarketplaceManager};
use crate::marketplace::types::{MarketplaceSource, SourceType};

pub const PLUGIN_HELP: &str = "\
🧩 Plugin commands:
  /plugin marketplace add <url> [as <name>]  — add a marketplace source (hub/git/local)
  /plugin marketplace list                   — list configured sources
  /plugin marketplace remove <name|url>      — remove a source
  /plugin list [source]                      — list plugins in a hub catalog
  /plugin install <name>[@source]            — install a plugin from a hub
  /plugin install <name> --force             — install despite a blocking security scan
  /plugin uninstall <name>[@source]          — remove an installed plugin
  /plugin help                               — show this help";

/// Try parsing `text` as a `/plugin` command and execute it.
/// Returns `Some(reply)` if it matched (even on error), or `None` when the text
/// is not a plugin command (fall through to the next dispatcher / the agent).
pub async fn dispatch_plugin_command(
    manager: Arc<Mutex<MarketplaceManager>>,
    text: &str,
) -> Option<String> {
    let t = text.trim();
    // Require `/plugin` or `plugin` followed by end-of-string or whitespace, so
    // `/plugins` or `/pluginfoo` do not falsely match.
    let after = t
        .strip_prefix("/plugin")
        .or_else(|| t.strip_prefix("plugin"))
        .filter(|r| r.is_empty() || r.starts_with(char::is_whitespace))?
        .trim();

    let mut parts = after.split_whitespace();
    let sub = parts.next().unwrap_or("help").to_lowercase();

    match sub.as_str() {
        "help" | "" => Some(PLUGIN_HELP.to_string()),

        "marketplace" | "mp" | "source" | "sources" => {
            let action = parts.next().unwrap_or("list").to_lowercase();
            let rest: Vec<String> = parts.map(str::to_string).collect();
            marketplace_subcommand(manager, &action, &rest).await
        }

        "list" | "ls" => {
            let source_key = parts.next().map(str::to_string);
            Some(list_plugins(manager, source_key).await)
        }

        "install" | "i" | "add" => {
            let Some(arg) = parts.next() else {
                return Some("Usage: /plugin install <name>[@source]".to_string());
            };
            let (name, source_key) = parse_plugin_ref(arg);
            // `--force` has to be typed by the human on the command line. It is
            // deliberately not inferable from anything the package or the agent
            // says, so a plugin cannot talk its own way past the scan.
            let force = parts.any(|p| p == "--force");
            Some(install_plugin(manager, name, source_key, force).await)
        }

        "uninstall" | "remove" | "rm" | "uni" => {
            let Some(arg) = parts.next() else {
                return Some("Usage: /plugin uninstall <name>[@source]".to_string());
            };
            let (name, source_key) = parse_plugin_ref(arg);
            Some(uninstall_plugin(manager, name, source_key).await)
        }

        other => Some(format!(
            "❓ Unknown plugin command: `{other}`\n\n{PLUGIN_HELP}"
        )),
    }
}

// ── marketplace <action> ────────────────────────────────────────────────────

async fn marketplace_subcommand(
    manager: Arc<Mutex<MarketplaceManager>>,
    action: &str,
    rest: &[String],
) -> Option<String> {
    match action {
        "add" => {
            let Some(url) = rest.first().cloned() else {
                return Some("Usage: /plugin marketplace add <url> [as <name>]".to_string());
            };
            // Optional `as <name>`.
            let name_override = match (rest.get(1), rest.get(2)) {
                (Some(kw), Some(n)) if kw.eq_ignore_ascii_case("as") => Some(n.clone()),
                _ => None,
            };
            Some(add_source(manager, url, name_override).await)
        }
        "list" | "ls" => Some(list_sources(manager).await),
        "remove" | "rm" | "del" | "delete" => {
            let Some(key) = rest.first().cloned() else {
                return Some("Usage: /plugin marketplace remove <name|url>".to_string());
            };
            Some(remove_source(manager, key).await)
        }
        other => Some(format!(
            "❓ Unknown marketplace command: `{other}`\n\n{PLUGIN_HELP}"
        )),
    }
}

async fn add_source(
    manager: Arc<Mutex<MarketplaceManager>>,
    url: String,
    name_override: Option<String>,
) -> String {
    let res = tokio::task::spawn_blocking(move || {
        let source_type = crate::marketplace::infer_source_type(Some(&url), None);
        let name = crate::marketplace::default_source_name(
            name_override.as_deref(),
            Some(&url),
            None,
            source_type,
        );
        let mut mgr = manager.lock().unwrap();
        let source = mgr
            .add_source(
                name,
                source_type,
                Some(url),
                None,
                None,
                None,
                Some(true),
            )
            .map_err(|e| e.to_string())?;
        // Pull the catalog immediately so a new hub is browsable/installable
        // without a separate sync — matches the REST add flow.
        if source_type == SourceType::Hub {
            if let Err(e) = mgr.sync_source(&source.id) {
                let s = mgr.get_source(&source.id).unwrap_or(source);
                return Ok::<_, String>(format!(
                    "⚠️ Added source `{}` but catalog sync failed: {e}\n   Try `/plugin list {}` after checking the URL.",
                    s.name, s.name
                ));
            }
            let s = mgr.get_source(&source.id).unwrap_or(source);
            return Ok(format!(
                "✅ Added hub `{}` ({})\n   Browse it with `/plugin list {}`.",
                s.name,
                s.url.as_deref().unwrap_or(""),
                s.name
            ));
        }
        Ok(format!(
            "✅ Added {} source `{}`.",
            source_type_label(source_type),
            source.name
        ))
    })
    .await;

    match res {
        // The closure already formats a user-facing message on both arms.
        Ok(Ok(msg)) | Ok(Err(msg)) => msg,
        Err(e) => format!("❌ add-source task failed: {e}"),
    }
}

async fn list_sources(manager: Arc<Mutex<MarketplaceManager>>) -> String {
    let sources = tokio::task::spawn_blocking(move || manager.lock().unwrap().get_sources())
        .await
        .unwrap_or_default();
    if sources.is_empty() {
        return "📦 No marketplace sources configured.\n   Add one with `/plugin marketplace add <url>`.".to_string();
    }
    let mut lines = vec![
        format!("📦 Marketplace sources ({})", sources.len()),
        String::new(),
    ];
    for s in &sources {
        let flag = if s.enabled { "🟢" } else { "⚪" };
        let loc = s.url.as_deref().unwrap_or(&s.local_path);
        lines.push(format!(
            "{flag} {} · {} · {loc}",
            s.name,
            source_type_label(s.source_type)
        ));
        if let Some(err) = &s.sync_error {
            lines.push(format!("   ⚠️ {err}"));
        }
    }
    lines.join("\n")
}

async fn remove_source(manager: Arc<Mutex<MarketplaceManager>>, key: String) -> String {
    let res = tokio::task::spawn_blocking(move || {
        let mut mgr = manager.lock().unwrap();
        let source = resolve_source(&mgr, Some(&key), false)?;
        mgr.remove_source(&source.id).map_err(|e| e.to_string())?;
        Ok::<_, String>(format!("🗑️ Removed source `{}`.", source.name))
    })
    .await;
    match res {
        Ok(Ok(msg)) => msg,
        Ok(Err(e)) => format!("❌ {e}"),
        Err(e) => format!("❌ remove-source task failed: {e}"),
    }
}

// ── list / install / uninstall plugins ──────────────────────────────────────

async fn list_plugins(
    manager: Arc<Mutex<MarketplaceManager>>,
    source_key: Option<String>,
) -> String {
    let res = tokio::task::spawn_blocking(move || {
        let mgr = manager.lock().unwrap();
        let source = resolve_source(&mgr, source_key.as_deref(), true)?;
        let catalog = mgr.get_catalog(&source.id).map_err(|e| e.to_string())?;
        Ok::<_, String>((source, catalog))
    })
    .await;

    let (source, catalog) = match res {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return format!("❌ {e}"),
        Err(e) => return format!("❌ list task failed: {e}"),
    };

    if catalog.plugins.is_empty() {
        return format!("📭 Hub `{}` has no plugins in its catalog.", source.name);
    }
    let mut lines = vec![
        format!(
            "🧩 Plugins in `{}` ({})",
            source.name,
            catalog.plugins.len()
        ),
        String::new(),
    ];
    for p in &catalog.plugins {
        let desc = p.description.as_deref().unwrap_or("");
        let ver = p
            .version
            .as_deref()
            .map(|v| format!(" v{v}"))
            .unwrap_or_default();
        if desc.is_empty() {
            lines.push(format!("• {}{ver}", p.name));
        } else {
            lines.push(format!("• {}{ver} — {desc}", p.name));
        }
    }
    lines.push(String::new());
    lines.push(format!(
        "Install with `/plugin install <name>@{}`.",
        source.name
    ));
    lines.join("\n")
}

async fn install_plugin(
    manager: Arc<Mutex<MarketplaceManager>>,
    name: String,
    source_key: Option<String>,
    force: bool,
) -> String {
    let res = tokio::task::spawn_blocking(move || {
        let mut mgr = manager.lock().unwrap();
        let source = resolve_source(&mgr, source_key.as_deref(), true)?;
        let policy = crate::security::ScanPolicy::from_config(&crate::config::Config::from_env());
        let outcome = mgr
            .install_hub_plugin(&source.id, &name, policy, force)
            .map_err(|e| e.to_string())?;

        let (dir, scan) = match outcome {
            InstallOutcome::Blocked { report, staged_dir } => {
                return Ok::<_, String>(format!(
                    "🛑 Refusing to install `{name}`: it failed the pre-install security scan \
                     (risk {}/100). Nothing was recorded or enabled.\n\n```\n{}\n```\n\
                     The clone is left at `{}` if you want to review it. \
                     To install anyway: `/plugin install {name}@{} --force`",
                    report.risk_score(),
                    report.summary(),
                    staged_dir.display(),
                    source.name,
                ));
            }
            InstallOutcome::Installed { dir, scan } => (dir, scan),
        };

        let mut msg = format!(
            "✅ Installed `{name}` from `{}` → {}",
            source.name,
            dir.display()
        );
        // A warn-level install still shows its report in chat: the whole point
        // of scanning is that the human sees what they just took on.
        if let Some(report) = &scan {
            if !report.findings.is_empty() {
                msg.push_str(&format!(
                    "\n\n⚠️ The security scan flagged this package (risk {}/100) but did not \
                     block it:\n```\n{}\n```",
                    report.risk_score(),
                    report.summary()
                ));
            }
        }
        Ok::<_, String>(msg)
    })
    .await;
    match res {
        Ok(Ok(msg)) => msg,
        Ok(Err(e)) => format!("❌ {e}"),
        Err(e) => format!("❌ install task failed: {e}"),
    }
}

async fn uninstall_plugin(
    manager: Arc<Mutex<MarketplaceManager>>,
    name: String,
    source_key: Option<String>,
) -> String {
    let res = tokio::task::spawn_blocking(move || {
        let mut mgr = manager.lock().unwrap();
        let source = resolve_source(&mgr, source_key.as_deref(), true)?;
        let removed = mgr
            .uninstall_hub_plugin(&source.id, &name)
            .map_err(|e| e.to_string())?;
        if removed {
            Ok::<_, String>(format!("🗑️ Uninstalled `{name}` from `{}`.", source.name))
        } else {
            Ok(format!(
                "ℹ️ `{name}` was not installed from `{}`.",
                source.name
            ))
        }
    })
    .await;
    match res {
        Ok(Ok(msg)) => msg,
        Ok(Err(e)) => format!("❌ {e}"),
        Err(e) => format!("❌ uninstall task failed: {e}"),
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Split `name@source` into `(name, Some(source))`; a bare `name` yields
/// `(name, None)`. Splits on the last `@` so plugin names may not contain it
/// but source keys are taken as-is.
fn parse_plugin_ref(arg: &str) -> (String, Option<String>) {
    match arg.rsplit_once('@') {
        Some((name, src)) if !name.is_empty() && !src.is_empty() => {
            (name.to_string(), Some(src.to_string()))
        }
        _ => (arg.to_string(), None),
    }
}

/// Resolve a marketplace source from a user-supplied key.
///
/// * exact case-insensitive name match wins,
/// * else a unique case-insensitive substring match,
/// * else a unique id-prefix match.
///
/// When `key` is `None` and `hub_only` is set, defaults to the sole hub source
/// (error if zero or more than one). Returns a human-readable error string.
fn resolve_source(
    mgr: &MarketplaceManager,
    key: Option<&str>,
    hub_only: bool,
) -> Result<MarketplaceSource, String> {
    let sources = mgr.get_sources();
    let candidates: Vec<MarketplaceSource> = if hub_only {
        sources
            .into_iter()
            .filter(|s| s.source_type == SourceType::Hub)
            .collect()
    } else {
        sources
    };

    let Some(key) = key else {
        return match candidates.as_slice() {
            [] => Err(if hub_only {
                "No hub sources configured. Add one with `/plugin marketplace add <url>`."
                    .to_string()
            } else {
                "No marketplace sources configured.".to_string()
            }),
            [only] => Ok(only.clone()),
            many => Err(format!(
                "Multiple sources — specify one with `@<source>`:\n{}",
                many.iter()
                    .map(|s| format!("  • {}", s.name))
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
        };
    };

    // 1. exact name (case-insensitive)
    if let Some(s) = candidates.iter().find(|s| s.name.eq_ignore_ascii_case(key)) {
        return Ok(s.clone());
    }
    // 2. unique substring match on name
    let key_lc = key.to_lowercase();
    let subs: Vec<&MarketplaceSource> = candidates
        .iter()
        .filter(|s| s.name.to_lowercase().contains(&key_lc))
        .collect();
    if subs.len() == 1 {
        return Ok(subs[0].clone());
    }
    // 3. unique id-prefix match
    let ids: Vec<&MarketplaceSource> = candidates
        .iter()
        .filter(|s| s.id.starts_with(key))
        .collect();
    if ids.len() == 1 {
        return Ok(ids[0].clone());
    }

    if subs.len() > 1 {
        return Err(format!(
            "`{key}` is ambiguous — matches: {}",
            subs.iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Err(format!(
        "No {}source matches `{key}`. List them with `/plugin marketplace list`.",
        if hub_only { "hub " } else { "" }
    ))
}

fn source_type_label(t: SourceType) -> &'static str {
    match t {
        SourceType::Hub => "hub",
        SourceType::Git => "git",
        SourceType::Local => "local",
    }
}

// ─── `/app …` — Space App update commands ────────────────────────────────────

pub const APP_HELP: &str = "\
📦 App commands (Space App từ hub):
  /app outdated                 — liệt kê app đã cài có bản mới trên hub
  /app update <id>              — cập nhật một app lên bản mới nhất
  /app update all               — cập nhật mọi app đang có bản mới
  /app help                     — hiện trợ giúp này";

/// The daemon's own loopback base URL. App state, hub access and registration
/// all live in the daemon, so the chat command is a thin client over the same
/// REST endpoints the CLI and Web UI use, rather than a second implementation.
fn daemon_base() -> String {
    let cfg = crate::config::Config::from_env();
    format!("http://127.0.0.1:{}", cfg.ui_server.port)
}

/// Try parsing `text` as an `/app …` command (Space App updates). Requires the
/// leading slash — `app` is too common a word to match bare. Returns
/// `Some(reply)` when it matched, or `None` to fall through to the agent.
pub async fn dispatch_app_command(text: &str) -> Option<String> {
    let after = text
        .trim()
        .strip_prefix("/app")
        .filter(|r| r.is_empty() || r.starts_with(char::is_whitespace))?
        .trim();

    let mut parts = after.split_whitespace();
    let sub = parts.next().unwrap_or("help").to_lowercase();
    match sub.as_str() {
        "help" | "" => Some(APP_HELP.to_string()),
        "outdated" | "updates" | "outofdate" => Some(app_outdated().await),
        "update" | "upgrade" => {
            let target = parts.next().unwrap_or("").to_string();
            Some(app_update_cmd(&target).await)
        }
        other => Some(format!("Lệnh /app không rõ: `{other}`\n{APP_HELP}")),
    }
}

async fn app_fetch_updates() -> Result<Vec<serde_json::Value>, String> {
    let url = format!("{}/api/space/apps/updates", daemon_base());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("daemon trả về HTTP {}", resp.status()));
    }
    resp.json::<Vec<serde_json::Value>>()
        .await
        .map_err(|e| e.to_string())
}

async fn app_outdated() -> String {
    let updates = match app_fetch_updates().await {
        Ok(u) => u,
        Err(e) => return format!("❌ Không kiểm tra được cập nhật: {e}"),
    };
    let outdated: Vec<&serde_json::Value> = updates
        .iter()
        .filter(|u| {
            u.get("hasUpdate")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .collect();
    if outdated.is_empty() {
        return "✅ Mọi app đã ở phiên bản mới nhất.".to_string();
    }
    let mut out = String::from("📦 Có bản mới:\n");
    for u in outdated {
        let id = u.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let cur = u
            .get("installed")
            .and_then(|v| v.as_str())
            .unwrap_or("chưa rõ");
        let latest = u.get("latest").and_then(|v| v.as_str()).unwrap_or("?");
        out.push_str(&format!("  • {id}: {cur} → {latest}\n"));
    }
    out.push_str("\nGõ `/app update <id>` hoặc `/app update all`.");
    out
}

async fn app_update_one(id: &str) -> Result<(bool, String), String> {
    let url = format!("{}/api/space/apps/{id}/update", daemon_base());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.post(&url).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        let msg = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("(không rõ lý do)");
        return Err(format!("HTTP {status}: {msg}"));
    }
    let updated = body
        .get("updated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let latest = body
        .get("latest")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    Ok((updated, latest))
}

async fn app_update_cmd(target: &str) -> String {
    if target.is_empty() {
        return format!("Cần chỉ rõ app. {APP_HELP}");
    }
    if target.eq_ignore_ascii_case("all") {
        let updates = match app_fetch_updates().await {
            Ok(u) => u,
            Err(e) => return format!("❌ Không lấy được danh sách app: {e}"),
        };
        let ids: Vec<String> = updates
            .iter()
            .filter(|u| {
                u.get("hasUpdate")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
            .filter_map(|u| u.get("id").and_then(|v| v.as_str()).map(str::to_string))
            .collect();
        if ids.is_empty() {
            return "✅ Không có app nào cần cập nhật.".to_string();
        }
        let mut out = String::new();
        for id in ids {
            match app_update_one(&id).await {
                Ok((true, latest)) => out.push_str(&format!("✓ {id} → {latest}\n")),
                Ok((false, _)) => out.push_str(&format!("• {id} đã mới nhất\n")),
                Err(e) => out.push_str(&format!("⚠ {id}: {e}\n")),
            }
        }
        return out;
    }
    match app_update_one(target).await {
        Ok((true, latest)) => format!("✓ Đã cập nhật {target} → {latest}"),
        Ok((false, _)) => format!("• {target} đã ở bản mới nhất."),
        Err(e) => format!("❌ Cập nhật {target} thất bại: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ref_splits_on_at() {
        assert_eq!(parse_plugin_ref("foo"), ("foo".to_string(), None));
        assert_eq!(
            parse_plugin_ref("foo@senclaw"),
            ("foo".to_string(), Some("senclaw".to_string()))
        );
        // Leading/trailing degenerate `@` falls back to a bare name.
        assert_eq!(parse_plugin_ref("@foo"), ("@foo".to_string(), None));
        assert_eq!(parse_plugin_ref("foo@"), ("foo@".to_string(), None));
    }

    #[test]
    fn help_and_prefix_matching() {
        // These do not touch the manager, so a dummy is fine — but we only test
        // the prefix filter here via a lightweight reimplementation to avoid
        // constructing a manager. The real dispatch is covered by integration.
        let is_cmd = |t: &str| {
            let t = t.trim();
            t.strip_prefix("/plugin")
                .or_else(|| t.strip_prefix("plugin"))
                .filter(|r| r.is_empty() || r.starts_with(char::is_whitespace))
                .is_some()
        };
        assert!(is_cmd("/plugin"));
        assert!(is_cmd("/plugin install foo@bar"));
        assert!(is_cmd("plugin list"));
        assert!(!is_cmd("/plugins"));
        assert!(!is_cmd("/pluginfoo"));
        assert!(!is_cmd("hello /plugin"));
    }
}
