//! `senclaw space-server` over real stdio MCP, managing real Space Apps.
//!
//! Everything else about these tools is tested in-process. This proves the
//! claim that actually matters to a user typing into a chat: the subprocess an
//! agent is handed **publishes** `space_app_list` / `start` / `stop` /
//! `restart` / `mcp_list`, and calling one of them reaches the daemon and comes
//! back with the app.
//!
//! It is the only test that exercises the wiring end to end — the env var
//! `space_mcp_config` sets, `from_env` reading it, the rmcp tool router
//! publishing the new tools, and the loopback HTTP hop. A tool that compiles,
//! is unit-tested, and never appears in `tools/list` is the failure this
//! catches and nothing else does.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;

use senclaw::config::Config;
use senclaw::db::Db;
use senclaw::gateway::ui_server::{build_router, UiState};
use serde_json::{json, Value};

// ─── A daemon to manage ──────────────────────────────────────────────────────

fn temp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("app-stdio-e2e-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn serve(dir: &std::path::Path) -> (String, std::path::PathBuf) {
    let mut cfg = Config::from_env();
    cfg.paths.db_path = dir.join("test.db");
    cfg.paths.cognitive_db_path = dir.join("test_cognitive.db");
    let db_path = cfg.paths.db_path.clone();
    let db = Arc::new(Db::open(&cfg).unwrap());
    let state = Arc::new(UiState {
        config: Arc::new(cfg),
        db: Some(db),
        group_manager: None,
        wiki_manager: None,
        persona_registry: None,
        agent_api: None,
        dispatch_bridge: None,
        mcp_manager: Some(Arc::new(senclaw::mcp::manager::McpManager::new(
            dir.to_path_buf(),
            dir.to_path_buf(),
        ))),
        marketplace_manager: None,
        workbench_bridge: None,
        space_mcp_launcher: Some(Arc::new(
            senclaw::gateway::ui_server::space_mcp::SpaceMcpLauncher::new(),
        )),
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
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, build_router(state)).await.unwrap();
    });
    (format!("http://{addr}"), db_path)
}

fn install_app(db_path: &std::path::Path, id: &str, manifest: Value) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO space_apps (id, manifest, enabled, installed_at) \
         VALUES (?1, ?2, 1, 0)",
        rusqlite::params![id, manifest.to_string()],
    )
    .unwrap();
}

// ─── An MCP client, in as few lines as the protocol allows ───────────────────

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Server {
    /// Spawn `senclaw space-server` exactly the way `space_mcp_config` does.
    fn spawn(dir: &std::path::Path, db_path: &std::path::Path, base_url: &str) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_senclaw"))
            .arg("space-server")
            .env("SENCLAW_DB_PATH", db_path)
            .env("SENCLAW_GROUP_FOLDER", "test-group")
            .env("SENCLAW_CHAT_JID", "test:jid")
            .env("SENCLAW_SPACE_API_URL", base_url)
            // Keep the subprocess off the developer's real ~/.senclaw.
            .env("SENCLAW_COGNITIVE_DB_PATH", dir.join("cog.db"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn senclaw space-server");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut s = Server {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        s.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "space-app-stdio-test", "version": "0" },
            }),
        );
        s.notify("notifications/initialized");
        s
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        self.read_reply(id)
    }

    fn notify(&mut self, method: &str) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": {} }));
    }

    fn send(&mut self, obj: Value) {
        writeln!(self.stdin, "{obj}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Skip notifications and anything that is not our response — the server
    /// emits both, and treating either as a protocol error makes the test flaky
    /// for reasons that have nothing to do with the tools.
    fn read_reply(&mut self, want: i64) -> Value {
        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).expect("read stdout");
            assert!(n > 0, "server closed stdout before answering id {want}");
            let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if v.get("id").and_then(Value::as_i64) == Some(want) {
                assert!(v.get("error").is_none(), "JSON-RPC error: {v}");
                return v["result"].clone();
            }
        }
    }

    /// Call a tool and return its single text content block verbatim. Success
    /// carries JSON; a rejection carries a sentence meant for the agent to read,
    /// which is why this does not parse.
    fn call_text(&mut self, name: &str, args: Value) -> String {
        let res = self.request("tools/call", json!({ "name": name, "arguments": args }));
        res["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text content in {res}"))
            .to_string()
    }

    /// Call a tool that is expected to succeed, and parse its JSON.
    fn call(&mut self, name: &str, args: Value) -> Value {
        let text = self.call_text(name, args);
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("tool returned non-JSON ({e}): {text}"))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// One test, one subprocess: spawning `senclaw` is seconds of process startup,
/// and every assertion below is about the same running server.
#[test]
fn the_space_server_publishes_and_serves_the_app_lifecycle_tools() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = temp_dir();
    let (base_url, db_path) = rt.block_on(serve(&dir));
    // The runtime must outlive the subprocess: the daemon it calls lives on it.
    let _guard = rt.enter();

    install_app(
        &db_path,
        "kanban",
        json!({
            "id": "kanban", "name": "Kanban Board",
            "runtime": { "kind": "server", "start": "./k", "port": 4340, "mode": "session" },
            "mcp": { "name": "kanban-mcp", "path": "/mcp" },
        }),
    );
    install_app(
        &db_path,
        "watcher",
        json!({
            "id": "watcher", "name": "Watcher",
            "runtime": { "kind": "server", "start": "./w", "port": 4341, "mode": "background" },
        }),
    );

    let mut srv = Server::spawn(&dir, &db_path, &base_url);

    // 1. The tools exist. A tool that compiles but never reaches `tools/list`
    //    is invisible to every agent, and nothing else in the suite notices.
    let listed = srv.request("tools/list", json!({}));
    let names: Vec<&str> = listed["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for want in [
        "space_app_list",
        "space_app_start",
        "space_app_stop",
        "space_app_restart",
        "space_app_mcp_list",
    ] {
        assert!(
            names.contains(&want),
            "{want} missing from tools/list: {names:?}"
        );
    }
    // The tools it already had must survive gaining new ones.
    assert!(names.contains(&"space_note_create"), "{names:?}");

    // 2. Listing reaches the daemon and comes back with the real fleet.
    let v = srv.call("space_app_list", json!({}));
    assert_eq!(v["count"], 2, "{v}");
    let kanban = v["apps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "kanban")
        .expect("kanban row");
    assert_eq!(kanban["name"], "Kanban Board");
    assert_eq!(kanban["mode"], "session");
    assert_eq!(kanban["port"], 4340);

    // 3. Filters are honoured across the wire, not just in the unit test.
    let filtered = srv.call("space_app_list", json!({ "query": "watch" }));
    assert_eq!(filtered["count"], 1);
    assert_eq!(filtered["apps"][0]["id"], "watcher");

    // 4. Stop mutates daemon state a later call can see — the round trip that
    //    proves these are not read-only wrappers.
    let stopped = srv.call("space_app_stop", json!({ "app_id": "watcher" }));
    assert_eq!(stopped["success"], true, "{stopped}");
    assert_eq!(stopped["mode"], "background");
    let after = srv.call("space_app_list", json!({ "query": "watcher" }));
    assert_eq!(
        after["apps"][0]["userStopped"], true,
        "the stop must stick, or a background app is back within a supervisor tick"
    );

    // 5. The per-app MCP view resolves the server name from the manifest.
    let mcp = srv.call("space_app_mcp_list", json!({ "app_id": "kanban" }));
    assert_eq!(mcp["count"], 1, "{mcp}");
    assert_eq!(mcp["apps"][0]["mcpName"], "kanban-mcp");
    assert_eq!(mcp["callFormat"], "mcp__<mcpName>__<tool>");

    // 6. A bad id is refused by the tool, not turned into some other request.
    let bad = srv.call_text("space_app_start", json!({ "app_id": "../../etc" }));
    assert!(
        bad.contains("app_id"),
        "a malformed id must name the offending parameter: {bad}"
    );
    // And the daemon is untouched by it — the rejection happened before any
    // request went out, which is the whole point of validating here as well.
    assert_eq!(srv.call("space_app_list", json!({}))["count"], 2);
}
