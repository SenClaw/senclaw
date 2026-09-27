//! End-to-end test for the brush (rust-bash) sandbox's kill-enforced timeout.
//!
//! brush has no cooperative cancellation, so the sandbox runs in a child
//! process that the parent kills on the deadline. This exercises the real
//! `senclaw brush-sandbox` child via `SENCLAW_BIN` → `CARGO_BIN_EXE_senclaw`.

use senclaw::gateway::ui_server::bash_sandbox;

#[tokio::test]
async fn infinite_loop_is_killed_by_timeout() {
    std::env::set_var("SENCLAW_BIN", env!("CARGO_BIN_EXE_senclaw"));

    let v = bash_sandbox::run("while true; do :; done".to_string(), 400).await;

    assert_eq!(v["timed_out"], true, "expected a timeout outcome, got: {v}");
    assert_eq!(v["ok"], false);
}

#[tokio::test]
async fn normal_script_runs_via_child_process() {
    std::env::set_var("SENCLAW_BIN", env!("CARGO_BIN_EXE_senclaw"));

    // 30s, not 5s: this test asks "does the child run a script and hand back
    // its output", and the budget has to clear the *fixed* cost of getting
    // there — spawning a ~150 MB debug binary and building a brush Shell, which
    // is ~2.5s on an idle machine and more on a loaded CI runner. At 5s it
    // failed roughly two runs in three here while the sandbox itself was fine.
    // The deadline mechanism is what `infinite_loop_is_killed_by_timeout`
    // covers, on a budget short enough to mean something.
    let v = bash_sandbox::run(
        "for i in 1 2 3; do echo \"line $i\"; done".to_string(),
        30_000,
    )
    .await;

    assert_eq!(v["ok"], true, "got: {v}");
    assert!(
        v["result"].as_str().unwrap_or_default().contains("line 3"),
        "got: {v}"
    );
}
