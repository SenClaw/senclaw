//! Orchestration: pick a backend, run the thing, record what happened.
//!
//! Everything the REST API and the MCP server can do goes through here, so the
//! two entry points cannot drift — a limit enforced in the HTTP handler but not
//! in the MCP tool would be no limit at all, since the agent uses MCP.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::sandbox::backend::{self, ExecSpec, Outcome, MAX_TIMEOUT_MS};
use crate::sandbox::caps::{self, DirectKind};
use crate::sandbox::code;
use crate::sandbox::config;
use crate::sandbox::db::{self, Db, NewSandbox, Run, Sandbox};

/// What `create` accepts. Anything omitted takes a documented default rather
/// than an implicit one.
pub struct CreateReq {
    pub name: Option<String>,
    pub backend: Option<String>,
    pub image: Option<String>,
    pub network: bool,
    pub cpus: Option<f64>,
    pub memory_mb: Option<i64>,
    pub timeout_ms: Option<i64>,
    pub env: Value,
    pub mounts: Vec<crate::sandbox::mounts::Mount>,
    /// `None` = take the app default from settings.
    pub fs_mode: Option<crate::sandbox::fsmode::FsMode>,
    pub ports: crate::sandbox::ports::PortPolicy,
}

pub async fn create(db: &Db, req: CreateReq) -> Result<Sandbox> {
    let defaults = crate::sandbox::settings::load(db);

    // Each branch probes only what it needs. Asking for the full picture here
    // would make every `sbx_run` — the most common call in the app — wait on
    // the Docker daemon, which on a machine with Docker Desktop broken costs
    // four seconds before a 40 ms snippet even starts.
    let direct = caps::direct_caps(false).await;

    let backend = match req.backend.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some("direct") => {
            if !direct.available {
                return Err(anyhow!("the `direct` backend is unavailable: {}", direct.detail));
            }
            "direct".to_string()
        }
        Some("docker") => {
            let docker = caps::docker_caps(false).await;
            if !docker.available {
                // Naming the measured reason beats "backend unavailable": the
                // usual cause is a stopped Docker Desktop, and that is fixable
                // in one click once the message says so.
                return Err(anyhow!("the `docker` backend is unavailable: {}", docker.detail));
            }
            "docker".to_string()
        }
        Some(other) => {
            return Err(anyhow!(
                "unknown backend `{other}`: only `direct` and `docker` exist"
            ))
        }
        None => {
            // An OS-enforced direct sandbox is the default, and settling it
            // here means the common path never touches Docker at all.
            if direct.kind.is_enforced() {
                "direct".to_string()
            } else {
                let docker = caps::docker_caps(false).await;
                if docker.available {
                    "docker".to_string()
                } else if direct.available {
                    // Degraded direct: usable, and honestly labelled on every
                    // run it produces.
                    "direct".to_string()
                } else {
                    return Err(anyhow!(
                        "this machine cannot run any sandbox yet. Docker: {} / Direct: {}",
                        docker.detail,
                        direct.detail
                    ));
                }
            }
        }
    };

    if backend == "docker" && req.image.is_none() && config::default_image().is_empty() {
        return Err(anyhow!("the docker backend needs an `image`"));
    }

    let id_dir = db::new_id();
    let workdir = config::workspaces_dir().join(&id_dir);
    std::fs::create_dir_all(workdir.join(".runs"))
        .map_err(|e| anyhow!("cannot create sandbox directory: {e}"))?;

    let image = if backend == "docker" {
        Some(req.image.unwrap_or_else(config::default_image))
    } else {
        None
    };

    let sb = db.create_sandbox(NewSandbox {
        name: req
            .name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("sandbox-{}", &id_dir[..8])),
        backend,
        image,
        workdir: workdir.to_string_lossy().to_string(),
        network: req.network,
        cpus: req.cpus.unwrap_or(defaults.default_cpus).clamp(0.1, 32.0),
        memory_mb: req
            .memory_mb
            .unwrap_or(defaults.default_memory_mb)
            .clamp(64, 65_536),
        pids_limit: 256,
        timeout_ms: req
            .timeout_ms
            .unwrap_or(defaults.default_timeout_ms)
            .clamp(1_000, MAX_TIMEOUT_MS as i64),
        env: req.env,
        mounts: req.mounts,
        fs_mode: req.fs_mode.unwrap_or(defaults.default_fs_mode),
        ports: req.ports,
    })?;
    Ok(sb)
}

/// Make the sandbox ready to accept a run.
///
/// For `direct` there is nothing to start. For `docker` this creates or adopts
/// the container — done lazily on first use rather than at creation, so making
/// a sandbox never blocks on an image pull.
pub async fn ensure_started(db: &Db, sb: &Sandbox) -> Result<Sandbox> {
    if sb.backend != "docker" {
        return Ok(sb.clone());
    }
    match backend::docker::start(sb).await {
        Ok(cid) => {
            db.set_status(&sb.id, "running", Some(&cid), None)?;
            db.sandbox(&sb.id)
        }
        Err(e) => {
            let msg = e.to_string();
            db.set_status(&sb.id, "error", None, Some(&msg))?;
            Err(anyhow!(msg))
        }
    }
}

pub async fn stop(db: &Db, sb: &Sandbox) -> Result<()> {
    if sb.backend == "docker" {
        backend::docker::stop(sb).await?;
    }
    db.set_status(&sb.id, "stopped", None, None)?;
    db.clear_container(&sb.id)?;
    Ok(())
}

/// Delete a sandbox. `purge` also removes its files — kept separate because
/// losing a workspace's contents to a mis-click is not recoverable.
pub async fn delete(db: &Db, sb: &Sandbox, purge: bool) -> Result<()> {
    let _ = stop(db, sb).await;
    if purge {
        let dir = PathBuf::from(&sb.workdir);
        // Refuse to recursively delete anything that is not one of our own
        // workspace directories, whatever the DB row says.
        if dir.starts_with(config::workspaces_dir()) && dir.parent() == Some(&config::workspaces_dir())
        {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    db.delete_sandbox(&sb.id)?;
    Ok(())
}

/// Program and arguments for a **shell command**, or `None` on Unix.
///
/// Unix feeds the script to `sh -s` on stdin and needs nothing here. Windows
/// has no `/bin/sh`, so the script is written to a `.cmd` file and `cmd.exe` is
/// pointed at it. Writing a file rather than building a command line keeps the
/// same guarantee as the Unix path: user text is never parsed as arguments.
pub fn shell_argv(sb: &Sandbox) -> Option<Vec<String>> {
    if !cfg!(windows) || sb.backend != "direct" {
        return None;
    }
    None // filled by `write_windows_script` at exec time, which has the script
}

/// Program and arguments for a **code snippet**, or `None` on Unix.
///
/// The interpreter is resolved here rather than inside the sandbox because on
/// Windows the AppContainer must be granted the interpreter's own directory
/// before launch — and to grant it, we have to know where it is.
pub async fn code_argv(lang: &'static crate::sandbox::code::Lang, rel: &str) -> Option<Vec<String>> {
    if !cfg!(windows) {
        return None;
    }
    for name in lang.interpreters {
        if let Some(path) = caps::which(name).await {
            return Some(vec![path, rel.to_string()]);
        }
    }
    None
}

/// Which isolation the `direct` backend would apply right now.
///
/// Deliberately the direct-only probe, not the full one: the full probe also
/// asks Docker, and on a machine with a broken Docker daemon that added four
/// seconds to every single direct run.
async fn direct_kind() -> DirectKind {
    caps::direct_caps(false).await.kind
}

/// Run a shell script in the sandbox and record the result.
pub async fn exec(
    db: &Db,
    sb: &Sandbox,
    script: &str,
    timeout_ms: Option<i64>,
    extra_env: BTreeMap<String, String>,
    kind: &str,
    language: Option<&str>,
    source: &str,
    argv: Option<Vec<String>>,
) -> Result<Run> {
    let timeout_ms = timeout_ms
        .unwrap_or(sb.timeout_ms)
        .clamp(1_000, MAX_TIMEOUT_MS as i64) as u64;

    // Tracing is opt-in per sandbox. When off, nothing is written into the
    // sandbox and no environment is injected — a traced run and an untraced one
    // must not behave differently by accident.
    let mut extra_env = extra_env;
    let session = if sb.trace_enabled {
        match crate::sandbox::trace::Session::begin(std::path::Path::new(&sb.workdir)) {
            Ok(s) => {
                // The workload reads these paths from inside, so they are the
                // paths *it* sees: the host path for direct, /work for docker.
                let home = if sb.backend == "docker" {
                    backend::docker::WORK.to_string()
                } else {
                    sb.workdir.clone()
                };
                for (k, v) in crate::sandbox::trace::env(&home) {
                    extra_env.entry(k).or_insert(v);
                }
                Some(s)
            }
            // Failing to set up tracing must not fail the run — the user asked
            // to run code, and observing it is the secondary goal.
            Err(_) => None,
        }
    } else {
        None
    };

    // On Windows a shell command still needs a program to run. The script goes
    // to a `.cmd` file and `cmd.exe` is pointed at it — never at a command line
    // built from the user's text.
    let argv = match argv {
        Some(a) => Some(a),
        None if cfg!(windows) && sb.backend == "direct" => {
            let rel = format!(".runs/{}.cmd", db::new_id());
            crate::sandbox::files::write(
                &crate::sandbox::files::Scope::of(sb),
                &rel,
                &format!("@echo off\r\n{}\r\n", script.replace('\n', "\r\n")),
            )
            .map_err(|e| anyhow!("cannot write the script: {e}"))?;
            let cmd_exe = std::env::var("COMSPEC")
                .unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".to_string());
            Some(vec![cmd_exe, "/d".into(), "/c".into(), rel])
        }
        None => None,
    };

    let spec = ExecSpec {
        script: script.to_string(),
        timeout_ms,
        extra_env,
        argv,
    };

    let outcome: Outcome = match sb.backend.as_str() {
        "docker" => {
            let sb = ensure_started(db, sb).await?;
            backend::docker::exec(&sb, &spec).await
        }
        "direct" => {
            // The allowlist only matters in `allowlist` mode, but it is read
            // per run either way so a change in settings takes effect on the
            // next run rather than on the next sandbox.
            let allow = crate::sandbox::settings::load(db).allowlist;
            #[cfg(windows)]
            {
                backend::direct_windows::exec(sb, &spec, &allow).await
            }
            #[cfg(not(windows))]
            {
                backend::direct::exec(sb, &spec, direct_kind().await, &allow).await
            }
        }
        other => return Err(anyhow!("invalid backend `{other}`")),
    };

    let run = Run {
        id: db::new_id(),
        sandbox_id: sb.id.clone(),
        kind: kind.to_string(),
        language: language.map(str::to_string),
        source: source.to_string(),
        exit_code: outcome.exit_code.map(|c| c as i64),
        stdout: outcome.stdout,
        stderr: outcome.stderr,
        truncated: outcome.truncated,
        timed_out: outcome.timed_out,
        isolation: outcome.isolation,
        network: sb.network,
        duration_ms: outcome.duration_ms,
        created_at: db::now_ms(),
    };
    db.insert_run(&run)?;
    db.touch(&sb.id)?;

    if let Some(session) = session {
        let (events, truncated) = session.finish();
        if truncated {
            // Say so rather than letting a silently capped timeline read as
            // "that is everything the code did".
            let _ = db.insert_events(
                &sb.id,
                &run.id,
                &[crate::sandbox::trace::Event {
                    ts_ms: db::now_ms(),
                    pid: 0,
                    source: "senclaw".into(),
                    kind: "trace.truncated".into(),
                    target: format!("limit of {} events per run", crate::sandbox::trace::MAX_EVENTS),
                    detail: "further events were not recorded".into(),
                }],
            );
        }
        let _ = db.insert_events(&sb.id, &run.id, &events);
    }

    Ok(run)
}

/// Run a snippet in a named language.
pub async fn run_code(
    db: &Db,
    sb: &Sandbox,
    language: &str,
    source: &str,
    timeout_ms: Option<i64>,
    extra_env: BTreeMap<String, String>,
) -> Result<Run> {
    let lang = code::lookup(language)?;
    // The runtime switches from Plugins → Sandbox are enforced here because
    // every entry point (REST, MCP, REPL, scheduler) funnels through this
    // function — a gate anywhere shallower would have a way around it.
    let p = crate::sandbox::policy::load(db);
    match lang.name {
        "python" if !p.run_python => {
            return Err(anyhow!(
                "Python execution is switched off (Plugins → Sandbox → Run Python)"
            ))
        }
        "javascript" | "typescript" if !p.run_node => {
            return Err(anyhow!(
                "Node.js execution is switched off (Plugins → Sandbox → Run Node.js)"
            ))
        }
        _ => {}
    }
    let run_id = db::new_id();
    let rel = code::source_path(&run_id, lang);

    // Written on the host — the same directory the container sees at /work.
    crate::sandbox::files::write(&crate::sandbox::files::Scope::of(sb), &rel, source)
        .map_err(|e| anyhow!("cannot write the source file: {e}"))?;

    let script = code::launch_script(lang, &rel);
    exec(
        db,
        sb,
        &script,
        timeout_ms,
        extra_env,
        "code",
        Some(lang.name),
        source,
        code_argv(lang, &rel).await,
    )
    .await
}

/// A one-shot run in a throwaway sandbox: create, run, delete.
///
/// This is the call an agent reaches for most ("run this Python snippet"), and
/// doing it in one step keeps it from leaving a sandbox behind on every
/// question. Cleanup runs even when the snippet fails.
pub async fn run_once(
    db: &Db,
    language: &str,
    source: &str,
    backend_name: Option<String>,
    network: bool,
    timeout_ms: Option<i64>,
) -> Result<(Run, Sandbox)> {
    let sb = create(
        db,
        CreateReq {
            name: Some("one-shot".into()),
            backend: backend_name,
            image: None,
            network,
            cpus: None,
            memory_mb: None,
            timeout_ms,
            env: json!({}),
            mounts: Vec::new(),
            fs_mode: None,
            ports: Default::default(),
        },
    )
    .await?;

    let result = run_code(db, &sb, language, source, timeout_ms, BTreeMap::new()).await;
    // The sandbox goes away whether or not the snippet worked. Its run rows go
    // with it (the `runs` foreign key cascades), which is the right trade: a
    // throwaway sandbox should not leave history behind. The output is not lost
    // — it travels back in the returned `Run` value.
    let _ = delete(db, &sb, true).await;
    result.map(|r| (r, sb))
}

/// Install packages inside a sandbox. Convenience over `exec`, but it picks the
/// manager instead of making the caller remember whether the image has `apt`.
pub async fn install(
    db: &Db,
    sb: &Sandbox,
    manager: &str,
    packages: &[String],
    timeout_ms: Option<i64>,
) -> Result<Run> {
    if packages.is_empty() {
        return Err(anyhow!("no packages given"));
    }
    // Package names go through the same no-interpolation rule as everything
    // else: anything that is not a plausible package spec is refused rather
    // than quoted, because quoting is where command builders go wrong.
    for p in packages {
        if !p.chars().all(|c| c.is_ascii_alphanumeric() || "-_.+=<>[]!~/:@".contains(c)) {
            return Err(anyhow!("invalid package name: `{p}`"));
        }
    }
    let list = packages.join(" ");
    let script = match manager.trim().to_lowercase().as_str() {
        "pip" | "pip3" | "python" => format!("set -e\npython3 -m pip install --no-input {list}\n"),
        "npm" | "node" => format!("set -e\nnpm install {list}\n"),
        "apt" | "apt-get" => format!(
            "set -e\nif ! command -v apt-get >/dev/null 2>&1; then \
               echo 'this image has no apt-get' >&2; exit 127; fi\n\
             apt-get update -qq\nDEBIAN_FRONTEND=noninteractive apt-get install -y -qq {list}\n"
        ),
        other => return Err(anyhow!("package manager `{other}` is not supported (pip, npm, apt)")),
    };

    if !sb.network {
        return Err(anyhow!(
            "sandbox `{}` has the network off, so packages cannot be installed — turn the network on and retry",
            sb.name
        ));
    }
    // Installs are slower than a normal run; default them higher rather than
    // letting a 30s default kill an apt-get halfway through.
    let t = timeout_ms.or(Some(300_000));
    exec(
        db,
        sb,
        &script,
        t,
        BTreeMap::new(),
        "exec",
        None,
        &format!("{manager} install {list}"),
        shell_argv(sb),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One data root for the whole test binary.
    ///
    /// `SANDBOX_DATA_DIR` is process-global, so a per-test temp dir is a race:
    /// tests run in parallel, one test's `set_var` moves the root out from under
    /// another test that already created its sandbox there. Each sandbox gets a
    /// UUID subdirectory anyway, so a shared root keeps them from colliding.
    fn tmp_data() -> &'static std::path::Path {
        static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        let d = ROOT.get_or_init(|| {
            let d = tempfile::tempdir().unwrap();
            std::env::set_var("SANDBOX_DATA_DIR", d.path());
            d
        });
        d.path()
    }

    #[tokio::test]
    async fn create_rejects_an_unknown_backend_by_name() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let e = create(&db, req(Some("qemu".into()))).await.unwrap_err().to_string();
        assert!(e.contains("qemu"));
    }

    #[tokio::test]
    async fn create_explains_why_docker_is_unusable_rather_than_saying_no() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let caps = caps::probe(true).await;
        if caps.docker.available {
            return; // Docker is up on this machine; nothing to assert.
        }
        let e = create(&db, req(Some("docker".into()))).await.unwrap_err().to_string();
        assert!(
            e.contains("daemon") || e.contains("Docker") || e.contains("PATH"),
            "the refusal must carry the measured reason, got: {e}"
        );
    }

    #[tokio::test]
    async fn create_defaults_the_backend_and_makes_the_workdir() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let sb = create(&db, req(None)).await.unwrap();
        assert!(PathBuf::from(&sb.workdir).is_dir());
        assert!(sb.workdir.starts_with(config::workspaces_dir().to_str().unwrap()));
        assert!(!sb.network, "network must be off unless asked for");
    }

    #[tokio::test]
    async fn limits_are_clamped_at_creation() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let mut r = req(None);
        r.cpus = Some(999.0);
        r.memory_mb = Some(1);
        r.timeout_ms = Some(1);
        let sb = create(&db, r).await.unwrap();
        assert_eq!(sb.cpus, 32.0);
        assert_eq!(sb.memory_mb, 64);
        assert_eq!(sb.timeout_ms, 1_000);
    }

    #[tokio::test]
    async fn install_refuses_shell_metacharacters_in_package_names() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let sb = create(&db, req(None)).await.unwrap();
        let bad = vec!["requests; rm -rf /".to_string()];
        let e = install(&db, &sb, "pip", &bad, None).await.unwrap_err().to_string();
        assert!(e.contains("invalid package name"));
    }

    #[tokio::test]
    async fn install_without_network_is_refused_before_it_runs() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let sb = create(&db, req(None)).await.unwrap();
        let e = install(&db, &sb, "pip", &["requests".into()], None)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("network off"));
    }

    #[tokio::test]
    async fn delete_with_purge_only_touches_our_own_workspace_dir() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let sb = create(&db, req(None)).await.unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep.txt"), "x").unwrap();

        // A row whose workdir points somewhere else must not lead to that
        // directory being wiped.
        let mut evil = sb.clone();
        evil.workdir = outside.path().to_string_lossy().to_string();
        delete(&db, &evil, true).await.unwrap();
        assert!(outside.path().join("keep.txt").exists(), "purge escaped the workspace root");
    }

    // ── end-to-end: these actually execute code under the real OS sandbox ────
    //
    // They are the only tests that can tell whether the confinement works;
    // asserting on a generated Seatbelt profile proves the text is right, not
    // that the kernel enforced it. They skip themselves when the host offers no
    // enforced direct backend, so CI on a bare container stays green — a skip
    // is printed rather than silently passing.

    async fn enforced_sandbox(db: &Db) -> Option<Sandbox> {
        let caps = caps::probe(true).await;
        if !caps.direct.kind.is_enforced() {
            eprintln!("SKIP: this machine has no enforced direct isolation ({})", caps.direct.detail);
            return None;
        }
        Some(create(db, req(Some("direct".into()))).await.unwrap())
    }

    #[tokio::test]
    async fn python_actually_runs_and_returns_its_output() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let Some(sb) = enforced_sandbox(&db).await else { return };

        let run = run_code(&db, &sb, "python", "print(6 * 7)", None, BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(run.exit_code, Some(0), "stderr: {}", run.stderr);
        assert_eq!(run.stdout.trim(), "42");
        assert!(run.isolation == "seatbelt" || run.isolation == "bubblewrap");
    }

    #[tokio::test]
    async fn a_snippet_full_of_quotes_survives_intact() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let Some(sb) = enforced_sandbox(&db).await else { return };

        // Every quoting style at once. This is the payload that breaks any
        // implementation building a `sh -c "…"` command line.
        let code = r#"print("it's \"quoted\"", '$(whoami)', "`id`", 'a;b|c&d')"#;
        let run = run_code(&db, &sb, "python", code, None, BTreeMap::new()).await.unwrap();
        assert_eq!(run.exit_code, Some(0), "stderr: {}", run.stderr);
        assert_eq!(run.stdout.trim(), "it's \"quoted\" $(whoami) `id` a;b|c&d");
    }

    #[tokio::test]
    async fn writing_inside_the_sandbox_is_allowed() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let Some(sb) = enforced_sandbox(&db).await else { return };

        let run = run_code(
            &db,
            &sb,
            "python",
            "open('made.txt','w').write('ok'); print('wrote')",
            None,
            BTreeMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(run.exit_code, Some(0), "stderr: {}", run.stderr);
        assert!(PathBuf::from(&sb.workdir).join("made.txt").exists());
    }

    #[tokio::test]
    async fn writing_outside_the_sandbox_is_blocked_by_the_os() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let Some(sb) = enforced_sandbox(&db).await else { return };

        // A path the test process can definitely write to, so a success here
        // means the sandbox failed rather than the path being bad.
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("escaped.txt");
        assert!(std::fs::write(&target, "control").is_ok(), "test setup");
        std::fs::remove_file(&target).unwrap();

        let code = format!(
            "open({:?},'w').write('escaped')\nprint('WROTE OUTSIDE')",
            target.to_string_lossy()
        );
        let run = run_code(&db, &sb, "python", &code, None, BTreeMap::new()).await.unwrap();

        assert!(
            !target.exists(),
            "sandboxed code wrote outside its directory — isolation is not working"
        );
        assert_ne!(run.exit_code, Some(0), "the write should have failed");
    }

    #[tokio::test]
    async fn a_runaway_loop_is_killed_at_the_deadline() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let Some(sb) = enforced_sandbox(&db).await else { return };

        let started = std::time::Instant::now();
        let run = run_code(
            &db,
            &sb,
            "python",
            "while True: pass",
            Some(1_500),
            BTreeMap::new(),
        )
        .await
        .unwrap();
        assert!(run.timed_out, "a busy loop must hit the deadline");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "the kill did not happen near the deadline"
        );
    }

    #[tokio::test]
    async fn the_network_is_off_unless_the_sandbox_enables_it() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let Some(sb) = enforced_sandbox(&db).await else { return };

        // A TCP connect to a public address. With the network denied this fails
        // fast; the short timeout keeps an offline CI machine from waiting.
        let code = "import socket\n\
                    s=socket.socket(); s.settimeout(4)\n\
                    s.connect(('1.1.1.1', 53))\n\
                    print('CONNECTED')\n";
        let run = run_code(&db, &sb, "python", code, Some(15_000), BTreeMap::new())
            .await
            .unwrap();
        assert!(
            !run.stdout.contains("CONNECTED"),
            "network reached from a sandbox created with network off"
        );
    }

    #[tokio::test]
    async fn the_daemons_secrets_are_not_visible_to_sandboxed_code() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        std::env::set_var("ANTHROPIC_API_KEY", "sk-should-never-be-seen");
        let Some(sb) = enforced_sandbox(&db).await else {
            std::env::remove_var("ANTHROPIC_API_KEY");
            return;
        };

        let run = run_code(
            &db,
            &sb,
            "python",
            "import os; print(os.environ.get('ANTHROPIC_API_KEY', 'ABSENT'))",
            None,
            BTreeMap::new(),
        )
        .await
        .unwrap();
        std::env::remove_var("ANTHROPIC_API_KEY");
        assert_eq!(run.stdout.trim(), "ABSENT", "an API key leaked into the sandbox");
    }

    /// Read a file that exists outside the sandbox and report which it was.
    const READ_PROBE: &str = "import sys\n\
                              try:\n    print('READ:', open(sys.argv[1] if len(sys.argv)>1 else PATH).read().strip())\n\
                              except Exception as e:\n    print('DENIED')\n";

    fn read_probe(path: &std::path::Path) -> String {
        format!("PATH = {:?}\n{READ_PROBE}", path.to_string_lossy())
    }

    #[tokio::test]
    async fn an_opened_port_can_be_served_on_and_reached_from_this_machine() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let caps = caps::direct_caps(true).await;
        if caps.kind != DirectKind::Seatbelt {
            eprintln!("SKIP: per-port rules are only enforced by Seatbelt here");
            return;
        }
        // A high port unlikely to collide with anything on the machine.
        const PORT: u16 = 18771;
        let mut r = req(Some("direct".into()));
        r.ports = crate::sandbox::ports::validate(&[PORT], &[], &[]).unwrap();
        let sb = create(&db, r).await.unwrap();

        // Serve one request, then exit. The sandbox has no general network —
        // only this port is open — so a reply proves the port rule works.
        // `allow_reuse_address` and a server-side timeout so this can never
        // leave a listener behind: if the client never knocks, the server exits
        // on its own rather than outliving the test as an orphan.
        let code = format!(
            "import http.server, socketserver\n\
             class S(socketserver.TCPServer):\n\
             \x20   allow_reuse_address = True\n\
             \x20   timeout = 10\n\
             class H(http.server.BaseHTTPRequestHandler):\n\
             \x20   def do_GET(s):\n\
             \x20       s.send_response(200); s.end_headers(); s.wfile.write(b'SERVED')\n\
             \x20   def log_message(s, *a): pass\n\
             with S(('127.0.0.1', {PORT}), H) as sv:\n\
             \x20   sv.handle_request()\n"
        );
        let db2 = db.clone();
        let sb2 = sb.clone();
        let server = tokio::spawn(async move {
            run_code(&db2, &sb2, "python", &code, Some(20_000), BTreeMap::new()).await
        });

        // Give the server a moment, then knock on the door from outside.
        let mut body = String::new();
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            if let Ok(resp) = reqwest::get(format!("http://127.0.0.1:{PORT}/")).await {
                body = resp.text().await.unwrap_or_default();
                break;
            }
        }
        let run = server.await.unwrap().unwrap();
        assert!(
            body.contains("SERVED"),
            "an opened port must be reachable from this machine. stderr: {}",
            run.stderr
        );
    }

    #[tokio::test]
    async fn a_port_that_was_not_opened_cannot_be_bound() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        if caps::direct_caps(true).await.kind != DirectKind::Seatbelt {
            return;
        }
        let mut r = req(Some("direct".into()));
        // 18772 is open; 18773 is not.
        r.ports = crate::sandbox::ports::validate(&[18772], &[], &[]).unwrap();
        let sb = create(&db, r).await.unwrap();

        let probe = |port: u16| {
            format!(
                "import socket\n\
                 s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\n\
                 try:\n\x20   s.bind(('127.0.0.1', {port})); print('BOUND')\n\
                 except Exception:\n\x20   print('REFUSED')\n"
            )
        };
        let ok = run_code(&db, &sb, "python", &probe(18772), None, BTreeMap::new())
            .await
            .unwrap();
        assert!(ok.stdout.contains("BOUND"), "the opened port must bind: {}", ok.stderr);

        let no = run_code(&db, &sb, "python", &probe(18773), None, BTreeMap::new())
            .await
            .unwrap();
        assert!(
            no.stdout.contains("REFUSED"),
            "a port that was never opened must not bind — port isolation is not working: {}",
            no.stdout
        );
    }

    /// The escape this guards against was demonstrated against a live daemon:
    /// with `network: true`, code inside a sandbox reached SenClaw's own REST
    /// API on 127.0.0.1 — which needs no credentials, because its trust
    /// boundary is the loopback interface — read configuration it could not
    /// read off the disk, and created itself a second sandbox with the whole
    /// disk mounted. A file-read deny means nothing if the daemon will fetch
    /// the file for you.
    ///
    /// This runs a real listener on this machine and checks the sandbox cannot
    /// reach it, network switch on and all — then that naming the port in
    /// `loopback` hands exactly that one service back.
    #[tokio::test]
    async fn this_machines_services_are_unreachable_even_with_the_network_on() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        if caps::direct_caps(true).await.kind != DirectKind::Seatbelt {
            eprintln!("SKIP: loopback egress rules are only enforced by Seatbelt here");
            return;
        }
        // Stand-in for the daemon: a listener owned by this test, so nothing
        // outside it is touched.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepting = std::thread::spawn(move || {
            listener.set_nonblocking(false).ok();
            // Accept for a bounded time so the thread can never outlive the test.
            let _ = listener
                .incoming()
                .take(2)
                .for_each(|c| drop(c));
        });

        let probe = format!(
            "import socket\n\
             s = socket.socket(); s.settimeout(4)\n\
             try:\n\x20   s.connect(('127.0.0.1', {port})); print('REACHED')\n\
             except Exception:\n\x20   print('BLOCKED')\n"
        );

        let mut r = req(Some("direct".into()));
        r.network = true;
        let open = create(&db, r).await.unwrap();
        let out = run_code(&db, &open, "python", &probe, None, BTreeMap::new())
            .await
            .unwrap();
        assert!(
            out.stdout.contains("BLOCKED"),
            "network:true must not include this machine's own services — that is a \
             sandbox escape, not a network. stdout: {} stderr: {}",
            out.stdout,
            out.stderr
        );

        let mut r2 = req(Some("direct".into()));
        r2.network = true;
        r2.ports = crate::sandbox::ports::validate(&[], &[], &[port]).unwrap();
        let named = create(&db, r2).await.unwrap();
        let out2 = run_code(&db, &named, "python", &probe, None, BTreeMap::new())
            .await
            .unwrap();
        assert!(
            out2.stdout.contains("REACHED"),
            "a port named in `loopback` must be reachable, or an egress proxy — the \
             only way to restrict a sandbox to one website — cannot work. stdout: {} stderr: {}",
            out2.stdout,
            out2.stderr
        );
        // Unblock the accept loop if nothing connected.
        let _ = std::net::TcpStream::connect(("127.0.0.1", port));
        let _ = accepting.join();
    }

    #[tokio::test]
    async fn connect_rules_open_one_remote_port_and_no_other() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        if caps::direct_caps(true).await.kind != DirectKind::Seatbelt {
            return;
        }
        let mut r = req(Some("direct".into()));
        // DNS out, nothing else. Note the sandbox's `network` flag stays false:
        // the port rule is the entire permission.
        r.ports = crate::sandbox::ports::validate(&[], &[53], &[]).unwrap();
        let sb = create(&db, r).await.unwrap();

        let probe = |port: u16| {
            format!(
                "import socket\n\
                 s = socket.socket(); s.settimeout(4)\n\
                 try:\n\x20   s.connect(('1.1.1.1', {port})); print('CONNECTED')\n\
                 except Exception:\n\x20   print('BLOCKED')\n"
            )
        };
        let blocked = run_code(&db, &sb, "python", &probe(443), Some(15_000), BTreeMap::new())
            .await
            .unwrap();
        assert!(
            !blocked.stdout.contains("CONNECTED"),
            "a remote port that was not opened must stay closed: {}",
            blocked.stdout
        );
        // The allowed direction is checked in the opposite sense: on a machine
        // with no internet it legitimately fails, so only the block is asserted
        // as a hard rule, and the allow is reported.
        let allowed = run_code(&db, &sb, "python", &probe(53), Some(15_000), BTreeMap::new())
            .await
            .unwrap();
        eprintln!("connect :53 (allowed) → {}", allowed.stdout.trim());
    }

    #[tokio::test]
    async fn tracing_records_files_processes_and_network_from_a_real_run() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let Some(sb) = enforced_sandbox(&db).await else { return };
        let sb = db.set_trace(&sb.id, true).unwrap();

        let run = run_code(
            &db,
            &sb,
            "python",
            "import subprocess, socket\n\
             open('result.txt','w').write('xin chao')\n\
             open('result.txt').read()\n\
             subprocess.run(['/bin/echo','hi'], capture_output=True)\n\
             s = socket.socket(); s.settimeout(2)\n\
             try:\n    s.connect(('1.1.1.1', 53))\n\
             except Exception:\n    pass\n\
             try:\n    socket.getaddrinfo('example.invalid', 80)\n\
             except Exception:\n    pass\n",
            Some(20_000),
            BTreeMap::new(),
        )
        .await
        .unwrap();

        let events = db.list_events(&sb.id, Some(&run.id), None, 500).unwrap();
        let has = |kind: &str, needle: &str| {
            events
                .iter()
                .any(|e| e.kind == kind && (e.target.contains(needle) || e.detail.contains(needle)))
        };

        assert!(has("file.write", "result.txt"), "no write event: {events:#?}");
        assert!(has("file.read", "result.txt"), "no read event");
        assert!(has("proc.spawn", "echo"), "no process event");
        assert!(has("net.connect", "1.1.1.1"), "no network event with the address");
        assert!(has("net.dns", "example.invalid"), "no DNS event with the hostname");

        // The point of the noise filter: a handful of meaningful events, not
        // several hundred lines of CPython loading its own standard library.
        assert!(
            events.len() < 60,
            "the timeline is drowning in interpreter noise: {} events",
            events.len()
        );
    }

    #[tokio::test]
    async fn tracing_is_off_by_default_and_leaves_no_trace_directory() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let Some(sb) = enforced_sandbox(&db).await else { return };
        assert!(!sb.trace_enabled, "tracing must be opt-in");

        let run = run_code(&db, &sb, "python", "print(1)", None, BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(run.exit_code, Some(0));
        assert!(db.list_events(&sb.id, None, None, 50).unwrap().is_empty());
        // An untraced run must not differ from one on a build without the
        // feature — no shim files, no injected environment.
        assert!(
            !PathBuf::from(&sb.workdir).join(".trace").exists(),
            "tracing wrote into a sandbox that never asked for it"
        );
    }

    #[tokio::test]
    async fn a_second_traced_run_does_not_replay_the_first() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let Some(sb) = enforced_sandbox(&db).await else { return };
        let sb = db.set_trace(&sb.id, true).unwrap();

        let r1 = run_code(&db, &sb, "python", "open('one.txt','w').write('1')", None, BTreeMap::new())
            .await
            .unwrap();
        let r2 = run_code(&db, &sb, "python", "open('two.txt','w').write('2')", None, BTreeMap::new())
            .await
            .unwrap();

        let e2 = db.list_events(&sb.id, Some(&r2.id), None, 200).unwrap();
        assert!(e2.iter().any(|e| e.target.contains("two.txt")));
        assert!(
            !e2.iter().any(|e| e.target.contains("one.txt")),
            "the log offset is not being honoured — run 1's events leaked into run 2"
        );
        assert!(db
            .list_events(&sb.id, Some(&r1.id), None, 200)
            .unwrap()
            .iter()
            .any(|e| e.target.contains("one.txt")));
    }

    #[tokio::test]
    async fn the_directory_diff_catches_writes_from_a_non_python_program() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let Some(sb) = enforced_sandbox(&db).await else { return };
        let sb = db.set_trace(&sb.id, true).unwrap();

        // A plain shell redirect: no Python, no Node, so only the snapshot diff
        // can see it. This is the fallback that makes tracing useful for any
        // language rather than only the two with hooks.
        let run = runner_exec_shell(&db, &sb, "echo content > from-shell.txt").await;
        assert_eq!(run.exit_code, Some(0), "stderr: {}", run.stderr);

        let events = db.list_events(&sb.id, Some(&run.id), None, 200).unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.source == "diff" && e.target.contains("from-shell.txt")),
            "the diff missed a file the shell created: {events:#?}"
        );
    }

    async fn runner_exec_shell(db: &Db, sb: &Sandbox, cmd: &str) -> Run {
        exec(db, sb, cmd, None, BTreeMap::new(), "exec", None, cmd, shell_argv(sb))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn strict_mode_blocks_reading_the_rest_of_the_disk() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("private.txt");
        std::fs::write(&secret, "private data").unwrap();

        if !caps::direct_caps(true).await.kind.is_enforced() {
            eprintln!("SKIP: this machine has no enforced direct isolation");
            return;
        }
        let mut r = req(Some("direct".into()));
        r.fs_mode = Some(crate::sandbox::fsmode::FsMode::Strict);
        let sb = create(&db, r).await.unwrap();

        let run = run_code(&db, &sb, "python", &read_probe(&secret), None, BTreeMap::new())
            .await
            .unwrap();
        assert!(
            !run.stdout.contains("private data"),
            "strict mode read a file outside the sandbox: {}",
            run.stdout
        );
        assert!(run.stdout.contains("DENIED"), "stdout: {} stderr: {}", run.stdout, run.stderr);
    }

    #[tokio::test]
    async fn strict_mode_still_lets_the_interpreter_start() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        if !caps::direct_caps(true).await.kind.is_enforced() {
            return;
        }
        let mut r = req(Some("direct".into()));
        r.fs_mode = Some(crate::sandbox::fsmode::FsMode::Strict);
        let sb = create(&db, r).await.unwrap();

        // The whole risk of a read jail is jailing the interpreter out of its
        // own standard library. Importing a few stdlib modules proves it did not.
        let run = run_code(
            &db,
            &sb,
            "python",
            "import json, os, socket, base64, sqlite3\nprint('STDLIB OK', json.dumps([1,2])) ",
            None,
            BTreeMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(run.exit_code, Some(0), "stderr: {}", run.stderr);
        assert!(run.stdout.contains("STDLIB OK"));
    }

    #[tokio::test]
    async fn open_mode_can_read_the_disk_and_strict_is_the_difference() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let f = outside.path().join("record.txt");
        std::fs::write(&f, "noi dung").unwrap();

        if !caps::direct_caps(true).await.kind.is_enforced() {
            return;
        }
        let mut r = req(Some("direct".into()));
        r.fs_mode = Some(crate::sandbox::fsmode::FsMode::Open);
        let sb = create(&db, r).await.unwrap();

        let run = run_code(&db, &sb, "python", &read_probe(&f), None, BTreeMap::new())
            .await
            .unwrap();
        // This is the mode that trades the read boundary away; if it cannot read
        // then `strict` proves nothing by comparison.
        assert!(
            run.stdout.contains("noi dung"),
            "open mode should read outside the sandbox: {} / {}",
            run.stdout,
            run.stderr
        );
    }

    #[tokio::test]
    async fn allowlist_mode_opens_exactly_the_configured_folder_and_no_more() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let allowed = outside.path().join("duoc-phep");
        let denied = outside.path().join("khong-duoc");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&denied).unwrap();
        std::fs::write(allowed.join("a.txt"), "allowed").unwrap();
        std::fs::write(denied.join("b.txt"), "forbidden").unwrap();

        if !caps::direct_caps(true).await.kind.is_enforced() {
            return;
        }
        // `canonicalize` because the allowlist is matched on the resolved path,
        // and a temp dir on macOS is reached through a symlink.
        let allowed_real = allowed.canonicalize().unwrap();
        crate::sandbox::settings::save(
            &db,
            &crate::sandbox::settings::Settings {
                allowlist: vec![allowed_real.to_string_lossy().to_string()],
                ..Default::default()
            },
        )
        .unwrap();

        let mut r = req(Some("direct".into()));
        r.fs_mode = Some(crate::sandbox::fsmode::FsMode::Allowlist);
        let sb = create(&db, r).await.unwrap();

        let ok = run_code(
            &db,
            &sb,
            "python",
            &read_probe(&allowed_real.join("a.txt")),
            None,
            BTreeMap::new(),
        )
        .await
        .unwrap();
        assert!(
            ok.stdout.contains("allowed"),
            "the allowlisted folder must be readable: {} / {}",
            ok.stdout,
            ok.stderr
        );

        let no = run_code(
            &db,
            &sb,
            "python",
            &read_probe(&denied.canonicalize().unwrap().join("b.txt")),
            None,
            BTreeMap::new(),
        )
        .await
        .unwrap();
        assert!(
            !no.stdout.contains("forbidden"),
            "allowlist mode leaked a folder that was not on the list: {}",
            no.stdout
        );
    }

    #[tokio::test]
    async fn a_mount_is_readable_even_in_strict_mode() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let host = tempfile::tempdir().unwrap();
        let shared = host.path().join("mounted");
        std::fs::create_dir(&shared).unwrap();
        std::fs::write(shared.join("c.txt"), "via mount").unwrap();

        if !caps::direct_caps(true).await.kind.is_enforced() {
            return;
        }
        let mut r = req(Some("direct".into()));
        r.fs_mode = Some(crate::sandbox::fsmode::FsMode::Strict);
        r.mounts = vec![crate::sandbox::mounts::validate(shared.to_str().unwrap(), "mounted", false).unwrap()];
        let sb = create(&db, r).await.unwrap();

        // Strict blocks the disk; a mount is the sanctioned way back in, and if
        // it did not work the feature would be pointless under the new default.
        let run = run_code(
            &db,
            &sb,
            "python",
            "print(open('mounted/c.txt').read())",
            None,
            BTreeMap::new(),
        )
        .await
        .unwrap();
        assert!(
            run.stdout.contains("via mount"),
            "a mount must stay readable under strict: {} / {}",
            run.stdout,
            run.stderr
        );
    }

    #[tokio::test]
    async fn toggling_the_network_changes_what_the_next_run_can_reach() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let Some(sb) = enforced_sandbox(&db).await else { return };

        const PROBE: &str = "import socket\n\
                             s=socket.socket(); s.settimeout(4)\n\
                             try:\n    s.connect(('1.1.1.1',53)); print('CONNECTED')\n\
                             except Exception as e:\n    print('BLOCKED')\n";

        // Off at creation.
        let off = run_code(&db, &sb, "python", PROBE, Some(15_000), BTreeMap::new())
            .await
            .unwrap();
        assert!(!off.stdout.contains("CONNECTED"), "network reachable while off");

        // Flip it on. The sandbox already exists — this is the case that breaks
        // if the confinement is built once at creation and cached rather than
        // regenerated per run.
        let sb = db
            .update_limits(&sb.id, None, Some(true), None, None, None, None)
            .unwrap();
        let on = run_code(&db, &sb, "python", PROBE, Some(15_000), BTreeMap::new())
            .await
            .unwrap();
        // A machine with no internet legitimately reports BLOCKED here, so the
        // assertion is on the flag that travelled with the run, plus the
        // stronger direction (off really blocks) checked above and below.
        assert!(on.network, "the run did not pick up the new setting");

        // And back off again.
        let sb = db
            .update_limits(&sb.id, None, Some(false), None, None, None, None)
            .unwrap();
        let off2 = run_code(&db, &sb, "python", PROBE, Some(15_000), BTreeMap::new())
            .await
            .unwrap();
        assert!(
            !off2.stdout.contains("CONNECTED"),
            "turning the network back off did not take effect on the next run"
        );
        assert!(!off2.network);
    }

    #[tokio::test]
    async fn a_direct_run_never_waits_on_the_docker_probe() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        if !caps::direct_caps(true).await.kind.is_enforced() {
            return;
        }
        // Clear both caches so the run below would pay the Docker probe if the
        // code still asked for it.
        let _ = caps::probe(true).await;
        let sb = create(&db, req(Some("direct".into()))).await.unwrap();

        let started = std::time::Instant::now();
        let run = run_code(&db, &sb, "python", "print(1)", None, BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(run.exit_code, Some(0), "stderr: {}", run.stderr);
        // The Docker probe's own timeout is 4s. A direct run that took anywhere
        // near that is waiting on a daemon it does not use — which is exactly
        // the regression this guards, and it is invisible without timing it.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "a direct run took {:?} — it is probably probing Docker",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_mounted_folder_is_readable_and_writable_from_inside() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let host = tempfile::tempdir().unwrap();
        let shared = host.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::write(shared.join("in.txt"), "real data").unwrap();

        let mut r = req(Some("direct".into()));
        r.mounts = vec![crate::sandbox::mounts::validate(shared.to_str().unwrap(), "shared", false).unwrap()];
        let caps = caps::probe(true).await;
        if !caps.direct.kind.is_enforced() {
            eprintln!("SKIP: this machine has no enforced direct isolation");
            return;
        }
        let sb = create(&db, r).await.unwrap();

        let run = run_code(
            &db,
            &sb,
            "python",
            "print(open('shared/in.txt').read())\n\
             open('shared/out.txt','w').write('from the sandbox')",
            None,
            BTreeMap::new(),
        )
        .await
        .unwrap();

        assert_eq!(run.exit_code, Some(0), "stderr: {}", run.stderr);
        assert!(run.stdout.contains("real data"), "mount was not readable");
        assert_eq!(
            std::fs::read_to_string(shared.join("out.txt")).unwrap(),
            "from the sandbox",
            "a read-write mount must let the sandbox write back to the real folder"
        );
    }

    #[tokio::test]
    async fn a_read_only_mount_refuses_writes() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let host = tempfile::tempdir().unwrap();
        let shared = host.path().join("readonly");
        std::fs::create_dir(&shared).unwrap();
        std::fs::write(shared.join("orig.txt"), "must not be modified").unwrap();

        let caps = caps::probe(true).await;
        if !caps.direct.kind.is_enforced() {
            return;
        }
        let mut r = req(Some("direct".into()));
        r.mounts = vec![crate::sandbox::mounts::validate(shared.to_str().unwrap(), "readonly", true).unwrap()];
        let sb = create(&db, r).await.unwrap();

        let run = run_code(
            &db,
            &sb,
            "python",
            "print(open('readonly/orig.txt').read())\n\
             try:\n    open('readonly/orig.txt','w').write('modified')\n    print('WROTE')\n\
             except Exception as e:\n    print('REFUSED')",
            None,
            BTreeMap::new(),
        )
        .await
        .unwrap();

        assert!(run.stdout.contains("must not be modified"), "stderr: {}", run.stderr);
        assert_eq!(
            std::fs::read_to_string(shared.join("orig.txt")).unwrap(),
            "must not be modified",
            "a read-only mount was written through"
        );
        assert!(!run.stdout.contains("WROTE"));
    }

    #[tokio::test]
    async fn the_file_browser_can_walk_into_a_mount_but_not_past_it() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        let host = tempfile::tempdir().unwrap();
        let shared = host.path().join("browse");
        std::fs::create_dir(&shared).unwrap();
        std::fs::write(shared.join("a.txt"), "hello").unwrap();
        // A sibling of the mount, deliberately NOT mounted.
        std::fs::write(host.path().join("outside.txt"), "secret").unwrap();

        let mut r = req(Some("direct".into()));
        r.mounts = vec![crate::sandbox::mounts::validate(shared.to_str().unwrap(), "browse", false).unwrap()];
        let sb = create(&db, r).await.unwrap();
        crate::sandbox::mounts::materialise_symlinks(&PathBuf::from(&sb.workdir), &sb.mounts).unwrap();

        let scope = crate::sandbox::files::Scope::of(&sb);
        // Into the mount: allowed, because it was declared.
        assert_eq!(crate::sandbox::files::read(&scope, "browse/a.txt").unwrap(), "hello");
        // One step beyond it: still refused.
        assert!(
            crate::sandbox::files::read(&scope, "browse/../outside.txt").is_err(),
            "a mount must not become a door to its parent directory"
        );
    }

    #[tokio::test]
    async fn run_once_cleans_up_its_throwaway_sandbox() {
        let _d = tmp_data();
        let db = Db::open_memory().unwrap();
        if !caps::probe(true).await.direct.kind.is_enforced() {
            return;
        }
        let (run, sb) = run_once(&db, "python", "print('hi')", Some("direct".into()), false, None)
            .await
            .unwrap();
        assert_eq!(run.stdout.trim(), "hi");
        assert!(db.sandbox(&sb.id).is_err(), "the throwaway sandbox outlived the run");
        assert!(!PathBuf::from(&sb.workdir).exists(), "its files were left behind");
        // History goes with the sandbox — the output survives in the returned
        // value, which is what the caller actually reads.
        assert!(db.run(&run.id).is_err(), "a throwaway run should not linger in history");
    }

    fn req(backend: Option<String>) -> CreateReq {
        CreateReq {
            name: None,
            backend,
            image: None,
            network: false,
            cpus: None,
            memory_mb: None,
            timeout_ms: None,
            env: json!({}),
            mounts: Vec::new(),
            fs_mode: None,
            ports: Default::default(),
        }
    }

    // (`req` is mutated by the fs-mode tests above via `r.fs_mode = …`.)
}
