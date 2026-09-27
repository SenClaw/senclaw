//! `MCPDispatcher` — a generic autonomous work dispatcher. It drives any number of
//! [`DispatchSource`]s (a Kanban board, a review queue, …): each tick it reclaims
//! dead workers, claims ready items, and runs one persona worker agent per item via
//! [`run_one_shot`], then reports the outcome back to the source.
//!
//! The engine knows nothing about Kanban — everything source-specific is behind the
//! SDK's `DispatchSource` trait (see `app_space_sdk::dispatch`). See
//! `docs/mcp-dispatcher-design.md`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use app_space_sdk::dispatch::{
    Capacity, DispatchSource, McpServerSpec, Outcome, WorkItem, Workspace,
};

use crate::agent::isolated_runner::{run_one_shot, McpInject, OneShotOptions};
use crate::agent::persona_registry::PersonaRegistry;
use crate::zen_core::McpServerConfig;

/// Runtime knobs for the dispatcher.
pub struct DispatcherConfig {
    /// Poll cadence (clamped to a 5s floor).
    pub interval_secs: u64,
    /// Max worker agents running at once across all sources.
    pub max_concurrent: usize,
    /// Max concurrent items per assignee (passed to sources; they enforce it).
    pub per_assignee: usize,
    /// Cap on a worker's agent turns (None = runtime default).
    pub max_agent_turns: Option<usize>,
    /// Per-item run timeout when the item doesn't specify one.
    pub default_timeout_secs: u64,
    /// Root for scratch workspaces.
    pub workdir_root: PathBuf,
    /// Global config path — the `dispatchEnabled` flag is read from here each
    /// tick, so the Settings toggle takes effect live.
    pub config_path: PathBuf,
}

pub struct MCPDispatcher {
    sources: Vec<Arc<dyn DispatchSource>>,
    personas: Arc<Mutex<PersonaRegistry>>,
    cfg: DispatcherConfig,
    active: AtomicUsize,
}

impl MCPDispatcher {
    pub fn new(
        sources: Vec<Arc<dyn DispatchSource>>,
        personas: Arc<Mutex<PersonaRegistry>>,
        cfg: DispatcherConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            sources,
            personas,
            cfg,
            active: AtomicUsize::new(0),
        })
    }

    /// Spawn the poll loop. The spawned task holds an `Arc<Self>`, so the loop
    /// runs for the life of the process.
    pub fn start(self: &Arc<Self>) {
        let this = Arc::clone(self);
        let interval = Duration::from_secs(this.cfg.interval_secs.max(5));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.tick().await; // skip the immediate first tick
            loop {
                tick.tick().await;
                this.tick().await;
            }
        });
        tracing::info!(
            "[mcp-dispatch] started — {} source(s), max_concurrent={}",
            self.sources.len(),
            self.cfg.max_concurrent
        );
    }

    async fn tick(self: &Arc<Self>) {
        // Live on/off — read the persisted Settings toggle each tick.
        if !crate::gateway::group_manager::get_dispatch_enabled(&self.cfg.config_path) {
            return;
        }
        for src in &self.sources {
            if let Err(e) = src.reclaim().await {
                tracing::warn!("[mcp-dispatch] {} reclaim: {e}", src.id());
            }
            let free = self
                .cfg
                .max_concurrent
                .saturating_sub(self.active.load(Ordering::SeqCst));
            if free == 0 {
                continue;
            }
            let cap = Capacity {
                total: free,
                per_assignee: self.cfg.per_assignee,
            };
            let items = match src.poll_ready(cap).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("[mcp-dispatch] {} poll: {e}", src.id());
                    continue;
                }
            };
            for item in items {
                self.active.fetch_add(1, Ordering::SeqCst);
                let me = Arc::clone(self);
                let src = Arc::clone(src);
                tokio::spawn(async move {
                    me.run_item(src, item).await;
                    me.active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        }
    }

    async fn run_item(&self, src: Arc<dyn DispatchSource>, item: WorkItem) {
        tracing::info!(
            "[mcp-dispatch] {} → run {} (assignee={:?})",
            src.id(),
            item.id,
            item.assignee
        );
        // Extend the lease while the worker runs.
        let hb = {
            let src = Arc::clone(&src);
            let id = item.id.clone();
            tokio::spawn(async move {
                let mut t = tokio::time::interval(Duration::from_secs(60));
                t.tick().await;
                loop {
                    t.tick().await;
                    let _ = src.heartbeat(&id).await;
                }
            })
        };
        let outcome = self.execute(&item).await;
        hb.abort();
        if let Err(e) = src.finalize(&item.id, outcome).await {
            tracing::warn!("[mcp-dispatch] {} finalize {}: {e}", src.id(), item.id);
        }
    }

    /// Run one item as a tool-enabled agent and map the result to an [`Outcome`].
    async fn execute(&self, item: &WorkItem) -> Outcome {
        // Resolve the persona (clone out from behind the lock).
        let persona = {
            let reg = self.personas.lock().unwrap();
            item.assignee.as_deref().and_then(|a| reg.get(a).cloned())
        };
        let (system_prompt, use_tools) = match persona {
            Some(p) => {
                let sp = match &item.guidance {
                    Some(g) => format!("{}\n\n{}", p.system_prompt, g),
                    None => p.system_prompt.clone(),
                };
                (Some(sp), p.tools.unwrap_or_default())
            }
            None => (item.guidance.clone(), Vec::new()),
        };

        let (working_dir, worktree) = match self.resolve_workspace(item) {
            Ok(d) => d,
            Err(e) => {
                return Outcome::Failed {
                    error: format!("workspace: {e}"),
                }
            }
        };
        let timeout =
            Duration::from_secs(item.timeout_secs.unwrap_or(self.cfg.default_timeout_secs));

        let opts = OneShotOptions {
            prompt: item.prompt.clone(),
            working_dir,
            use_tools,
            system_prompt,
            mcp_configs: self.build_mcp(&item.mcp),
            timeout: Some(timeout),
            max_agent_turns: self.cfg.max_agent_turns,
            ..Default::default()
        };

        match run_one_shot(opts).await {
            Ok(r) if r.timed_out => Outcome::TimedOut,
            Ok(r) if r.errored || r.aborted => Outcome::Failed {
                error: r.error_message.unwrap_or_else(|| "agent error".into()),
            },
            Ok(r) => {
                let mut summary = r
                    .text
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let mut metadata = serde_json::json!({ "turns": r.turn_count, "secs": r.duration.as_secs() });
                if let Some(wt) = &worktree {
                    // The board's checkout is untouched; say where the work is.
                    let wt_summary = crate::worktree::summary(std::path::Path::new(&wt.path));
                    summary = format!("{summary} — {wt_summary}");
                    metadata["worktree"] = serde_json::json!({
                        "path": wt.path,
                        "branch": wt.branch,
                        "base": wt.base,
                        "summary": wt_summary,
                    });
                }
                Outcome::Completed { summary, metadata }
            }
            Err(e) => Outcome::Failed {
                error: e.to_string(),
            },
        }
    }

    /// Turn SDK MCP specs into injectable configs. Only native `Stdio` servers are
    /// supported (the built-in Kanban uses `senclaw kanban-server`); `Http` specs
    /// are skipped with a warning — the stdio↔HTTP bridge was removed when Kanban
    /// moved in-process.
    fn build_mcp(&self, specs: &[McpServerSpec]) -> Vec<McpInject> {
        specs
            .iter()
            .filter_map(|s| match s {
                McpServerSpec::Stdio { name, command, args, env } => Some(McpInject {
                    config: McpServerConfig {
                        name: name.clone(),
                        command: command.clone(),
                        args: args.clone(),
                        env: env.clone(),
                        request_timeout_secs: Some(300),
                    },
                    scope: "dispatch".into(),
                }),
                McpServerSpec::Http { name, .. } => {
                    tracing::warn!(
                        "[mcp-dispatch] skipping HTTP MCP spec '{name}' — only stdio servers are supported"
                    );
                    None
                }
            })
            .collect()
    }

    /// The directory a worker runs in, plus the worktree it was given (so the
    /// outcome can report the branch). A `Worktree` request in a folder that
    /// is not a git repository runs in the folder itself — with a warning, not
    /// a failure, because the card's work still has to happen somewhere.
    fn resolve_workspace(&self, item: &WorkItem) -> std::io::Result<(String, Option<crate::worktree::WorktreeInfo>)> {
        match &item.workspace {
            Workspace::Dir { path } => Ok((path.clone(), None)),
            Workspace::Worktree { repo, branch } => {
                let name = branch
                    .as_deref()
                    .map(|b| b.trim_start_matches(crate::worktree::BRANCH_PREFIX).to_string())
                    .unwrap_or_else(|| format!("item-{}", item.id));
                match crate::worktree::create(std::path::Path::new(repo), &name, &format!("dispatch:{}", item.id)) {
                    Ok(info) => Ok((info.path.clone(), Some(info))),
                    Err(e) => {
                        tracing::warn!(item = %item.id, error = %e, "[mcp-dispatch] worktree unavailable; running in the repository directory");
                        Ok((repo.clone(), None))
                    }
                }
            }
            Workspace::Scratch => {
                let safe: String = item
                    .id
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
                    .collect();
                let dir = self
                    .cfg
                    .workdir_root
                    .join(if safe.is_empty() { "item" } else { &safe });
                std::fs::create_dir_all(&dir)?;
                Ok((dir.to_string_lossy().to_string(), None))
            }
        }
    }
}
