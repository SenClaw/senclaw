//! Per-Space-App sandbox settings — the three questions the Plugins → Space
//! Apps dialog asks about one app:
//!
//! 1. **Does it run inside the sandbox at all?** (`enabled`)
//! 2. **Which folders does it get?** (`read_mode` + `folders`, on top of the
//!    app's own directory and its own data directory, which are always granted)
//! 3. **How much of the network?** (`network`: none / everything / only these
//!    sites — see [`NetMode::Hosts`])
//!
//! A Space App is not a snippet: it is a long-lived server the daemon spawns,
//! reaches over loopback, and proxies a UI from. So this is deliberately *not*
//! the `Sandbox` row model used for `sbx_exec` runs — there is no workspace to
//! copy into, no run to record, and the app's own paths must keep working
//! unchanged. What is shared is the enforcement machinery: the same Seatbelt
//! profile builder and the same bubblewrap argument builder.
//!
//! # Why the defaults are what they are
//!
//! Turning the switch on must not silently break the app, or nobody will keep
//! it on. So `enabled` buys the **write jail** first (the app can only write its
//! own folders), with reads `open` and the network untouched. Narrowing reads to
//! `strict` and egress to an allowlist are the second and third steps, each of
//! which can break an app in a way the user can see and undo.
//!
//! `daemon_api` is the one grant that looks wrong and is not: nearly every app
//! calls SenClaw's own API on loopback for the AI bridge
//! (`SENCLAW_BASE_URL/api/space/apps/<id>/bridge`), so refusing it by default
//! would break the AI features of most installed apps. It is a checkbox, and
//! the dialog says what it costs.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::sandbox::db::Db;
use crate::sandbox::fsmode::FsMode;
use crate::sandbox::shared_db;

/// How much of the network a sandboxed app gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetMode {
    /// No outbound network at all.
    Off,
    /// Everything, like an app running outside the sandbox.
    All,
    /// Only the hostnames in `hosts`, through the allowlisting proxy.
    ///
    /// This is a *proxy* rather than a firewall rule because no OS sandbox here
    /// can filter by hostname — macOS Seatbelt's profile language accepts only
    /// `*` and `localhost` as a remote host, and bubblewrap has no per-host
    /// concept either. So the sandbox is given no direct egress whatsoever and
    /// one loopback port: SenClaw's allowlisting proxy. An HTTP client that
    /// honours `HTTP_PROXY` reaches the allowed sites; one that ignores it
    /// reaches *nothing*, because its direct connection is denied by the
    /// sandbox. Wrong-way failures are therefore closed, not open.
    Hosts,
}

impl Default for NetMode {
    fn default() -> Self {
        // Not `Off`: the point of the first switch is the write jail, and an app
        // that silently loses the internet looks broken rather than confined.
        NetMode::All
    }
}

impl NetMode {
    pub fn as_str(self) -> &'static str {
        match self {
            NetMode::Off => "off",
            NetMode::All => "all",
            NetMode::Hosts => "hosts",
        }
    }
}

/// A folder the user granted this app beyond its own directories.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AppFolder {
    /// Absolute path on this machine, canonicalised on save.
    pub path: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSandbox {
    /// Run this app's server process inside the OS sandbox.
    pub enabled: bool,
    /// How much of the disk the app may **read**. `open` (everything but the
    /// credential stores) is the default because a server needs its runtime:
    /// the interpreter, `node_modules`, the system libraries. `strict` keeps
    /// the system roots and the app's own folders and removes the rest of the
    /// user's disk.
    pub read_mode: FsMode,
    /// Extra folders, on top of the app's own directory + data directory.
    pub folders: Vec<AppFolder>,
    pub network: NetMode,
    /// Hostnames for `network: hosts`. `*.example.com` covers the subdomains
    /// and the apex.
    pub hosts: Vec<String>,
    /// The app may call SenClaw's own API on loopback — required by the AI
    /// bridge, which is what most apps use for anything intelligent.
    pub daemon_api: bool,
    /// Other services on this machine the app may dial (a database, another
    /// app). Loopback is closed apart from these.
    pub loopback: Vec<u16>,
    /// The app's own manifest declares `sandbox.force` — the settings dialog
    /// may not turn the sandbox off. Set from the manifest on install and on
    /// boot ([`crate::apps::sandbox_decl`]), never by the user directly; an app
    /// that drops the declaration drops the lock with it.
    pub forced: bool,
}

impl Default for AppSandbox {
    fn default() -> Self {
        AppSandbox {
            enabled: false,
            read_mode: FsMode::Open,
            folders: Vec::new(),
            network: NetMode::All,
            hosts: Vec::new(),
            daemon_api: true,
            loopback: Vec::new(),
            forced: false,
        }
    }
}

/// Hostnames that are never allowlistable, whatever the user types.
///
/// `localhost` is the interesting one: routing it through the proxy would hand
/// the sandbox back the loopback interface the profile just closed — SenClaw's
/// own unauthenticated API included. The link-local addresses are the cloud
/// metadata endpoints, which hand out instance credentials to anything that
/// asks. Local services get named as ports in `loopback`, where the user can
/// see exactly which one they opened.
const NEVER_ALLOWED_HOSTS: &[&str] = &[
    "localhost",
    "127.0.0.1",
    "0.0.0.0",
    "::1",
    "169.254.169.254",
    "metadata",
    "metadata.google.internal",
];

/// Keeps a dialog (and a proxy decision) readable and bounded.
const MAX_HOSTS: usize = 64;
const MAX_FOLDERS: usize = 16;

/// Normalise one allowlist entry. Accepts what people actually paste — a URL, a
/// host with a port, a trailing dot, mixed case — and returns the bare
/// hostname, or an explanation of why it cannot be allowlisted.
pub fn normalise_host(raw: &str) -> Result<String> {
    let mut h = raw.trim().to_ascii_lowercase();
    if h.is_empty() {
        return Err(anyhow!("empty hostname"));
    }
    // A pasted URL: keep the host, drop scheme, path, credentials, port.
    if let Some(rest) = h.split("://").nth(1) {
        h = rest.to_string();
    }
    if let Some((_, after)) = h.rsplit_once('@') {
        h = after.to_string();
    }
    h = h.split('/').next().unwrap_or_default().to_string();
    // `host:port` loses the port — the allowlist is about *where*, and which
    // port is allowed is already decided by the proxy (80/443).
    if let Some((host, port)) = h.rsplit_once(':') {
        if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            h = host.to_string();
        }
    }
    let h = h.trim_end_matches('.').to_string();
    if h.is_empty() {
        return Err(anyhow!("`{raw}` has no hostname in it"));
    }
    let body = h.strip_prefix("*.").unwrap_or(&h);
    if body.is_empty() || body.contains('*') {
        return Err(anyhow!(
            "`{raw}`: a wildcard is only allowed as a leading `*.` label"
        ));
    }
    // No IPv6 literals. They are never what someone means by "this website",
    // and `[::1]` is a spelling of loopback that would otherwise slip past the
    // never-allowed list below.
    if body.contains(':') || body.contains('[') || body.contains(']') {
        return Err(anyhow!(
            "`{raw}`: use a hostname (or an IPv4 address), not an IPv6 literal"
        ));
    }
    if body
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_'))
    {
        return Err(anyhow!("`{raw}` is not a hostname"));
    }
    if NEVER_ALLOWED_HOSTS.contains(&body) {
        return Err(anyhow!(
            "`{body}` cannot be allowlisted: it is this machine itself (or a cloud metadata \
             endpoint). Open a specific port under \"local services\" instead — that way the \
             port you granted is visible."
        ));
    }
    Ok(h)
}

/// True when `host` is covered by the allowlist. `*.example.com` matches the
/// subdomains **and** the apex, because that is what people mean by it.
pub fn host_allowed(host: &str, allow: &[String]) -> bool {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    allow.iter().any(|entry| {
        let e = entry.trim().to_ascii_lowercase();
        match e.strip_prefix("*.") {
            Some(suffix) => h == suffix || h.ends_with(&format!(".{suffix}")),
            None => h == e,
        }
    })
}

/// Check and canonicalise a config before it is stored. Rejects rather than
/// repairs: a folder the user believes they granted but did not is discovered
/// much later, through confusion.
pub fn validate(cfg: &AppSandbox) -> Result<AppSandbox> {
    let mut out = cfg.clone();

    if out.folders.len() > MAX_FOLDERS {
        return Err(anyhow!(
            "too many folders: {} (max {MAX_FOLDERS})",
            out.folders.len()
        ));
    }
    let mut folders: Vec<AppFolder> = Vec::new();
    for f in &out.folders {
        // Same guard list as sandbox mounts: no `/`, no `$HOME` itself, no
        // credential store, nothing inside the sandbox engine's own data.
        let m = crate::sandbox::mounts::validate(&f.path, "", f.read_only)
            .map_err(|e| anyhow!("folder `{}`: {e}", f.path))?;
        if !folders.iter().any(|x: &AppFolder| x.path == m.source) {
            folders.push(AppFolder {
                path: m.source,
                read_only: f.read_only,
            });
        }
    }
    out.folders = folders;

    if out.hosts.len() > MAX_HOSTS {
        return Err(anyhow!("too many hosts: {} (max {MAX_HOSTS})", out.hosts.len()));
    }
    let mut hosts: Vec<String> = Vec::new();
    for h in &out.hosts {
        if h.trim().is_empty() {
            continue;
        }
        let h = normalise_host(h)?;
        if !hosts.contains(&h) {
            hosts.push(h);
        }
    }
    out.hosts = hosts;

    out.loopback = crate::sandbox::ports::validate(&[], &[], &out.loopback)?.loopback;
    Ok(out)
}

fn key(app_id: &str) -> String {
    format!("app_sandbox:{app_id}")
}

pub fn load(db: &Db, app_id: &str) -> AppSandbox {
    db.setting(&key(app_id))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<AppSandbox>(&s).ok())
        .unwrap_or_default()
}

pub fn save(db: &Db, app_id: &str, cfg: &AppSandbox) -> Result<AppSandbox> {
    let clean = validate(cfg)?;
    db.set_setting(&key(app_id), &serde_json::to_string(&clean)?)?;
    Ok(clean)
}

/// The config in force right now, straight from the shared engine DB. Falls
/// back to the default (sandbox off) when the engine is unavailable, which
/// keeps app launches on their historical path instead of failing them.
pub fn current(app_id: &str) -> AppSandbox {
    match shared_db() {
        Some(db) => load(&db, app_id),
        None => AppSandbox::default(),
    }
}

/// The app's own data directories — always granted read+write, because they are
/// where the app keeps its database and the app is the only thing in them.
///
/// Apps in this repo settled on several spellings of "my data dir" over time
/// (`~/.senclaw/apps/<id>`, `~/.senclaw/space-apps/<id>`, `~/.senclaw/<id>`,
/// …). Rather than force a migration, every per-app spelling is granted: each
/// is scoped to this one app, so granting a path the app happens not to use
/// costs nothing.
pub fn own_data_dirs(app_id: &str) -> Vec<PathBuf> {
    match std::env::var("HOME") {
        Ok(h) if !h.trim().is_empty() => own_data_dirs_in(Path::new(&h), app_id),
        _ => Vec::new(),
    }
}

/// The list itself, with `$HOME` passed in — pure, so it can be asserted on
/// without a test mutating the process environment out from under its
/// neighbours.
pub fn own_data_dirs_in(home: &Path, app_id: &str) -> Vec<PathBuf> {
    let root = home.join(".senclaw");
    vec![
        root.join("apps").join(app_id),
        root.join("space-apps").join(app_id),
        root.join("space-apps-data").join(app_id),
        root.join("space-app-data").join(app_id),
        root.join(app_id),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_switch_only_buys_the_write_jail() {
        let d = AppSandbox::default();
        assert!(!d.enabled, "must be opt-in per app");
        assert_eq!(d.read_mode, FsMode::Open, "an app needs its runtime readable");
        assert_eq!(d.network, NetMode::All, "losing the internet silently looks broken");
        assert!(d.daemon_api, "the AI bridge is loopback; most apps use it");
    }

    #[test]
    fn a_pasted_url_becomes_a_hostname() {
        assert_eq!(normalise_host("https://API.Example.com/v1/x?y=1").unwrap(), "api.example.com");
        assert_eq!(normalise_host("example.com:8443").unwrap(), "example.com");
        assert_eq!(normalise_host("  example.com.  ").unwrap(), "example.com");
        assert_eq!(normalise_host("*.example.com").unwrap(), "*.example.com");
        assert!(normalise_host("").is_err());
        assert!(normalise_host("exa*mple.com").is_err(), "wildcard only as a leading label");
        assert!(normalise_host("has space.com").is_err());
    }

    #[test]
    fn this_machine_can_never_be_allowlisted() {
        // Allowlisting `localhost` would route the sandbox back to the loopback
        // interface the profile just closed — the daemon's own unauthenticated
        // API included. That is the escape this whole feature exists to avoid.
        for h in ["localhost", "127.0.0.1", "http://localhost:18788", "169.254.169.254"] {
            let e = normalise_host(h).unwrap_err().to_string();
            assert!(e.contains("cannot be allowlisted"), "{h} → {e}");
        }
        // …and the error points at the mechanism that *is* right for it.
        assert!(normalise_host("localhost").unwrap_err().to_string().contains("local services"));
        // `[::1]` is loopback spelled a way the list above would not catch, so
        // IPv6 literals are refused outright rather than pattern-matched.
        for h in ["[::1]:443", "[::1]", "fe80::1"] {
            assert!(normalise_host(h).is_err(), "{h} must not be allowlistable");
        }
    }

    #[test]
    fn wildcards_cover_the_apex_and_the_subdomains_only() {
        let allow = vec!["*.example.com".to_string(), "openai.com".to_string()];
        assert!(host_allowed("example.com", &allow));
        assert!(host_allowed("api.example.com", &allow));
        assert!(host_allowed("a.b.example.com", &allow));
        assert!(host_allowed("openai.com", &allow));
        // The classic suffix-match bug: a look-alike domain must not pass.
        assert!(!host_allowed("notexample.com", &allow));
        assert!(!host_allowed("example.com.evil.net", &allow));
        assert!(!host_allowed("api.openai.com", &allow), "no wildcard was asked for");
        assert!(!host_allowed("example.org", &allow));
    }

    #[test]
    fn matching_is_case_and_trailing_dot_insensitive() {
        let allow = vec!["Example.COM".to_string()];
        assert!(host_allowed("example.com.", &allow));
        assert!(host_allowed("EXAMPLE.com", &allow));
    }

    #[test]
    fn validate_canonicalises_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("data");
        std::fs::create_dir_all(&sub).unwrap();
        let weird = format!("{}/../data", sub.to_string_lossy());
        let cfg = AppSandbox {
            folders: vec![
                AppFolder { path: sub.to_string_lossy().to_string(), read_only: false },
                AppFolder { path: weird, read_only: false },
            ],
            hosts: vec!["Example.com".into(), "https://example.com/x".into(), "  ".into()],
            ..Default::default()
        };
        let out = validate(&cfg).unwrap();
        assert_eq!(out.folders.len(), 1, "the same folder twice is one folder");
        assert_eq!(out.hosts, vec!["example.com"], "and the same host twice is one host");
    }

    #[test]
    fn the_folder_guard_list_is_the_mount_guard_list() {
        // No re-implementation: mounting `~/.ssh` is refused for a sandbox, so
        // granting it to an app is refused too.
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return;
        }
        let ssh = format!("{home}/.ssh");
        if !std::path::Path::new(&ssh).is_dir() {
            return; // no ~/.ssh on this machine; nothing to assert against
        }
        let cfg = AppSandbox {
            folders: vec![AppFolder { path: ssh, read_only: true }],
            ..Default::default()
        };
        assert!(validate(&cfg).is_err(), "the credential stores stay out of reach");
    }

    #[test]
    fn save_load_round_trips_per_app() {
        let db = Db::open_memory().unwrap();
        let mut cfg = AppSandbox { enabled: true, network: NetMode::Hosts, ..Default::default() };
        cfg.hosts = vec!["api.openai.com".into()];
        save(&db, "crm", &cfg).unwrap();

        let back = load(&db, "crm");
        assert!(back.enabled && back.network == NetMode::Hosts);
        assert_eq!(back.hosts, vec!["api.openai.com"]);
        // One app's setting is not another's.
        assert!(!load(&db, "kanban").enabled);

        db.set_setting(&key("crm"), "{not json").unwrap();
        assert!(!load(&db, "crm").enabled, "a corrupt row must fall back to off");
    }

    #[test]
    fn a_partial_row_keeps_the_defaults_for_what_it_omits() {
        let db = Db::open_memory().unwrap();
        db.set_setting(&key("news"), r#"{"enabled":true}"#).unwrap();
        let c = load(&db, "news");
        assert!(c.enabled);
        assert!(c.daemon_api && c.network == NetMode::All);
    }

    #[test]
    fn every_granted_data_dir_belongs_to_the_one_app() {
        let dirs = own_data_dirs_in(Path::new("/Users/tester"), "crm");
        assert!(!dirs.is_empty());
        for d in &dirs {
            let s = d.to_string_lossy();
            assert!(s.contains("crm"), "{s} would hand over another app's data");
            assert!(s.starts_with("/Users/tester/.senclaw"), "{s}");
        }
    }
}
