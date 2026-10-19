//! Sessions in a project: where they work, what their MCP children are told,
//! which workspace-state file each uses, and what reaches the system prompt.
//!
//! These build real cores through `AgentPool::get_or_create` against a
//! recording `CoreApi`, so what is checked is what the pool actually handed
//! the runtime — not a helper that computes it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::db::Db;
use crate::mcp::helper::McpServerConfig;
use crate::projects::{FactsWrite, NewProject, Project};
use crate::types::GroupBinding;

use super::traits::CoreApi;
use super::AgentPool;

#[derive(Default)]
struct RecordingCore {
    working_dirs: Mutex<HashMap<String, String>>,
    mcp: Mutex<Vec<(String, McpServerConfig)>>,
    project_context: Mutex<Vec<(String, Option<String>)>>,
}

impl CoreApi for RecordingCore {
    fn set_working_dir(&self, jid: &str, dir: &str) {
        self.working_dirs
            .lock()
            .unwrap()
            .insert(jid.to_string(), dir.to_string());
    }

    fn add_or_update_mcp_server(&self, jid: &str, cfg: &McpServerConfig) -> anyhow::Result<()> {
        self.mcp
            .lock()
            .unwrap()
            .push((jid.to_string(), cfg.clone()));
        Ok(())
    }

    fn set_project_context(&self, jid: &str, block: Option<String>) {
        self.project_context
            .lock()
            .unwrap()
            .push((jid.to_string(), block));
    }
}

impl RecordingCore {
    fn working_dir(&self, jid: &str) -> Option<String> {
        self.working_dirs.lock().unwrap().get(jid).cloned()
    }

    /// The environment of the bundled `core-server` this session was given.
    fn core_env(&self, jid: &str) -> HashMap<String, String> {
        self.mcp
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(j, c)| j == jid && c.name == crate::mcp::core_server::SERVER_NAME)
            .map(|(_, c)| c.env.clone().into_iter().collect())
            .unwrap_or_else(|| panic!("{jid} was given no core server"))
    }

    fn last_project_context(&self, jid: &str) -> Option<Option<String>> {
        self.project_context
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(j, _)| j == jid)
            .map(|(_, b)| b.clone())
    }
}

struct World {
    pool: Arc<AgentPool>,
    core: Arc<RecordingCore>,
    db: Arc<Db>,
    config: Arc<crate::config::Config>,
    /// The pool's `senclaw_home`: where workspace-state files are written.
    home: PathBuf,
    tmp: tempfile::TempDir,
}

fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(tmp.path()).unwrap();
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let mut cfg = crate::config::Config::from_env();
    cfg.paths.profiles_dir = base.join("data").join("profiles");
    cfg.paths.projects_dir = base.join("data").join("projects");
    cfg.paths.workspace_dir = base.join("data").join("workspace");
    cfg.paths.global_config_path = base.join("config.json");
    cfg.paths.db_path = base.join("senclaw.db");
    let config = Arc::new(cfg);

    let db = Arc::new(Db::open_in_memory(&config).unwrap());
    let core = Arc::new(RecordingCore::default());
    let pool = AgentPool::new(core.clone() as Arc<dyn CoreApi>);
    pool.set_config(config.clone());
    pool.set_db(db.clone());
    pool.set_senclaw_home(home.clone());
    World {
        pool,
        core,
        db,
        config,
        home,
        tmp,
    }
}

impl World {
    fn folder(&self, name: &str) -> PathBuf {
        let p = self.tmp.path().join("repos").join(name);
        std::fs::create_dir_all(&p).unwrap();
        std::fs::canonicalize(p).unwrap()
    }

    fn project(&self, name: &str) -> (Project, PathBuf) {
        let root = self.folder(name);
        let p = self
            .db
            .create_project(
                NewProject {
                    name: name.into(),
                    root: root.to_string_lossy().to_string(),
                    description: String::new(),
                    facts_write: FactsWrite::Project,
                    inherit_common: true,
                },
                1,
            )
            .unwrap();
        (p, root)
    }

    fn state_file(&self, name: &str) -> PathBuf {
        self.home.join(name)
    }

    fn current_dir_in(&self, file: &std::path::Path) -> String {
        let raw =
            std::fs::read_to_string(file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        serde_json::from_str::<serde_json::Value>(&raw).unwrap()["currentDir"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

fn session(jid: &str, group_type: &str, project: Option<&Project>) -> GroupBinding {
    GroupBinding {
        jid: jid.into(),
        folder: "main".into(),
        name: jid.into(),
        channel: String::new(),
        group_type: group_type.into(),
        requires_trigger: false,
        allowed_tools: None,
        allowed_paths: None,
        allowed_work_dirs: None,
        bot_token: None,
        max_messages: None,
        llm_config_id: None,
        last_active: None,
        added_at: "2026-10-18T00:00:00Z".into(),
        project_id: project.map(|p| p.id.clone()),
    }
}

#[tokio::test]
async fn two_sessions_of_one_profile_in_two_projects_keep_their_own_folders() {
    let w = world();
    let (pa, root_a) = w.project("alpha");
    let (pb, root_b) = w.project("beta");
    // One agent profile ("main") serves all three sessions, as in real use.
    let a = session("web:main:a", "chat", Some(&pa));
    let b = session("web:main:b", "code", Some(&pb));
    let outside_dir = w.folder("elsewhere");
    let mut outside = session("web:main:c", "code", None);
    outside.allowed_work_dirs = Some(vec![outside_dir.to_string_lossy().to_string()]);

    for s in [&a, &b, &outside] {
        w.pool.get_or_create(s).await.unwrap();
    }

    // Each core works where its project is.
    assert_eq!(w.core.working_dir("web:main:a").as_deref(), root_a.to_str());
    assert_eq!(w.core.working_dir("web:main:b").as_deref(), root_b.to_str());
    assert_eq!(
        w.core.working_dir("web:main:c").as_deref(),
        outside_dir.to_str()
    );

    let (env_a, env_b, env_c) = (
        w.core.core_env("web:main:a"),
        w.core.core_env("web:main:b"),
        w.core.core_env("web:main:c"),
    );
    // The children are told the project, the default workspace and the only
    // folder the session may switch to.
    for (env, root, project) in [(&env_a, &root_a, &pa), (&env_b, &root_b, &pb)] {
        assert_eq!(env["SENCLAW_PROJECT_ROOT"], root.to_str().unwrap());
        assert_eq!(
            env["SENCLAW_PROJECT_BANK"],
            format!("project:{}", project.id)
        );
        assert_eq!(env["SENCLAW_DEFAULT_WORKSPACE"], root.to_str().unwrap());
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&env["SENCLAW_ALLOWED_WORK_DIRS"]).unwrap(),
            [root.to_str().unwrap()]
        );
    }
    assert!(
        !env_c.contains_key("SENCLAW_PROJECT_ROOT"),
        "a session outside a project is not told one"
    );
    assert!(!env_c.contains_key("SENCLAW_PROJECT_BANK"));

    // Three sessions, three files: what the workspace, docs, datagen and
    // dispatch tools read cannot be another session's.
    let files = [
        env_a["SENCLAW_WORKSPACE_STATE_FILE"].clone(),
        env_b["SENCLAW_WORKSPACE_STATE_FILE"].clone(),
        env_c["SENCLAW_WORKSPACE_STATE_FILE"].clone(),
    ];
    assert_eq!(
        files.iter().collect::<std::collections::HashSet<_>>().len(),
        3,
        "{files:?}"
    );
    assert_eq!(
        files[0],
        w.state_file(&crate::projects::workspace_state_file_name(
            &crate::projects::workspace_state_key(&a)
        ))
        .to_str()
        .unwrap()
    );
    assert_eq!(
        files[2],
        w.state_file("workspace-state-main.json").to_str().unwrap(),
        "outside a project the profile's file, as before"
    );

    assert_eq!(
        w.current_dir_in(&PathBuf::from(&files[0])),
        root_a.to_str().unwrap()
    );
    assert_eq!(
        w.current_dir_in(&PathBuf::from(&files[1])),
        root_b.to_str().unwrap()
    );
    assert_eq!(
        w.current_dir_in(&PathBuf::from(&files[2])),
        outside_dir.to_str().unwrap()
    );
}

#[tokio::test]
async fn building_a_project_core_never_touches_the_profiles_shared_file() {
    let w = world();
    let profile_file = w.state_file("workspace-state-main.json");
    std::fs::write(
        &profile_file,
        r#"{"currentDir":"/the/profiles/dir","updatedAt":""}"#,
    )
    .unwrap();
    let (p, _) = w.project("alpha");
    w.pool
        .get_or_create(&session("web:main:a", "code", Some(&p)))
        .await
        .unwrap();
    w.pool
        .get_or_create(&session("web:main:b", "chat", Some(&p)))
        .await
        .unwrap();
    assert_eq!(w.current_dir_in(&profile_file), "/the/profiles/dir");
}

#[tokio::test]
async fn a_project_session_restarts_in_its_root_even_if_its_file_says_otherwise() {
    let w = world();
    let (p, root) = w.project("alpha");
    let s = session("web:main:a", "chat", Some(&p));
    let own = w.state_file(&crate::projects::workspace_state_file_name(
        &crate::projects::workspace_state_key(&s),
    ));
    std::fs::write(&own, r#"{"currentDir":"/somewhere/else","updatedAt":""}"#).unwrap();
    w.pool.get_or_create(&s).await.unwrap();
    assert_eq!(w.core.working_dir(&s.jid).as_deref(), root.to_str());
    assert_eq!(w.current_dir_in(&own), root.to_str().unwrap());
}

#[tokio::test]
async fn a_chat_in_a_project_uses_the_root_not_the_profiles_default_or_configured_dirs() {
    let w = world();
    // The profile has switching allowed to some other place in config.json.
    std::fs::write(
        &w.config.paths.global_config_path,
        r#"{"agents":{"main":{"allowed_work_dirs":["/configured/elsewhere"]}}}"#,
    )
    .unwrap();
    let (p, root) = w.project("alpha");
    let mut s = session("web:main:a", "chat", Some(&p));
    s.allowed_work_dirs = Some(vec!["/client/sent/this".into()]);
    w.pool.get_or_create(&s).await.unwrap();
    let env = w.core.core_env(&s.jid);
    assert_eq!(w.core.working_dir(&s.jid).as_deref(), root.to_str());
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&env["SENCLAW_ALLOWED_WORK_DIRS"]).unwrap(),
        [root.to_str().unwrap()]
    );
}

#[tokio::test]
async fn a_root_that_is_gone_stops_the_session_instead_of_falling_back_to_another_folder() {
    let w = world();
    let (p, root) = w.project("alpha");
    std::fs::remove_dir_all(&root).unwrap();
    let s = session("web:main:a", "chat", Some(&p));
    let err = w.pool.get_or_create(&s).await.unwrap_err().to_string();
    assert!(
        err.contains("alpha") && err.contains("does not exist"),
        "{err}"
    );
    assert_eq!(w.core.working_dir(&s.jid), None, "no core was built");
    assert!(w.core.mcp.lock().unwrap().is_empty());
    assert!(!w.pool.state.lock().unwrap().cores.contains(&s.jid));
}

#[cfg(unix)]
#[tokio::test]
async fn a_root_that_became_a_forbidden_folder_is_refused_too() {
    let w = world();
    let (p, root) = w.project("alpha");
    // The folder is replaced by a link to somewhere a root may not be.
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::create_dir_all(&w.config.paths.profiles_dir).unwrap();
    std::os::unix::fs::symlink(&w.config.paths.profiles_dir, &root).unwrap();
    let err = w
        .pool
        .get_or_create(&session("web:main:a", "chat", Some(&p)))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("senclaw_profiles"), "{err}");
    assert_eq!(w.core.working_dir("web:main:a"), None);
}

#[tokio::test]
async fn a_session_whose_project_was_deleted_runs_as_an_ordinary_one() {
    let w = world();
    let (p, _) = w.project("alpha");
    let s = session("web:main:a", "chat", Some(&p));
    w.db.delete_project(&p.id).unwrap();
    w.pool.get_or_create(&s).await.unwrap();
    let env = w.core.core_env(&s.jid);
    assert!(!env.contains_key("SENCLAW_PROJECT_ROOT"));
    assert!(w.pool.state.lock().unwrap().cores.contains(&s.jid));
}

#[tokio::test]
async fn the_prompt_block_follows_the_context_file_every_turn() {
    let w = world();
    let (p, root) = w.project("alpha");
    let s = session("web:main:a", "chat", Some(&p));

    // No context yet: the block is empty and is still pushed (to clear any old one).
    w.pool.push_project_context(&s.jid, &s);
    assert_eq!(w.core.last_project_context(&s.jid), Some(None));

    let first = crate::projects::write_context(
        &w.config.paths.projects_dir,
        &p.id,
        "Dự án alpha: daemon Rust.",
        0,
    )
    .unwrap();
    w.pool.push_project_context(&s.jid, &s);
    let block = w
        .core
        .last_project_context(&s.jid)
        .unwrap()
        .expect("a block");
    assert!(
        block.starts_with(&format!(
            "<project name=\"alpha\" root=\"{}\">",
            root.display()
        )),
        "{block}"
    );
    assert!(block.contains("Dự án alpha: daemon Rust."));

    // An edit shows on the next turn with nothing restarted.
    crate::projects::write_context(
        &w.config.paths.projects_dir,
        &p.id,
        "Đã đổi sang Go.",
        first.updated_at,
    )
    .unwrap();
    w.pool.push_project_context(&s.jid, &s);
    let block = w.core.last_project_context(&s.jid).unwrap().unwrap();
    assert!(block.contains("Đã đổi sang Go.") && !block.contains("daemon Rust"));

    // The project goes away: the next turn pushes nothing.
    w.db.delete_project(&p.id).unwrap();
    w.pool.push_project_context(&s.jid, &s);
    assert_eq!(w.core.last_project_context(&s.jid), Some(None));
}

#[tokio::test]
async fn a_session_outside_any_project_is_always_pushed_no_block() {
    let w = world();
    let (p, _) = w.project("alpha");
    crate::projects::write_context(&w.config.paths.projects_dir, &p.id, "secret plans", 0).unwrap();
    let s = session("web:main:z", "chat", None);
    w.pool.push_project_context(&s.jid, &s);
    assert_eq!(
        w.core.last_project_context(&s.jid),
        Some(None),
        "another project's context never leaks in"
    );
}
