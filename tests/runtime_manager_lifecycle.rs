//! End-to-end runtime lifecycle against a **real child process**
//! (`examples/echo_runtime.rs`, built on `sen-runtime-sdk`'s server
//! scaffold): install-local -> single-flight start -> health gate -> a real
//! HTTP call with the per-launch token -> idle sweep -> cleanup. Everything
//! runs under a temp `HOME` and ephemeral ports.

use std::path::PathBuf;
use std::time::Duration;

use sen_runtime_sdk::manifest::{Slot, MANIFEST_FILE};
use senclaw::runtime::manager::{RuntimeManager, RuntimeManagerConfig};

/// Locate the compiled `echo_runtime` example, building it once if this is
/// the first test run in a fresh `target/` (cargo does not build examples
/// ahead of integration tests the way it does `[[bin]]` targets).
fn echo_runtime_binary() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let exe_name = if cfg!(windows) { "echo_runtime.exe" } else { "echo_runtime" };
    let candidates = ["debug", "release"].map(|profile| manifest_dir.join("target").join(profile).join("examples").join(exe_name));
    if let Some(found) = candidates.iter().find(|c| c.is_file()) {
        return found.clone();
    }
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--example", "echo_runtime"])
        .current_dir(&manifest_dir)
        .status()
        .expect("run cargo build --example echo_runtime");
    assert!(status.success(), "failed to build examples/echo_runtime.rs");
    candidates.into_iter().find(|c| c.is_file()).expect("echo_runtime binary missing after building it")
}

/// A package source directory `install_local` can install: the example
/// binary plus a manifest declaring it an `ocr` service with a 1s idle
/// timeout (so the idle-sweep half of this test does not need to wait long).
fn write_fixture_package(dir: &std::path::Path) {
    let bin_dir = dir.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let bin_name = if cfg!(windows) { "echo_runtime.exe" } else { "echo_runtime" };
    let dest = bin_dir.join(bin_name);
    std::fs::copy(echo_runtime_binary(), &dest).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let platform = sen_runtime_sdk::platform::current();
    let manifest = format!(
        r#"{{
          "schemaVersion": 1,
          "id": "echo-runtime-test",
          "name": "Echo Runtime (test fixture)",
          "version": "0.0.1",
          "type": "ocr",
          "slots": ["ocr"],
          "capabilities": ["ocr"],
          "platforms": ["{platform}"],
          "mode": "service",
          "entry": {{ "command": "bin/{bin_name}", "args": ["serve", "--host", "{{host}}", "--port", "{{port}}"] }},
          "health": {{ "path": "/health", "startupTimeoutSecs": 15 }},
          "idleTimeoutSecs": 1
        }}"#
    );
    std::fs::write(dir.join(MANIFEST_FILE), manifest).unwrap();
}

fn manager(home: &std::path::Path) -> std::sync::Arc<RuntimeManager> {
    RuntimeManager::new(RuntimeManagerConfig {
        runtimes_dir: home.join("runtimes"),
        runtime_data_dir: home.join("runtime-data"),
        runtime_logs_dir: home.join("logs"),
        bundled_dir: None,
        local_models_dir: home.join("local-models"),
        config_path: home.join("config.json"),
        home: home.to_path_buf(),
        index_url: "file:///dev/null".to_string(),
    })
}

#[tokio::test]
async fn install_supervise_call_idle_and_clean_up_a_real_runtime_process() {
    let home = tempfile::tempdir().unwrap();
    let src = home.path().join("fixture-src");
    write_fixture_package(&src);

    let mgr = manager(home.path());

    // install-local
    let installed = mgr.install_local(&src).unwrap();
    assert_eq!(installed.manifest.id, "echo-runtime-test");
    assert!(installed.dir.join(MANIFEST_FILE).is_file());

    // The sole compatible candidate for `ocr` auto-selects.
    let dial = mgr.ensure_slot_started(Slot::Ocr).await.expect("the echo runtime must spawn and become healthy");
    assert!(!dial.token.is_empty());
    let key = dial.process_key.clone();
    assert_eq!(key, "service:echo-runtime-test");
    let proc = mgr.process(&key).expect("supervisor must track the spawned process");
    assert!(proc.is_ready());

    // A real HTTP call against the process, with its per-launch token — the
    // same shape `src/runtime/proxy.rs` makes on a caller's behalf.
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/api/ocr/models", dial.base_url))
        .bearer_auth(&dial.token)
        .send()
        .await
        .expect("the runtime must answer over HTTP");
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["echo"], true);

    // No/wrong token is refused — every route but /health needs the bearer.
    let unauthed = client.get(format!("{}/api/ocr/models", dial.base_url)).send().await.unwrap();
    assert_eq!(unauthed.status(), reqwest::StatusCode::UNAUTHORIZED);

    // A second concurrent ensure_slot_started for the same slot reuses the
    // running process rather than spawning a second one (single-flight).
    let second = mgr.ensure_slot_started(Slot::Ocr).await.unwrap();
    assert_eq!(second.base_url, dial.base_url, "must reuse the already-healthy process");
    assert_eq!(proc.launches.load(std::sync::atomic::Ordering::Relaxed), 1);

    // Idle sweep: idleTimeoutSecs is 1 in the fixture manifest, and nothing
    // has touched the process since the calls above.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    mgr.sweep_idle_once().await;
    assert!(mgr.process(&key).is_none(), "the idle runtime must have been stopped");

    // The process really exited (not just forgotten): starting it again must
    // spawn a fresh one and health-gate it from scratch.
    let restarted = mgr.ensure_slot_started(Slot::Ocr).await.unwrap();
    assert_ne!(restarted.base_url, dial.base_url, "a fresh process gets a fresh port");

    // Cleanup: stop_all must leave nothing running.
    mgr.stop_all().await;
    assert!(mgr.processes().is_empty());
}

/// `docs/runtime-protocol.md` §3.2 step 9: a runtime that crashes
/// *after* becoming healthy is otherwise invisible — nothing polls for it in
/// the background, so it must be the *next request* that discovers and
/// recovers from it, not a 5-15 minute wait for the idle sweep.
#[tokio::test]
async fn a_process_that_crashes_after_becoming_healthy_is_detected_and_respawned_on_the_next_request() {
    let home = tempfile::tempdir().unwrap();
    let src = home.path().join("fixture-src");
    write_fixture_package(&src);
    let mgr = manager(home.path());
    mgr.install_local(&src).unwrap();

    let dial = mgr.ensure_slot_started(Slot::Ocr).await.unwrap();
    let key = dial.process_key.clone();
    let proc = mgr.process(&key).unwrap();
    assert!(proc.is_ready());
    assert_eq!(proc.launches.load(std::sync::atomic::Ordering::Relaxed), 1);

    // Simulate a crash: tell the runtime to exit on its own, bypassing the
    // daemon's own `stop()` entirely — the tracked process is left `Ready`
    // with a now-stale child handle, exactly like an OOM'd model or a
    // llama.cpp assertion failure would leave it.
    let client = reqwest::Client::new();
    client
        .post(format!("{}/runtime/shutdown", dial.base_url))
        .bearer_auth(&dial.token)
        .send()
        .await
        .expect("the runtime answers its own shutdown route");

    let mut exited = false;
    for _ in 0..100 {
        if client.get(format!("{}/health", dial.base_url)).send().await.is_err() {
            exited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(exited, "the runtime must actually stop listening after /runtime/shutdown");

    // Nothing in the daemon polls for this on its own (no background restart
    // loop) — the tracked process still reports `Ready` until the *next*
    // request notices.
    assert!(proc.is_ready(), "a crash is only discovered reactively, never by a background poller");

    // The next request is what discovers and recovers from the crash: the
    // fast path in `ensure_started` sees the tracked process is no longer
    // really running, marks it `Failed`, evicts it, and spawns a fresh one.
    // The listener closing (detected above) and the OS finishing reaping the
    // exited child are not the same instant — `try_wait` is non-blocking, so
    // a request landing in that narrow gap can still see the stale process
    // once. A real caller's next request would not arrive that fast; this
    // retries the way one eventually would, and single-flight makes calling
    // it again from an already-recovered state harmless.
    let mut second = mgr.ensure_slot_started(Slot::Ocr).await.expect("must respawn after detecting the crash");
    for _ in 0..40 {
        if second.token != dial.token {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        second = mgr.ensure_slot_started(Slot::Ocr).await.expect("must respawn after detecting the crash");
    }
    // A genuinely fresh process gets a fresh per-launch token (a 64-hex CSPRNG
    // value) — unlike the port, which the OS can legitimately hand back once
    // the old listener has actually closed, so it is not a reliable "this is
    // a different process" signal here.
    assert_ne!(second.token, dial.token, "a fresh process gets a fresh per-launch token");
    let new_proc = mgr.process(&second.process_key).unwrap();
    assert!(new_proc.is_ready());
    assert_eq!(
        new_proc.launches.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "the crash-loop signal: launches must count both the original start and this respawn"
    );

    // A real HTTP call against the respawned process proves it is actually
    // serving, not just tracked as ready.
    let resp = client.get(format!("{}/api/ocr/models", second.base_url)).bearer_auth(&second.token).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    mgr.stop_all().await;
}

#[tokio::test]
async fn uninstall_refuses_a_running_version_and_force_stops_it_first() {
    let home = tempfile::tempdir().unwrap();
    let src = home.path().join("fixture-src");
    write_fixture_package(&src);
    let mgr = manager(home.path());
    mgr.install_local(&src).unwrap();
    mgr.ensure_slot_started(Slot::Ocr).await.unwrap();

    let refused = mgr.uninstall("echo-runtime-test", "0.0.1", false).await;
    assert!(refused.is_err(), "must not uninstall a running version without force");

    mgr.uninstall("echo-runtime-test", "0.0.1", true).await.expect("force uninstall must stop then remove it");
    assert!(mgr.installed().is_empty());
    assert!(mgr.processes().is_empty());
}
