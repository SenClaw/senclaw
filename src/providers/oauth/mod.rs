//! OAuth sign-in for subscription-backed LLM providers.
//!
//! SenClaw normally talks to an LLM with an API key held in `config.json`.
//! This module adds the other shape: an OAuth account (Claude Code, OpenAI
//! Codex, Google Antigravity) whose access token is short-lived and has to be
//! refreshed. Tokens never enter `config.json` — an `LlmConfig` only stores the
//! *id* of an account, and the material lives in `oauth.json` (0600) behind
//! [`store`].
//!
//! Layout:
//! - [`pkce`] — verifier/challenge/state primitives
//! - [`provider`] — static per-provider constants
//! - [`store`] — on-disk accounts + the redacted projection for HTTP
//! - [`token`] — code exchange and refresh calls
//! - [`flow`] — browser sign-in with a loopback callback
//! - [`discovery`] — asking a provider which models the account may use
//! - [`transport`] — turning an account into request headers for the LLM layer
//!
//! ## A note on identity
//!
//! Requests built here identify themselves as SenClaw. We deliberately do not
//! reproduce the vendor-client fingerprints (spoofed User-Agent, SDK telemetry
//! headers, synthetic device/session ids, decoy tool declarations) that other
//! routers use to make third-party traffic indistinguishable from the vendor's
//! own client. Those exist to defeat detection, and shipping them would make
//! every SenClaw install complicit in that. The consequence is honest: a
//! provider that chooses to reject non-first-party clients will reject us, and
//! the user sees a clear error instead of a silent ban later.

pub mod discovery;
pub mod flow;
pub mod pkce;
pub mod provider;
pub mod store;
pub mod token;
pub mod transport;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use tokio::sync::Mutex as AsyncMutex;

use flow::FlowState;
use provider::OauthProviderDef;
use store::{OauthAccount, RedactedAccount};
use token::{RefreshFailure, TokenResponse};

/// How often the background task looks for tokens approaching expiry.
const REFRESH_TICK: Duration = Duration::from_secs(60);

/// Owns the account store, the HTTP client, and in-flight sign-in state.
pub struct OauthManager {
    path: PathBuf,
    accounts: RwLock<Vec<OauthAccount>>,
    flows: RwLock<HashMap<String, FlowState>>,
    /// Serialises refreshes per account so a burst of concurrent 401s produces
    /// one refresh, not N racing ones that invalidate each other's tokens.
    refresh_locks: RwLock<HashMap<String, Arc<AsyncMutex<()>>>>,
    pub(crate) http: reqwest::Client,
}

impl OauthManager {
    /// Open (or start) the store at `path`.
    pub fn new(path: PathBuf) -> Self {
        let accounts = store::load(&path).unwrap_or_else(|e| {
            safe_eprintln!("[oauth] could not read {}: {e}", path.display());
            Vec::new()
        });
        Self {
            path,
            accounts: RwLock::new(accounts),
            flows: RwLock::new(HashMap::new()),
            refresh_locks: RwLock::new(HashMap::new()),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
        }
    }

    fn now() -> i64 {
        chrono::Utc::now().timestamp()
    }

    // ----- account access -------------------------------------------------

    /// Every account, with all token material stripped. Safe for HTTP.
    pub fn accounts_redacted(&self) -> Vec<RedactedAccount> {
        let now = Self::now();
        self.accounts
            .read()
            .map(|a| a.iter().map(|acc| acc.redact(now)).collect())
            .unwrap_or_default()
    }

    /// Full account record, tokens included. Callers must not serialise this
    /// into a response.
    pub fn account(&self, id: &str) -> Option<OauthAccount> {
        self.accounts
            .read()
            .ok()?
            .iter()
            .find(|a| a.id == id)
            .cloned()
    }

    /// True when at least one account exists for `provider`.
    pub fn has_provider(&self, provider: &str) -> bool {
        self.accounts
            .read()
            .map(|a| a.iter().any(|acc| acc.provider == provider))
            .unwrap_or(false)
    }

    /// Current access token without touching the network.
    ///
    /// Exists because `ZenEngine::resolve_model_profile_with` is synchronous:
    /// it cannot await a refresh. A token that has gone stale between ticks is
    /// caught by the 401 retry in the transport layer instead.
    pub fn access_token(&self, account_id: &str) -> Option<String> {
        self.account(account_id).map(|a| a.access_token)
    }

    // ----- mutation -------------------------------------------------------

    fn persist(&self) -> Result<()> {
        let snapshot = self
            .accounts
            .read()
            .map_err(|_| anyhow!("oauth account lock poisoned"))?
            .clone();
        store::save(&self.path, &snapshot)
    }

    /// Insert or replace an account, then flush to disk.
    pub fn upsert(&self, account: OauthAccount) -> Result<()> {
        {
            let mut accounts = self
                .accounts
                .write()
                .map_err(|_| anyhow!("oauth account lock poisoned"))?;
            match accounts.iter_mut().find(|a| a.id == account.id) {
                Some(existing) => *existing = account,
                None => accounts.push(account),
            }
        }
        self.persist()
    }

    /// Forget an account. Returns whether anything was removed.
    pub fn remove(&self, account_id: &str) -> Result<bool> {
        let removed = {
            let mut accounts = self
                .accounts
                .write()
                .map_err(|_| anyhow!("oauth account lock poisoned"))?;
            let before = accounts.len();
            accounts.retain(|a| a.id != account_id);
            before != accounts.len()
        };
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    /// Store a provider-specific value on an account (e.g. a discovered
    /// project id) without disturbing its tokens.
    pub fn set_extra(&self, account_id: &str, key: &str, value: serde_json::Value) -> Result<()> {
        {
            let mut accounts = self
                .accounts
                .write()
                .map_err(|_| anyhow!("oauth account lock poisoned"))?;
            let Some(acc) = accounts.iter_mut().find(|a| a.id == account_id) else {
                bail!("no such OAuth account `{account_id}`");
            };
            acc.extra.insert(key.to_string(), value);
        }
        self.persist()
    }

    // ----- sign-in --------------------------------------------------------

    /// Turn a fresh token response into a stored account.
    ///
    /// Re-signing in to an account we already hold updates it in place, keyed
    /// on provider + email, so repeat sign-ins don't pile up duplicates.
    pub async fn adopt_tokens(
        &self,
        def: &OauthProviderDef,
        tokens: TokenResponse,
    ) -> Result<OauthAccount> {
        let now = Self::now();
        let email = self.discover_email(def, &tokens).await;

        let existing = {
            let accounts = self
                .accounts
                .read()
                .map_err(|_| anyhow!("oauth account lock poisoned"))?;
            accounts
                .iter()
                .find(|a| a.provider == def.id && a.email.is_some() && a.email == email)
                .cloned()
        };

        let label = match &email {
            Some(e) => format!("{} ({e})", def.display_name),
            None => def.display_name.to_string(),
        };

        let account = OauthAccount {
            id: existing
                .as_ref()
                .map(|a| a.id.clone())
                .unwrap_or_else(|| format!("oauth_{}_{}", def.id, now)),
            provider: def.id.to_string(),
            label,
            access_token: tokens.access_token,
            // A refresh response often omits the refresh token, meaning "keep
            // using the one you have". Losing it would force a manual re-login.
            refresh_token: tokens
                .refresh_token
                .or_else(|| existing.as_ref().and_then(|a| a.refresh_token.clone())),
            expires_at: tokens.expires_in.map(|s| now + s),
            scope: tokens.scope,
            email,
            extra: existing
                .as_ref()
                .map(|a| a.extra.clone())
                .unwrap_or_default(),
            created_at: existing.as_ref().map(|a| a.created_at).unwrap_or(now),
            last_refresh_at: Some(now),
            last_error: None,
        };

        self.upsert(account.clone())?;
        Ok(account)
    }

    /// Best-effort account labelling. A failure here costs a nice label, not
    /// the sign-in, so every path degrades to `None`.
    async fn discover_email(
        &self,
        def: &OauthProviderDef,
        tokens: &TokenResponse,
    ) -> Option<String> {
        if let Some(id_token) = tokens.id_token.as_deref() {
            if let Some(email) = token::email_from_id_token(id_token) {
                return Some(email);
            }
        }
        if def.id == "antigravity" {
            let resp = self
                .http
                .get(provider::GOOGLE_USERINFO_URL)
                .bearer_auth(&tokens.access_token)
                .send()
                .await
                .ok()?;
            if !resp.status().is_success() {
                return None;
            }
            let json: serde_json::Value = resp.json().await.ok()?;
            return json
                .get("email")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
        None
    }

    // ----- refresh --------------------------------------------------------

    fn refresh_lock(&self, account_id: &str) -> Arc<AsyncMutex<()>> {
        if let Ok(locks) = self.refresh_locks.read() {
            if let Some(l) = locks.get(account_id) {
                return Arc::clone(l);
            }
        }
        let lock = Arc::new(AsyncMutex::new(()));
        if let Ok(mut locks) = self.refresh_locks.write() {
            return Arc::clone(locks.entry(account_id.to_string()).or_insert(lock));
        }
        lock
    }

    /// Refresh one account now, regardless of how much life its token has left.
    pub async fn refresh_account(&self, account_id: &str) -> Result<()> {
        let lock = self.refresh_lock(account_id);
        let _guard = lock.lock().await;

        let account = self
            .account(account_id)
            .ok_or_else(|| anyhow!("no such OAuth account `{account_id}`"))?;
        let def = provider::get(&account.provider)
            .ok_or_else(|| anyhow!("unknown provider `{}`", account.provider))?;
        let Some(refresh_token) = account.refresh_token.clone() else {
            bail!("{} has no refresh token — sign in again", account.label);
        };

        match token::refresh(&self.http, def, &refresh_token).await {
            Ok(tokens) => {
                self.adopt_tokens(def, tokens).await?;
                Ok(())
            }
            Err(e) => {
                let message = e.to_string();
                let verdict = classify_error(&message);
                // Record the failure so the UI can show a dead account rather
                // than letting every chat fail with an opaque 401.
                let note = match verdict {
                    RefreshFailure::Unrecoverable => {
                        format!("sign in again — refresh rejected: {message}")
                    }
                    RefreshFailure::Transient => message.clone(),
                };
                let _ = self.mark_error(account_id, &note);
                Err(e)
            }
        }
    }

    /// Refresh only if the token is inside the provider's lead window.
    /// Returns the token that should be used for the next call.
    pub async fn ensure_fresh(&self, account_id: &str) -> Result<String> {
        let account = self
            .account(account_id)
            .ok_or_else(|| anyhow!("no such OAuth account `{account_id}`"))?;
        let def = provider::get(&account.provider)
            .ok_or_else(|| anyhow!("unknown provider `{}`", account.provider))?;

        if account.needs_refresh(Self::now(), def.refresh_lead_secs) {
            self.refresh_account(account_id).await?;
        }
        self.access_token(account_id)
            .ok_or_else(|| anyhow!("account `{account_id}` vanished during refresh"))
    }

    fn mark_error(&self, account_id: &str, message: &str) -> Result<()> {
        {
            let mut accounts = self
                .accounts
                .write()
                .map_err(|_| anyhow!("oauth account lock poisoned"))?;
            if let Some(acc) = accounts.iter_mut().find(|a| a.id == account_id) {
                acc.last_error = Some(message.to_string());
            }
        }
        self.persist()
    }

    /// Account ids whose tokens are inside the refresh window right now.
    pub fn accounts_due_for_refresh(&self) -> Vec<String> {
        let now = Self::now();
        self.accounts
            .read()
            .map(|accounts| {
                accounts
                    .iter()
                    .filter(|a| {
                        a.refresh_token.is_some()
                            && provider::get(&a.provider)
                                .is_some_and(|def| a.needs_refresh(now, def.refresh_lead_secs))
                    })
                    .map(|a| a.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    // ----- flow bookkeeping ----------------------------------------------

    pub(crate) fn set_flow_state(&self, flow_id: &str, state: FlowState) {
        if let Ok(mut flows) = self.flows.write() {
            flows.insert(flow_id.to_string(), state);
        }
    }

    /// Poll an in-flight sign-in.
    pub fn flow_state(&self, flow_id: &str) -> Option<FlowState> {
        self.flows.read().ok()?.get(flow_id).cloned()
    }
}

/// Map a refresh error string onto a verdict. The HTTP status is embedded in
/// the message by [`token::post_token`]'s error format.
fn classify_error(message: &str) -> RefreshFailure {
    let status = message
        .split('(')
        .nth(1)
        .and_then(|s| s.split(&[' ', ')'][..]).next())
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    token::classify_failure(status, message)
}

// ----- process-wide handle ------------------------------------------------

static GLOBAL: OnceLock<Arc<OauthManager>> = OnceLock::new();

/// Install the process-wide manager. Called once during daemon boot; later
/// calls keep the first instance so tests and re-entry stay consistent.
pub fn init(path: PathBuf) -> Arc<OauthManager> {
    Arc::clone(GLOBAL.get_or_init(|| Arc::new(OauthManager::new(path))))
}

/// The manager installed by [`init`], if the daemon has booted.
///
/// Returns `None` in unit tests and CLI subcommands that never call `init`,
/// which lets the LLM layer fall back to API-key behaviour cleanly.
pub fn global() -> Option<Arc<OauthManager>> {
    GLOBAL.get().map(Arc::clone)
}

/// Synchronous token lookup for the LLM config resolver.
pub fn access_token_for(account_id: &str) -> Option<String> {
    global()?.access_token(account_id)
}

/// Keep every stored token ahead of its expiry.
///
/// Proactive because the only other opportunity is the 401 retry, and a
/// mid-conversation refresh stalls a user-visible response.
pub fn spawn_background_refresher(manager: Arc<OauthManager>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(REFRESH_TICK);
        loop {
            ticker.tick().await;
            for account_id in manager.accounts_due_for_refresh() {
                if let Err(e) = manager.refresh_account(&account_id).await {
                    safe_eprintln!("[oauth] refresh failed for {account_id}: {e}");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> (OauthManager, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth.json");
        (OauthManager::new(path), dir)
    }

    fn account(id: &str, provider: &str, expires_at: Option<i64>) -> OauthAccount {
        OauthAccount {
            id: id.into(),
            provider: provider.into(),
            label: "acct".into(),
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at,
            scope: None,
            email: Some("dev@example.com".into()),
            extra: serde_json::Map::new(),
            created_at: 0,
            last_refresh_at: None,
            last_error: None,
        }
    }

    #[test]
    fn a_fresh_manager_is_empty() {
        let (m, _d) = manager();
        assert!(m.accounts_redacted().is_empty());
        assert!(m.account("nope").is_none());
        assert!(!m.has_provider("claude"));
    }

    #[test]
    fn upsert_then_read_back() {
        let (m, _d) = manager();
        m.upsert(account("a1", "claude", None)).unwrap();
        assert_eq!(m.accounts_redacted().len(), 1);
        assert_eq!(m.access_token("a1").as_deref(), Some("at"));
        assert!(m.has_provider("claude"));
    }

    #[test]
    fn upsert_replaces_rather_than_duplicates() {
        let (m, _d) = manager();
        m.upsert(account("a1", "claude", None)).unwrap();
        let mut updated = account("a1", "claude", None);
        updated.access_token = "at2".into();
        m.upsert(updated).unwrap();

        assert_eq!(m.accounts_redacted().len(), 1);
        assert_eq!(m.access_token("a1").as_deref(), Some("at2"));
    }

    #[test]
    fn accounts_survive_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth.json");
        {
            let m = OauthManager::new(path.clone());
            m.upsert(account("a1", "codex", Some(999))).unwrap();
        }
        let reopened = OauthManager::new(path);
        assert_eq!(reopened.accounts_redacted().len(), 1);
        assert_eq!(reopened.access_token("a1").as_deref(), Some("at"));
    }

    #[test]
    fn remove_reports_whether_it_did_anything() {
        let (m, _d) = manager();
        m.upsert(account("a1", "claude", None)).unwrap();
        assert!(m.remove("a1").unwrap());
        assert!(!m.remove("a1").unwrap());
        assert!(m.accounts_redacted().is_empty());
    }

    #[test]
    fn set_extra_keeps_tokens_intact() {
        let (m, _d) = manager();
        m.upsert(account("a1", "antigravity", None)).unwrap();
        m.set_extra("a1", "projectId", serde_json::json!("proj-7"))
            .unwrap();

        let acc = m.account("a1").unwrap();
        assert_eq!(acc.extra.get("projectId").unwrap(), "proj-7");
        assert_eq!(acc.access_token, "at");
        assert_eq!(acc.refresh_token.as_deref(), Some("rt"));
    }

    #[test]
    fn set_extra_on_a_missing_account_is_an_error() {
        let (m, _d) = manager();
        assert!(m.set_extra("ghost", "k", serde_json::json!(1)).is_err());
    }

    #[test]
    fn due_for_refresh_respects_the_provider_lead_window() {
        let (m, _d) = manager();
        let now = OauthManager::now();

        // Claude's lead is 4h — expiring in 1h is inside the window.
        m.upsert(account("soon", "claude", Some(now + 3_600)))
            .unwrap();
        // Well outside any lead window.
        m.upsert(account("later", "claude", Some(now + 60 * 60 * 24)))
            .unwrap();
        // No expiry advertised — never proactively refreshed.
        m.upsert(account("unknown", "claude", None)).unwrap();

        let due = m.accounts_due_for_refresh();
        assert!(due.contains(&"soon".to_string()), "{due:?}");
        assert!(!due.contains(&"later".to_string()), "{due:?}");
        assert!(!due.contains(&"unknown".to_string()), "{due:?}");
    }

    #[test]
    fn an_account_without_a_refresh_token_is_never_scheduled() {
        let (m, _d) = manager();
        let now = OauthManager::now();
        let mut acc = account("a1", "claude", Some(now + 60));
        acc.refresh_token = None;
        m.upsert(acc).unwrap();
        assert!(m.accounts_due_for_refresh().is_empty());
    }

    #[test]
    fn an_unknown_provider_is_never_scheduled() {
        let (m, _d) = manager();
        let now = OauthManager::now();
        m.upsert(account("a1", "not-a-provider", Some(now + 1)))
            .unwrap();
        assert!(m.accounts_due_for_refresh().is_empty());
    }

    #[tokio::test]
    async fn refreshing_an_account_with_no_refresh_token_explains_itself() {
        let (m, _d) = manager();
        let mut acc = account("a1", "claude", None);
        acc.refresh_token = None;
        m.upsert(acc).unwrap();

        let err = m.refresh_account("a1").await.unwrap_err().to_string();
        assert!(err.contains("sign in again"), "{err}");
    }

    #[tokio::test]
    async fn refreshing_an_unknown_account_is_an_error() {
        let (m, _d) = manager();
        assert!(m.refresh_account("ghost").await.is_err());
        assert!(m.ensure_fresh("ghost").await.is_err());
    }

    #[tokio::test]
    async fn ensure_fresh_skips_the_network_for_a_healthy_token() {
        let (m, _d) = manager();
        let now = OauthManager::now();
        // Expiry far outside the lead window: no refresh attempt, so no
        // network call — which is what makes this test deterministic offline.
        m.upsert(account("a1", "claude", Some(now + 60 * 60 * 24)))
            .unwrap();
        assert_eq!(m.ensure_fresh("a1").await.unwrap(), "at");
    }

    #[test]
    fn flow_state_round_trips() {
        let (m, _d) = manager();
        assert!(m.flow_state("f1").is_none());
        m.set_flow_state("f1", FlowState::Pending);
        assert!(matches!(m.flow_state("f1"), Some(FlowState::Pending)));
        m.set_flow_state(
            "f1",
            FlowState::Completed {
                account_id: "a1".into(),
                label: "l".into(),
            },
        );
        assert!(matches!(
            m.flow_state("f1"),
            Some(FlowState::Completed { .. })
        ));
    }

    #[test]
    fn error_classification_reads_the_status_out_of_the_message() {
        assert_eq!(
            classify_error("claude refresh_token rejected (400 Bad Request): {}"),
            RefreshFailure::Unrecoverable
        );
        assert_eq!(
            classify_error("codex refresh_token rejected (503 Service Unavailable): busy"),
            RefreshFailure::Transient
        );
        // Body wins even when the status looks retryable.
        assert_eq!(
            classify_error("x rejected (500 Internal): invalid_grant"),
            RefreshFailure::Unrecoverable
        );
    }

    #[test]
    fn redacted_output_never_carries_tokens() {
        let (m, _d) = manager();
        m.upsert(account("a1", "claude", Some(1))).unwrap();
        let json = serde_json::to_string(&m.accounts_redacted()).unwrap();
        assert!(!json.contains("\"at\""), "{json}");
        assert!(!json.contains("\"rt\""), "{json}");
        assert!(json.contains("hasRefreshToken"));
    }

    #[test]
    fn a_corrupt_store_file_degrades_to_empty_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        let m = OauthManager::new(path);
        assert!(m.accounts_redacted().is_empty());
    }
}
