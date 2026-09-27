//! Executes one background task: resolve its prompt, run it, record it.
//!
//! Built on [`crate::agent::isolated_runner::run_one_shot`] rather than
//! `VirtualWorkerPool::run`, because the pool injects MCP servers *globally*
//! (`set_extra_mcp_servers`) — unsafe for concurrent heterogeneous background
//! tasks. `run_one_shot` injects per call. (`AgentPool::run_isolated` is a dead
//! stub whose "wait for idle" returns immediately; do not use it.)

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use chrono::Utc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{BackgroundEventSink, NativeRegistry};
use crate::agent::isolated_runner::{run_one_shot, McpInject, OnActivity, OneShotOptions};
use crate::agent::persona_registry::PersonaRegistry;
use crate::config::BackgroundConfig;
use crate::db::Db;
use crate::types::{
    BackgroundContinuity, BackgroundJobKind, BackgroundPromptKind, BackgroundRun,
    BackgroundRunStatus, BackgroundTask, BackgroundTriggerKind,
};
use crate::zen_core::{AgentMode, McpServerConfig};

/// How many prior run summaries `continuity = thread` injects.
const THREAD_CONTEXT_RUNS: i64 = 5;
/// Cap on a single injected summary, so a chatty run can't blow the context.
const THREAD_SUMMARY_CHARS: usize = 600;

pub struct BackgroundRunner {
    pub(crate) db: Arc<Db>,
    pub(crate) cfg: BackgroundConfig,
    pub(crate) personas: Option<Arc<std::sync::Mutex<PersonaRegistry>>>,
    pub(crate) events: Arc<dyn BackgroundEventSink>,
    pub(crate) native: Arc<NativeRegistry>,
    /// Fallback working dir when a task declares none.
    pub(crate) scratch_dir: String,
}

/// Outcome of one run, handed back to the scheduler for backoff bookkeeping.
pub struct RunOutcome {
    pub run_id: String,
    pub status: BackgroundRunStatus,
}

impl BackgroundRunner {
    /// Run a task to completion and record everything about it.
    ///
    /// Never returns `Err` for a task-level failure — a failed run is a
    /// recorded outcome, not an error of the runner. `Err` is reserved for
    /// bookkeeping that itself broke.
    pub async fn execute(
        &self,
        task: &BackgroundTask,
        trigger: BackgroundTriggerKind,
        cancel: CancellationToken,
    ) -> Result<RunOutcome> {
        let run_id = Uuid::new_v4().to_string();
        let session_id = format!("bg:{run_id}");
        let started = Instant::now();
        let started_at = Utc::now().to_rfc3339();

        self.db.insert_background_run(&BackgroundRun {
            id: run_id.clone(),
            task_id: task.id.clone(),
            session_id: session_id.clone(),
            trigger_kind: trigger,
            status: BackgroundRunStatus::Running,
            started_at: started_at.clone(),
            finished_at: None,
            duration_ms: None,
            turn_count: None,
            tokens_in: None,
            tokens_out: None,
            prompt: None,
            result: None,
            error: None,
        })?;
        self.db.mark_background_task_run(&task.id, &started_at)?;
        self.events.run_started(task, &run_id, trigger);

        tracing::info!(
            task_id = %task.id, run_id = %run_id, title = %task.title,
            trigger = trigger.as_str(), "[background] run started"
        );

        let outcome = self.run_body(task, &run_id, cancel).await;

        let duration_ms = started.elapsed().as_millis() as i64;
        let (status, error) = match outcome {
            Ok(Body::Ran {
                prompt,
                text,
                turns,
                tokens,
            }) => {
                self.db.finish_background_run(
                    &run_id,
                    BackgroundRunStatus::Success,
                    Some(&prompt),
                    Some(&text),
                    None,
                    duration_ms,
                    turns,
                    Some((tokens.0 as i64, tokens.1 as i64)),
                )?;
                (BackgroundRunStatus::Success, None)
            }
            Ok(Body::Skipped { reason }) => {
                // The reason goes in `result`, not `error`: a skip is an
                // outcome, not a fault, and it must not read as one in the UI.
                self.db.finish_background_run(
                    &run_id,
                    BackgroundRunStatus::Skipped,
                    None,
                    Some(&reason),
                    None,
                    duration_ms,
                    None,
                    None,
                )?;
                (BackgroundRunStatus::Skipped, None)
            }
            Ok(Body::Failed { status, message }) => {
                self.db.finish_background_run(
                    &run_id,
                    status,
                    None,
                    None,
                    Some(&message),
                    duration_ms,
                    None,
                    None,
                )?;
                (status, Some(message))
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.db.finish_background_run(
                    &run_id,
                    BackgroundRunStatus::Error,
                    None,
                    None,
                    Some(&msg),
                    duration_ms,
                    None,
                    None,
                )?;
                (BackgroundRunStatus::Error, Some(msg))
            }
        };

        // A skip is not a failure — a `template` task with nothing to do is
        // healthy, and counting it would quarantine the quietest tasks first.
        // A deliberate cancel isn't the task's fault either.
        if status.is_failure() {
            let quarantined = self.db.record_background_failure(&task.id)?;
            if quarantined {
                tracing::warn!(
                    task_id = %task.id, title = %task.title,
                    "[background] auto-paused after {} consecutive failures",
                    task.max_failures
                );
                if let Some(t) = self.db.get_background_task(&task.id)? {
                    self.events.task_changed(&t);
                }
            }
        } else if status == BackgroundRunStatus::Success {
            self.db.reset_background_failures(&task.id)?;
        }

        self.events
            .run_finished(&task.id, &run_id, status, duration_ms, error.as_deref());
        tracing::info!(
            task_id = %task.id, run_id = %run_id, status = status.as_str(),
            duration_ms, "[background] run finished"
        );

        Ok(RunOutcome { run_id, status })
    }

    async fn run_body(
        &self,
        task: &BackgroundTask,
        run_id: &str,
        cancel: CancellationToken,
    ) -> Result<Body> {
        if task.job_kind == BackgroundJobKind::Native {
            return self.run_native(task, cancel).await;
        }

        // Notify-only task: deliver an OS notification and stop. No agent — the
        // message is the prompt, so spinning up an LLM (which has no
        // notification tool and just flails, as the "nhắc tôi 2 phút" task did)
        // adds cost, latency, and a failure mode for nothing.
        if task.notify {
            let message = task.prompt.clone().unwrap_or_default();
            let message = message.trim();
            if message.is_empty() {
                return Ok(Body::Failed {
                    status: BackgroundRunStatus::Error,
                    message: "notify task has no message".into(),
                });
            }
            self.events.notify(&task.title, message);
            return Ok(Body::Ran {
                prompt: message.to_owned(),
                text: format!("đã gửi thông báo: {message}"),
                turns: Some(0),
                tokens: (0, 0),
            });
        }

        let prompt = match self.resolve_prompt(task, run_id, cancel.clone()).await? {
            Resolved::Prompt(p) => p,
            Resolved::Skip(reason) => return Ok(Body::Skipped { reason }),
        };

        let (system_prompt, persona_tools) = self.resolve_persona(task)?;
        let use_tools = if !task.use_tools.is_empty() {
            task.use_tools.clone()
        } else {
            persona_tools
        };

        let opts = OneShotOptions {
            prompt: prompt.clone(),
            working_dir: task
                .workspace_dir
                .clone()
                .unwrap_or_else(|| self.scratch_dir.clone()),
            instance_id: Some(format!("bg:{run_id}")),
            use_tools,
            system_prompt,
            custom_rules: self.continuity_context(task)?,
            agent_mode: AgentMode::Agent,
            mcp_configs: self.resolve_mcp(task),
            timeout: Some(Duration::from_secs(
                task.timeout_secs
                    .map(|s| s as u64)
                    .unwrap_or(self.cfg.default_timeout_secs),
            )),
            max_agent_turns: Some(
                task.max_turns
                    .map(|t| t as usize)
                    .unwrap_or(self.cfg.max_agent_turns),
            ),
            model_config_id: task.model_id.clone(),
            cancel: Some(cancel),
            on_activity: Some(self.activity_sink(task, run_id)),
            ..Default::default()
        };

        let res = run_one_shot(opts).await?;

        if res.aborted {
            return Ok(Body::Failed {
                status: BackgroundRunStatus::Cancelled,
                message: "run cancelled".into(),
            });
        }
        if res.timed_out {
            return Ok(Body::Failed {
                status: BackgroundRunStatus::Timeout,
                message: format!(
                    "timed out after {}s",
                    task.timeout_secs
                        .map(|s| s as u64)
                        .unwrap_or(self.cfg.default_timeout_secs)
                ),
            });
        }
        if res.errored {
            return Ok(Body::Failed {
                status: BackgroundRunStatus::Error,
                message: res
                    .error_message
                    .unwrap_or_else(|| "agent reported an error".into()),
            });
        }

        Ok(Body::Ran {
            prompt,
            text: res.text,
            turns: Some(res.turn_count as i64),
            tokens: (res.tokens_in, res.tokens_out),
        })
    }

    async fn run_native(&self, task: &BackgroundTask, cancel: CancellationToken) -> Result<Body> {
        let key = task
            .native_job
            .as_deref()
            .ok_or_else(|| anyhow!("native task has no native_job key"))?;
        let Some(job) = self.native.get(key) else {
            // Honest failure rather than a silent no-op: a native row whose key
            // isn't registered means a boot-order bug, and it should be visible.
            return Ok(Body::Failed {
                status: BackgroundRunStatus::Error,
                message: format!("native job '{key}' is not registered"),
            });
        };
        match job(cancel).await {
            Ok(summary) => Ok(Body::Ran {
                prompt: format!("[native] {key}"),
                text: summary,
                turns: None,
                tokens: (0, 0),
            }),
            Err(e) => Ok(Body::Failed {
                status: BackgroundRunStatus::Error,
                message: format!("{e:#}"),
            }),
        }
    }

    /// static | template+contextUrl | generator → the prompt actually sent.
    async fn resolve_prompt(
        &self,
        task: &BackgroundTask,
        run_id: &str,
        cancel: CancellationToken,
    ) -> Result<Resolved> {
        let raw = task.prompt.clone().unwrap_or_default();
        if raw.trim().is_empty() && task.prompt_kind != BackgroundPromptKind::Template {
            return Err(anyhow!("task has an empty prompt"));
        }

        match task.prompt_kind {
            BackgroundPromptKind::Static => Ok(Resolved::Prompt(raw)),

            BackgroundPromptKind::Template => {
                let url = task
                    .context_url
                    .as_deref()
                    .ok_or_else(|| anyhow!("template prompt has no context_url"))?;
                let vars = self.fetch_context(url, task).await?;
                let obj = match &vars {
                    serde_json::Value::Object(m) => m.clone(),
                    _ => return Err(anyhow!("context_url must return a JSON object")),
                };

                // Nothing to do → skip, and cost zero tokens. This is the whole
                // reason `template` is the recommended shape for App tasks.
                if is_empty_context(&obj) {
                    return Ok(Resolved::Skip(format!(
                        "nothing to do (context from {url} was empty)"
                    )));
                }
                Ok(Resolved::Prompt(render_template(&raw, &obj)))
            }

            BackgroundPromptKind::Generator => {
                // One tool-less turn whose output becomes the real prompt.
                // Doubles the token cost and can hallucinate its own task, so
                // the skill steers callers to `template` where possible.
                let res = run_one_shot(OneShotOptions {
                    prompt: raw,
                    working_dir: self.scratch_dir.clone(),
                    instance_id: Some(format!("bg:{run_id}:gen")),
                    max_agent_turns: Some(1),
                    model_config_id: task.model_id.clone(),
                    timeout: Some(Duration::from_secs(120)),
                    cancel: Some(cancel),
                    ..Default::default()
                })
                .await?;
                let text = res.text.trim().to_owned();
                if text.is_empty() {
                    return Ok(Resolved::Skip("generator produced no prompt".into()));
                }
                Ok(Resolved::Prompt(text))
            }
        }
    }

    async fn fetch_context(&self, url: &str, task: &BackgroundTask) -> Result<serde_json::Value> {
        // `since` lets the App decide what a missed window means — it knows its
        // own data far better than the scheduler does (design §16 q5).
        let url = match &task.last_run {
            Some(last) if !url.contains('?') => format!("{url}?since={last}"),
            Some(last) => format!("{url}&since={last}"),
            None => url.to_owned(),
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(anyhow!("context_url {url} returned HTTP {}", resp.status()));
        }
        Ok(resp.json().await?)
    }

    fn resolve_persona(&self, task: &BackgroundTask) -> Result<(Option<String>, Vec<String>)> {
        let Some(name) = task.persona.as_deref() else {
            return Ok((None, Vec::new()));
        };
        let Some(reg) = &self.personas else {
            return Ok((None, Vec::new()));
        };
        // Clone out from behind the lock: a std Mutex guard can't cross an
        // await point, and callers of this do await afterwards.
        let found = {
            let guard = reg.lock().unwrap();
            guard.get(name).cloned()
        };
        match found {
            Some(p) => Ok((Some(p.system_prompt), p.tools.unwrap_or_default())),
            None => {
                tracing::warn!(
                    task_id = %task.id, persona = name,
                    "[background] persona not found; running without it"
                );
                Ok((None, Vec::new()))
            }
        }
    }

    /// Per-run MCP injection. A bad spec degrades to "no extra MCP" rather than
    /// failing the run — the prompt may well not need it, and a hard failure
    /// here would quarantine the task for a config typo.
    fn resolve_mcp(&self, task: &BackgroundTask) -> Vec<McpInject> {
        let Some(raw) = task.mcp_json.as_deref() else {
            return Vec::new();
        };
        match serde_json::from_str::<Vec<McpSpec>>(raw) {
            Ok(v) => v
                .into_iter()
                .map(|s| McpInject {
                    config: s.config,
                    scope: s.scope,
                })
                .collect(),
            Err(e) => {
                tracing::warn!(
                    task_id = %task.id, error = %e,
                    "[background] mcp spec is not valid JSON; running without extra MCP"
                );
                Vec::new()
            }
        }
    }

    /// `continuity = thread`: inject recent run summaries.
    ///
    /// A background task has no chat history to accumulate, so this is its only
    /// memory of itself. Without it, a follow-up task contacts the same
    /// customer every single day.
    fn continuity_context(&self, task: &BackgroundTask) -> Result<Option<String>> {
        if task.continuity != BackgroundContinuity::Thread {
            return Ok(None);
        }
        let runs = self
            .db
            .list_background_runs(&task.id, THREAD_CONTEXT_RUNS)?;
        let summaries: Vec<String> = runs
            .iter()
            .filter(|r| r.status == BackgroundRunStatus::Success)
            .filter_map(|r| r.result.as_deref().map(|t| (r.started_at.as_str(), t)))
            .map(|(at, text)| {
                let clipped =
                    crate::util::text::truncate_on_char_boundary(text, THREAD_SUMMARY_CHARS);
                format!("- {at}: {clipped}")
            })
            .collect();
        if summaries.is_empty() {
            return Ok(None);
        }
        Ok(Some(format!(
            "## What you did on previous runs\n\n{}\n\nDo not repeat work already done above.",
            summaries.join("\n")
        )))
    }

    /// Persist + push every activity line. This is the background-session
    /// transcript — the only way anyone can see what an unattended run did.
    fn activity_sink(&self, task: &BackgroundTask, run_id: &str) -> OnActivity {
        let db = self.db.clone();
        let events = self.events.clone();
        let run_id = run_id.to_owned();
        let task_id = task.id.clone();
        OnActivity(Arc::new(move |kind: &str, detail: &str| {
            if let Err(e) = db.insert_background_activity(&run_id, kind, detail) {
                tracing::debug!(error = %e, "[background] activity insert failed");
            }
            events.run_activity(&task_id, &run_id, kind, detail);
        }))
    }
}

/// Deserializable form of [`McpInject`], since `McpInject` itself isn't serde.
#[derive(serde::Deserialize)]
struct McpSpec {
    #[serde(flatten)]
    config: McpServerConfig,
    #[serde(default = "default_mcp_scope")]
    scope: String,
}

fn default_mcp_scope() -> String {
    "background".to_owned()
}

enum Body {
    Ran {
        prompt: String,
        text: String,
        turns: Option<i64>,
        /// Billed token totals from the one-shot agent run (0/0 for
        /// notify-only tasks, which never call an LLM).
        tokens: (u64, u64),
    },
    Skipped {
        reason: String,
    },
    Failed {
        status: BackgroundRunStatus,
        message: String,
    },
}

enum Resolved {
    Prompt(String),
    Skip(String),
}

/// A context object counts as empty when every value is empty — `{}`,
/// `{"customers": []}`, `{"items": ""}` all mean "nothing to do".
fn is_empty_context(obj: &serde_json::Map<String, serde_json::Value>) -> bool {
    obj.values().all(|v| match v {
        serde_json::Value::Null => true,
        serde_json::Value::Array(a) => a.is_empty(),
        serde_json::Value::Object(o) => o.is_empty(),
        serde_json::Value::String(s) => s.trim().is_empty(),
        _ => false,
    })
}

/// `{{var}}` substitution. Non-string values are rendered as compact JSON so a
/// list of customers arrives as a list, not as `[object Object]`.
fn render_template(template: &str, vars: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut out = template.to_owned();
    for (k, v) in vars {
        let rendered = match v {
            serde_json::Value::String(s) => s.clone(),
            other => serde_json::to_string_pretty(other).unwrap_or_default(),
        };
        out = out.replace(&format!("{{{{{k}}}}}"), &rendered);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn empty_context_covers_the_shapes_an_app_actually_returns() {
        assert!(is_empty_context(&obj(json!({}))));
        assert!(is_empty_context(&obj(json!({ "customers": [] }))));
        assert!(is_empty_context(&obj(json!({ "items": "   " }))));
        assert!(is_empty_context(&obj(json!({ "a": null, "b": {} }))));
        // One non-empty value is enough to make the run worthwhile.
        assert!(!is_empty_context(&obj(json!({ "customers": ["an"] }))));
        assert!(!is_empty_context(&obj(json!({ "a": [], "b": ["x"] }))));
        // A bare number is data, not emptiness.
        assert!(!is_empty_context(&obj(json!({ "count": 0 }))));
    }

    #[test]
    fn template_renders_scalars_bare_and_structures_as_json() {
        let vars = obj(json!({
            "name": "An",
            "customers": [{ "id": 1 }],
        }));
        let out = render_template("Hi {{name}}, follow up:\n{{customers}}", &vars);
        assert!(out.starts_with("Hi An, follow up:"));
        assert!(out.contains("\"id\": 1"));
        // Strings must not gain JSON quotes — they're prose in a prompt.
        assert!(!out.contains("\"An\""));
    }

    #[test]
    fn unknown_placeholders_survive_rather_than_blanking() {
        // Leaving `{{missing}}` visible makes a template bug obvious in the run
        // record; silently emptying it would produce a plausible-looking prompt
        // that quietly means something else.
        let out = render_template("a {{missing}} b", &obj(json!({ "other": "x" })));
        assert_eq!(out, "a {{missing}} b");
    }
}
