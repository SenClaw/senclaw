//! `/api/system/*` — what this daemon is and how it was installed.
//!
//! The desktop app and the web console show the running core's version from
//! here, and the desktop compares it with the core in its own bundle: a daemon
//! it adopted on the UI port (one started by the CLI, or left over from an
//! older install) can be another release than the app it is talking to.

use axum::response::Json;
use serde_json::{json, Value};

use crate::build_info::{self, InstallKind};

// ===== /api/system/version =====

pub(crate) async fn system_version() -> Json<Value> {
    let exe = std::env::current_exe().ok().map(|p| p.canonicalize().unwrap_or(p));
    let kind = exe.as_deref().map(build_info::install_kind).unwrap_or(InstallKind::Standalone);
    let bundle = exe.as_deref().and_then(build_info::desktop_bundle_of);
    Json(json!({
        "version": build_info::VERSION,
        "gitHash": build_info::GIT_HASH,
        "gitDescribe": build_info::GIT_DESCRIBE,
        "buildTime": build_info::build_time_utc(),
        "buildTarget": build_info::BUILD_TARGET,
        // What may update this binary: the desktop app's own updater for
        // `desktop-bundle`, `senclaw update` for `standalone`, nothing for `dev`.
        "installKind": kind,
        "exePath": exe.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "bundlePath": bundle.map(|p| p.to_string_lossy().into_owned()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_this_build_and_where_it_runs_from() {
        let Json(v) = system_version().await;
        assert_eq!(v["version"], build_info::VERSION);
        assert_eq!(v["gitHash"], build_info::GIT_HASH);
        assert_eq!(v["buildTarget"], build_info::BUILD_TARGET);
        // The test binary is a debug build under Cargo's target dir.
        assert_eq!(v["installKind"], "dev");
        assert!(v["exePath"].as_str().is_some_and(|p| !p.is_empty()));
        assert!(v["bundlePath"].is_null());
    }
}
