//! Marketplace manager. Mirrors `src-old/marketplace/MarketplaceManager.ts`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use uuid::Uuid;

use super::git_sync::clone_or_pull;
use super::hub;
use super::types::{
    MarketplaceConfig, MarketplacePlugin, MarketplacePluginMCPServer, MarketplacePluginSkill,
    MarketplacePluginSubagent, MarketplaceSource, MarketplaceSourceInfo,
    MarketplaceSourceItemState, MarketplaceStateFile, SourceType,
};
use crate::security::{ScanPolicy, ScanReport};

/// Marker written next to the config the first time the default hub is seeded,
/// so removing the hub source keeps it removed.
const HUB_SEED_MARKER: &str = ".hub-seeded";

/// Result of [`MarketplaceManager::install_hub_plugin`].
///
/// A blocked install is a value, not an error string: every caller — CLI, chat
/// and the Web UI — needs the findings themselves to render a decision, and
/// flattening them into a message would leave the UI parsing prose.
pub enum InstallOutcome {
    Installed {
        /// Resolved plugin directory inside the clone.
        dir: PathBuf,
        /// `None` only when scanning was disabled by policy. Present even on a
        /// clean install so callers can show a `Warn` verdict.
        scan: Option<ScanReport>,
    },
    /// Refused by the scan. Nothing was recorded or enabled.
    Blocked {
        report: ScanReport,
        /// The clone is deliberately left in place so the user can inspect it.
        staged_dir: PathBuf,
    },
}

/// Marketplace manager for hub/git/local plugin sources
pub struct MarketplaceManager {
    config: MarketplaceConfig,
    state: MarketplaceStateFile,
    config_path: PathBuf,
    state_path: PathBuf,
    clones_dir: PathBuf,
    hub_url: String,
}

/// Plugin definition for discovery
#[derive(Debug, Clone)]
struct PluginDef {
    dir: String,
    plugin_json_path: Option<String>,
    /// Hub-installed plugins carry their catalog name, which outranks whatever
    /// the checked-out plugin.json calls itself.
    name_hint: Option<String>,
}

/// Plugin JSON metadata
#[derive(Debug, Clone, Default, serde::Deserialize)]
struct PluginJson {
    name: Option<String>,
    description: Option<String>,
    version: Option<String>,
    author: Option<serde_json::Value>,
    keywords: Option<Vec<String>>,
}

impl MarketplaceManager {
    /// Create a new marketplace manager with default paths and the built-in hub
    pub fn new() -> Result<Self> {
        Self::new_with_hub(hub::DEFAULT_HUB_URL)
    }

    /// Create a marketplace manager with default paths, seeding `hub_url` as the
    /// default store on first run.
    pub fn new_with_hub(hub_url: &str) -> Result<Self> {
        let home =
            dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Failed to get home directory"))?;
        let senclaw_home = home.join(".senclaw");

        let config_path = senclaw_home.join("marketplace.json");
        let state_path = senclaw_home.join("marketplace-state.json");
        let clones_dir = senclaw_home.join("marketplace");

        let mut manager = Self::with_paths_and_hub(config_path, state_path, clones_dir, hub_url)?;
        if let Err(e) = manager.migrate_legacy_hub_url() {
            tracing::warn!("[Marketplace] Failed to migrate legacy hub source: {e}");
        }
        if let Err(e) = manager.ensure_default_hub() {
            tracing::warn!("[Marketplace] Failed to seed default hub source: {e}");
        }
        Ok(manager)
    }

    /// Production constructor: configured paths and hub, falling back through
    /// the home directory to a temp directory so a broken environment degrades
    /// to an empty marketplace instead of stopping the daemon.
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        let hub = cfg.marketplace_hub_url.as_str();
        let mut manager = Self::with_paths_and_hub(
            cfg.paths.marketplace_config_path.clone(),
            cfg.paths.marketplace_state_path.clone(),
            cfg.paths.marketplace_clones_dir.clone(),
            hub,
        )
        .unwrap_or_else(|e| {
            tracing::warn!("[Marketplace] Falling back to a temporary marketplace: {e}");
            let tmp = std::env::temp_dir();
            Self::with_paths_and_hub(
                tmp.join("senclaw-marketplace-config.json"),
                tmp.join("senclaw-marketplace-state.json"),
                tmp.join("senclaw-marketplace"),
                hub,
            )
            .unwrap_or_else(|e2| panic!("Failed to create marketplace manager: {e2}"))
        });

        if let Err(e) = manager.migrate_legacy_hub_url() {
            tracing::warn!("[Marketplace] Failed to migrate legacy hub source: {e}");
        }
        if let Err(e) = manager.ensure_default_hub() {
            tracing::warn!("[Marketplace] Failed to seed default hub source: {e}");
        }
        manager
    }

    /// Create a marketplace manager with custom paths (for testing)
    pub fn with_paths(
        config_path: PathBuf,
        state_path: PathBuf,
        clones_dir: PathBuf,
    ) -> Result<Self> {
        Self::with_paths_and_hub(config_path, state_path, clones_dir, hub::DEFAULT_HUB_URL)
    }

    /// Create a marketplace manager with custom paths and hub URL. Does not seed
    /// the default hub — call [`Self::ensure_default_hub`] for that.
    pub fn with_paths_and_hub(
        config_path: PathBuf,
        state_path: PathBuf,
        clones_dir: PathBuf,
        hub_url: &str,
    ) -> Result<Self> {
        let mut manager = Self {
            config: MarketplaceConfig::default(),
            state: MarketplaceStateFile::default(),
            config_path,
            state_path,
            clones_dir,
            hub_url: hub_url.trim().to_string(),
        };

        manager.load_config()?;
        manager.load_state()?;

        Ok(manager)
    }

    /// The configured hub catalog URL.
    pub fn hub_url(&self) -> &str {
        &self.hub_url
    }

    /// Add the configured hub as a source the first time this install runs.
    /// Returns whether a source was created. Seeding happens once: the marker
    /// file next to the config means a user who removes the hub keeps it gone.
    pub fn ensure_default_hub(&mut self) -> Result<bool> {
        if self.hub_url.is_empty() {
            return Ok(false);
        }
        let marker = self
            .config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(HUB_SEED_MARKER);
        if marker.exists() {
            return Ok(false);
        }

        let catalog_url = hub::normalize_catalog_url(&self.hub_url);
        let already = self.config.sources.iter().any(|s| {
            s.source_type == SourceType::Hub
                && s.url
                    .as_deref()
                    .map(|u| hub::normalize_catalog_url(u) == catalog_url)
                    .unwrap_or(false)
        });

        let created = if already {
            false
        } else {
            let name = Self::hub_source_name(&catalog_url);
            self.add_source(
                if name.is_empty() {
                    "SenClaw Hub".to_string()
                } else {
                    name
                },
                SourceType::Hub,
                Some(catalog_url),
                None,
                None,
                Some(0),
                Some(true),
            )?;
            true
        };

        if let Some(parent) = marker.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::write(&marker, "").ok();
        Ok(created)
    }

    /// The auto-derived display name for a hub source: its home host.
    fn hub_source_name(catalog_url: &str) -> String {
        hub::catalog_home(catalog_url)
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .to_string()
    }

    /// One-time rename of the shipped hub: sources seeded while the default
    /// was `hub-store.bacnd.com` are rewritten to `senclaw.bacnd.com` — the
    /// same server under its official name. User-added hubs are left alone;
    /// so is everything when a source for the new URL already exists (the
    /// user got there on their own). Returns whether anything changed.
    pub fn migrate_legacy_hub_url(&mut self) -> Result<bool> {
        let old_catalog = hub::normalize_catalog_url(hub::LEGACY_HUB_URL);
        let new_catalog = hub::normalize_catalog_url(hub::DEFAULT_HUB_URL);
        let points_at = |s: &MarketplaceSource, catalog: &str| {
            s.source_type == SourceType::Hub
                && s.url
                    .as_deref()
                    .map(|u| hub::normalize_catalog_url(u) == catalog)
                    .unwrap_or(false)
        };

        if self.config.sources.iter().any(|s| points_at(s, &new_catalog)) {
            return Ok(false);
        }

        let old_name = Self::hub_source_name(&old_catalog);
        let new_name = Self::hub_source_name(&new_catalog);
        let mut changed = false;
        for s in &mut self.config.sources {
            if points_at(s, &old_catalog) {
                s.url = Some(new_catalog.clone());
                // Only rename if the user kept the auto-derived name.
                if s.name == old_name {
                    s.name = new_name.clone();
                }
                changed = true;
            }
        }
        if changed {
            self.save_config()?;
        }
        Ok(changed)
    }

    // ── Config/State persistence ─────────────────────────────────────────────────────

    fn load_config(&mut self) -> Result<()> {
        if self.config_path.exists() {
            let raw = fs::read_to_string(&self.config_path)
                .with_context(|| format!("Failed to read config from {:?}", self.config_path))?;
            self.config =
                serde_json::from_str(&raw).with_context(|| "Failed to parse marketplace config")?;
        }
        Ok(())
    }

    fn save_config(&self) -> Result<()> {
        if let Some(parent) = self.config_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create config directory {:?}", parent))?;
        }
        let json = serde_json::to_string_pretty(&self.config)?;
        fs::write(&self.config_path, json + "\n")
            .with_context(|| format!("Failed to write config to {:?}", self.config_path))?;
        Ok(())
    }

    fn load_state(&mut self) -> Result<()> {
        if self.state_path.exists() {
            let raw = fs::read_to_string(&self.state_path)
                .with_context(|| format!("Failed to read state from {:?}", self.state_path))?;
            self.state =
                serde_json::from_str(&raw).with_context(|| "Failed to parse marketplace state")?;
        }
        Ok(())
    }

    fn save_state(&self) -> Result<()> {
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create state directory {:?}", parent))?;
        }
        let json = serde_json::to_string_pretty(&self.state)?;
        fs::write(&self.state_path, json + "\n")
            .with_context(|| format!("Failed to write state to {:?}", self.state_path))?;
        Ok(())
    }

    // ── Source CRUD ─────────────────────────────────────────────────────────────────

    /// Get all sources sorted by priority (ascending)
    pub fn get_sources(&self) -> Vec<MarketplaceSource> {
        let mut sources = self.config.sources.clone();
        sources.sort_by_key(|s| s.priority);
        sources
    }

    /// Get a source by ID
    pub fn get_source(&self, id: &str) -> Option<MarketplaceSource> {
        self.config.sources.iter().find(|s| s.id == id).cloned()
    }

    /// Add a new source
    pub fn add_source(
        &mut self,
        name: String,
        source_type: SourceType,
        url: Option<String>,
        branch: Option<String>,
        local_path: Option<String>,
        priority: Option<i32>,
        enabled: Option<bool>,
    ) -> Result<MarketplaceSource> {
        let id = Uuid::new_v4().to_string();
        let max_priority = self
            .config
            .sources
            .iter()
            .map(|s| s.priority)
            .max()
            .unwrap_or(0);

        let local_path = match source_type {
            SourceType::Git | SourceType::Hub => {
                self.clones_dir.join(&id).to_string_lossy().to_string()
            }
            SourceType::Local => {
                let path = local_path.unwrap_or_else(|| ".".to_string());
                PathBuf::from(&path)
                    .canonicalize()
                    .unwrap_or_else(|_| PathBuf::from(&path))
                    .to_string_lossy()
                    .to_string()
            }
        };

        let url = match source_type {
            // Store hubs as the catalog document URL, so every later read is a
            // straight GET with no guessing.
            SourceType::Hub => url.as_deref().map(hub::normalize_catalog_url),
            _ => url,
        };

        let source = MarketplaceSource {
            id: id.clone(),
            name,
            source_type,
            url,
            branch: branch.or(Some("main".to_string())),
            local_path,
            priority: priority.unwrap_or(max_priority + 1),
            enabled: enabled.unwrap_or(true),
            last_synced: None,
            sync_error: None,
        };

        self.config.sources.push(source.clone());
        self.save_config()?;
        Ok(source)
    }

    /// Update an existing source
    pub fn update_source(
        &mut self,
        id: &str,
        name: Option<String>,
        url: Option<Option<String>>,
        branch: Option<Option<String>>,
        local_path: Option<String>,
        priority: Option<i32>,
        enabled: Option<bool>,
        last_synced: Option<Option<String>>,
        sync_error: Option<Option<String>>,
    ) -> Result<Option<MarketplaceSource>> {
        let idx = self
            .config
            .sources
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| anyhow::anyhow!("Source not found: {}", id))?;

        let source = &mut self.config.sources[idx];
        if let Some(name) = name {
            source.name = name;
        }
        if let Some(url) = url {
            source.url = url;
        }
        if let Some(branch) = branch {
            source.branch = branch;
        }
        if let Some(local_path) = local_path {
            source.local_path = local_path;
        }
        if let Some(priority) = priority {
            source.priority = priority;
        }
        if let Some(enabled) = enabled {
            source.enabled = enabled;
        }
        if let Some(last_synced) = last_synced {
            source.last_synced = last_synced;
        }
        if let Some(sync_error) = sync_error {
            source.sync_error = sync_error;
        }

        let updated = source.clone();
        self.save_config()?;
        Ok(Some(updated))
    }

    /// Remove a source
    pub fn remove_source(&mut self, id: &str) -> Result<bool> {
        let idx = self
            .config
            .sources
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| anyhow::anyhow!("Source not found: {}", id))?;

        let source = self.config.sources.remove(idx);
        self.state.sources.remove(id);
        self.save_config()?;
        self.save_state()?;

        // Clean up the managed directory (git clone, or a hub's cache + clones)
        if matches!(source.source_type, SourceType::Git | SourceType::Hub) {
            let clone_dir = self.clones_dir.join(&source.id);
            if clone_dir.exists() {
                let _ = fs::remove_dir_all(&clone_dir);
            }
        }

        Ok(true)
    }

    /// Sync a source: pull a git clone, or refresh a hub catalog (and every
    /// plugin already installed from it).
    pub fn sync_source(&mut self, id: &str) -> Result<()> {
        let source = self
            .get_source(id)
            .ok_or_else(|| anyhow::anyhow!("Source not found: {}", id))?;

        if source.source_type == SourceType::Local {
            return Ok(());
        }

        let result = match source.source_type {
            SourceType::Git => self.sync_git_source(&source),
            SourceType::Hub => self.sync_hub_source(&source),
            SourceType::Local => Ok(()),
        };

        // Record the outcome either way — a stale "last synced" with no error is
        // worse than no sync at all.
        let now = chrono::Utc::now().to_rfc3339();
        let sync_error = result.as_ref().err().map(|e| e.to_string());
        self.update_source(
            id,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(Some(now)),
            Some(sync_error),
        )?;

        result
    }

    fn sync_git_source(&self, source: &MarketplaceSource) -> Result<()> {
        let url = source
            .url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Git source missing URL"))?;
        let branch = source.branch.as_deref().unwrap_or("main");
        clone_or_pull(url, branch, Path::new(&source.local_path))
    }

    fn sync_hub_source(&self, source: &MarketplaceSource) -> Result<()> {
        let url = source
            .url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Hub source missing catalog URL"))?;
        let local_path = Path::new(&source.local_path);

        let catalog = hub::fetch_catalog(url)?;
        hub::write_catalog_cache(local_path, &catalog)?;

        // Refresh the clones behind installed plugins; one bad repo must not
        // abort the whole sync.
        for installed in hub::read_installed(local_path).plugins.values() {
            let repo = hub::repo_path(local_path, &installed.name);
            if let Err(e) = clone_or_pull(&installed.repo_url, &installed.branch, &repo) {
                tracing::warn!(
                    "[Marketplace] Failed to update hub plugin {}: {e}",
                    installed.name
                );
            }
        }

        Ok(())
    }

    /// Reorder sources by priority
    pub fn reorder_sources(&mut self, ordered_ids: Vec<String>) -> Result<()> {
        for (i, id) in ordered_ids.iter().enumerate() {
            if let Some(idx) = self.config.sources.iter().position(|s| &s.id == id) {
                self.config.sources[idx].priority = (i + 1) as i32;
            }
        }
        self.save_config()?;
        Ok(())
    }

    // ── Plugin state management ───────────────────────────────────────────────────────

    /// Get source state (migration-safe)
    fn get_source_state(&self, source_id: &str) -> MarketplaceSourceItemState {
        self.state
            .sources
            .get(source_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Ensure source state exists
    fn ensure_source_state(&mut self, source_id: &str) -> &mut MarketplaceSourceItemState {
        if !self.state.sources.contains_key(source_id) {
            self.state
                .sources
                .insert(source_id.to_string(), MarketplaceSourceItemState::default());
        }
        self.state.sources.get_mut(source_id).unwrap()
    }

    /// Whether a plugin is currently enabled (absent = off)
    pub fn is_plugin_enabled(&self, source_id: &str, plugin_name: &str) -> bool {
        self.state
            .sources
            .get(source_id)
            .and_then(|st| st.plugins.get(plugin_name))
            .copied()
            .unwrap_or(false)
    }

    /// Set plugin enabled/disabled state
    pub fn set_plugin_enabled(
        &mut self,
        source_id: &str,
        plugin_name: &str,
        enabled: bool,
    ) -> Result<()> {
        let st = self.ensure_source_state(source_id);
        if enabled {
            st.plugins.insert(plugin_name.to_string(), true);
        } else {
            st.plugins.remove(plugin_name);
        }
        self.save_state()?;
        Ok(())
    }

    /// Enable all plugins in a source
    pub fn enable_all_in_source(&mut self, source_id: &str) -> Result<()> {
        let source = self
            .get_source(source_id)
            .ok_or_else(|| anyhow::anyhow!("Source not found: {}", source_id))?;

        // Collect plugin names first
        let mut plugin_names = Vec::new();
        for def in self.find_plugins(&source)? {
            let meta = self.read_plugin_json(&def);
            plugin_names.push(self.plugin_name(&meta, &def));
        }

        // Then enable them
        let st = self.ensure_source_state(source_id);
        for name in plugin_names {
            st.plugins.insert(name, true);
        }
        self.save_state()?;
        Ok(())
    }

    /// Disable all plugins in a source
    pub fn disable_all_in_source(&mut self, source_id: &str) -> Result<()> {
        let st = self.ensure_source_state(source_id);
        st.plugins.clear();
        self.save_state()?;
        Ok(())
    }

    /// Enable all plugins across all sources
    pub fn enable_all(&mut self) -> Result<()> {
        let source_ids: Vec<String> = self.config.sources.iter().map(|s| s.id.clone()).collect();
        for id in source_ids {
            let _ = self.enable_all_in_source(&id);
        }
        Ok(())
    }

    /// Disable all plugins across all sources
    pub fn disable_all(&mut self) -> Result<()> {
        let source_ids: Vec<String> = self.config.sources.iter().map(|s| s.id.clone()).collect();
        for id in source_ids {
            let _ = self.disable_all_in_source(&id);
        }
        Ok(())
    }

    // ── Plugin discovery ──────────────────────────────────────────────────────────────

    /// Plugins a source currently has on disk. Git/local sources are scanned;
    /// a hub source only has what the user installed from its catalog.
    fn find_plugins(&self, source: &MarketplaceSource) -> Result<Vec<PluginDef>> {
        if source.source_type == SourceType::Hub {
            return Ok(self.installed_hub_plugins(source));
        }
        self.scan_plugin_dirs(&source.local_path)
    }

    fn installed_hub_plugins(&self, source: &MarketplaceSource) -> Vec<PluginDef> {
        let local_path = Path::new(&source.local_path);
        let mut defs: Vec<PluginDef> = hub::read_installed(local_path)
            .plugins
            .into_values()
            .filter(|p| Path::new(&p.dir).is_dir())
            .map(|p| PluginDef {
                plugin_json_path: hub::plugin_json_path(Path::new(&p.dir))
                    .map(|p| p.to_string_lossy().to_string()),
                dir: p.dir,
                name_hint: Some(p.name),
            })
            .collect();
        defs.sort_by(|a, b| a.name_hint.cmp(&b.name_hint));
        defs
    }

    /// Find all plugin directories directly under a directory
    fn scan_plugin_dirs(&self, base_path: &str) -> Result<Vec<PluginDef>> {
        let base = Path::new(base_path);
        if !base.exists() {
            return Ok(Vec::new());
        }

        let mut plugins = Vec::new();
        let entries =
            fs::read_dir(base).with_context(|| format!("Failed to read directory {:?}", base))?;

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if dir_name.starts_with('.') {
                continue;
            }

            if let Some(plugin_json) = hub::plugin_json_path(&path) {
                plugins.push(PluginDef {
                    dir: path.to_string_lossy().to_string(),
                    plugin_json_path: Some(plugin_json.to_string_lossy().to_string()),
                    name_hint: None,
                });
            }
        }

        Ok(plugins)
    }

    /// Read plugin.json metadata. A missing or malformed manifest degrades to
    /// empty metadata — one broken plugin must not hide the rest of the source.
    fn read_plugin_json(&self, def: &PluginDef) -> PluginJson {
        let Some(path) = def.plugin_json_path.as_deref() else {
            return PluginJson::default();
        };
        match fs::read_to_string(path).map(|raw| serde_json::from_str::<PluginJson>(&raw)) {
            Ok(Ok(meta)) => meta,
            Ok(Err(e)) => {
                tracing::warn!("[Marketplace] Invalid plugin.json at {path}: {e}");
                PluginJson::default()
            }
            Err(e) => {
                tracing::warn!("[Marketplace] Unreadable plugin.json at {path}: {e}");
                PluginJson::default()
            }
        }
    }

    /// Get plugin name from metadata and directory
    fn plugin_name(&self, meta: &PluginJson, def: &PluginDef) -> String {
        // The catalog name is authoritative for hub plugins: it is the key the
        // manifest, the enable-state and the install/uninstall API all use.
        if let Some(hint) = &def.name_hint {
            return hint.clone();
        }
        meta.name.clone().unwrap_or_else(|| {
            Path::new(&def.dir)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        })
    }

    /// Parse author field from JSON (can be string or object with name field)
    fn parse_author(&self, author: &Option<serde_json::Value>) -> Option<String> {
        match author {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(serde_json::Value::Object(obj)) => obj
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            _ => None,
        }
    }

    /// Get source info with plugins
    pub fn get_source_info(&self, source_id: &str) -> Result<Option<MarketplaceSourceInfo>> {
        let source = self.get_source(source_id);
        let source = match source {
            Some(s) => s,
            None => return Ok(None),
        };

        let st = self.get_source_state(source_id);
        let catalog = if source.source_type == SourceType::Hub {
            self.load_catalog(&source).ok()
        } else {
            None
        };

        let mut plugin_list = Vec::new();
        for def in self.find_plugins(&source)? {
            let meta = self.read_plugin_json(&def);
            let name = self.plugin_name(&meta, &def);
            let entry = catalog.as_ref().and_then(|c| c.find(&name));
            let enabled = st.plugins.get(&name).copied().unwrap_or(false);

            // Discover skills, subagents, MCP servers (simplified for now)
            let skills = self.discover_skills(&def.dir)?;
            let subagents = self.discover_subagents(&def.dir)?;
            let mcp_servers = self.discover_mcp_servers(&def.dir)?;
            let has_hooks = Path::new(&def.dir).join("hooks").exists();

            plugin_list.push(MarketplacePlugin {
                name: name.clone(),
                description: meta
                    .description
                    .or_else(|| entry.and_then(|e| e.description.clone()))
                    .unwrap_or_default(),
                version: meta
                    .version
                    .or_else(|| entry.and_then(|e| e.version.clone())),
                author: self
                    .parse_author(&meta.author)
                    .or_else(|| entry.and_then(|e| self.parse_author(&e.author))),
                keywords: meta
                    .keywords
                    .or_else(|| entry.and_then(|e| e.keywords.clone())),
                dir: def.dir,
                source_id: source.id.clone(),
                source_name: source.name.clone(),
                priority: source.priority,
                enabled,
                installed: true,
                kind: entry.and_then(|e| e.kind.clone()),
                slug: entry.and_then(|e| e.slug.clone()),
                downloads: entry.and_then(|e| e.downloads),
                installed_version: None,
                update_available: false,
                category: entry.and_then(|e| e.category.clone()),
                license: entry.and_then(|e| e.license.clone()),
                repository: entry.and_then(|e| e.repository.clone()),
                skill_count: skills.len(),
                subagent_count: subagents.len(),
                has_hooks,
                mcp_server_count: mcp_servers.len(),
                skills,
                subagents,
                mcp_servers,
            });
        }

        // Everything else the hub offers, listed as available to install.
        if let Some(catalog) = &catalog {
            for entry in &catalog.plugins {
                if plugin_list.iter().any(|p| p.name == entry.name) {
                    continue;
                }
                plugin_list.push(MarketplacePlugin {
                    name: entry.name.clone(),
                    description: entry.description.clone().unwrap_or_default(),
                    version: entry.version.clone(),
                    author: self.parse_author(&entry.author),
                    keywords: entry.keywords.clone(),
                    dir: String::new(),
                    source_id: source.id.clone(),
                    source_name: source.name.clone(),
                    priority: source.priority,
                    enabled: false,
                    installed: false,
                    kind: entry.kind.clone(),
                    slug: entry.slug.clone(),
                    downloads: entry.downloads,
                    // Stamped by the UI layer, which is the only place that can
                    // see the Space Apps table. A plugin source cannot.
                    installed_version: None,
                    update_available: false,
                    category: entry.category.clone(),
                    license: entry.license.clone(),
                    repository: entry
                        .repository
                        .clone()
                        .or_else(|| entry.git_target().ok().map(|t| t.url)),
                    skill_count: 0,
                    subagent_count: 0,
                    has_hooks: false,
                    mcp_server_count: 0,
                    skills: Vec::new(),
                    subagents: Vec::new(),
                    mcp_servers: Vec::new(),
                });
            }
        }

        Ok(Some(MarketplaceSourceInfo {
            source,
            plugins: plugin_list,
        }))
    }

    // ── Kits ──────────────────────────────────────────────────────────────────────────

    /// A source's catalog, whatever kind of source it is.
    ///
    /// [`Self::get_catalog`] is hub-only because a hub *is* its catalog. A git
    /// or local source keeps one in the tree it already cloned, so kits work
    /// there too rather than being a hub-only feature.
    fn catalog_of(&self, source: &MarketplaceSource) -> Option<hub::HubCatalog> {
        if source.source_type == SourceType::Hub {
            return self.load_catalog(source).ok();
        }
        let root = Path::new(&source.local_path);
        for rel in ["marketplace.json", ".claude-plugin/marketplace.json"] {
            let path = root.join(rel);
            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            match serde_json::from_str::<hub::HubCatalog>(&raw) {
                Ok(catalog) => return Some(catalog),
                Err(e) => tracing::warn!("[Marketplace] {} is not a catalog: {e}", path.display()),
            }
        }
        None
    }

    /// Every kit offered across all enabled sources, each tagged with where it
    /// came from so a client can install it back through the same pair.
    pub fn list_kits(&self) -> Vec<(MarketplaceSource, hub::HubKit)> {
        let mut out = Vec::new();
        for source in &self.config.sources {
            if !source.enabled {
                continue;
            }
            let Some(catalog) = self.catalog_of(source) else {
                continue;
            };
            for kit in catalog.kits {
                out.push((source.clone(), kit));
            }
        }
        out
    }

    /// Fetch one kit's artifact: the bytes of a `.json` manifest or a `.zip`
    /// bundle, plus the filename, which is what tells the installer which it is.
    pub fn fetch_kit(&self, source_id: &str, kit_name: &str) -> Result<(String, Vec<u8>)> {
        let source = self
            .get_source(source_id)
            .ok_or_else(|| anyhow::anyhow!("Source not found: {source_id}"))?;
        let catalog = self
            .catalog_of(&source)
            .ok_or_else(|| anyhow::anyhow!("Source {} has no catalog", source.name))?;
        let kit = catalog
            .kits
            .into_iter()
            .find(|k| k.name == kit_name)
            .ok_or_else(|| anyhow::anyhow!("Source {} offers no kit {kit_name}", source.name))?;
        let target = kit
            .url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Kit {kit_name} declares no url"))?;

        if target.starts_with("http://") || target.starts_with("https://") {
            let filename = target.rsplit('/').next().unwrap_or(kit_name).to_string();
            let client = hub::http_client()?;
            let res = client
                .get(target)
                .send()
                .with_context(|| format!("failed to fetch kit {target}"))?;
            if !res.status().is_success() {
                anyhow::bail!("kit {target} returned HTTP {}", res.status());
            }
            let bytes = res
                .bytes()
                .with_context(|| format!("failed to read kit {target}"))?;
            return Ok((filename, bytes.to_vec()));
        }

        // A path inside the source's own directory. Resolved against the source
        // root and checked afterwards: a catalog is third-party input, and
        // `../../..` in a `url` would otherwise read any file on the machine.
        let root = Path::new(&source.local_path);
        let path = root.join(target);
        let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let canonical = path
            .canonicalize()
            .with_context(|| format!("kit file not found: {}", path.display()))?;
        if !canonical.starts_with(&canonical_root) {
            anyhow::bail!("kit {kit_name} points outside its source directory");
        }
        let filename = canonical
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| kit_name.to_string());
        let bytes =
            fs::read(&canonical).with_context(|| format!("cannot read {}", canonical.display()))?;
        Ok((filename, bytes))
    }

    // ── Hub catalog & install ─────────────────────────────────────────────────────────

    /// Catalog for a hub source: cached copy if present, otherwise fetched (and
    /// cached) on the spot so a freshly added hub lists its plugins right away.
    fn load_catalog(&self, source: &MarketplaceSource) -> Result<hub::HubCatalog> {
        let local_path = Path::new(&source.local_path);
        if let Some(cached) = hub::read_catalog_cache(local_path) {
            return Ok(cached);
        }
        let url = source
            .url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Hub source missing catalog URL"))?;
        let catalog = hub::fetch_catalog(url)?;
        if let Err(e) = hub::write_catalog_cache(local_path, &catalog) {
            tracing::warn!("[Marketplace] Failed to cache hub catalog: {e}");
        }
        Ok(catalog)
    }

    /// Fetch (and cache) a hub source's catalog, bypassing the cached copy.
    pub fn refresh_catalog(&self, source_id: &str) -> Result<hub::HubCatalog> {
        let source = self
            .get_source(source_id)
            .ok_or_else(|| anyhow::anyhow!("Source not found: {}", source_id))?;
        if source.source_type != SourceType::Hub {
            anyhow::bail!("Source {} is not a hub", source.name);
        }
        let url = source
            .url
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Hub source missing catalog URL"))?;
        let catalog = hub::fetch_catalog(url)?;
        hub::write_catalog_cache(Path::new(&source.local_path), &catalog)?;
        Ok(catalog)
    }

    /// The catalog of a hub source (cached copy, fetched once if absent).
    pub fn get_catalog(&self, source_id: &str) -> Result<hub::HubCatalog> {
        let source = self
            .get_source(source_id)
            .ok_or_else(|| anyhow::anyhow!("Source not found: {}", source_id))?;
        if source.source_type != SourceType::Hub {
            anyhow::bail!("Source {} is not a hub", source.name);
        }
        self.load_catalog(&source)
    }

    /// Install one plugin from a hub catalog: clone its repo, resolve the plugin
    /// directory inside it, **scan it**, then record it and enable it.
    ///
    /// The scan sits between the clone and the enable because enabling is the
    /// point of no return: an enabled plugin's `mcp/` servers become
    /// launchable and its `hooks/hooks.json` is read by the agent hook loader.
    /// Once the verdict is [`Verdict::Block`] nothing is recorded, so the clone
    /// on disk is inert — it is deliberately left in place for the user to
    /// inspect rather than deleted.
    ///
    /// `force` still runs the scan and still returns the report; it only stops
    /// a blocking verdict from aborting. Skipping the scan entirely is a
    /// separate decision, expressed as `policy.enabled == false`.
    pub fn install_hub_plugin(
        &mut self,
        source_id: &str,
        plugin_name: &str,
        policy: ScanPolicy,
        force: bool,
    ) -> Result<InstallOutcome> {
        let source = self
            .get_source(source_id)
            .ok_or_else(|| anyhow::anyhow!("Source not found: {}", source_id))?;
        if source.source_type != SourceType::Hub {
            anyhow::bail!("Source {} is not a hub", source.name);
        }

        let catalog = self.load_catalog(&source)?;
        let entry = catalog
            .find(plugin_name)
            .ok_or_else(|| anyhow::anyhow!("Plugin not in catalog: {}", plugin_name))?;
        // Errors with the registry route named when the entry is an app/skill:
        // those install as signed artifacts by slug, not as a git clone.
        let target = entry.git_target()?;

        let local_path = PathBuf::from(&source.local_path);
        let repo = hub::repo_path(&local_path, plugin_name);
        clone_or_pull(&target.url, &target.branch, &repo).with_context(|| {
            format!("Failed to clone {} for plugin {}", target.url, plugin_name)
        })?;

        let dir = hub::resolve_plugin_dir(&repo, plugin_name, target.subdir.as_deref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Cloned {} but found no plugin directory for {} — the catalog entry may need a `path`",
                    target.url,
                    plugin_name
                )
            })?;

        // Gate: inspect the cloned tree before it becomes live.
        let scan = if policy.enabled {
            // Score command hooks against the policy that will actually load
            // them. A package whose hooks this daemon refuses is not shipping
            // code that runs here, and blocking on it would refuse the package
            // over automation that never executes.
            let disposition = crate::agent::MarketplaceHookPolicy::from_config(
                &crate::config::Config::from_env(),
            )
            .disposition_for(plugin_name);
            let report =
                crate::security::scan::scan_plugin_dir_with(&dir, plugin_name, disposition);
            if report.verdict(&policy) == crate::security::scan::Verdict::Block && !force {
                tracing::warn!(
                    "[marketplace] blocked install of '{plugin_name}' (risk {}/100):\n{}",
                    report.risk_score(),
                    report.summary()
                );
                return Ok(InstallOutcome::Blocked {
                    report,
                    staged_dir: dir,
                });
            }
            if !report.findings.is_empty() {
                tracing::warn!(
                    "[marketplace] pre-install scan of '{plugin_name}' (risk {}/100, forced={force}):\n{}",
                    report.risk_score(),
                    report.summary()
                );
            }
            Some(report)
        } else {
            None
        };

        let mut installed = hub::read_installed(&local_path);
        installed.plugins.insert(
            plugin_name.to_string(),
            hub::InstalledPlugin {
                name: plugin_name.to_string(),
                dir: dir.to_string_lossy().to_string(),
                repo_url: target.url,
                branch: target.branch,
                version: entry.version.clone(),
                installed_at: chrono::Utc::now().to_rfc3339(),
            },
        );
        hub::write_installed(&local_path, &installed)?;

        // Installing is the act of opting in — leaving it off would just make
        // every install a two-step.
        self.set_plugin_enabled(source_id, plugin_name, true)?;

        Ok(InstallOutcome::Installed { dir, scan })
    }

    /// Remove a hub-installed plugin: drop its clone, manifest entry and state.
    pub fn uninstall_hub_plugin(&mut self, source_id: &str, plugin_name: &str) -> Result<bool> {
        let source = self
            .get_source(source_id)
            .ok_or_else(|| anyhow::anyhow!("Source not found: {}", source_id))?;
        if source.source_type != SourceType::Hub {
            anyhow::bail!("Source {} is not a hub", source.name);
        }

        let local_path = PathBuf::from(&source.local_path);
        let mut installed = hub::read_installed(&local_path);
        let removed = installed.plugins.remove(plugin_name).is_some();
        hub::write_installed(&local_path, &installed)?;

        let repo = hub::repo_path(&local_path, plugin_name);
        if repo.exists() {
            fs::remove_dir_all(&repo).with_context(|| format!("Failed to remove {:?}", repo))?;
        }

        self.set_plugin_enabled(source_id, plugin_name, false)?;
        Ok(removed)
    }

    /// Discover skills in a plugin directory
    fn discover_skills(&self, dir: &str) -> Result<Vec<MarketplacePluginSkill>> {
        let skills_dir = Path::new(dir).join("skills");
        if !skills_dir.exists() {
            return Ok(Vec::new());
        }

        let mut skills = Vec::new();
        // Simplified: just list directories
        if let Ok(entries) = fs::read_dir(&skills_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    skills.push(MarketplacePluginSkill {
                        name: entry.file_name().to_string_lossy().to_string(),
                        description: String::new(),
                        disabled: false,
                    });
                }
            }
        }
        Ok(skills)
    }

    /// Discover subagents in a plugin directory
    fn discover_subagents(&self, dir: &str) -> Result<Vec<MarketplacePluginSubagent>> {
        let subagents_dir = Path::new(dir).join("subagents");
        if !subagents_dir.exists() {
            return Ok(Vec::new());
        }

        let mut subagents = Vec::new();
        if let Ok(entries) = fs::read_dir(&subagents_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    subagents.push(MarketplacePluginSubagent {
                        name: entry.file_name().to_string_lossy().to_string(),
                        description: String::new(),
                        disabled: false,
                    });
                }
            }
        }
        Ok(subagents)
    }

    /// Discover MCP servers in a plugin directory
    /// Read the MCP servers a plugin declares in its `.mcp.json`.
    ///
    /// The file is the contract; an `mcp/` directory listing is not, and
    /// reading directory names was why a plugin's servers were named but
    /// never launchable. `${CLAUDE_PLUGIN_ROOT}` expands to the plugin's own
    /// directory in the command, its arguments, the url and every env value.
    ///
    /// A server whose command the security scan calls dangerous beyond the
    /// baseline "this plugin runs code" finding is returned carrying
    /// [`MarketplacePluginMCPServer::blocked_reason`] — listed so the user can
    /// see it, never auto-registered.
    fn discover_mcp_servers(&self, dir: &str) -> Result<Vec<MarketplacePluginMCPServer>> {
        let path = Path::new(dir).join(".mcp.json");
        let path = if path.exists() {
            path
        } else {
            Path::new(dir).join("mcp.json")
        };
        let Ok(text) = fs::read_to_string(&path) else {
            return Ok(Vec::new());
        };

        let mut servers = Vec::new();
        for (name, cfg) in crate::mcp::config::parse_plugin_mcp_json(&text) {
            let expand = |v: &str| crate::mcp::config::expand_plugin_root(v, dir);
            let command = cfg
                .get("command")
                .and_then(|v| v.as_str())
                .map(|c| expand(c));
            let url = cfg.get("url").and_then(|v| v.as_str()).map(|u| expand(u));
            // `type` is the plugin spelling; fall back to what the entry
            // actually carries, so a `command`-only entry is stdio.
            let transport = cfg
                .get("type")
                .or_else(|| cfg.get("transport"))
                .and_then(|v| v.as_str())
                .map(str::to_ascii_lowercase)
                .unwrap_or_else(|| {
                    if url.is_some() {
                        "http".to_string()
                    } else {
                        "stdio".to_string()
                    }
                });
            let args: Vec<String> = cfg
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(|v| expand(v))
                        .collect()
                })
                .unwrap_or_default();
            let map_of = |field: &str| -> std::collections::HashMap<String, String> {
                cfg.get(field)
                    .and_then(|v| v.as_object())
                    .map(|o| {
                        o.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), expand(s))))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let mut env = map_of("env");
            env.insert("SENCLAW_PLUGIN_ROOT".to_string(), dir.to_string());
            env.insert("CLAUDE_PLUGIN_ROOT".to_string(), dir.to_string());

            let blocked_reason = command.as_deref().and_then(dangerous_command_reason);

            servers.push(MarketplacePluginMCPServer {
                name,
                transport,
                description: cfg
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                use_tools: cfg.get("useTools").and_then(|v| v.as_array()).map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_string)
                        .collect()
                }),
                command,
                args,
                env,
                url,
                headers: map_of("headers"),
                plugin_root: Some(dir.to_string()),
                blocked_reason,
            });
        }
        Ok(servers)
    }

    /// Get all enabled MCP servers from all enabled plugins across all sources.
    /// Mirrors TS MarketplaceManager.getMCPServerDefs().
    pub fn get_enabled_mcp_servers(&self) -> Vec<MarketplacePluginMCPServer> {
        let mut all_servers = Vec::new();

        for source in &self.config.sources {
            if !source.enabled {
                continue;
            }

            let st = self.get_source_state(&source.id);
            if let Ok(plugins) = self.find_plugins(source) {
                for def in plugins {
                    let meta = self.read_plugin_json(&def);
                    let name = self.plugin_name(&meta, &def);

                    // Only include if plugin is enabled
                    if !st.plugins.get(&name).copied().unwrap_or(false) {
                        continue;
                    }

                    // Get MCP servers for this plugin
                    if let Ok(servers) = self.discover_mcp_servers(&def.dir) {
                        for mut server in servers {
                            // Prefix with plugin name to avoid conflicts
                            server.name = format!("mkt__{}__{}", name, server.name);
                            all_servers.push(server);
                        }
                    }
                }
            }
        }

        all_servers
    }

    /// Enabled plugins' `(name, dir)` pairs across all enabled sources — the
    /// widget registry scans these for `widgets/widgets.json`, and the plugin
    /// widget-static route resolves plugin names through it. Mirrors
    /// [`Self::get_enabled_mcp_servers`]'s source/enable filtering.
    pub fn enabled_plugin_dirs(&self) -> Vec<(String, std::path::PathBuf)> {
        let mut out = Vec::new();
        for source in &self.config.sources {
            if !source.enabled {
                continue;
            }
            let st = self.get_source_state(&source.id);
            if let Ok(plugins) = self.find_plugins(source) {
                for def in plugins {
                    let meta = self.read_plugin_json(&def);
                    let name = self.plugin_name(&meta, &def);
                    if !st.plugins.get(&name).copied().unwrap_or(false) {
                        continue;
                    }
                    out.push((name, std::path::PathBuf::from(&def.dir)));
                }
            }
        }
        out
    }
}

/// Whether a plugin's MCP command is dangerous beyond the baseline
/// "enabling this plugin lets the daemon spawn a server" finding every
/// `.mcp.json` entry produces. Gating on the baseline would gate every plugin
/// MCP server ever written, which is the same as not shipping the feature; a
/// command carrying a destructive verb or shell chaining is a different thing
/// and does not get registered on a plugin toggle alone.
fn dangerous_command_reason(command: &str) -> Option<String> {
    if crate::util::shell_safety::has_dangerous_command(command) {
        return Some(format!(
            "the command contains a destructive or privileged verb: {command}"
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn manager(temp: &TempDir) -> MarketplaceManager {
        MarketplaceManager::with_paths(
            temp.path().join("config.json"),
            temp.path().join("state.json"),
            temp.path().join("clones"),
        )
        .unwrap()
    }

    #[test]
    fn reads_the_flat_plugin_mcp_json_shape() {
        // A Claude Code plugin's file *is* the server map. Reading only the
        // nested shape made such a plugin look like it declared nothing.
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("plug");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".mcp.json"),
            r#"{"weather":{"command":"${CLAUDE_PLUGIN_ROOT}/bin/serve","args":["--root","${CLAUDE_PLUGIN_ROOT}"],"env":{"CFG":"${CLAUDE_PLUGIN_ROOT}/cfg.json"}}}"#,
        )
        .unwrap();

        let mgr = manager(&temp);
        let servers = mgr
            .discover_mcp_servers(dir.to_str().unwrap())
            .unwrap();
        assert_eq!(servers.len(), 1);
        let s = &servers[0];
        assert_eq!(s.name, "weather");
        assert_eq!(s.transport, "stdio");
        let root = dir.to_str().unwrap();
        assert_eq!(s.command.as_deref(), Some(&*format!("{root}/bin/serve")));
        assert_eq!(s.args, vec!["--root".to_string(), root.to_string()]);
        assert_eq!(s.env.get("CFG").map(String::as_str), Some(&*format!("{root}/cfg.json")));
        // Both spellings of the root are injected so a server can find itself.
        assert_eq!(s.env.get("CLAUDE_PLUGIN_ROOT").map(String::as_str), Some(root));
        assert_eq!(s.env.get("SENCLAW_PLUGIN_ROOT").map(String::as_str), Some(root));
        assert!(s.blocked_reason.is_none());
    }

    #[test]
    fn reads_the_nested_shape_and_http_servers() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("plug");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".mcp.json"),
            r#"{"mcpServers":{"remote":{"type":"http","url":"https://example.test/mcp","headers":{"A":"b"}}}}"#,
        )
        .unwrap();

        let mgr = manager(&temp);
        let servers = mgr.discover_mcp_servers(dir.to_str().unwrap()).unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].transport, "http");
        assert_eq!(servers[0].url.as_deref(), Some("https://example.test/mcp"));
        assert_eq!(servers[0].headers.get("A").map(String::as_str), Some("b"));
    }

    #[test]
    fn a_destructive_command_is_flagged_rather_than_launched() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("plug");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".mcp.json"),
            r#"{"evil":{"command":"sudo rm -rf /tmp/x"}}"#,
        )
        .unwrap();

        let mgr = manager(&temp);
        let servers = mgr.discover_mcp_servers(dir.to_str().unwrap()).unwrap();
        assert!(servers[0].blocked_reason.is_some());
    }

    #[test]
    fn a_plugin_without_the_file_declares_nothing() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("plug");
        std::fs::create_dir_all(&dir).unwrap();
        let mgr = manager(&temp);
        assert!(mgr
            .discover_mcp_servers(dir.to_str().unwrap())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn test_manager_creation() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("config.json");
        let state_path = temp.path().join("state.json");
        let clones_dir = temp.path().join("clones");

        let manager = MarketplaceManager::with_paths(config_path, state_path, clones_dir).unwrap();
        assert!(manager.get_sources().is_empty());
    }

    #[test]
    fn test_migrate_legacy_hub_url() {
        let temp = TempDir::new().unwrap();
        let mut manager = MarketplaceManager::with_paths(
            temp.path().join("config.json"),
            temp.path().join("state.json"),
            temp.path().join("clones"),
        )
        .unwrap();

        // Seeded under the old default: URL and auto-derived name both move.
        manager
            .add_source(
                "hub-store.bacnd.com".to_string(),
                SourceType::Hub,
                Some(format!("{}/marketplace.json", hub::LEGACY_HUB_URL)),
                None,
                None,
                Some(0),
                Some(true),
            )
            .unwrap();
        assert!(manager.migrate_legacy_hub_url().unwrap());
        let s = &manager.get_sources()[0];
        assert_eq!(s.url.as_deref(), Some("https://senclaw.bacnd.com/marketplace.json"));
        assert_eq!(s.name, "senclaw.bacnd.com");

        // Second run is a no-op: the new URL already exists.
        assert!(!manager.migrate_legacy_hub_url().unwrap());
    }

    #[test]
    fn test_migrate_legacy_hub_keeps_custom_name_and_other_hubs() {
        let temp = TempDir::new().unwrap();
        let mut manager = MarketplaceManager::with_paths(
            temp.path().join("config.json"),
            temp.path().join("state.json"),
            temp.path().join("clones"),
        )
        .unwrap();

        manager
            .add_source(
                "My Store".to_string(), // user-renamed → name survives
                SourceType::Hub,
                Some(hub::LEGACY_HUB_URL.to_string()),
                None,
                None,
                Some(0),
                Some(true),
            )
            .unwrap();
        manager
            .add_source(
                "other".to_string(),
                SourceType::Hub,
                Some("https://example.com/marketplace.json".to_string()),
                None,
                None,
                Some(1),
                Some(true),
            )
            .unwrap();

        assert!(manager.migrate_legacy_hub_url().unwrap());
        let sources = manager.get_sources();
        let mine = sources.iter().find(|s| s.name == "My Store").unwrap();
        assert_eq!(mine.url.as_deref(), Some("https://senclaw.bacnd.com/marketplace.json"));
        let other = sources.iter().find(|s| s.name == "other").unwrap();
        assert_eq!(other.url.as_deref(), Some("https://example.com/marketplace.json"));
    }

    #[test]
    fn test_add_source() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("config.json");
        let state_path = temp.path().join("state.json");
        let clones_dir = temp.path().join("clones");

        let mut manager =
            MarketplaceManager::with_paths(config_path, state_path, clones_dir).unwrap();
        let source = manager
            .add_source(
                "test".to_string(),
                SourceType::Local,
                None,
                None,
                Some(temp.path().to_string_lossy().to_string()),
                None,
                None,
            )
            .unwrap();

        assert_eq!(source.name, "test");
        assert_eq!(manager.get_sources().len(), 1);
    }

    #[test]
    fn test_remove_source() {
        let temp = TempDir::new().unwrap();
        let config_path = temp.path().join("config.json");
        let state_path = temp.path().join("state.json");
        let clones_dir = temp.path().join("clones");

        let mut manager =
            MarketplaceManager::with_paths(config_path, state_path, clones_dir).unwrap();
        let source = manager
            .add_source(
                "test".to_string(),
                SourceType::Local,
                None,
                None,
                Some(temp.path().to_string_lossy().to_string()),
                None,
                None,
            )
            .unwrap();

        let result = manager.remove_source(&source.id).unwrap();
        assert!(result);
        assert!(manager.get_sources().is_empty());
    }

    /// Manager over a temp dir with the given hub, nothing seeded yet.
    fn hub_manager(temp: &TempDir, hub_url: &str) -> MarketplaceManager {
        MarketplaceManager::with_paths_and_hub(
            temp.path().join("config.json"),
            temp.path().join("state.json"),
            temp.path().join("clones"),
            hub_url,
        )
        .unwrap()
    }

    #[test]
    fn seeds_the_default_hub_exactly_once() {
        let temp = TempDir::new().unwrap();
        let mut manager = hub_manager(&temp, "https://hub.example.com");

        assert!(manager.ensure_default_hub().unwrap());
        let sources = manager.get_sources();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].source_type, SourceType::Hub);
        assert_eq!(
            sources[0].url.as_deref(),
            Some("https://hub.example.com/marketplace.json")
        );

        // Re-seeding is a no-op, and a removed hub stays removed.
        assert!(!manager.ensure_default_hub().unwrap());
        manager.remove_source(&sources[0].id).unwrap();
        assert!(!manager.ensure_default_hub().unwrap());
        assert!(manager.get_sources().is_empty());
    }

    #[test]
    fn lists_catalog_plugins_as_available_until_installed() {
        let temp = TempDir::new().unwrap();
        let mut manager = hub_manager(&temp, "https://hub.example.com");
        let source = manager
            .add_source(
                "hub".into(),
                SourceType::Hub,
                Some("https://hub.example.com".into()),
                None,
                None,
                None,
                None,
            )
            .unwrap();

        // Seed the cache directly: get_source_info must not need the network.
        let local_path = PathBuf::from(&source.local_path);
        let catalog: hub::HubCatalog = serde_json::from_str(
            r#"{"plugins":[{"name":"demo","source":{"source":"github","repo":"acme/demo"},
                 "description":"Demo","version":"1.0.0","category":"development"}]}"#,
        )
        .unwrap();
        hub::write_catalog_cache(&local_path, &catalog).unwrap();

        let info = manager.get_source_info(&source.id).unwrap().unwrap();
        assert_eq!(info.plugins.len(), 1);
        let plugin = &info.plugins[0];
        assert_eq!(plugin.name, "demo");
        assert!(!plugin.installed);
        assert!(!plugin.enabled);
        assert_eq!(plugin.category.as_deref(), Some("development"));

        // Fake an install (no clone), then the same entry reads back installed.
        let dir = local_path.join("repos").join("demo");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("plugin.json"), r#"{"name":"demo"}"#).unwrap();
        let mut installed = hub::read_installed(&local_path);
        installed.plugins.insert(
            "demo".into(),
            hub::InstalledPlugin {
                name: "demo".into(),
                dir: dir.to_string_lossy().to_string(),
                repo_url: "https://github.com/acme/demo".into(),
                branch: "main".into(),
                version: Some("1.0.0".into()),
                installed_at: "2026-07-20T00:00:00Z".into(),
            },
        );
        hub::write_installed(&local_path, &installed).unwrap();
        manager
            .set_plugin_enabled(&source.id, "demo", true)
            .unwrap();

        let info = manager.get_source_info(&source.id).unwrap().unwrap();
        assert_eq!(
            info.plugins.len(),
            1,
            "installed plugin must not be listed twice"
        );
        assert!(info.plugins[0].installed);
        assert!(info.plugins[0].enabled);

        // Uninstall clears the clone, the manifest entry and the enable state.
        assert!(manager.uninstall_hub_plugin(&source.id, "demo").unwrap());
        assert!(!dir.exists());
        let info = manager.get_source_info(&source.id).unwrap().unwrap();
        assert!(!info.plugins[0].installed);
        assert!(!info.plugins[0].enabled);
    }
}
