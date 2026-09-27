//! Applying the sandbox an app declares for itself.
//!
//! Per-app sandbox settings start *off* and live in the engine DB, edited from
//! Plugins → Space Apps. That is right for an app the user found somewhere: no
//! app should be able to quietly widen its own confinement. It is wrong for the
//! other direction — an app that only ever talks to one API, and knows it, had
//! no way to say so, and the user had no way to know that narrowing it was safe.
//!
//! So a manifest may declare a `sandbox` block:
//!
//! ```json
//! "sandbox": {
//!   "force": true,
//!   "readMode": "strict",
//!   "network": "hosts",
//!   "hosts": ["api.openai.com"],
//!   "daemonApi": true,
//!   "loopback": [],
//!   "folders": [{ "path": "~/Movies", "readOnly": true }]
//! }
//! ```
//!
//! Two rules keep this from being a hole:
//!
//! 1. A declaration is applied on install and on every boot **only** when it is
//!    `force`d, or when the user has never set anything for this app. An app
//!    update can therefore not undo a choice the user made.
//! 2. `force` is stored on the saved config, and the settings endpoint refuses
//!    to disable a forced sandbox. The app asked to be confined; letting the
//!    dialog turn that off would make the declaration decorative.
//!
//! Nothing here can *widen* anything on its own behalf that the user would not
//! see: `validate()` in `app_policy` still runs, so the same folder guard list
//! (no `/`, no `$HOME`, no credential stores) and the same never-allowlistable
//! hosts apply to a declaration as to a hand-typed setting.

use std::path::PathBuf;

use crate::sandbox::app_policy::{self, AppFolder, AppSandbox, NetMode};
use crate::sandbox::fsmode::FsMode;

use super::manifest::SandboxDecl;

/// Turn a declaration into a config, on top of whatever is stored today.
///
/// Returns `None` when nothing is declared, or when a non-forced declaration
/// meets a user who has already decided.
pub fn resolve(decl: &SandboxDecl, stored: Option<&AppSandbox>, user_has_chosen: bool) -> Option<AppSandbox> {
    if !decl.declared {
        // The app says nothing. If a previous version of it forced a sandbox,
        // that flag must not survive the manifest that no longer asks for it.
        return match stored {
            Some(s) if s.forced => Some(AppSandbox { forced: false, ..s.clone() }),
            _ => None,
        };
    }
    if user_has_chosen && !decl.force {
        return None;
    }

    let mut cfg = stored.cloned().unwrap_or_default();
    cfg.enabled = decl.enabled;
    cfg.forced = decl.force;
    if let Some(rm) = decl.read_mode.as_deref().and_then(FsMode::parse) {
        cfg.read_mode = rm;
    }
    if let Some(net) = decl.network.as_deref() {
        cfg.network = match net.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "no" => NetMode::Off,
            "hosts" | "allowlist" | "sites" => NetMode::Hosts,
            _ => NetMode::All,
        };
    }
    if !decl.hosts.is_empty() {
        cfg.hosts = decl.hosts.clone();
    }
    if let Some(api) = decl.daemon_api {
        cfg.daemon_api = api;
    }
    if !decl.loopback.is_empty() {
        cfg.loopback = decl.loopback.clone();
    }
    if !decl.folders.is_empty() {
        cfg.folders = decl
            .folders
            .iter()
            .map(|(p, ro)| AppFolder { path: expand_home(p), read_only: *ro })
            .collect();
    }
    Some(cfg)
}

/// `~/Movies` → `/Users/x/Movies`. A manifest is written once and installed on
/// many machines, so the only portable way to name a folder in `$HOME` is `~`.
fn expand_home(path: &str) -> String {
    let p = path.trim();
    if p == "~" || p.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            let rest = p.trim_start_matches('~').trim_start_matches('/');
            let joined: PathBuf = if rest.is_empty() { home } else { home.join(rest) };
            return joined.to_string_lossy().to_string();
        }
    }
    p.to_string()
}

/// Apply an app's declaration to the shared engine DB. Best-effort and quiet:
/// a machine with no sandbox engine available keeps launching apps as before.
///
/// `user_has_chosen` is "there is already a row for this app" — the user opened
/// the dialog and pressed Save at least once.
pub fn apply(app_id: &str, manifest: &serde_json::Value) -> Option<AppSandbox> {
    let decl = SandboxDecl::parse(manifest);
    let db = crate::sandbox::shared_db()?;
    let stored = app_policy::load(&db, app_id);
    let existed = has_row(&db, app_id);
    let next = resolve(&decl, Some(&stored), existed)?;
    if next == stored {
        return Some(stored);
    }
    match app_policy::save(&db, app_id, &next) {
        Ok(saved) => {
            tracing::info!(
                "[space-sandbox] '{app_id}': applied declared sandbox (enabled={}, network={}, \
                 read={}, forced={})",
                saved.enabled,
                saved.network.as_str(),
                saved.read_mode.as_str(),
                saved.forced
            );
            Some(saved)
        }
        Err(e) => {
            tracing::warn!("[space-sandbox] '{app_id}': declared sandbox rejected: {e:#}");
            None
        }
    }
}

fn has_row(db: &crate::sandbox::db::Db, app_id: &str) -> bool {
    db.setting(&format!("app_sandbox:{app_id}"))
        .ok()
        .flatten()
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::manifest::SandboxDecl;
    use serde_json::json;

    fn decl(v: serde_json::Value) -> SandboxDecl {
        SandboxDecl::parse(&v)
    }

    #[test]
    fn an_app_that_declares_nothing_changes_nothing() {
        assert!(resolve(&decl(json!({})), None, false).is_none());
        let stored = AppSandbox { enabled: true, ..Default::default() };
        assert!(resolve(&decl(json!({})), Some(&stored), true).is_none());
    }

    #[test]
    fn a_declaration_applies_on_a_fresh_install() {
        let d = decl(json!({"sandbox": {"enabled": true, "network": "hosts", "hosts": ["api.openai.com"]}}));
        let c = resolve(&d, None, false).unwrap();
        assert!(c.enabled && !c.forced);
        assert_eq!(c.network, NetMode::Hosts);
        assert_eq!(c.hosts, vec!["api.openai.com"]);
    }

    #[test]
    fn a_users_choice_survives_an_app_update() {
        // The property that stops a manifest from being a way to widen itself:
        // once the user has saved a setting, a non-forced declaration is inert.
        let d = decl(json!({"sandbox": {"enabled": true, "network": "off"}}));
        let stored = AppSandbox { enabled: false, ..Default::default() };
        assert!(resolve(&d, Some(&stored), true).is_none());
    }

    #[test]
    fn force_overrides_the_user_every_time() {
        let d = decl(json!({"sandbox": {"force": true, "readMode": "strict", "network": "off"}}));
        let stored = AppSandbox { enabled: false, ..Default::default() };
        let c = resolve(&d, Some(&stored), true).unwrap();
        assert!(c.enabled && c.forced);
        assert_eq!(c.read_mode, FsMode::Strict);
        assert_eq!(c.network, NetMode::Off);
    }

    #[test]
    fn dropping_force_from_a_manifest_drops_the_flag() {
        // Otherwise an app could force a sandbox once and leave the user unable
        // to change it for as long as the install lives.
        let stored = AppSandbox { enabled: true, forced: true, ..Default::default() };
        let c = resolve(&decl(json!({})), Some(&stored), true).unwrap();
        assert!(!c.forced);
        assert!(c.enabled, "the confinement stays; only the lock is released");
    }

    #[test]
    fn a_declaration_only_overrides_what_it_mentions() {
        let d = decl(json!({"sandbox": {"force": true, "network": "off"}}));
        let stored = AppSandbox {
            read_mode: FsMode::Strict,
            loopback: vec![5432],
            ..Default::default()
        };
        let c = resolve(&d, Some(&stored), true).unwrap();
        assert_eq!(c.read_mode, FsMode::Strict, "not mentioned → not touched");
        assert_eq!(c.loopback, vec![5432]);
        assert_eq!(c.network, NetMode::Off);
    }

    #[test]
    fn a_home_relative_folder_becomes_an_absolute_one() {
        let home = dirs::home_dir().unwrap_or_default().to_string_lossy().to_string();
        if home.is_empty() {
            return;
        }
        let d = decl(json!({"sandbox": {"force": true, "folders": [{"path": "~/Movies", "readOnly": true}]}}));
        let c = resolve(&d, None, false).unwrap();
        assert_eq!(c.folders[0].path, format!("{home}/Movies"));
        assert!(c.folders[0].read_only);
        // An already-absolute path is left alone.
        let d = decl(json!({"sandbox": {"force": true, "folders": ["/tmp/x"]}}));
        assert_eq!(resolve(&d, None, false).unwrap().folders[0].path, "/tmp/x");
    }
}
