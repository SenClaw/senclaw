//! Per-app access tokens — the identity a Space App presents to the daemon.
//!
//! # Why this exists
//!
//! Every `/api/space/apps/<id>/…` route used to be authenticated by one thing:
//! the request arrived over loopback. That is a boundary around the *machine*,
//! not around the *app*. Inside it, app A could POST to app B's
//! `/bridge` (which runs a full tool-enabled agent), read B's `/config` (where
//! API keys and cookies live) and query B's SQLite file — with nothing but B's
//! id, which is public. Confining an app with the per-app sandbox did not help:
//! an app allowed to reach the daemon was allowed to reach *all* of it.
//!
//! So the daemon now mints one secret per installed app, hands it to that app's
//! process in `SENCLAW_TOKEN_ACCESS_APP`, and treats it as the app's name. A
//! request carrying a token may only touch the id that token belongs to —
//! anything else is 403, never a silent redirect to the caller's own data.
//!
//! # What it does not do
//!
//! The token identifies an app to the daemon; it does not identify a *process*
//! to the operating system. A program running unconfined as the user can read
//! `~/.senclaw/senclaw.db` and take any token in it, so this is not a boundary
//! against local malware — that program already has the database it would be
//! attacking. It is a boundary between **apps**, and it is a real one when the
//! app is confined by the per-app sandbox (`docs/space-app-sandbox.md`), which
//! is what stops it reading the database in the first place. The two features
//! are meant to be used together.
//!
//! # Shape
//!
//! `sca_<64 hex chars>` — 32 bytes of entropy behind a fixed prefix. The prefix
//! is load-bearing: an app token and the daemon's own API token both arrive as
//! `Authorization: Bearer …`, and the prefix is how the middleware tells them
//! apart without a database lookup per request.

use anyhow::Result;
use rand::RngCore;
use rusqlite::{params, OptionalExtension};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use crate::db::Db;

/// Prefix on every app access token. See the module docs.
pub const TOKEN_PREFIX: &str = "sca_";

/// Env var carrying the app's own access token into its process.
pub const ENV_APP_TOKEN: &str = "SENCLAW_TOKEN_ACCESS_APP";

/// Env var carrying the Space-App API contract version into its process.
pub const ENV_API_VERSION: &str = "SENCLAW_API_VERSION";

/// Header an app (or the daemon's proxy) uses to present the access token.
pub const HEADER_APP_TOKEN: &str = "x-senclaw-app-token";

/// Header carrying the API contract version, both directions.
pub const HEADER_API_VERSION: &str = "x-senclaw-api-version";

/// Current version of the Space-App API contract.
///
/// Bumped when a change would break an app built against the previous one —
/// a removed bridge action, a renamed field, a route that moves. Additive
/// changes (a new action, a new optional field) do not bump it.
///
/// | version | contract |
/// |---|---|
/// | 1 | Loopback-trust era: no app identity, no version header. |
/// | 2 | Per-app access tokens (`SENCLAW_TOKEN_ACCESS_APP`), version header. |
pub const API_VERSION: u32 = 2;

/// The oldest contract this daemon still answers. Requests declaring an older
/// version are served rather than refused — an app pinned to v1 predates the
/// token entirely, and refusing it would break every installed app on upgrade.
pub const MIN_API_VERSION: u32 = 1;

/// How the daemon treats an app-scoped request that carries **no** token.
///
/// A token that *is* present is verified and scoped in every mode — a wrong or
/// foreign token is refused whatever this says. The mode only decides what
/// happens when there is no token at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenMode {
    /// Serve it, exactly as before this feature existed. The escape hatch for
    /// an app that reaches the daemon with its own HTTP client and has not been
    /// taught to send the token.
    Off,
    /// Serve it, but log it once per app — the way to find out which apps would
    /// break under [`Self::Strict`] without breaking them first.
    Warn,
    /// Refuse it. Only requests carrying this app's token — or coming from the
    /// daemon's own UI (see `gateway::ui_server::app_auth`) — reach app-scoped
    /// data routes. **The default.**
    Strict,
}

/// What the daemon uses when `SENCLAW_APP_TOKEN_MODE` is unset or unreadable.
///
/// Requiring the token is the safe end: an app that reaches the daemon over
/// loopback without proving who it is gets refused rather than served. Every
/// SDK sends the token on its own, and the daemon's proxy stamps it on
/// everything it forwards, so the apps that break are the ones hand-rolling an
/// HTTP client to `/api/space/apps/<id>/…` — and those break loudly, with a
/// 401 that names the variable to set.
pub const DEFAULT_TOKEN_MODE: TokenMode = TokenMode::Strict;

/// Where the mode in force actually came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeSource {
    /// The operator chose it in the UI; stored in the database.
    Ui,
    /// `SENCLAW_APP_TOKEN_MODE` in the daemon's environment.
    Env,
    /// Neither — [`DEFAULT_TOKEN_MODE`].
    Default,
}

impl ModeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ui => "ui",
            Self::Env => "env",
            Self::Default => "default",
        }
    }
}

/// KV key holding the operator's chosen mode.
///
/// `router_state` rather than a table of its own: this is one scalar, and a
/// migration for one scalar is a migration to maintain forever. The `space:`
/// prefix keeps it out of the router's own `lastAgent:` namespace.
const MODE_KEY: &str = "space:appTokenMode";

/// The chosen mode, cached. `None` = not read yet; `Some(None)` = read, and the
/// operator has chosen nothing, so the environment decides.
///
/// Cached because the middleware asks on every app-scoped request, and this
/// answer changes only when someone clicks a button.
fn mode_cache() -> &'static RwLock<Option<Option<TokenMode>>> {
    static C: OnceLock<RwLock<Option<Option<TokenMode>>>> = OnceLock::new();
    C.get_or_init(|| RwLock::new(None))
}

/// The mode the operator chose, or `None` to follow the environment.
pub fn mode_override(db: &Db) -> Option<TokenMode> {
    if let Ok(c) = mode_cache().read() {
        if let Some(cached) = *c {
            return cached;
        }
    }
    let found = db
        .get_router_state(MODE_KEY)
        .ok()
        .flatten()
        .and_then(|raw| TokenMode::parse_opt(&raw));
    if let Ok(mut c) = mode_cache().write() {
        *c = Some(found);
    }
    found
}

/// Choose a mode, or pass `None` to hand the decision back to the environment.
pub fn set_mode_override(db: &Db, mode: Option<TokenMode>) -> Result<()> {
    match mode {
        Some(m) => db.set_router_state(MODE_KEY, m.as_str())?,
        None => db.delete_router_state(MODE_KEY)?,
    }
    if let Ok(mut c) = mode_cache().write() {
        *c = Some(mode);
    }
    Ok(())
}

/// The mode in force, and where it came from.
///
/// `env_mode` is what [`crate::config::Config`] read at startup; pass
/// `env_present` so a value that merely *equals* the default is not reported as
/// having been configured.
pub fn effective_mode(db: &Db, env_mode: TokenMode, env_present: bool) -> (TokenMode, ModeSource) {
    match mode_override(db) {
        Some(m) => (m, ModeSource::Ui),
        None if env_present => (env_mode, ModeSource::Env),
        None => (env_mode, ModeSource::Default),
    }
}

/// Forget the cached choice. Tests only — the daemon has one database.
#[cfg(test)]
fn mode_cache_clear() {
    if let Ok(mut c) = mode_cache().write() {
        *c = None;
    }
}

impl TokenMode {
    /// Parse an explicitly written mode. `None` for anything unrecognised —
    /// the caller decides, and [`Self::from_env_value`] is what the daemon uses.
    pub fn parse_opt(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "strict" | "on" | "require" | "required" => Some(Self::Strict),
            "warn" | "log" => Some(Self::Warn),
            "off" | "none" | "disabled" | "false" | "0" => Some(Self::Off),
            _ => None,
        }
    }

    /// The mode for a raw `SENCLAW_APP_TOKEN_MODE` value, empty or not.
    ///
    /// An unrecognised value falls back to [`DEFAULT_TOKEN_MODE`] and says so.
    /// It must never fall back to [`Self::Off`]: `SENCLAW_APP_TOKEN_MODE=of`
    /// would then read as "disable app isolation", and a typo that silently
    /// turns a security control off is the failure this whole feature exists
    /// to avoid.
    pub fn from_env_value(raw: &str) -> Self {
        let raw = raw.trim();
        if raw.is_empty() {
            return DEFAULT_TOKEN_MODE;
        }
        match Self::parse_opt(raw) {
            Some(m) => m,
            None => {
                tracing::warn!(
                    "[app-auth] SENCLAW_APP_TOKEN_MODE={raw:?} is not one of off|warn|strict — \
                     using {} (the default)",
                    DEFAULT_TOKEN_MODE.as_str()
                );
                DEFAULT_TOKEN_MODE
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Warn => "warn",
            Self::Strict => "strict",
        }
    }

    pub fn is_strict(self) -> bool {
        matches!(self, Self::Strict)
    }
}

impl std::fmt::Display for TokenMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ===== Generation =====

/// A fresh token. 32 bytes of OS entropy, hex, behind [`TOKEN_PREFIX`].
pub fn generate() -> String {
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("{TOKEN_PREFIX}{hex}")
}

/// True when `raw` has the shape of an app token. A cheap pre-filter, not a
/// validity check — [`resolve`] is what says whether it is a *real* token.
pub fn looks_like_app_token(raw: &str) -> bool {
    let raw = raw.trim();
    raw.len() == TOKEN_PREFIX.len() + 64
        && raw.starts_with(TOKEN_PREFIX)
        && raw[TOKEN_PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
}

/// Constant-time compare. `==` on a secret short-circuits at the first
/// differing byte and leaks the matched prefix length through timing.
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

// ===== Store =====

/// The token table, both ways round, so neither hot path touches SQLite per
/// request: `by_token` answers the middleware's "whose token is this?", and
/// `by_app` answers the proxy's "what do I stamp on this forward?" — and the
/// proxy runs on every asset of every app iframe.
///
/// Filled from the table on first use and kept in step by [`ensure`] /
/// [`rotate`] / [`revoke`], which are the only writers.
#[derive(Default)]
struct Cache {
    by_token: HashMap<String, String>,
    by_app: HashMap<String, String>,
}

fn cache() -> &'static RwLock<Cache> {
    static CACHE: OnceLock<RwLock<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(Cache::default()))
}

fn cache_put(token: &str, app_id: &str) {
    if let Ok(mut c) = cache().write() {
        // Drop whatever this app held before, or a rotated token would keep
        // resolving from the reverse map.
        if let Some(old) = c.by_app.insert(app_id.to_string(), token.to_string()) {
            if old != token {
                c.by_token.remove(&old);
            }
        }
        c.by_token.insert(token.to_string(), app_id.to_string());
    }
}

fn cache_drop_app(app_id: &str) {
    if let Ok(mut c) = cache().write() {
        c.by_app.remove(app_id);
        c.by_token.retain(|_, v| v != app_id);
    }
}

fn cache_get(token: &str) -> Option<String> {
    cache().read().ok()?.by_token.get(token).cloned()
}

fn cache_get_app(app_id: &str) -> Option<String> {
    cache().read().ok()?.by_app.get(app_id).cloned()
}

/// Wipe the process-wide cache. Tests only — each test uses its own temp DB,
/// and a token minted by one would otherwise resolve inside another.
#[cfg(test)]
fn cache_clear() {
    if let Ok(mut c) = cache().write() {
        *c = Cache::default();
    }
}

/// This app's token, minting one on first call.
///
/// Called on every launch, so an app installed before this feature gets its
/// token the first time it starts — there is no migration step to forget.
pub fn ensure(db: &Db, app_id: &str) -> Result<String> {
    if let Some(existing) = load(db, app_id)? {
        return Ok(existing);
    }
    let token = generate();
    let now = chrono::Utc::now().timestamp();
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO space_app_tokens (app_id, token, created_at, rotated_at)
             VALUES (?1, ?2, ?3, NULL)
             ON CONFLICT(app_id) DO NOTHING",
            params![app_id, &token, now],
        )?;
        Ok(())
    })?;
    // A concurrent launch may have won the insert; the stored value is the
    // truth, not the one this call generated.
    let stored = load(db, app_id)?.unwrap_or(token);
    cache_put(&stored, app_id);
    Ok(stored)
}

/// This app's token if it has one. Does not mint.
pub fn load(db: &Db, app_id: &str) -> Result<Option<String>> {
    if let Some(cached) = cache_get_app(app_id) {
        return Ok(Some(cached));
    }
    let token: Option<String> = db.with_conn(|conn| {
        Ok(conn
            .query_row(
                "SELECT token FROM space_app_tokens WHERE app_id=?1",
                params![app_id],
                |row| row.get(0),
            )
            .optional()?)
    })?;
    if let Some(t) = &token {
        cache_put(t, app_id);
    }
    Ok(token)
}

/// Replace this app's token. The old one stops working immediately, so the app
/// must be restarted to pick the new one up — callers of the REST endpoint do
/// that for the operator.
pub fn rotate(db: &Db, app_id: &str) -> Result<String> {
    let token = generate();
    let now = chrono::Utc::now().timestamp();
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO space_app_tokens (app_id, token, created_at, rotated_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(app_id) DO UPDATE SET token=excluded.token, rotated_at=?3",
            params![app_id, &token, now],
        )?;
        Ok(())
    })?;
    cache_drop_app(app_id);
    cache_put(&token, app_id);
    Ok(token)
}

/// Forget an app's token — called when the app is uninstalled, so a stale row
/// cannot authenticate a later app that reuses the id.
pub fn revoke(db: &Db, app_id: &str) -> Result<()> {
    db.with_conn(|conn| {
        conn.execute(
            "DELETE FROM space_app_tokens WHERE app_id=?1",
            params![app_id],
        )?;
        Ok(())
    })?;
    cache_drop_app(app_id);
    Ok(())
}

/// Which app owns this token, or `None` when it belongs to no app.
///
/// The cache answers the common case; a miss falls back to the table, so a
/// token minted by another process (the CLI, a second daemon run) still
/// resolves. Malformed input never reaches the database.
pub fn resolve(db: &Db, presented: &str) -> Option<String> {
    let presented = presented.trim();
    if !looks_like_app_token(presented) {
        return None;
    }
    if let Some(app) = cache_get(presented) {
        return Some(app);
    }
    let found: Option<String> = db
        .with_conn(|conn| {
            Ok(conn
                .query_row(
                    "SELECT app_id FROM space_app_tokens WHERE token=?1",
                    params![presented],
                    |row| row.get(0),
                )
                .optional()?)
        })
        .ok()
        .flatten();
    if let Some(app) = &found {
        cache_put(presented, app);
    }
    found
}

/// Whether `presented` is exactly `app_id`'s token.
pub fn verify(db: &Db, app_id: &str, presented: &str) -> bool {
    match load(db, app_id) {
        Ok(Some(expected)) => ct_eq(presented.trim(), &expected),
        _ => false,
    }
}

/// Everything the launcher has to put in an app's environment. One function so
/// the three launch paths (server process, stdio MCP child, HTTP MCP headers)
/// cannot drift apart.
pub fn launch_env(db: &Db, app_id: &str, api_version: u32) -> Vec<(String, String)> {
    let mut env = vec![(ENV_API_VERSION.to_string(), api_version.to_string())];
    match ensure(db, app_id) {
        Ok(token) => env.push((ENV_APP_TOKEN.to_string(), token)),
        Err(e) => {
            // An app with no token still launches: in `off`/`warn` mode it
            // works exactly as before, and refusing to start the app over a
            // failed insert would turn a database hiccup into an outage.
            tracing::warn!("[app-token] '{app_id}': cannot mint an access token: {e}");
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh database *and* the right to be the only test using the cache.
    ///
    /// The cache is a process-wide singleton because a daemon has exactly one
    /// database. Tests break that assumption — each brings its own in-memory DB
    /// under the same app ids — so a token minted by one would otherwise be
    /// served to another from the cache. Serializing them is the honest fix;
    /// making the cache DB-aware would be complexity that only tests need.
    fn test_db() -> (Db, std::sync::MutexGuard<'static, ()>) {
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        let guard = LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        cache_clear();
        let db = Db::open_in_memory(&crate::config::Config::from_env()).expect("in-memory db");
        (db, guard)
    }

    #[test]
    fn generated_tokens_have_the_documented_shape() {
        let t = generate();
        assert!(t.starts_with(TOKEN_PREFIX));
        assert_eq!(t.len(), TOKEN_PREFIX.len() + 64);
        assert!(looks_like_app_token(&t));
        // 32 bytes of entropy — two calls colliding would mean the RNG is broken.
        assert_ne!(t, generate());
    }

    #[test]
    fn shape_check_rejects_the_daemon_token_and_garbage() {
        // The daemon's own API token is bare hex with no prefix. Telling the
        // two apart by shape is what keeps `Authorization: Bearer` unambiguous.
        assert!(!looks_like_app_token(&"a".repeat(64)));
        assert!(!looks_like_app_token("sca_short"));
        assert!(!looks_like_app_token(&format!(
            "{TOKEN_PREFIX}{}",
            "z".repeat(64)
        )));
        assert!(!looks_like_app_token(""));
    }

    #[test]
    fn ensure_is_idempotent_and_scoped_per_app() {
        let (db, _guard) = test_db();
        let a1 = ensure(&db, "alpha").unwrap();
        let a2 = ensure(&db, "alpha").unwrap();
        let b = ensure(&db, "beta").unwrap();
        assert_eq!(a1, a2, "a second launch must not change the token");
        assert_ne!(a1, b);
        assert_eq!(resolve(&db, &a1).as_deref(), Some("alpha"));
        assert_eq!(resolve(&db, &b).as_deref(), Some("beta"));
    }

    #[test]
    fn a_token_never_verifies_against_another_app() {
        let (db, _guard) = test_db();
        let alpha = ensure(&db, "alpha").unwrap();
        ensure(&db, "beta").unwrap();
        assert!(verify(&db, "alpha", &alpha));
        // The whole point of the feature: alpha's token is not beta's.
        assert!(!verify(&db, "beta", &alpha));
        assert!(!verify(&db, "alpha", "sca_deadbeef"));
    }

    #[test]
    fn rotate_invalidates_the_old_token() {
        let (db, _guard) = test_db();
        let old = ensure(&db, "alpha").unwrap();
        let new = rotate(&db, "alpha").unwrap();
        assert_ne!(old, new);
        assert!(!verify(&db, "alpha", &old));
        assert!(verify(&db, "alpha", &new));
        assert_eq!(
            resolve(&db, &old),
            None,
            "the cache must not keep serving it"
        );
        assert_eq!(resolve(&db, &new).as_deref(), Some("alpha"));
        // The reverse map feeds the proxy, which stamps this on every forward.
        // Left stale, the daemon would keep handing apps a token it no longer
        // accepts itself.
        assert_eq!(load(&db, "alpha").unwrap().as_deref(), Some(new.as_str()));
    }

    #[test]
    fn revoke_forgets_the_app() {
        let (db, _guard) = test_db();
        let t = ensure(&db, "alpha").unwrap();
        revoke(&db, "alpha").unwrap();
        assert_eq!(resolve(&db, &t), None);
        assert!(load(&db, "alpha").unwrap().is_none());
        // Reinstalling under the same id mints a different secret, so a token
        // captured from the previous install is worthless.
        assert_ne!(ensure(&db, "alpha").unwrap(), t);
    }

    #[test]
    fn launch_env_carries_both_variables() {
        let (db, _guard) = test_db();
        let env = launch_env(&db, "alpha", API_VERSION);
        let map: HashMap<_, _> = env.into_iter().collect();
        assert_eq!(map.get(ENV_API_VERSION).map(String::as_str), Some("2"));
        let token = map.get(ENV_APP_TOKEN).expect("token in env");
        assert!(verify(&db, "alpha", token));
    }

    #[test]
    fn mode_parsing_reads_what_the_operator_wrote() {
        assert_eq!(TokenMode::from_env_value("strict"), TokenMode::Strict);
        assert_eq!(TokenMode::from_env_value("  STRICT "), TokenMode::Strict);
        assert_eq!(TokenMode::from_env_value("warn"), TokenMode::Warn);
        assert_eq!(TokenMode::from_env_value("off"), TokenMode::Off);
        assert_eq!(TokenMode::from_env_value("disabled"), TokenMode::Off);
    }

    #[test]
    fn an_unset_or_misspelled_mode_still_requires_the_token() {
        // Unset is the fleet's normal state — it must be the safe end.
        assert_eq!(TokenMode::from_env_value(""), TokenMode::Strict);
        assert_eq!(TokenMode::from_env_value("   "), TokenMode::Strict);
        // And a typo must NOT read as "turn app isolation off". `off` has to be
        // spelled correctly to disable enforcement; anything else keeps it on.
        assert_eq!(TokenMode::from_env_value("of"), TokenMode::Strict);
        assert_eq!(TokenMode::from_env_value("no"), TokenMode::Strict);
        assert_eq!(TokenMode::from_env_value("yes-please"), TokenMode::Strict);
        assert_eq!(TokenMode::parse_opt("of"), None);
    }

    #[test]
    fn the_ui_choice_wins_over_the_environment_and_can_be_handed_back() {
        let (db, _guard) = test_db();
        mode_cache_clear();

        // Nothing chosen: the environment decides, and the UI is told whether
        // that was a real setting or just the built-in default.
        assert_eq!(
            effective_mode(&db, TokenMode::Warn, true),
            (TokenMode::Warn, ModeSource::Env)
        );
        assert_eq!(
            effective_mode(&db, DEFAULT_TOKEN_MODE, false),
            (DEFAULT_TOKEN_MODE, ModeSource::Default)
        );

        // The operator turns enforcement off from the UI — it must win even
        // over an explicit env var, or the switch would appear to do nothing on
        // a machine that sets one.
        set_mode_override(&db, Some(TokenMode::Off)).unwrap();
        assert_eq!(
            effective_mode(&db, TokenMode::Strict, true),
            (TokenMode::Off, ModeSource::Ui)
        );

        // And handing the decision back restores the environment's answer
        // rather than freezing the last choice.
        set_mode_override(&db, None).unwrap();
        assert_eq!(
            effective_mode(&db, TokenMode::Strict, true),
            (TokenMode::Strict, ModeSource::Env)
        );
    }

    #[test]
    fn a_stored_choice_survives_a_restart() {
        let (db, _guard) = test_db();
        mode_cache_clear();
        set_mode_override(&db, Some(TokenMode::Warn)).unwrap();
        // A restart is a cold cache reading the same row back.
        mode_cache_clear();
        assert_eq!(mode_override(&db), Some(TokenMode::Warn));
    }

    #[test]
    fn a_corrupted_stored_value_falls_back_rather_than_disabling() {
        let (db, _guard) = test_db();
        mode_cache_clear();
        db.set_router_state(MODE_KEY, "gibberish").unwrap();
        // Unreadable must mean "no choice recorded", so the environment/default
        // applies. Reading it as Off would let a bad write silently switch app
        // isolation off.
        assert_eq!(mode_override(&db), None);
        assert_eq!(
            effective_mode(&db, DEFAULT_TOKEN_MODE, false).0,
            DEFAULT_TOKEN_MODE
        );
    }

    #[test]
    fn constant_time_compare_still_compares() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "abcd"));
    }
}
