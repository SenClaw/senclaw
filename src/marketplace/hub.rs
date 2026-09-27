//! Hub store support — marketplace catalogs served over plain HTTP.
//!
//! A hub is a URL that serves a `marketplace.json` catalog (the Claude Code
//! marketplace format): an index of plugins, each pointing at a git repository.
//! Unlike a `git` source — which is one repo cloned wholesale and scanned for
//! plugin directories — a hub source is browsed first and installed per plugin:
//! only the plugins the user picks get cloned into the source directory.
//!
//! Layout under a hub source's `local_path`:
//!
//! ```text
//! <local_path>/catalog.json     cached copy of the remote marketplace.json
//! <local_path>/installed.json   manifest: plugin name → resolved dir + origin
//! <local_path>/repos/<plugin>/  git clone backing one installed plugin
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// The hub SenClaw ships with. Override per-install with `SENCLAW_HUB_URL`.
pub const DEFAULT_HUB_URL: &str = "https://senclaw.bacnd.com";

/// The hub's pre-rename hostname — same server as [`DEFAULT_HUB_URL`], kept
/// serving so old links don't 404. Sources seeded before the rename still
/// point here; [`super::MarketplaceManager::migrate_legacy_hub_url`] rewrites
/// them on load.
pub const LEGACY_HUB_URL: &str = "https://hub-store.bacnd.com";

/// Filename a hub URL is assumed to serve when it points at a directory.
const CATALOG_FILE: &str = "marketplace.json";

/// SenClaw's own catalog endpoint, relative to a hub's home.
///
/// `marketplace.json` is a third-party plugin index by format: the hub filters
/// it to `kind = "plugin"`, so a hub whose packages are apps serves an empty
/// document there and the store browses as empty even though every package is
/// installable. This endpoint is the same catalogue without that filter. A hub
/// that does not implement it just keeps its `marketplace.json` entries — see
/// [`fetch_catalog`].
const REGISTRY_CATALOG_PATH: &str = "/api/v1/packages";

const CATALOG_CACHE: &str = "catalog.json";
const INSTALLED_MANIFEST: &str = "installed.json";
const REPOS_DIR: &str = "repos";

// ── Catalog ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubOwner {
    pub name: Option<String>,
    pub url: Option<String>,
}

/// Where a catalog entry's code lives. Accepts both the shorthand
/// (`"source": "https://github.com/u/r"`) and the object form
/// (`"source": {"source": "github", "repo": "u/r"}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HubPluginSource {
    Url(String),
    Spec {
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        repo: Option<String>,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default, rename = "ref")]
        git_ref: Option<String>,
    },
}

/// A git checkout to perform for one catalog entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitTarget {
    pub url: String,
    pub branch: String,
    /// Sub-directory inside the repo holding the plugin, when the catalog says so.
    pub subdir: Option<String>,
}

impl HubPluginSource {
    /// Resolve the entry into something `git_sync::clone_or_pull` can take.
    pub fn git_target(&self) -> Result<GitTarget> {
        match self {
            Self::Url(s) => {
                let s = s.trim();
                if is_git_url(s) {
                    Ok(GitTarget {
                        url: s.to_string(),
                        branch: "main".to_string(),
                        subdir: None,
                    })
                } else if let Some(repo) = as_owner_repo(s) {
                    Ok(GitTarget {
                        url: github_url(&repo),
                        branch: "main".to_string(),
                        subdir: None,
                    })
                } else {
                    // A bare relative path only resolves inside a git-backed
                    // marketplace repo; an HTTP catalog has no tree to read.
                    bail!(
                        "plugin source {s:?} is a path relative to the marketplace repo, \
                         which an HTTP hub cannot resolve — add it as a git source instead"
                    )
                }
            }
            Self::Spec {
                source,
                repo,
                url,
                path,
                branch,
                git_ref,
            } => {
                let kind = source.as_deref().unwrap_or("git");
                let url = match kind {
                    "github" => {
                        let repo = repo.as_deref().or(url.as_deref()).ok_or_else(|| {
                            anyhow::anyhow!("github plugin source is missing `repo`")
                        })?;
                        if is_git_url(repo) {
                            repo.to_string()
                        } else {
                            github_url(repo)
                        }
                    }
                    "git" | "url" => url
                        .as_deref()
                        .or(repo.as_deref())
                        .ok_or_else(|| anyhow::anyhow!("git plugin source is missing `url`"))?
                        .to_string(),
                    other => bail!("unsupported plugin source type {other:?}"),
                };
                Ok(GitTarget {
                    url,
                    branch: branch
                        .clone()
                        .or_else(|| git_ref.clone())
                        .unwrap_or_else(|| "main".to_string()),
                    subdir: path.clone(),
                })
            }
        }
    }
}

fn is_git_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://") || s.starts_with("git@")
}

/// `"owner/repo"` → `Some("owner/repo")`; anything else → None.
fn as_owner_repo(s: &str) -> Option<String> {
    let mut parts = s.split('/');
    let (owner, repo) = (parts.next()?, parts.next()?);
    if parts.next().is_some() || owner.is_empty() || repo.is_empty() || owner.starts_with('.') {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

fn github_url(repo: &str) -> String {
    format!("https://github.com/{}", repo.trim_end_matches(".git"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubPlugin {
    pub name: String,
    /// Absent for registry entries: an app is fetched as a signed artifact by
    /// slug, not cloned from git, so there is no repository to point at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<HubPluginSource>,
    /// `plugin` (the default, for a `marketplace.json` entry), or `app` /
    /// `skill` / `workflow` for a registry entry. What the installing client
    /// must dispatch on — the two install paths are not interchangeable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `scope/name` — the registry coordinate `POST /api/marketplace/hub/install`
    /// takes. Absent for `marketplace.json` entries, which are keyed by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// 30-day download count, when the hub reports one. Browse-order signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloads: Option<u64>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub author: Option<serde_json::Value>,
    #[serde(default)]
    pub keywords: Option<Vec<String>>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
}

/// A kit offered by a marketplace source.
///
/// A kit is not a plugin and not an app: it installs into the daemon's own
/// building blocks (personas, skills, workflows, hooks, jobs) through
/// `/api/kits/install`, so it is listed separately rather than squeezed into
/// `plugins[]` behind another `kind`. What travels is one artifact — a `.json`
/// manifest or a `.zip` bundle — named by [`Self::url`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubKit {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    /// Where the artifact lives. An `http(s)://` URL is downloaded; anything
    /// else is a path inside the source's own directory, which is what lets a
    /// git source ship its kits in the repo it is already cloning.
    #[serde(default, alias = "source", alias = "path")]
    pub url: Option<String>,
    /// The kit id the manifest declares, when the catalog knows it. Lets the
    /// list mark a kit as already installed without downloading it first.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub author: Option<serde_json::Value>,
    #[serde(default)]
    pub keywords: Option<Vec<String>>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HubCatalog {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub owner: Option<HubOwner>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub plugins: Vec<HubPlugin>,
    /// Kits this source offers. Absent in every catalog written before kits
    /// existed, which is why it defaults rather than being required.
    #[serde(default)]
    pub kits: Vec<HubKit>,
}

impl HubPlugin {
    /// Whether this entry installs by cloning a git repo (the `/plugin install`
    /// path) rather than by pulling a signed artifact from the registry.
    pub fn is_git_plugin(&self) -> bool {
        self.source.is_some() && self.kind.as_deref().unwrap_or("plugin") == "plugin"
    }

    /// The clone to perform, or an error naming the right path when this entry
    /// is a registry package.
    ///
    /// Worth an explicit message: the two install routes look identical from the
    /// UI, and the failure would otherwise surface as "missing field `source`"
    /// deep in a deserializer.
    pub fn git_target(&self) -> Result<GitTarget> {
        match &self.source {
            Some(src) => src.git_target(),
            None => {
                let kind = self.kind.as_deref().unwrap_or("package");
                let slug = self.slug.as_deref().unwrap_or(&self.name);
                bail!(
                    "{} is a {kind} in the hub registry, not a git-hosted plugin — \
                     install it with POST /api/marketplace/hub/install {{\"slug\":\"{slug}\"}}",
                    self.name
                )
            }
        }
    }
}

impl HubCatalog {
    pub fn find(&self, name: &str) -> Option<&HubPlugin> {
        self.plugins
            .iter()
            .find(|p| p.name == name || p.slug.as_deref() == Some(name))
    }
}

/// One row of the hub's own `/api/v1/packages` listing.
#[derive(Debug, Clone, Deserialize)]
struct RegistryPackage {
    /// `scope/name`.
    slug: String,
    kind: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default, rename = "latestVersion")]
    latest_version: Option<String>,
    #[serde(default, rename = "downloads30d")]
    downloads30d: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct RegistryCatalog {
    #[serde(default)]
    packages: Vec<RegistryPackage>,
}

impl From<RegistryPackage> for HubPlugin {
    fn from(p: RegistryPackage) -> Self {
        // Display by bare name; the scope is carried in `slug`, which is what
        // installs are keyed on. Showing "senclaw/email" in a grid of names is
        // noise when every package shares one scope.
        let name = p
            .slug
            .rsplit_once('/')
            .map(|(_, n)| n.to_string())
            .unwrap_or_else(|| p.slug.clone());
        HubPlugin {
            name,
            source: None,
            kind: Some(p.kind),
            slug: Some(p.slug),
            downloads: p.downloads30d,
            description: p.description,
            version: p.latest_version,
            author: p.owner.map(serde_json::Value::String),
            keywords: None,
            repository: None,
            license: None,
            category: p.category,
            homepage: None,
        }
    }
}

// ── Catalog fetch ────────────────────────────────────────────────────────────

/// Turn whatever the user typed into the URL of the catalog document.
/// `https://senclaw.bacnd.com` → `https://senclaw.bacnd.com/marketplace.json`.
pub fn normalize_catalog_url(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    if url.ends_with(".json") {
        url.to_string()
    } else {
        format!("{url}/{CATALOG_FILE}")
    }
}

/// The site a catalog URL belongs to — what we show as the source's home.
pub fn catalog_home(url: &str) -> String {
    url.trim()
        .trim_end_matches('/')
        .trim_end_matches(CATALOG_FILE)
        .trim_end_matches('/')
        .to_string()
}

pub(crate) fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("senclaw/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build HTTP client")
}

/// Blocking HTTP GET of a catalog. Call from a blocking context
/// (`spawn_blocking`) — reqwest's blocking client cannot run on a reactor thread.
///
/// Two documents, merged. `marketplace.json` is the interop index and is
/// required; the hub's own `/api/v1/packages` carries the kinds that index
/// filters out (apps, skills, workflows) and is best-effort, because a plain
/// static hub serves only the former. Without the merge a hub that publishes
/// apps browses as an empty catalog while every one of its packages installs
/// fine — which is exactly what the shipped hub does.
pub fn fetch_catalog(url: &str) -> Result<HubCatalog> {
    let catalog_url = normalize_catalog_url(url);
    let client = http_client()?;

    let res = client
        .get(&catalog_url)
        .send()
        .with_context(|| format!("failed to fetch hub catalog {catalog_url}"))?;
    let status = res.status();
    if !status.is_success() {
        bail!("hub catalog {catalog_url} returned HTTP {status}");
    }
    let body = res
        .text()
        .with_context(|| format!("failed to read hub catalog {catalog_url}"))?;
    let mut catalog: HubCatalog = serde_json::from_str(&body)
        .with_context(|| format!("failed to parse hub catalog {catalog_url}"))?;

    let home = catalog_home(&catalog_url);
    match fetch_registry_catalog(&client, &home) {
        Ok(extra) => merge_registry(&mut catalog, extra),
        // A 404 here is the normal answer from a hub that only serves a static
        // marketplace.json. Downgrading it to a warning keeps those working
        // rather than failing the whole sync over an optional document.
        Err(e) => tracing::debug!("[Marketplace] no registry catalog at {home}: {e:#}"),
    }

    Ok(catalog)
}

/// GET `<home>/api/v1/packages`. Errors are the caller's to downgrade.
fn fetch_registry_catalog(
    client: &reqwest::blocking::Client,
    home: &str,
) -> Result<Vec<HubPlugin>> {
    let url = format!("{home}{REGISTRY_CATALOG_PATH}?limit=200");
    let res = client
        .get(&url)
        .send()
        .with_context(|| format!("failed to fetch {url}"))?;
    let status = res.status();
    if !status.is_success() {
        bail!("{url} returned HTTP {status}");
    }
    let body = res.text().with_context(|| format!("failed to read {url}"))?;
    let parsed: RegistryCatalog =
        serde_json::from_str(&body).with_context(|| format!("failed to parse {url}"))?;
    Ok(parsed.packages.into_iter().map(HubPlugin::from).collect())
}

/// Append registry entries the interop index did not already cover.
///
/// `marketplace.json` wins on a collision: its entry carries a git source, so it
/// is installable by both paths, while the registry row is installable only by
/// slug. Dropping the richer of the two would be a regression.
fn merge_registry(catalog: &mut HubCatalog, extra: Vec<HubPlugin>) {
    for pkg in extra {
        let seen = catalog
            .plugins
            .iter()
            .any(|p| p.name == pkg.name || (p.slug.is_some() && p.slug == pkg.slug));
        if !seen {
            catalog.plugins.push(pkg);
        }
    }
    // Most-installed first, then by name so the order is stable across syncs
    // when downloads tie (or when no hub reports them at all).
    catalog.plugins.sort_by(|a, b| {
        b.downloads
            .unwrap_or(0)
            .cmp(&a.downloads.unwrap_or(0))
            .then_with(|| a.name.cmp(&b.name))
    });
}

// ── On-disk state ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledPlugin {
    pub name: String,
    /// Absolute path of the plugin directory (may be nested inside the clone).
    pub dir: String,
    #[serde(rename = "repoUrl")]
    pub repo_url: String,
    pub branch: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(rename = "installedAt")]
    pub installed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InstalledFile {
    #[serde(default)]
    pub plugins: HashMap<String, InstalledPlugin>,
}

pub fn catalog_cache_path(local_path: &Path) -> PathBuf {
    local_path.join(CATALOG_CACHE)
}

pub fn installed_path(local_path: &Path) -> PathBuf {
    local_path.join(INSTALLED_MANIFEST)
}

pub fn repo_path(local_path: &Path, plugin: &str) -> PathBuf {
    local_path.join(REPOS_DIR).join(sanitize_name(plugin))
}

/// Keep a catalog-supplied name from escaping the source directory.
pub fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches(['-', '.'])
        .to_string()
}

pub fn read_catalog_cache(local_path: &Path) -> Option<HubCatalog> {
    let raw = std::fs::read_to_string(catalog_cache_path(local_path)).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn write_catalog_cache(local_path: &Path, catalog: &HubCatalog) -> Result<()> {
    std::fs::create_dir_all(local_path)
        .with_context(|| format!("failed to create {local_path:?}"))?;
    std::fs::write(
        catalog_cache_path(local_path),
        serde_json::to_string_pretty(catalog)? + "\n",
    )
    .context("failed to cache hub catalog")
}

pub fn read_installed(local_path: &Path) -> InstalledFile {
    std::fs::read_to_string(installed_path(local_path))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn write_installed(local_path: &Path, file: &InstalledFile) -> Result<()> {
    std::fs::create_dir_all(local_path)
        .with_context(|| format!("failed to create {local_path:?}"))?;
    std::fs::write(
        installed_path(local_path),
        serde_json::to_string_pretty(file)? + "\n",
    )
    .context("failed to write hub install manifest")
}

// ── Plugin directory resolution ──────────────────────────────────────────────

/// A directory is a plugin when it carries plugin.json, in either the flat
/// layout or the Claude Code `.claude-plugin/` layout.
pub fn is_plugin_dir(dir: &Path) -> bool {
    plugin_json_path(dir).is_some()
}

/// Path of a directory's plugin.json, checking both accepted locations.
pub fn plugin_json_path(dir: &Path) -> Option<PathBuf> {
    let flat = dir.join("plugin.json");
    if flat.is_file() {
        return Some(flat);
    }
    let nested = dir.join(".claude-plugin").join("plugin.json");
    if nested.is_file() {
        return Some(nested);
    }
    None
}

/// Find the plugin directory inside a freshly cloned repo. Catalog entries
/// range from "the repo *is* the plugin" to "the plugin is one of many
/// directories in a monorepo", so try the cheap guesses before walking.
pub fn resolve_plugin_dir(
    repo_root: &Path,
    plugin_name: &str,
    subdir: Option<&str>,
) -> Option<PathBuf> {
    if let Some(sub) = subdir {
        let explicit = repo_root.join(sub);
        // An explicit path wins even without plugin.json — the catalog said so.
        if explicit.is_dir() {
            return Some(explicit);
        }
    }

    let guesses = [
        repo_root.to_path_buf(),
        repo_root.join(plugin_name),
        repo_root.join("plugins").join(plugin_name),
    ];
    for dir in guesses {
        if is_plugin_dir(&dir) {
            return Some(dir);
        }
    }

    find_named_plugin_dir(repo_root, plugin_name, 3)
}

fn find_named_plugin_dir(root: &Path, plugin_name: &str, depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let entries = std::fs::read_dir(root).ok()?;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" || name == "target" {
            continue;
        }
        if name == plugin_name && is_plugin_dir(&path) {
            return Some(path);
        }
        subdirs.push(path);
    }
    subdirs
        .into_iter()
        .find_map(|dir| find_named_plugin_dir(&dir, plugin_name, depth - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_catalog_urls() {
        assert_eq!(
            normalize_catalog_url("https://hub-store.bacnd.com"),
            "https://hub-store.bacnd.com/marketplace.json"
        );
        assert_eq!(
            normalize_catalog_url("https://hub-store.bacnd.com/"),
            "https://hub-store.bacnd.com/marketplace.json"
        );
        assert_eq!(
            normalize_catalog_url("https://hub-store.bacnd.com/marketplace.json"),
            "https://hub-store.bacnd.com/marketplace.json"
        );
        assert_eq!(
            catalog_home("https://hub-store.bacnd.com/marketplace.json"),
            "https://hub-store.bacnd.com"
        );
    }

    #[test]
    fn parses_the_shipped_catalog_shape() {
        let raw = r#"{
            "name": "senclaw",
            "owner": { "name": "senclaw hub", "url": "https://hub-store.bacnd.com" },
            "description": "Skills, plugins, workflows and apps.",
            "plugins": [
                {
                    "name": "qodo-skills",
                    "source": { "source": "github", "repo": "qodo-ai/qodo-skills" },
                    "description": "Shift-left code review skills.",
                    "version": "0.6.1",
                    "author": { "name": "Qodo.ai" },
                    "keywords": ["code-review"],
                    "category": "development"
                }
            ]
        }"#;
        let catalog: HubCatalog = serde_json::from_str(raw).unwrap();
        assert_eq!(catalog.plugins.len(), 1);
        let target = catalog.find("qodo-skills").unwrap().git_target().unwrap();
        assert_eq!(
            target,
            GitTarget {
                url: "https://github.com/qodo-ai/qodo-skills".into(),
                branch: "main".into(),
                subdir: None,
            }
        );
    }

    #[test]
    fn accepts_shorthand_and_git_sources() {
        let shorthand: HubPluginSource = serde_json::from_str(r#""owner/repo""#).unwrap();
        assert_eq!(
            shorthand.git_target().unwrap().url,
            "https://github.com/owner/repo"
        );

        let direct: HubPluginSource =
            serde_json::from_str(r#""https://git.example.com/a/b.git""#).unwrap();
        assert_eq!(
            direct.git_target().unwrap().url,
            "https://git.example.com/a/b.git"
        );

        let spec: HubPluginSource = serde_json::from_str(
            r#"{"source":"git","url":"https://x/y.git","path":"pkgs/p","branch":"dev"}"#,
        )
        .unwrap();
        let target = spec.git_target().unwrap();
        assert_eq!(target.branch, "dev");
        assert_eq!(target.subdir.as_deref(), Some("pkgs/p"));
    }

    #[test]
    fn rejects_repo_relative_sources() {
        let rel: HubPluginSource = serde_json::from_str(r#""./plugins/local""#).unwrap();
        assert!(rel.git_target().is_err());
    }

    /// The exact document the shipped hub serves: 8 apps published, and an
    /// index that filters to `kind = "plugin"` — so it lists nothing. This is
    /// what made the store browse as empty.
    #[test]
    fn merges_registry_packages_into_an_empty_plugin_index() {
        let mut catalog: HubCatalog = serde_json::from_str(
            r#"{"name":"senclaw","description":"...","plugins":[]}"#,
        )
        .unwrap();
        let registry: RegistryCatalog = serde_json::from_str(
            r#"{"packages":[
                {"slug":"senclaw/email","kind":"app","description":"Mail",
                 "latestVersion":"1.0.0","owner":"senclaw","downloads30d":12},
                {"slug":"senclaw/mindmap","kind":"app","latestVersion":"0.2.0",
                 "owner":"senclaw","downloads30d":40}
            ],"count":2,"limit":200}"#,
        )
        .unwrap();

        merge_registry(
            &mut catalog,
            registry.packages.into_iter().map(HubPlugin::from).collect(),
        );

        // Most-downloaded first, and displayed by bare name rather than slug.
        let names: Vec<&str> = catalog.plugins.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["mindmap", "email"]);

        let email = catalog.find("email").unwrap();
        assert_eq!(email.kind.as_deref(), Some("app"));
        assert_eq!(email.slug.as_deref(), Some("senclaw/email"));
        assert_eq!(email.version.as_deref(), Some("1.0.0"));
        // Findable by slug too — that is the coordinate an install carries.
        assert_eq!(catalog.find("senclaw/email").unwrap().name, "email");
    }

    /// A registry row cannot be git-cloned, and the error has to say where to
    /// go instead — otherwise the UI shows a bare "missing field `source`".
    #[test]
    fn registry_entries_refuse_the_git_install_path() {
        let mut catalog = HubCatalog::default();
        merge_registry(
            &mut catalog,
            vec![HubPlugin::from(
                serde_json::from_str::<RegistryPackage>(
                    r#"{"slug":"senclaw/email","kind":"app"}"#,
                )
                .unwrap(),
            )],
        );

        let err = catalog.find("email").unwrap().git_target().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("hub/install"), "{msg}");
        assert!(msg.contains("senclaw/email"), "{msg}");
        assert!(!catalog.find("email").unwrap().is_git_plugin());
    }

    /// marketplace.json wins a name collision: its entry carries a git source,
    /// so it installs by either route while the registry row installs by one.
    #[test]
    fn a_marketplace_entry_is_not_replaced_by_its_registry_row() {
        let mut catalog: HubCatalog = serde_json::from_str(
            r#"{"plugins":[{"name":"qodo-skills","source":"qodo-ai/qodo-skills","version":"0.6.1"}]}"#,
        )
        .unwrap();
        merge_registry(
            &mut catalog,
            vec![HubPlugin::from(
                serde_json::from_str::<RegistryPackage>(
                    r#"{"slug":"qodo/qodo-skills","kind":"plugin","latestVersion":"9.9.9"}"#,
                )
                .unwrap(),
            )],
        );

        assert_eq!(catalog.plugins.len(), 1);
        let kept = catalog.find("qodo-skills").unwrap();
        assert_eq!(kept.version.as_deref(), Some("0.6.1"));
        assert!(kept.is_git_plugin());
        assert_eq!(
            kept.git_target().unwrap().url,
            "https://github.com/qodo-ai/qodo-skills"
        );
    }

    /// A cached catalog written before this change has no `kind`/`slug`, and a
    /// plain static hub still serves entries in that shape.
    #[test]
    fn plugin_entries_stay_readable_without_the_new_fields() {
        let entry: HubPlugin =
            serde_json::from_str(r#"{"name":"p","source":"owner/repo"}"#).unwrap();
        assert!(entry.is_git_plugin());
        assert_eq!(entry.kind, None);
        assert_eq!(entry.slug, None);
        assert_eq!(entry.git_target().unwrap().branch, "main");

        // …and the new fields stay out of the serialized form, so a cache round
        // trip does not rewrite every entry.
        let round = serde_json::to_string(&entry).unwrap();
        assert!(!round.contains("kind"), "{round}");
        assert!(!round.contains("slug"), "{round}");
    }

    #[test]
    fn sanitizes_plugin_names_into_paths() {
        assert_eq!(sanitize_name("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_name("code-modernization"), "code-modernization");
    }

    #[test]
    fn resolves_plugin_dir_layouts() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();

        // Flat: repo root is the plugin.
        std::fs::write(root.join("plugin.json"), "{}").unwrap();
        assert_eq!(resolve_plugin_dir(root, "anything", None).unwrap(), root);

        // Monorepo: .claude-plugin/plugin.json in a nested directory.
        let nested = root.join("packages").join("code-modernization");
        std::fs::create_dir_all(nested.join(".claude-plugin")).unwrap();
        std::fs::write(nested.join(".claude-plugin").join("plugin.json"), "{}").unwrap();
        std::fs::remove_file(root.join("plugin.json")).unwrap();
        assert_eq!(
            resolve_plugin_dir(root, "code-modernization", None).unwrap(),
            nested
        );
    }
}
