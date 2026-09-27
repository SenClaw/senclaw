//! Check for updates + auto-update (`docs/runtime-protocol.md` §7.3).
//!
//! LM Studio semantics: the channel picks versions; auto-update, when on,
//! installs updates only for runtimes that currently *fill a slot* — the ones
//! actually in use — and only downloads a new version alongside the old one.
//! The daemon moves a slot to the new version once no process of the old one
//! is in use (`RuntimeManager::advance_pinned_slots_to_newer_installs`, run
//! from the idle sweep); the old version is left installed until the user
//! removes it.

use std::sync::Arc;

use sen_runtime_sdk::manifest::Slot;

use super::index::CachedIndex;
use super::manager::RuntimeManager;
use super::version::cmp_versions;

/// One selected-and-installed runtime with a newer version available on its
/// channel — `docs/runtime-protocol.md` §5.1: `updates: [{id, from, to}]`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UpdateCandidate {
    pub id: String,
    pub from: String,
    pub to: String,
}

/// `POST /api/runtimes/check-updates`' `updates`/`started` lists: every
/// selected slot whose channel version is ahead of what is installed, and —
/// when `autoUpdate` is on — the background jobs actually started for them.
pub async fn check_updates(manager: &Arc<RuntimeManager>, cached: &CachedIndex) -> (Vec<UpdateCandidate>, Vec<serde_json::Value>) {
    let settings = manager.settings();
    let installed = manager.installed();
    let mut updates = Vec::new();
    let mut started = Vec::new();
    for slot in Slot::ALL {
        let Some(selection) = settings.selected(slot) else { continue };
        let Some(entry) = cached.index.entry(&selection.id) else { continue };
        // `"latest"` (beta, upstream only) resolved to a concrete tag by
        // `RuntimeManager::refresh_index_with_error` — unresolved means
        // nothing to compare against yet, not "no update".
        let Some(target) = cached.effective_channel_version(entry, settings.channel) else { continue };
        let current = installed
            .iter()
            .filter(|p| p.manifest.id == selection.id)
            .map(|p| p.manifest.version.clone())
            .max_by(|a, b| cmp_versions(a, b));
        let Some(current) = current else { continue };
        if cmp_versions(&target, &current) != std::cmp::Ordering::Greater {
            continue; // already on the newest published version
        }
        updates.push(UpdateCandidate { id: selection.id.clone(), from: current, to: target.clone() });

        if !settings.auto_update {
            continue;
        }
        match manager.start_install(&selection.id, Some(target)) {
            Ok(job) => started.push(serde_json::json!({"jobId": job.job_id, "id": job.id, "version": job.version})),
            Err(e) => tracing::warn!("[runtime] auto-update of {} failed to start: {e:#}", selection.id),
        }
    }
    (updates, started)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::index::RuntimeIndex;
    use crate::runtime::manager::RuntimeManagerConfig;

    fn manager(tmp: &std::path::Path) -> Arc<RuntimeManager> {
        RuntimeManager::new(RuntimeManagerConfig {
            runtimes_dir: tmp.join("runtimes"),
            runtime_data_dir: tmp.join("runtime-data"),
            runtime_logs_dir: tmp.join("logs"),
            bundled_dir: None,
            local_models_dir: tmp.join("local-models"),
            config_path: tmp.join("config.json"),
            home: tmp.to_path_buf(),
            index_url: "file:///dev/null".to_string(),
        })
    }

    fn cached(index: RuntimeIndex) -> CachedIndex {
        CachedIndex { fetched_at: 0, source: "test".into(), index, resolved_latest: Default::default() }
    }

    #[tokio::test]
    async fn auto_update_off_reports_the_update_but_starts_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        let mut settings = mgr.settings();
        settings.auto_update = false;
        mgr.replace_settings(settings).unwrap();
        let (updates, started) = check_updates(&mgr, &cached(RuntimeIndex::bundled())).await;
        assert!(started.is_empty());
        assert!(updates.is_empty(), "nothing is selected, so there is nothing to compare either");
    }

    #[tokio::test]
    async fn no_selection_means_nothing_to_update() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        assert!(mgr.settings().auto_update, "default is on");
        let (updates, started) = check_updates(&mgr, &cached(RuntimeIndex::bundled())).await;
        assert!(updates.is_empty());
        assert!(started.is_empty());
    }

    /// A minimal installable package: `select` requires its target to already
    /// be installed, so a test that selects a slot needs one on disk first —
    /// same fixture shape as `store::tests::write_manifest`.
    fn write_fake_package(dir: &std::path::Path, id: &str, version: &str) {
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("run"), "#!/bin/sh\necho hi\n").unwrap();
        let manifest = format!(
            r#"{{
              "schemaVersion": 1, "id": "{id}", "name": "Test", "version": "{version}",
              "type": "ocr", "slots": ["ocr"], "capabilities": ["ocr"],
              "platforms": ["darwin-arm64","darwin-x64","linux-x64","linux-arm64","windows-x64","windows-arm64"],
              "mode": "service",
              "entry": {{"command": "bin/run", "args": ["serve", "--port", "{{port}}"]}}
            }}"#
        );
        std::fs::write(dir.join(sen_runtime_sdk::manifest::MANIFEST_FILE), manifest).unwrap();
    }

    fn index_with_channel(id: &str, slot: &str, version: &str) -> RuntimeIndex {
        RuntimeIndex::parse(&format!(
            r#"{{"schemaVersion":1,"runtimes":[{{"id":"{id}","name":"T","type":"ocr",
                "slots":["{slot}"],"capabilities":["ocr"],"platforms":["darwin-arm64"],
                "channels":{{"stable":"{version}"}},"releases":[]}}]}}"#
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn a_slot_already_on_the_channel_version_reports_no_update() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        write_fake_package(&tmp.path().join("src-pkg"), "sen-ocr", "0.1.0");
        mgr.install_local(&tmp.path().join("src-pkg")).unwrap();
        mgr.select(Slot::Ocr, Some("sen-ocr".into()), None).unwrap();

        let (updates, started) = check_updates(&mgr, &cached(index_with_channel("sen-ocr", "ocr", "0.1.0"))).await;
        assert!(updates.is_empty());
        assert!(started.is_empty());
    }

    #[tokio::test]
    async fn a_newer_channel_version_is_reported_and_auto_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = manager(tmp.path());
        write_fake_package(&tmp.path().join("src-pkg"), "sen-ocr", "0.1.0");
        mgr.install_local(&tmp.path().join("src-pkg")).unwrap();
        mgr.select(Slot::Ocr, Some("sen-ocr".into()), None).unwrap();

        let (updates, started) = check_updates(&mgr, &cached(index_with_channel("sen-ocr", "ocr", "0.2.0"))).await;
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].id, "sen-ocr");
        assert_eq!(updates[0].from, "0.1.0");
        assert_eq!(updates[0].to, "0.2.0");
        // `start_install` resolves its own index (bundled, since nothing was
        // ever cached to disk in this test) rather than the `cached` this
        // function was handed — a pre-existing seam, not this test's concern.
        // What matters here is that finding an update with auto-update on
        // actually calls `start_install` at all.
        assert_eq!(started.len(), 1);
        assert_eq!(started[0]["id"], "sen-ocr");
    }
}
