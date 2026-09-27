//! One app, one process — enforced across repeated starts.
//!
//! The map of tracked children holds the **only** handle to each app's process.
//! Anything that inserts over an existing entry orphans whatever it replaced:
//! the process keeps running, keeps its port, and stops being visible to the
//! daemon that started it. That is how a machine ends up with three copies of
//! one app and a launch counter reading "6×" while the user restarts it again.
//!
//! These tests spawn real processes and assert the invariant the hard way: same
//! pid, one launch, no strays.

use std::path::PathBuf;
use std::sync::Arc;

/// pids listening on `port`, as the OS sees them.
///
/// This, not a tracked pid, is the invariant worth asserting. Every app is
/// launched through `sh -c "<start>"`, so the handle the daemon holds is the
/// *shell wrapper*; tokio's `kill_on_drop` reaps that wrapper when its map
/// entry goes away, while the real server — a grandchild — carries on holding
/// the port. Asserting on the wrapper's pid would pass while two copies of the
/// app were running.
fn listeners_on(port: u16) -> Vec<u32> {
    let out = std::process::Command::new("lsof")
        .args(["-nP", "-sTCP:LISTEN", &format!("-iTCP:{port}"), "-t"])
        .output();
    let Ok(out) = out else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .collect()
}

use senclaw::config::Config;
use senclaw::db::Db;
use senclaw::gateway::ui_server::space_mcp::SpaceMcpLauncher;

/// A stand-in Space App: Python's own HTTP server, which answers 200 on `/`.
/// Skipped rather than failed when python3 is not on PATH — a missing
/// interpreter is not a regression in this code.
fn python3() -> Option<String> {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v python3")
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

struct Fixture {
    _dir: PathBuf,
    app_dir: PathBuf,
    db: Arc<Db>,
    manifest: serde_json::Value,
}

fn fixture(port: u16, start: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("one-proc-{}", uuid::Uuid::new_v4()));
    let app_dir = dir.join("app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let mut cfg = Config::from_env();
    cfg.paths.db_path = dir.join("test.db");
    // `Db::open` opens TWO files and `cognitive_db_path` is configured
    // independently — leaving it alone points this test at the developer's
    // real ~/.senclaw/senclaw_cognitive.db and runs schema migrations on it.
    cfg.paths.cognitive_db_path = dir.join("test_cognitive.db");
    let db = Arc::new(Db::open(&cfg).unwrap());
    let manifest = serde_json::json!({
        "id": "oneproc",
        "runtime": {
            "kind": "server",
            "mode": "background",
            "start": start,
            "port": port,
            "healthPath": "/",
        }
    });
    Fixture { _dir: dir, app_dir, db, manifest }
}

#[tokio::test]
async fn repeated_starts_reuse_the_same_process() {
    let Some(py) = python3() else {
        eprintln!("skipped: python3 not on PATH");
        return;
    };
    let port = free_port();
    let f = fixture(port, &format!("{py} -m http.server {port} --bind 127.0.0.1"));
    let l = SpaceMcpLauncher::new();

    let p1 = l
        .ensure_running(&f.db, "oneproc", &f.app_dir, &f.manifest, "http://127.0.0.1:18788")
        .await
        .expect("first start");
    let pid1 = l.runtime_info("oneproc").await.unwrap().pid;

    // Second call: the app is healthy, so it must be *reused*, not started
    // again. A second spawn here is exactly the bug — it would answer on the
    // same port and the first process would become an untracked orphan.
    let p2 = l
        .ensure_running(&f.db, "oneproc", &f.app_dir, &f.manifest, "http://127.0.0.1:18788")
        .await
        .expect("second start");
    let pid2 = l.runtime_info("oneproc").await.unwrap().pid;

    assert_eq!(p1, port);
    assert_eq!(p2, port);
    assert_eq!(pid1, pid2, "a healthy app was started a second time");
    assert_eq!(
        l.launch_count("oneproc").await,
        1,
        "launch count climbed without a crash — the signature the monitor warns about"
    );

    l.shutdown().await;
}

#[tokio::test]
async fn a_restart_leaves_exactly_one_process() {
    let Some(py) = python3() else {
        eprintln!("skipped: python3 not on PATH");
        return;
    };
    let port = free_port();
    let f = fixture(port, &format!("{py} -m http.server {port} --bind 127.0.0.1"));
    let l = SpaceMcpLauncher::new();

    l.ensure_running(&f.db, "oneproc", &f.app_dir, &f.manifest, "http://127.0.0.1:18788")
        .await
        .expect("start");
    let first = l.runtime_info("oneproc").await.unwrap().pid;

    // Kill + respawn, the user-facing restart path.
    l.restart_app("oneproc").await;
    assert!(!l.is_running("oneproc").await, "restart_app must reap");

    l.ensure_running(&f.db, "oneproc", &f.app_dir, &f.manifest, "http://127.0.0.1:18788")
        .await
        .expect("restart");
    let second = l.runtime_info("oneproc").await.unwrap().pid;

    assert_ne!(first, second, "a restart should be a new process");
    // The old pid must be gone, not merely forgotten. `kill -0` succeeding
    // means it is still alive and now nobody's — the orphan case.
    let alive = std::process::Command::new("kill")
        .args(["-0", &first.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(!alive, "pid {first} survived the restart as an orphan");

    l.shutdown().await;
}

#[tokio::test]
async fn shutdown_leaves_nothing_behind() {
    let Some(py) = python3() else {
        eprintln!("skipped: python3 not on PATH");
        return;
    };
    let port = free_port();
    let f = fixture(port, &format!("{py} -m http.server {port} --bind 127.0.0.1"));
    let l = SpaceMcpLauncher::new();

    l.ensure_running(&f.db, "oneproc", &f.app_dir, &f.manifest, "http://127.0.0.1:18788")
        .await
        .expect("start");
    let pid = l.runtime_info("oneproc").await.unwrap().pid;

    l.shutdown().await;

    // Give the group a moment to die.
    for _ in 0..30 {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !alive {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("pid {pid} outlived the daemon — this is the weekly-orphan bug");
}

/// The regression this file exists for.
///
/// A tracked child that is **alive but not answering** used to fall straight
/// through to the spawn path, and the insert at the end overwrote its map entry
/// — dropping the only handle to it. The first process kept running, kept its
/// port, and became invisible to the daemon: an orphan. Do that a few times and
/// you get the picture the user sent us, three copies of one app and a launch
/// counter reading "6×".
///
/// The app here serves health for a moment, then stops serving while staying
/// alive — the shape of a wedged app, reproduced in about five seconds.
#[tokio::test]
async fn a_wedged_app_is_replaced_not_duplicated() {
    let Some(py) = python3() else {
        eprintln!("skipped: python3 not on PATH");
        return;
    };
    let port = free_port();
    let f = fixture(port, "PLACEHOLDER");

    // Serve for ~2s, then stop answering but keep the process alive.
    let script = f.app_dir.join("wedge.py");
    std::fs::write(
        &script,
        format!(
            r#"
import http.server, threading, time
srv = http.server.HTTPServer(('127.0.0.1', {port}), http.server.SimpleHTTPRequestHandler)
threading.Thread(target=srv.serve_forever, daemon=True).start()
time.sleep(2)
srv.shutdown(); srv.server_close()   # alive, but no longer answering
time.sleep(300)
"#
        ),
    )
    .unwrap();
    let mut manifest = f.manifest.clone();
    manifest["runtime"]["start"] =
        serde_json::json!(format!("{py} wedge.py; exit $?"));

    let l = SpaceMcpLauncher::new();
    l.ensure_running(&f.db, "oneproc", &f.app_dir, &manifest, "http://127.0.0.1:18788")
        .await
        .expect("first start");
    let wedged_wrapper = l.runtime_info("oneproc").await.unwrap().pid;
    let before = listeners_on(port);
    assert_eq!(before.len(), 1, "one server should hold the port");
    let wedged_server = before[0];
    assert_ne!(
        wedged_server, wedged_wrapper,
        "sanity: the app is a grandchild of the tracked shell, which is the \
         whole reason a pid assertion would be worthless here"
    );

    // Let it stop answering while staying alive, holding nothing.
    tokio::time::sleep(std::time::Duration::from_millis(2600)).await;

    l.ensure_running(&f.db, "oneproc", &f.app_dir, &manifest, "http://127.0.0.1:18788")
        .await
        .expect("second start replaces the wedged process");

    // The wedged *server* — not its shell — must be gone. Left alive it is an
    // untracked copy of the app: it will take the port back the moment the new
    // one restarts, and nothing in the daemon knows it exists.
    let alive = std::process::Command::new("kill")
        .args(["-0", &wedged_server.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(
        !alive,
        "the wedged app (pid {wedged_server}) is still running and untracked — two copies"
    );
    assert_eq!(l.launch_count("oneproc").await, 2, "one replacement, not two");

    l.shutdown().await;
}
