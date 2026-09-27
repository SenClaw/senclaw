//! Persistence for OAuth accounts.
//!
//! Deliberately a *separate* file from `config.json`. `GET /api/llm-config`
//! echoes stored config back verbatim and the daemon runs a permissive CORS
//! layer, so anything living in `config.json` is readable by any page the user
//! happens to have open. An API key leaking that way is bad; a refresh token
//! leaking that way hands over the whole subscription account. So tokens live
//! here, the file is 0600, and nothing in this module is serialised to an HTTP
//! response without going through [`RedactedAccount`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One signed-in account for one provider. A user may hold several per
/// provider (e.g. work and personal Claude).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OauthAccount {
    pub id: String,
    /// Provider id from [`crate::providers::oauth::provider`].
    pub provider: String,
    /// User-visible name; the account email when we can discover one.
    pub label: String,

    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(
        rename = "refreshToken",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub refresh_token: Option<String>,
    /// Unix seconds. `None` means the provider didn't say, so we never
    /// pre-emptively refresh and rely on the 401 retry instead.
    #[serde(rename = "expiresAt", default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,

    /// Provider-specific data that the transport needs later — e.g. the
    /// Antigravity Code Assist project id, or the Codex account id parsed out
    /// of the id_token.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,

    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(
        rename = "lastRefreshAt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub last_refresh_at: Option<i64>,
    /// Last refresh failure, surfaced in the UI so a dead account is obvious
    /// instead of failing every chat silently.
    #[serde(rename = "lastError", default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl OauthAccount {
    /// Seconds until expiry; `None` when the provider gave no expiry.
    pub fn expires_in(&self, now: i64) -> Option<i64> {
        self.expires_at.map(|e| e - now)
    }

    /// True when the token is past (or within `lead_secs` of) expiry and should
    /// be refreshed before the next call.
    pub fn needs_refresh(&self, now: i64, lead_secs: i64) -> bool {
        match self.expires_at {
            Some(exp) => now + lead_secs >= exp,
            // No expiry advertised — refresh reactively on 401, not on a clock.
            None => false,
        }
    }

    /// Already unusable. Distinct from [`Self::needs_refresh`], which is the
    /// proactive window.
    pub fn is_expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|exp| now >= exp)
    }

    /// The safe projection for HTTP responses.
    pub fn redact(&self, now: i64) -> RedactedAccount {
        RedactedAccount {
            id: self.id.clone(),
            provider: self.provider.clone(),
            label: self.label.clone(),
            email: self.email.clone(),
            expires_at: self.expires_at,
            expires_in: self.expires_in(now),
            expired: self.is_expired(now),
            has_refresh_token: self.refresh_token.is_some(),
            scope: self.scope.clone(),
            created_at: self.created_at,
            last_refresh_at: self.last_refresh_at,
            last_error: self.last_error.clone(),
        }
    }
}

/// What the REST layer is allowed to return. Contains no token material.
#[derive(Debug, Clone, Serialize)]
pub struct RedactedAccount {
    pub id: String,
    pub provider: String,
    pub label: String,
    pub email: Option<String>,
    #[serde(rename = "expiresAt")]
    pub expires_at: Option<i64>,
    #[serde(rename = "expiresIn")]
    pub expires_in: Option<i64>,
    pub expired: bool,
    #[serde(rename = "hasRefreshToken")]
    pub has_refresh_token: bool,
    pub scope: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "lastRefreshAt")]
    pub last_refresh_at: Option<i64>,
    #[serde(rename = "lastError")]
    pub last_error: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    accounts: Vec<OauthAccount>,
}

/// Read every account from `path`. A missing file is an empty store, not an
/// error — first run is the common case.
pub fn load(path: &Path) -> Result<Vec<OauthAccount>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read oauth store {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let parsed: StoreFile = serde_json::from_str(&raw)
        .with_context(|| format!("parse oauth store {}", path.display()))?;
    Ok(parsed.accounts)
}

/// Write every account to `path` atomically, owner-read/write only.
///
/// Atomic because a torn write here loses the refresh token, which means the
/// user has to re-authorise every provider by hand.
pub fn save(path: &Path, accounts: &[OauthAccount]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create oauth store dir {}", parent.display()))?;
    }

    let body = serde_json::to_string_pretty(&StoreFile {
        accounts: accounts.to_vec(),
    })?;

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("write {}", tmp.display()))?;
    restrict_permissions(&tmp)?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    // Re-apply after the rename: on some filesystems the destination inode
    // keeps its own mode if the file already existed.
    restrict_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    // Windows inherits the user profile ACL, which is already user-scoped.
    Ok(())
}

/// Default location: alongside `config.json` under the SenClaw home.
pub fn default_path(global_config_path: &Path) -> PathBuf {
    global_config_path
        .parent()
        .map(|p| p.join("oauth.json"))
        .unwrap_or_else(|| PathBuf::from("oauth.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str, expires_at: Option<i64>) -> OauthAccount {
        OauthAccount {
            id: id.to_string(),
            provider: "claude".into(),
            label: "test".into(),
            access_token: "secret-access".into(),
            refresh_token: Some("secret-refresh".into()),
            expires_at,
            scope: Some("user:inference".into()),
            email: Some("a@b.c".into()),
            extra: serde_json::Map::new(),
            created_at: 1_000,
            last_refresh_at: None,
            last_error: None,
        }
    }

    #[test]
    fn missing_file_loads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("oauth.json");
        assert!(load(&p).unwrap().is_empty());
    }

    #[test]
    fn empty_file_loads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("oauth.json");
        std::fs::write(&p, "   \n").unwrap();
        assert!(load(&p).unwrap().is_empty());
    }

    #[test]
    fn round_trips_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("oauth.json");
        let accounts = vec![account("a1", Some(5_000)), account("a2", None)];
        save(&p, &accounts).unwrap();

        let back = load(&p).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].id, "a1");
        assert_eq!(back[0].access_token, "secret-access");
        assert_eq!(back[0].refresh_token.as_deref(), Some("secret-refresh"));
        assert_eq!(back[1].expires_at, None);
    }

    #[test]
    fn save_creates_missing_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nested").join("deeper").join("oauth.json");
        save(&p, &[account("a1", None)]).unwrap();
        assert!(p.exists());
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("oauth.json");
        save(&p, &[account("a1", None)]).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "got {:o}", mode & 0o777);
    }

    #[cfg(unix)]
    #[test]
    fn overwriting_an_existing_loose_file_still_ends_up_locked_down() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("oauth.json");
        std::fs::write(&p, "{}").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

        save(&p, &[account("a1", None)]).unwrap();

        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "got {:o}", mode & 0o777);
    }

    #[test]
    fn no_temp_file_survives_a_save() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("oauth.json");
        save(&p, &[account("a1", None)]).unwrap();
        assert!(!p.with_extension("json.tmp").exists());
    }

    #[test]
    fn redaction_drops_all_token_material() {
        let acc = account("a1", Some(5_000));
        let red = acc.redact(4_000);
        let json = serde_json::to_string(&red).unwrap();
        assert!(!json.contains("secret-access"), "{json}");
        assert!(!json.contains("secret-refresh"), "{json}");
        assert!(red.has_refresh_token);
        assert_eq!(red.expires_in, Some(1_000));
        assert!(!red.expired);
    }

    #[test]
    fn expiry_helpers_track_the_clock() {
        let acc = account("a1", Some(5_000));
        assert!(!acc.is_expired(4_999));
        assert!(acc.is_expired(5_000));
        assert!(acc.is_expired(5_001));

        // Inside the lead window but not yet expired.
        assert!(acc.needs_refresh(4_500, 600));
        assert!(!acc.needs_refresh(4_000, 600));
    }

    #[test]
    fn an_account_without_expiry_is_never_proactively_refreshed() {
        let acc = account("a1", None);
        assert!(!acc.needs_refresh(i64::MAX / 2, 3_600));
        assert!(!acc.is_expired(i64::MAX / 2));
        assert_eq!(acc.expires_in(1_000), None);
    }

    #[test]
    fn default_path_sits_next_to_config_json() {
        let p = default_path(Path::new("/home/u/.senclaw/config.json"));
        assert_eq!(p, PathBuf::from("/home/u/.senclaw/oauth.json"));
    }
}
