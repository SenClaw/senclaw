//! Update-checking for installed Space Apps against the senclaw hub *package
//! registry* (`/api/v1/packages/...`, see [`super::registry`]).
//!
//! An install records where it came from so a later run can ask "is there a
//! newer version?". Two facts are needed and neither was persisted before this
//! feature existed:
//!
//! * the **slug** (`<scope>/<name>`) that names the hub package, and
//! * the **installed version**.
//!
//! Going forward both are stamped into the stored manifest under `manifest.hub`
//! by the install/update path. For apps installed before that stamp existed we
//! fall back to a derived slug — a published app's package *name is its app id*
//! under the `senclaw` scope (see `senclaw hub publish`, which uploads
//! `name: &app.id`) — and an unknown installed version, which simply means the
//! hub's latest is always offered.
//!
//! Local (dev) apps and apps with a synthetic `space-app-<uuid>` id have no hub
//! origin and are skipped: they were never installed from the registry.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::registry::{self, PackageDoc};

/// Provenance stamped into a Space App manifest at install/update time, under
/// `manifest["hub"]`. Serialized camelCase to match the rest of the manifest.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HubOrigin {
    pub scope: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<i64>,
}

impl HubOrigin {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.scope, self.name)
    }
}

/// Where an installed app came from, if it plausibly came from the hub.
///
/// Precedence: an explicit `manifest.hub` stamp wins; otherwise, for a
/// zip-installed app with a real id, derive `senclaw/<id>` with an unknown
/// version. Returns `None` for local installs and synthetic ids.
pub fn origin_from_manifest(manifest: &Value, app_id: &str) -> Option<HubOrigin> {
    if let Some(hub) = manifest.get("hub") {
        if let Ok(o) = serde_json::from_value::<HubOrigin>(hub.clone()) {
            if !o.name.is_empty() {
                return Some(o);
            }
        }
    }

    // A hand-registered local dev app is not a registry package.
    if manifest
        .get("install")
        .and_then(|i| i.get("type"))
        .and_then(|t| t.as_str())
        == Some("local")
    {
        return None;
    }
    // A synthetic id means the zip carried no id — nothing to look up.
    if app_id.is_empty() || app_id.starts_with("space-app-") {
        return None;
    }

    Some(HubOrigin {
        scope: registry::DEFAULT_SCOPE.to_string(),
        name: app_id.to_string(),
        // Prefer a version the manifest itself carries, if any.
        version: manifest
            .get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        ..Default::default()
    })
}

/// Parse the numeric `X.Y.Z` core of a semver, ignoring any pre-release/build.
fn parse_ver(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().split(['-', '+']).next()?;
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut it = parts.iter().map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

/// Is `latest` a version worth offering over `installed`?
///
/// An unknown installed version means the origin was never stamped, so the
/// hub's latest is offered. Two semvers compare numerically; anything that does
/// not parse falls back to "offer when the strings differ" so a non-semver bump
/// is still surfaced rather than silently hidden.
pub fn is_newer(latest: &str, installed: Option<&str>) -> bool {
    match installed {
        None => true,
        Some(cur) => match (parse_ver(latest), parse_ver(cur)) {
            (Some(l), Some(c)) => l > c,
            _ => latest.trim() != cur.trim(),
        },
    }
}

/// The result of checking one installed app against the hub.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    /// The app id as installed.
    pub id: String,
    /// `<scope>/<name>` on the hub.
    pub slug: String,
    /// The version currently installed, if it was recorded.
    pub installed: Option<String>,
    /// The hub's `latest` dist-tag, if the package resolved.
    pub latest: Option<String>,
    pub has_update: bool,
    /// The latest version is yanked — surfaced, never auto-applied.
    #[serde(default)]
    pub yanked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<String>,
    /// Why the check could not complete (e.g. not on the hub). Not an error the
    /// caller must handle — an app can legitimately be off-registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Turn a resolved package document into a status for one installed app.
fn status_from_pkg(id: &str, origin: &HubOrigin, pkg: &PackageDoc) -> UpdateStatus {
    let installed = origin.version.clone();
    match pkg.latest() {
        Some(v) => UpdateStatus {
            id: id.to_string(),
            slug: origin.slug(),
            installed: installed.clone(),
            latest: Some(v.version.clone()),
            has_update: !v.yanked && is_newer(&v.version, installed.as_deref()),
            yanked: v.yanked,
            deprecated: v.deprecated.clone(),
            error: None,
        },
        None => UpdateStatus {
            id: id.to_string(),
            slug: origin.slug(),
            installed,
            latest: None,
            has_update: false,
            yanked: false,
            deprecated: None,
            error: Some("gói chưa publish phiên bản nào".to_string()),
        },
    }
}

/// Check every hub-originating app in `apps` (each `(id, manifest)`) against the
/// hub. Apps with no hub origin are skipped (not reported). Network/lookup
/// failures become a per-app `error`, never aborting the whole sweep.
pub async fn check_updates(apps: &[(String, Value)], hub: &str) -> Vec<UpdateStatus> {
    let mut out = Vec::new();
    for (id, manifest) in apps {
        let Some(origin) = origin_from_manifest(manifest, id) else {
            continue;
        };
        let status = match registry::fetch_package(hub, &origin.scope, &origin.name).await {
            Ok(pkg) => status_from_pkg(id, &origin, &pkg),
            Err(e) => UpdateStatus {
                id: id.clone(),
                slug: origin.slug(),
                installed: origin.version.clone(),
                latest: None,
                has_update: false,
                yanked: false,
                deprecated: None,
                error: Some(e.to_string()),
            },
        };
        out.push(status);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn newer_compares_semver_numerically_not_lexically() {
        assert!(is_newer("1.10.0", Some("1.9.0"))); // 10 > 9 despite string order
        assert!(is_newer("2.0.0", Some("1.9.9")));
        assert!(!is_newer("1.0.0", Some("1.0.0")));
        assert!(!is_newer("1.0.0", Some("1.2.0")));
    }

    #[test]
    fn unknown_installed_version_always_offers_latest() {
        assert!(is_newer("1.0.0", None));
    }

    #[test]
    fn non_semver_offered_only_when_the_string_changes() {
        assert!(is_newer("2026-07-01", Some("2026-06-01")));
        assert!(!is_newer("nightly", Some("nightly")));
    }

    #[test]
    fn explicit_hub_stamp_wins() {
        let m = json!({
            "id": "ai-office",
            "hub": { "scope": "acme", "name": "office", "version": "1.2.3" }
        });
        let o = origin_from_manifest(&m, "ai-office").unwrap();
        assert_eq!(o.slug(), "acme/office");
        assert_eq!(o.version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn derives_senclaw_slug_from_app_id_for_unstamped_zip_installs() {
        let m = json!({ "id": "ai-office", "install": { "type": "zip" } });
        let o = origin_from_manifest(&m, "ai-office").unwrap();
        assert_eq!(o.slug(), "senclaw/ai-office");
        assert_eq!(o.version, None);
    }

    #[test]
    fn local_installs_and_synthetic_ids_have_no_hub_origin() {
        let local = json!({ "id": "dev", "install": { "type": "local" } });
        assert!(origin_from_manifest(&local, "dev").is_none());
        let synthetic = json!({ "id": "space-app-abc" });
        assert!(origin_from_manifest(&synthetic, "space-app-abc").is_none());
    }
}
