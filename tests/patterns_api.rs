//! End-to-end test for Zen Patterns: boots the real UI router on an ephemeral
//! port and drives the REST lifecycle over HTTP — create → list → render →
//! import → shadow → delete — against a temp patterns root.
//!
//! Exists because the unit tests cover the store and the renderer in
//! isolation, but not the wiring: route ordering (`/run` vs `/:name`), the
//! camelCase on the wire, and the status codes a UI branches on.
//!
//! Nothing here calls a model. `POST /run` is exercised with `dryRun`, which
//! is the whole point of that flag.

use std::io::Write;
use std::sync::Arc;

use senclaw::config::Config;
use senclaw::gateway::ui_server::{build_router, UiState};

fn temp_state() -> (Arc<UiState>, std::path::PathBuf) {
    let mut cfg = Config::from_env();
    let dir = std::env::temp_dir().join(format!("patterns-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    // Every path this test can reach must be redirected: `Config::from_env`
    // reads the developer's real ~/.senclaw, and a stray write there is
    // exactly the bug this file was added after.
    cfg.paths.patterns_dir = dir.join("patterns");
    cfg.paths.db_path = dir.join("test.db");
    cfg.paths.cognitive_db_path = dir.join("test_cognitive.db");
    // `/api/kits/available` reads the receipt ledger to mark what is already
    // installed. Left alone it reads the developer's real `~/.senclaw/kits`,
    // so the built-in-kit assertion below passes or fails depending on which
    // kits that machine happens to have — which is not a property of the code.
    cfg.paths.kits_dir = dir.join("kits");
    cfg.paths.virtual_agents_dir = dir.join("virtual-agents");
    cfg.paths.managed_skills_dir = dir.join("skills");
    cfg.paths.workflows_dir = dir.join("workflows");

    let state = Arc::new(UiState {
        config: Arc::new(cfg),
        db: None,
        group_manager: None,
        wiki_manager: None,
        persona_registry: None,
        agent_api: None,
        mcp_manager: None,
        dispatch_bridge: None,
        marketplace_manager: None,
        workbench_bridge: None,
        space_mcp_launcher: None,
        workflow_service: None,
        virtual_worker_pool: None,
        agent_states: None,
        background_scheduler: None,
        usage_recorder: None,
        checkpoints: None,
        runtime_manager: None,
        ws_port: 0,
        ws_token: String::new(),
        api_auth: Arc::new(senclaw::gateway::ui_server::auth::ApiAuth::disabled()),
    });
    (state, dir)
}

async fn serve() -> (String, reqwest::Client, std::path::PathBuf) {
    let (state, dir) = temp_state();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = build_router(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), reqwest::Client::new(), dir)
}

#[tokio::test]
async fn pattern_lifecycle_over_http() {
    let (base, http, _dir) = serve().await;

    // Empty daemon: the user source exists, nothing in it.
    let list: serde_json::Value = http
        .get(format!("{base}/api/patterns"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["patterns"].as_array().unwrap().len(), 0);
    assert_eq!(list["sources"][0]["id"], "user");
    assert_eq!(list["sources"][0]["writable"], true);

    // Create. A Vietnamese display name must fold to a typeable slug, not to
    // punctuation — a live daemon produced `t_m_t_t_th` before the fix.
    let created: serde_json::Value = http
        .post(format!("{base}/api/patterns"))
        .json(&serde_json::json!({
            "name": "Tóm Tắt Thử",
            "system": "# IDENTITY and PURPOSE\n\nBạn tóm tắt văn bản.\n\n# INPUT:",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["pattern"]["name"], "tom_tat_thu");

    // Same name twice is a conflict, not a silent overwrite.
    let dup = http
        .post(format!("{base}/api/patterns"))
        .json(&serde_json::json!({ "name": "tom_tat_thu", "system": "x" }))
        .send()
        .await
        .unwrap();
    assert_eq!(dup.status(), reqwest::StatusCode::CONFLICT);

    // Listed, with the description read out of the body.
    let list: serde_json::Value = http
        .get(format!("{base}/api/patterns?q=tom"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["patterns"].as_array().unwrap().len(), 1);
    assert_eq!(list["patterns"][0]["description"], "Bạn tóm tắt văn bản.");

    // `/run` must route as itself, not as a pattern named "run".
    let run: serde_json::Value = http
        .post(format!("{base}/api/patterns/run"))
        .json(&serde_json::json!({
            "name": "tom_tat_thu",
            "input": "xin chào thế giới",
            "language": "auto",
            "dryRun": true,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(run["dryRun"], true);
    // No `{{input}}` in the body, so the input becomes the user message…
    assert_eq!(run["rendered"]["user"], "xin chào thế giới");
    // …and the language rule is appended after the pattern's own text.
    let system = run["rendered"]["system"].as_str().unwrap();
    assert!(system.contains("# INPUT:"));
    assert!(system.find("# LANGUAGE").unwrap() > system.find("# INPUT:").unwrap());

    // Unknown name is a 404 with the daemon's own wording.
    let missing = http
        .get(format!("{base}/api/patterns/nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);

    // Delete.
    let del = http
        .delete(format!("{base}/api/patterns/tom_tat_thu"))
        .send()
        .await
        .unwrap();
    assert!(del.status().is_success());
    let list: serde_json::Value = http
        .get(format!("{base}/api/patterns"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["patterns"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn zip_import_lands_patterns_in_the_user_source() {
    let (base, http, _dir) = serve().await;

    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        for name in ["summarize", "extract_wisdom"] {
            w.start_file::<_, ()>(
                format!("fabric-main/{name}/system.md"),
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
            write!(w, "# IDENTITY\n\nDoes {name}.").unwrap();
        }
        w.start_file::<_, ()>("fabric-main/README.md", zip::write::SimpleFileOptions::default())
            .unwrap();
        write!(w, "ignored").unwrap();
        w.finish().unwrap();
    }

    let form = reqwest::multipart::Form::new().part(
        "file",
        reqwest::multipart::Part::bytes(buf).file_name("patterns.zip"),
    );
    let out: serde_json::Value = http
        .post(format!("{base}/api/patterns/import"))
        .multipart(form)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(out["found"], 2);
    assert_eq!(out["source"], "user");

    let list: serde_json::Value = http
        .get(format!("{base}/api/patterns"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = list["patterns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["extract_wisdom", "summarize"]);
}

#[tokio::test]
async fn a_source_is_added_toggled_and_removed_without_a_network_fetch() {
    let (base, http, _dir) = serve().await;

    // `sync: false` keeps this test offline; the clone path is the one thing
    // that genuinely needs a remote and is covered by the unit tests instead.
    let added: serde_json::Value = http
        .post(format!("{base}/api/patterns/sources"))
        .json(&serde_json::json!({
            "url": "https://github.com/danielmiessler/fabric.git",
            "ref": "v1.4.470",
            "subdir": "data/patterns",
            "sync": false,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Id derived from the repo name, with the `.git` suffix dropped.
    assert_eq!(added["source"], "fabric");

    let sources: serde_json::Value = http
        .get(format!("{base}/api/patterns/sources"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let fabric = sources["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "fabric")
        .unwrap()
        .clone();
    assert_eq!(fabric["kind"], "git");
    assert_eq!(fabric["writable"], false, "a checkout is never writable");
    assert_eq!(fabric["count"], 0);

    // Adding the same id twice is a conflict, so a kit can never silently
    // redirect a source the user already owns.
    let dup = http
        .post(format!("{base}/api/patterns/sources"))
        .json(&serde_json::json!({ "url": "https://example.invalid/fabric", "sync": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(dup.status(), reqwest::StatusCode::CONFLICT);

    let toggled: serde_json::Value = http
        .post(format!("{base}/api/patterns/sources/fabric/toggle"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(toggled["enabled"], false);

    assert!(http
        .delete(format!("{base}/api/patterns/sources/fabric"))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());

    // The user source is the one that cannot be removed: it is where "save a
    // copy" writes, so a daemon without it cannot accept a new pattern.
    let refused = http
        .delete(format!("{base}/api/patterns/sources/user"))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_bundled_fabric_kit_is_offered_without_a_marketplace() {
    let (base, http, _dir) = serve().await;

    // `marketplace_manager: None` — a kit compiled into the binary must still
    // be installable on a fresh machine.
    let out: serde_json::Value = http
        .get(format!("{base}/api/kits/available"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let fabric = out["kits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["id"] == "fabric")
        .expect("the built-in Fabric kit must be offered");
    assert_eq!(fabric["sourceId"], "builtin");
    assert_eq!(fabric["installable"], true);
    assert!(fabric["installedVersion"].is_null());
}

#[tokio::test]
async fn the_bundled_library_installs_offline_from_the_catalog() {
    let (base, http, _dir) = serve().await;

    // The catalog is what the "add a source" screen offers before the user
    // types anything — the bundled entry must be there and must not be a clone.
    let cat: serde_json::Value = http
        .get(format!("{base}/api/patterns/catalog"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = cat["catalog"].as_array().unwrap();
    let bundled = entries.iter().find(|e| e["id"] == "senclaw").unwrap();
    assert_eq!(bundled["kind"], "bundled");
    assert_eq!(bundled["installed"], false);
    assert!(bundled["count"].as_u64().unwrap() > 200);
    // Git presets carry the layout nobody should have to look up.
    let fabric = entries.iter().find(|e| e["id"] == "fabric").unwrap();
    assert_eq!(fabric["subdir"], "data/patterns");
    assert_eq!(fabric["strategiesSubdir"], "data/strategies");

    // Install it. No network is involved, so this is fast and deterministic.
    let out: serde_json::Value = http
        .post(format!("{base}/api/patterns/catalog/senclaw/install"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(out["source"], "senclaw");
    let installed = out["installed"].as_array().unwrap().len();
    assert!(installed > 200, "only {installed} patterns landed");

    let list: serde_json::Value = http
        .get(format!("{base}/api/patterns"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["patterns"].as_array().unwrap().len(), installed);
    // Strategies ride along — a library with no `cot` is half-installed.
    assert!(list["strategies"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["name"] == "cot"));

    // Both halves of the vendored library are present.
    for name in ["summarize", "tom_tat"] {
        assert!(
            http.get(format!("{base}/api/patterns/{name}"))
                .send()
                .await
                .unwrap()
                .status()
                .is_success(),
            "{name} missing after install"
        );
    }

    // Re-offering it now says installed, so the card stops inviting a repeat.
    let cat: serde_json::Value = http
        .get(format!("{base}/api/patterns/catalog"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        cat["catalog"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == "senclaw")
            .unwrap()["installed"],
        true
    );
}

#[tokio::test]
async fn a_bundled_pattern_renders_with_the_language_rule_last() {
    let (base, http, _dir) = serve().await;
    http.post(format!("{base}/api/patterns/catalog/senclaw/install"))
        .send()
        .await
        .unwrap();

    let run: serde_json::Value = http
        .post(format!("{base}/api/patterns/run"))
        .json(&serde_json::json!({
            "name": "tom_tat",
            "input": "Hôm nay trời đẹp. Giá vàng tăng 2%.",
            "language": "auto",
            "dryRun": true,
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let system = run["rendered"]["system"].as_str().unwrap();
    assert!(system.contains("# OUTPUT INSTRUCTIONS"));
    // The overlay has to come after the pattern's own instructions to win.
    assert!(system.find("# LANGUAGE").unwrap() > system.find("# OUTPUT INSTRUCTIONS").unwrap());
    assert_eq!(run["rendered"]["user"], "Hôm nay trời đẹp. Giá vàng tăng 2%.");
}
