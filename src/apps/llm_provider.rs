//! Space Apps that serve models, registered as ordinary LLM configs.
//!
//! An app declaring an [`LlmDecl`](super::manifest::LlmDecl) block becomes a
//! provider: the models it advertises appear in the same picker as OpenAI and
//! Anthropic, and turns routed to one arrive at the app's own
//! `/v1/chat/completions` over loopback.
//!
//! ## Why the registry is in memory, and the table is only the record
//!
//! Every model decision in SenClaw — the picker, `resolve_model_profile_at`,
//! and the vision check that wraps it — funnels through one function,
//! [`load_llm_configs`](crate::gateway::group_manager::load_llm_configs), which
//! takes a path to `config.json` and no database. Merging app providers into
//! *that* is what makes selecting an app's model actually route a turn; merging
//! them into the HTTP list instead would show models in the picker that fail
//! with "config not found" the moment one is chosen.
//!
//! So the durable record lives in `space_app_llm_providers`, and a snapshot of
//! it lives in a process-global read-mostly registry that `load_llm_configs`
//! appends from. Registration writes both. This mirrors how app *processes* are
//! already tracked — the row says what is installed, memory says what is live.
//!
//! ## Why every provider is addressed through the daemon's proxy
//!
//! Even a `background` app, which has a port of its own and is running. The MCP
//! registration distinguishes the two (a background app's MCP server points at
//! its real port) because an MCP connection is long-lived and the extra hop
//! would be paid once. An LLM turn is a fresh request every time, the hop is
//! loopback, and pointing at a recorded port means inheriting the whole class of
//! bug that port belongs to: it is stale after a restart, and an orphan from a
//! previous daemon run may be holding it. The proxy resolves the live process
//! every time, starts a stopped session app, and — because it calls
//! `launcher.touch` — is what keeps the idle reaper from stopping an app in the
//! middle of a conversation.

use anyhow::{Context, Result};
use rusqlite::params;
use std::path::Path;
use std::sync::RwLock;

use crate::db::Db;
use crate::gateway::group_manager::LlmConfig;

pub use app_space_sdk::llm::ModelCard;

/// Prefix marking a config as belonging to an app rather than to the user.
///
/// Config ids are `app:<app-id>:<model-id>`. Deterministic on purpose: the
/// active-model selection is stored by id, and a random id would deselect the
/// user's model on every daemon restart.
pub const ID_PREFIX: &str = "app:";

/// Where an app's last-known model list is kept, relative to the app directory.
/// Written by the app through `app_space_sdk::llm::publish_models`.
const MODELS_CACHE: &str = ".senclaw/llm-models.json";

/// One app's provider registration.
#[derive(Debug, Clone, PartialEq)]
pub struct AppProvider {
    pub app_id: String,
    /// Label shown in the picker, before the model name.
    pub label: String,
    /// Wire format — `openai` or `anthropic`.
    pub adapt: String,
    /// Fully resolved endpoint, ending at the path the adapter appends to.
    pub base_url: String,
    pub models: Vec<ModelCard>,
    /// Edit format the app declared for its models (`llm.editFormat`).
    pub edit_format: Option<crate::zen_core::EditFormat>,
}

impl AppProvider {
    /// Render this app's models as configs the rest of SenClaw already
    /// understands.
    ///
    /// `api_key` is deliberately empty. The daemon reaches the app over loopback
    /// — exempt from its own API token — and the proxy stamps the app's access
    /// token onto what it forwards, so there is no credential for this hop.
    /// Leaving one here would also leak it: `GET /api/llm-config` returns
    /// configs verbatim.
    pub fn to_configs(&self) -> Vec<LlmConfig> {
        self.models
            .iter()
            .map(|m| LlmConfig {
                id: config_id(&self.app_id, &m.id),
                label: format!(
                    "{} · {}",
                    self.label,
                    m.display_name.as_deref().unwrap_or(&m.id)
                ),
                provider: format!("{ID_PREFIX}{}", self.app_id),
                base_url: self.base_url.clone(),
                api_key: String::new(),
                model_name: m.id.clone(),
                adapt: self.adapt.clone(),
                max_tokens: m.max_output_tokens,
                context_length: m.context_length,
                // Always explicit, never `None`. `None` means "infer from the
                // model name", and a local checkpoint id like
                // `mlx-community__Qwen3.5-2B-OptiQ-4bit` matches no vendor
                // pattern — the inference would answer by accident in whichever
                // direction the regexes happen to fall. The app read the
                // model's own config; that answer is the one that counts.
                vision: Some(m.vision),
                auth: None,
                oauth_account_id: None,
                edit_format: self.edit_format,
            })
            .collect()
    }
}

/// `app:<app-id>:<model-id>`.
pub fn config_id(app_id: &str, model_id: &str) -> String {
    format!("{ID_PREFIX}{app_id}:{model_id}")
}

/// Split a config id back into `(app_id, model_id)`, or `None` when it is not
/// an app-provided config.
///
/// Splits into exactly three parts: a model id may itself contain `:` (a
/// HuggingFace revision, a tag), and splitting it further would silently look up
/// a model that does not exist.
pub fn parse_config_id(id: &str) -> Option<(&str, &str)> {
    let rest = id.strip_prefix(ID_PREFIX)?;
    let (app_id, model_id) = rest.split_once(':')?;
    if app_id.is_empty() || model_id.is_empty() {
        return None;
    }
    Some((app_id, model_id))
}

/// True for a config this module owns. Used to keep app configs out of the
/// user's `config.json`.
pub fn is_app_config(id: &str) -> bool {
    parse_config_id(id).is_some()
}

// ============================================================================
// The in-memory registry
// ============================================================================

static REGISTRY: RwLock<Vec<AppProvider>> = RwLock::new(Vec::new());

fn registry() -> std::sync::RwLockReadGuard<'static, Vec<AppProvider>> {
    // A poisoned lock means a panic happened while the registry was being
    // written. The data is a plain snapshot rebuilt from the table, so reading
    // it after a panic is safe — and refusing to would take every model in the
    // picker down with it.
    REGISTRY.read().unwrap_or_else(|e| e.into_inner())
}

/// Every app-provided config, in registration order. Appended to the user's own
/// configs by `load_llm_configs`.
pub fn configs() -> Vec<LlmConfig> {
    registry().iter().flat_map(|p| p.to_configs()).collect()
}

/// The registered providers themselves, for the UI and for diagnostics.
pub fn providers() -> Vec<AppProvider> {
    registry().clone()
}

/// Is this app registered as a provider right now?
pub fn is_registered(app_id: &str) -> bool {
    registry().iter().any(|p| p.app_id == app_id)
}

fn set_registry(next: Vec<AppProvider>) {
    let mut w = REGISTRY.write().unwrap_or_else(|e| e.into_inner());
    *w = next;
}

// ============================================================================
// Persistence
// ============================================================================

/// Register (or re-register) an app's provider, in the table and in memory.
///
/// An empty `models` list is refused rather than stored. The list is what the
/// picker is built from, and an app whose `/v1/models` failed during a restart
/// would otherwise erase its own models — the same rule, for the same reason, as
/// the MCP tool cache.
pub fn register(db: &Db, provider: &AppProvider) -> Result<()> {
    if provider.models.is_empty() {
        anyhow::bail!(
            "app '{}' advertised no models — refusing to register an empty provider",
            provider.app_id
        );
    }
    let models = serde_json::to_string(&provider.models)?;
    let now = now_secs();
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO space_app_llm_providers
                 (app_id, label, adapt, base_url, models, updated_at, edit_format)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(app_id) DO UPDATE SET
                 label=excluded.label,
                 adapt=excluded.adapt,
                 base_url=excluded.base_url,
                 models=excluded.models,
                 updated_at=excluded.updated_at,
                 edit_format=excluded.edit_format",
            params![
                &provider.app_id,
                &provider.label,
                &provider.adapt,
                &provider.base_url,
                &models,
                now,
                provider.edit_format.map(|f| f.as_str().to_string()),
            ],
        )?;
        Ok(())
    })
    .context("persist app LLM provider")?;
    refresh(db)
}

/// Drop an app's provider. Called on uninstall and on disable.
///
/// Also clears any active-model selection that pointed at one of its models.
/// Leaving it dangling is not harmless: `resolve_model_profile_at` falls back to
/// the *first* config when the selected id is missing, so the user would keep
/// chatting — silently, to a different model than the one the UI shows.
pub fn unregister(db: &Db, app_id: &str, config_path: &Path) -> Result<()> {
    db.with_conn(|conn| {
        conn.execute(
            "DELETE FROM space_app_llm_providers WHERE app_id=?1",
            params![app_id],
        )?;
        Ok(())
    })
    .context("delete app LLM provider")?;
    clear_active_selection(config_path, app_id);
    refresh(db)
}

/// Rebuild the in-memory registry from the table.
pub fn refresh(db: &Db) -> Result<()> {
    set_registry(load_all(db)?);
    Ok(())
}

/// Read every registration back out of the table.
pub fn load_all(db: &Db) -> Result<Vec<AppProvider>> {
    db.with_conn(|conn| {
        let mut stmt = conn.prepare(
            "SELECT app_id, label, adapt, base_url, models, edit_format
               FROM space_app_llm_providers
              ORDER BY app_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (app_id, label, adapt, base_url, models, edit_format) = r?;
            let edit_format = edit_format.as_deref().and_then(crate::zen_core::EditFormat::parse);
            // A row whose model JSON no longer parses (a schema change, a
            // truncated write) is skipped, not fatal: one broken app must not
            // take every other provider out of the picker with it.
            match serde_json::from_str::<Vec<ModelCard>>(&models) {
                Ok(models) => out.push(AppProvider {
                    app_id,
                    label,
                    adapt,
                    base_url,
                    models,
                    edit_format,
                }),
                Err(e) => tracing::warn!(
                    "[app-llm] '{app_id}': stored model list is unreadable ({e}) — skipped"
                ),
            }
        }
        Ok(out)
    })
    .context("load app LLM providers")
}

/// Forget any active-model pointer into this app.
fn clear_active_selection(config_path: &Path, app_id: &str) {
    use crate::gateway::group_manager::{
        load_llm_configs, set_active_cognitive_llm_config, set_active_llm_config,
        set_active_quick_llm_config,
    };
    let belongs = |id: &Option<String>| {
        id.as_deref()
            .and_then(parse_config_id)
            .is_some_and(|(a, _)| a == app_id)
    };
    let stored = load_llm_configs(config_path);
    if belongs(&stored.active_id) {
        let _ = set_active_llm_config(config_path, None);
    }
    if belongs(&stored.active_quick_id) {
        let _ = set_active_quick_llm_config(config_path, None);
    }
    if belongs(&stored.active_cognitive_id) {
        let _ = set_active_cognitive_llm_config(config_path, None);
    }
}

// ============================================================================
// Discovering an app's models
// ============================================================================

/// The model list an app wrote at its last successful startup.
///
/// This is what lets a **stopped** session app still appear in the picker.
/// Without it nothing would ever select one of its models, so nothing would ever
/// call it, so it would never start — the same bootstrap problem the MCP tool
/// cache solves, and solved the same way.
pub fn read_models_cache(app_dir: &Path) -> Vec<ModelCard> {
    let raw = match std::fs::read_to_string(app_dir.join(MODELS_CACHE)) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| serde_json::from_value::<Vec<ModelCard>>(v["models"].clone()).ok())
        .unwrap_or_default()
}

/// Ask a running app what it serves, over its OpenAI `/models` endpoint.
pub async fn fetch_models(
    http: &reqwest::Client,
    base_url: &str,
) -> Result<Vec<ModelCard>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let res = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = res.status();
    let body: serde_json::Value = res
        .json()
        .await
        .with_context(|| format!("{url} returned {status} with a non-JSON body"))?;
    if !status.is_success() {
        anyhow::bail!("{url} returned {status}");
    }
    let data = body["data"]
        .as_array()
        .context("`/models` response has no `data` array")?;

    let mut out = Vec::new();
    for m in data {
        let Some(id) = m["id"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        // `vision` decides between real image blocks and the OCR fallback, and
        // a text-only endpoint answers an image block with a hard 400 that
        // fails the whole turn. An entry that does not state it is dropped
        // rather than guessed at: a model missing from the picker is a visible
        // problem the app author fixes, a model that silently cannot be shown
        // an image is not.
        let Some(vision) = m["vision"].as_bool() else {
            tracing::warn!(
                "[app-llm] model '{id}' from {url} declares no `vision` field — skipped"
            );
            continue;
        };
        out.push(ModelCard {
            id: id.to_string(),
            display_name: m["display_name"].as_str().map(str::to_string),
            context_length: m["context_length"].as_u64().unwrap_or(8192) as u32,
            max_output_tokens: m["max_output_tokens"].as_u64().unwrap_or(4096) as u32,
            vision,
            tools: m["tools"].as_bool().unwrap_or(true),
        });
    }
    Ok(out)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(id: &str, vision: bool) -> ModelCard {
        ModelCard::new(id, 128_000, 8192, vision)
    }

    fn provider() -> AppProvider {
        AppProvider {
            app_id: "mlx-llm".into(),
            label: "MLX".into(),
            adapt: "openai".into(),
            base_url: "http://127.0.0.1:18788/api/space/apps/mlx-llm/proxy/v1".into(),
            models: vec![card("gemma-4-e2b", true), card("qwen3.5-2b", false)],
            edit_format: None,
        }
    }

    #[test]
    fn config_ids_are_deterministic_so_a_selection_survives_a_restart() {
        assert_eq!(config_id("mlx-llm", "gemma"), "app:mlx-llm:gemma");
        let a = provider().to_configs();
        let b = provider().to_configs();
        assert_eq!(
            a.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            b.iter().map(|c| c.id.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_model_id_containing_a_colon_round_trips_whole() {
        let id = config_id("mlx-llm", "org/model:q4_0");
        assert_eq!(parse_config_id(&id), Some(("mlx-llm", "org/model:q4_0")));
    }

    #[test]
    fn a_user_config_id_is_not_mistaken_for_an_app_one() {
        for id in ["llm_1700000000_1234", "app:", "app:onlyapp", "", "app::m", "app:a:"] {
            assert!(!is_app_config(id), "`{id}` must not parse as an app config");
        }
        assert!(is_app_config("app:mlx-llm:gemma"));
    }

    /// The vision flag is the one field that must not be left to inference —
    /// see `to_configs`.
    #[test]
    fn vision_is_carried_explicitly_onto_every_config() {
        let cfgs = provider().to_configs();
        assert_eq!(cfgs[0].vision, Some(true));
        assert_eq!(cfgs[1].vision, Some(false));
        assert!(
            cfgs.iter().all(|c| c.vision.is_some()),
            "`None` would re-enable name-based inference"
        );
    }

    #[test]
    fn no_credential_is_written_into_a_world_readable_config() {
        // `GET /api/llm-config` returns these verbatim.
        assert!(provider().to_configs().iter().all(|c| c.api_key.is_empty()));
    }

    #[test]
    fn the_label_names_both_the_app_and_the_model() {
        let p = AppProvider {
            models: vec![card("gemma-4-e2b", true).display_name("Gemma 4 E2B")],
            ..provider()
        };
        assert_eq!(p.to_configs()[0].label, "MLX · Gemma 4 E2B");
        // Falls back to the wire id when the app gave no display name.
        assert_eq!(provider().to_configs()[0].label, "MLX · gemma-4-e2b");
    }

    #[test]
    fn a_missing_or_malformed_cache_reads_as_no_models_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_models_cache(dir.path()).is_empty());

        std::fs::create_dir_all(dir.path().join(".senclaw")).unwrap();
        std::fs::write(dir.path().join(MODELS_CACHE), "{ truncated").unwrap();
        assert!(read_models_cache(dir.path()).is_empty());
    }

    // ── Against a real database ────────────────────────────────────────────
    //
    // These share one process-global registry, so they must not run in
    // parallel with each other. A mutex rather than one big test: a failure
    // should name which behaviour broke.
    static DB_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn test_db() -> Db {
        Db::open_in_memory(&crate::config::Config::from_env()).unwrap()
    }

    /// Registration must survive a daemon restart — the table is the record,
    /// the registry is only a snapshot of it.
    #[test]
    fn a_registration_reloads_from_the_table() {
        let _g = DB_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let db = test_db();
        register(&db, &provider()).unwrap();
        assert!(is_registered("mlx-llm"));

        // Simulate a fresh process: registry empty, table intact.
        set_registry(Vec::new());
        assert!(!is_registered("mlx-llm"));
        refresh(&db).unwrap();

        let back = providers();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0], provider());
        set_registry(Vec::new());
    }

    #[test]
    fn re_registering_replaces_rather_than_duplicates() {
        let _g = DB_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let db = test_db();
        register(&db, &provider()).unwrap();
        let updated = AppProvider {
            models: vec![card("gemma-4-e4b", true)],
            ..provider()
        };
        register(&db, &updated).unwrap();

        assert_eq!(providers().len(), 1, "app_id is the primary key");
        assert_eq!(configs().len(), 1);
        assert_eq!(configs()[0].model_name, "gemma-4-e4b");
        set_registry(Vec::new());
    }

    /// The rule the MCP tool cache follows, for the same reason: an app whose
    /// `/models` failed during a restart must not erase its own models from the
    /// picker.
    #[test]
    fn an_empty_model_list_is_refused_and_leaves_the_old_one_standing() {
        let _g = DB_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let db = test_db();
        register(&db, &provider()).unwrap();
        let before = configs().len();

        let empty = AppProvider {
            models: Vec::new(),
            ..provider()
        };
        assert!(register(&db, &empty).is_err());
        assert_eq!(configs().len(), before);
        set_registry(Vec::new());
    }

    #[test]
    fn unregistering_takes_the_models_out_of_the_picker() {
        let _g = DB_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let db = test_db();
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");

        register(&db, &provider()).unwrap();
        assert_eq!(configs().len(), 2);
        unregister(&db, "mlx-llm", &config_path).unwrap();
        assert!(configs().is_empty());
        assert!(load_all(&db).unwrap().is_empty());
    }

    /// A dangling active id is worse than a missing model: the profile resolver
    /// falls back to the *first* config when the selected one is gone, so the
    /// user keeps chatting to a model the UI is not showing.
    #[test]
    fn unregistering_clears_an_active_selection_into_the_app() {
        use crate::gateway::group_manager::{load_llm_configs, set_active_llm_config};
        let _g = DB_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let db = test_db();
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");

        register(&db, &provider()).unwrap();
        let chosen = config_id("mlx-llm", "gemma-4-e2b");
        set_active_llm_config(&config_path, Some(&chosen)).unwrap();
        assert_eq!(load_llm_configs(&config_path).active_id.as_deref(), Some(chosen.as_str()));

        unregister(&db, "mlx-llm", &config_path).unwrap();
        assert_eq!(load_llm_configs(&config_path).active_id, None);
    }

    /// A selection pointing at a *different* app must survive.
    #[test]
    fn unregistering_leaves_another_apps_selection_alone() {
        use crate::gateway::group_manager::{load_llm_configs, set_active_llm_config};
        let _g = DB_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let db = test_db();
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");

        register(&db, &provider()).unwrap();
        let other = config_id("other-app", "some-model");
        set_active_llm_config(&config_path, Some(&other)).unwrap();

        unregister(&db, "mlx-llm", &config_path).unwrap();
        assert_eq!(load_llm_configs(&config_path).active_id.as_deref(), Some(other.as_str()));
    }

    /// `load_llm_configs` is the single seam every model decision goes through —
    /// the picker, `resolve_model_profile_at`, and the vision check. If app
    /// models are not in *that* list, selecting one fails with "config not
    /// found" the moment a turn runs.
    #[test]
    fn app_models_reach_the_function_the_profile_resolver_reads() {
        use crate::gateway::group_manager::load_llm_configs;
        let _g = DB_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let db = test_db();
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");

        assert!(load_llm_configs(&config_path).configs.is_empty());
        register(&db, &provider()).unwrap();

        let ids: Vec<String> = load_llm_configs(&config_path)
            .configs
            .iter()
            .map(|c| c.id.clone())
            .collect();
        assert!(ids.contains(&config_id("mlx-llm", "gemma-4-e2b")));
        set_registry(Vec::new());
    }

    /// App configs are rebuilt on every read; freezing a copy into `config.json`
    /// would outlive the app — still in the picker after an uninstall, still
    /// naming a port nothing listens on.
    #[test]
    fn an_app_config_cannot_be_written_into_the_users_config_file() {
        use crate::gateway::group_manager::save_llm_config;
        let _g = DB_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let db = test_db();
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");

        register(&db, &provider()).unwrap();
        let cfg = configs().into_iter().next().unwrap();
        assert!(save_llm_config(&config_path, &cfg).is_err());
        set_registry(Vec::new());
    }

    #[test]
    fn the_cache_round_trips_what_the_sdk_writes() {
        let dir = tempfile::tempdir().unwrap();
        let models = vec![card("gemma-4-e2b", true)];
        app_space_sdk::llm::publish_models(dir.path(), &models).unwrap();
        let back = read_models_cache(dir.path());
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, "gemma-4-e2b");
        assert!(back[0].vision);
    }
}
